//! App-specific transports implement this boundary; the core owns identity and delivery claims.
use crate::{
    attachments,
    config::{Config, atomic, json_file, private_dir},
    event::{Event, Plan, digest},
    expressions,
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
        if !plan.reply.starts_with(e.agent.prefix()) {
            bail!("invalid_reply_prefix")
        }
        expressions::validate_plan(plan)?;
        let route = store.route(&e.conversation)?;
        let name = route.as_deref().unwrap_or(&e.title);
        if name.is_empty() {
            return Ok(json!({"status":"held","reason":"missing_target"}));
        }
        let mut p = json!({"chat_name":name,"room_name_verified":route.is_some(),"trigger_body":e.body,"reply":plan.reply,"expires_at":crate::event::now()+75.0});
        // Staging is local only; the sender pastes the image after the text receipt in the same run,
        // so the second launch and second target scan of a separate attachment request disappear.
        let mut sticker = Value::Null;
        if let Some(id) = &plan.sticker_id {
            let cfg = self.cfg.clone();
            let (job, selected) = (key.to_owned(), id.clone());
            let staged =
                tokio::task::spawn_blocking(move || expressions::prepare(&cfg, &job, &selected))
                    .await;
            // A sticker is decoration: any staging problem degrades to a text-only reply.
            if let Ok(Ok(mut artifact)) = staged
                && store.reserve_expression(
                    key,
                    &e.conversation,
                    artifact["sha256"].as_str().unwrap(),
                    artifact["family"].as_str().unwrap(),
                )?
            {
                p["attachment_path"] = artifact["path"].clone();
                artifact.as_object_mut().unwrap().remove("path");
                sticker = artifact;
            }
        }
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
        let mut result = self.native_request(&p).await?;
        if !sticker.is_null() && settle_sticker(&mut result, sticker) {
            store.verify_expression(key)?;
        }
        validate_receipt(
            &mut result,
            route.as_deref().unwrap_or(""),
            plan.bundle_id.is_some(),
        );
        if let Some(name) = result["verified_chat_name"].as_str() {
            if route.as_ref().is_some_and(|expected| expected != name) {
                return Ok(json!({"status":"sending_uncertain","reason":"native_target_mismatch"}));
            }
            store.save_route(&e.conversation, name)?;
        }
        store.note_intro(&e.conversation, e.agent, &plan.reply, &result)?;
        Ok(result)
    }
}
impl Kakao {
    /// Runs the sender's full target verification without writing anything, so the scan of the
    /// chat list overlaps model inference and the following send reuses its fresh result.
    pub async fn prewarm(&self, store: &Store, e: &Event) -> Result<Value> {
        let route = store.route(&e.conversation)?;
        let name = route.as_deref().unwrap_or(&e.title);
        if name.is_empty() {
            return Ok(json!({"status":"held","reason":"missing_target"}));
        }
        self.native_request(
            &json!({"chat_name":name,"room_name_verified":route.is_some(),"trigger_body":e.body,
            "reply":e.agent.prefix(),"probe":true,"expires_at":crate::event::now()+75.0}),
        )
        .await
    }
}
/// A sticker is decoration: once the text is verified the reply counts as delivered, and an
/// unverified image is only recorded on the receipt. Returns whether the image was verified.
pub fn settle_sticker(result: &mut Value, mut sticker: Value) -> bool {
    let image_sent = result["status"] == "sent_verified" && result["attachment_sent"] == true;
    sticker["sent"] = json!(image_sent);
    if !image_sent && result["text_sent"] == true {
        sticker["outcome"] = json!({"status":result["status"],"reason":result["reason"]});
        result["status"] = json!("sent_verified");
        result["reason"] = json!("sticker_not_verified");
    }
    result["sticker"] = sticker;
    image_sent
}
fn validate_receipt(result: &mut Value, expected: &str, attachment: bool) {
    if result["status"] == "sending" {
        result["status"] = json!("sending_uncertain");
    }
    if !matches!(
        result["status"].as_str(),
        Some("sent_verified" | "held" | "partial_file_held" | "sending_uncertain")
    ) {
        *result = json!({"status":"sending_uncertain","reason":"unknown_native_result"});
        return;
    }
    if result["status"] == "sent_verified"
        && (result["text_sent"] != true
            || result["verified_chat_name"]
                .as_str()
                .is_none_or(str::is_empty)
            || attachment && result["attachment_sent"] != true)
    {
        *result = json!({"status":"sending_uncertain","reason":"incomplete_native_verification"});
        return;
    }
    if !expected.is_empty()
        && result["verified_chat_name"]
            .as_str()
            .is_some_and(|n| n != expected)
    {
        *result = json!({"status":"sending_uncertain","reason":"native_target_mismatch"});
    }
}
impl Kakao {
    async fn native_request(&self, p: &Value) -> Result<Value> {
        let request = nonce()?;
        let base = &self.cfg.kakao.sender_ipc;
        private_dir(base)?;
        let path = base.join("sender-requests").join(format!("{request}.json"));
        atomic(&path, p)?;
        let receipt = base.join("sender-receipts").join(format!("{request}.json"));
        let result = self.invoke_sender(&request, &receipt).await;
        let _ = std::fs::remove_file(path);
        result
    }
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
