//! App-specific transports implement this boundary; the core owns identity and delivery claims.
use crate::{
    attachments,
    config::{Config, atomic, json_file, private_dir},
    event::{Event, Plan, digest},
    store::Store,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{future::Future, io::Read, path::Path, time::Duration};

pub trait Adapter: Send + Sync {
    fn provider(&self) -> &str;
    fn send(
        &self,
        store: &Store,
        key: &str,
        event: &Event,
        plan: &Plan,
    ) -> impl Future<Output = Result<Value>> + Send;
}
#[derive(Clone)]
pub struct Kakao {
    pub cfg: Config,
}
pub fn nonce() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(digest(bytes))
}
impl Adapter for Kakao {
    fn provider(&self) -> &str {
        "kakao"
    }
    async fn send(&self, store: &Store, key: &str, e: &Event, plan: &Plan) -> Result<Value> {
        self.cfg
            .validate_channel(&e.conversation.provider, &e.conversation.account)?;
        if !plan.reply.starts_with(crate::event::PREFIX) {
            bail!("invalid_reply_prefix")
        }
        let route = store.route(&e.conversation)?;
        let name = route.as_deref().unwrap_or(&e.title);
        if name.is_empty() {
            return Ok(json!({"status":"held","reason":"missing_target"}));
        }
        let mut p = json!({"chat_name":name,"room_name_verified":route.is_some(),"trigger_body":e.body,"reply":plan.reply,"expires_at":crate::event::now()+75.0});
        if let Some(id) = &plan.bundle_id {
            // Pure staging work may be blocking; no UI/model call occurs during ZIP processing.
            let cfg = self.cfg.clone();
            let event = e.clone();
            let key = key.to_owned();
            let id = id.clone();
            let artifact = match tokio::task::spawn_blocking(move || {
                attachments::prepare(&cfg, &event, &key, &id)
            })
            .await
            {
                Ok(Ok(v)) => v,
                _ => return Ok(json!({"status":"held","reason":"attachment_validation_failed"})),
            };
            p["attachment_path"] = artifact["path"].clone();
        }
        let request = nonce()?;
        let base = &self.cfg.kakao.sender_ipc;
        private_dir(base)?;
        let path = base.join("sender-requests").join(format!("{request}.json"));
        atomic(&path, &p)?;
        let receipt = base.join("sender-receipts").join(format!("{request}.json"));
        let result = self.invoke_sender(&request, &receipt).await;
        let _ = std::fs::remove_file(&path);
        let mut result = result?;
        if result["status"] == "sending" {
            result["status"] = json!("sending_uncertain")
        }
        if !matches!(
            result["status"].as_str(),
            Some("sent_verified" | "held" | "partial_file_held" | "sending_uncertain")
        ) {
            return Ok(json!({"status":"sending_uncertain","reason":"unknown_native_result"}));
        }
        if result["status"] == "sent_verified"
            && (result["text_sent"] != true
                || result["verified_chat_name"]
                    .as_str()
                    .is_none_or(str::is_empty)
                || plan.bundle_id.is_some() && result["attachment_sent"] != true)
        {
            return Ok(
                json!({"status":"sending_uncertain","reason":"incomplete_native_verification"}),
            );
        }
        if let Some(name) = result["verified_chat_name"].as_str() {
            if route.as_ref().is_some_and(|expected| expected != name) {
                return Ok(json!({"status":"sending_uncertain","reason":"native_target_mismatch"}));
            }
            store.save_route(&e.conversation, name)?;
        }
        store.note_intro(&e.conversation, &plan.reply, &result)?;
        Ok(result)
    }
}
impl Kakao {
    async fn invoke_sender(&self, request: &str, receipt: &Path) -> Result<Value> {
        let launch = tokio::time::timeout(
            Duration::from_secs(8),
            tokio::process::Command::new("/usr/bin/open")
                .args(["-n", "-g"])
                .arg(&self.cfg.kakao.sender_app)
                .args(["--args", "--request", request, "--ipc-dir"])
                .arg(&self.cfg.kakao.sender_ipc)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        if !matches!(launch,Ok(Ok(status)) if status.success()) {
            return Ok(
                json!({"status":"sending_uncertain","reason":"sender_launch_or_transport_uncertain"}),
            );
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            if receipt.exists() {
                return json_file(receipt);
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // A stale phase file cannot prove that UI input/send has not occurred.
        // Preserve uncertainty; never terminate/retry based on that file.
        Ok(json!({"status":"sending_uncertain","reason":"sender_completion_not_observed"}))
    }
}
