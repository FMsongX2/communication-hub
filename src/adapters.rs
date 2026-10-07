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
        if name.is_empty() && crate::loco::target(&self.cfg, store, &e.conversation)?.is_none() {
            return Ok(json!({"status":"held","reason":"missing_target"}));
        }
        let deferred_deadline = store.deferred_deadline(key)?;
        let retry_deadline = deferred_deadline
            .unwrap_or(e.occurred_at + self.cfg.kakao.deferred_delivery_ttl_seconds as f64);
        let mut p = json!({"chat_name":name,"room_name_verified":route.is_some(),"trigger_body":e.body,"reply":plan.reply,"expires_at":(crate::event::now()+75.0).min(retry_deadline),"locked_ax_text":self.cfg.kakao.locked_ax_text,"resumed_locked":deferred_deadline.is_some()});
        registry_hint(store, e, &mut p)?;
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
                p["attachment_sha256"] = artifact["sha256"].clone();
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
            p["attachment_sha256"] = artifact["sha256"].clone();
        }
        if crate::loco::managed(&self.cfg, store, &e.conversation)? {
            let mut result = self.authorized_loco_send(store, key, e, &p).await?;
            // Protocol failures never become deferred UI retries.
            result["defer_locked_delivery"] = json!(false);
            if !sticker.is_null() && settle_sticker(&mut result, sticker) {
                store.verify_expression(key)?;
            }
            store.note_intro(&e.conversation, e.agent, &plan.reply, &result)?;
            return Ok(result);
        }
        let mut result = self.authorized_native_request(store, e, &p).await?;
        result["defer_locked_delivery"] = json!(self.cfg.kakao.defer_locked_delivery);
        result["retry_deadline"] = json!(retry_deadline);
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
        note_list_size(store, e, &result)?;
        store.note_intro(&e.conversation, e.agent, &plan.reply, &result)?;
        Ok(result)
    }
}
/// An approved, verified room lets the sender skip the chat-list scan while the list size holds.
fn registry_hint(store: &Store, e: &Event, request: &mut Value) -> Result<()> {
    if let Some(room) = store.room(&e.conversation)?
        && room["approved"] == true
        && room["verified_rows"].is_i64()
    {
        request["room_verified_rows"] = room["verified_rows"].clone();
    }
    Ok(())
}
/// A full scan in an approved room refreshes the list size its name was proven unique at.
fn note_list_size(store: &Store, e: &Event, receipt: &Value) -> Result<()> {
    if let Some(rows) = receipt["list_rows"].as_i64()
        && receipt["verified_chat_name"].is_string()
        && store
            .room(&e.conversation)?
            .is_some_and(|r| r["approved"] == true)
    {
        store.note_room_verified(&e.conversation, rows)?;
    }
    Ok(())
}
impl Kakao {
    /// Readiness for the exact event route; a mapped protocol room never probes UI state.
    pub async fn event_session_state(&self, e: &Event) -> Result<Value> {
        if let Some((loco, _)) = crate::loco::target_from_disk(&self.cfg, &e.conversation)? {
            return crate::loco::diagnostic(&loco, "status").await;
        }
        if self.cfg.kakao.loco.is_some()
            && crate::loco::managed(
                &self.cfg,
                &Store::open(self.cfg.state.clone())?,
                &e.conversation,
            )?
        {
            return Ok(crate::loco::held("loco_mapping_revoked"));
        }
        self.session_state().await
    }
    async fn authorized_loco_send(
        &self,
        store: &Store,
        key: &str,
        e: &Event,
        p: &Value,
    ) -> Result<Value> {
        let _guard = ui_lock().lock().await;
        if let Some(reason) = native_gate(&self.cfg, store, e, true)? {
            return Ok(crate::loco::held(&reason));
        }
        // The CLI's explicit controlled probe is the sole paused-send exception. The phase is
        // read from the host delivery journal, never accepted from an incoming Event.
        let controlled = store.delivery_phase(key)?.as_deref() == Some("controlled");
        if !self.cfg.external_auto_send
            || (!controlled && json_file(&self.cfg.state.join("control.json"))?["paused"] == true)
        {
            return Ok(crate::loco::held("delivery_paused"));
        }
        let Some((loco, chat_id)) = crate::loco::target(&self.cfg, store, &e.conversation)? else {
            return Ok(crate::loco::held("loco_mapping_revoked"));
        };
        let binding = store.loco_binding(&self.cfg, &e.conversation)?.unwrap();
        let params = json!({"delivery_id":key,"expected_user_id":loco.expected_user_id,
            "chat_id":chat_id,"approval_epoch":binding["approval_epoch"],"reply":p["reply"],"attachment_path":p["attachment_path"],
            "attachment_sha256":p["attachment_sha256"],"expires_at":p["expires_at"]});
        Ok(crate::loco::send(&loco, params).await)
    }
    /// Read-only session state; does not activate Kakao, write a draft, paste, or send.
    pub async fn session_state(&self) -> Result<Value> {
        self.native_request(&json!({"chat_name":"session-probe","trigger_body":"session-probe",
            "reply":crate::event::PREFIX,"session_probe":true,"expires_at":crate::event::now()+75.0})).await
    }
    /// Runs the sender's full target verification without writing anything, so the scan of the
    /// chat list overlaps model inference and the following send reuses its fresh result.
    pub async fn prewarm(&self, store: &Store, e: &Event) -> Result<Value> {
        if let Some((loco, _)) = crate::loco::target(&self.cfg, store, &e.conversation)? {
            if let Some(reason) = native_gate(&self.cfg, store, e, false)? {
                return Ok(crate::loco::held(&reason));
            }
            return crate::loco::diagnostic(&loco, "status").await;
        }
        if crate::loco::managed(&self.cfg, store, &e.conversation)? {
            return Ok(crate::loco::held("loco_mapping_revoked"));
        }
        let route = store.route(&e.conversation)?;
        let name = route.as_deref().unwrap_or(&e.title);
        if name.is_empty() {
            return Ok(json!({"status":"held","reason":"missing_target"}));
        }
        let mut p = json!({"chat_name":name,"room_name_verified":route.is_some(),"trigger_body":e.body,
            "reply":e.agent.prefix(),"probe":true,"expires_at":crate::event::now()+75.0});
        registry_hint(store, e, &mut p)?;
        let receipt = self
            .native_request_inner(&p, Some((store, e, false)))
            .await?;
        if receipt["status"] == "ready" {
            note_list_size(store, e, &receipt)?;
        }
        Ok(receipt)
    }
    /// Proves from the chat list alone that exactly one room carries `title`.
    pub async fn verify_name(&self, title: &str) -> Result<Value> {
        if title.is_empty() || title.chars().count() > 200 {
            return Ok(json!({"status":"held","reason":"missing_target"}));
        }
        self.native_request(
            &json!({"chat_name":title,"room_name_verified":true,"trigger_body":"room-verification",
            "reply":crate::event::PREFIX,"verify_room":true,"expires_at":crate::event::now()+75.0}),
        )
        .await
    }
    /// Reads the chat list's room names for the dashboard picker; names shared by several rooms
    /// are flagged because they cannot be addressed safely.
    pub async fn list_rooms(&self, store: &Store) -> Result<Value> {
        if let Some(loco) = &self.cfg.kakao.loco {
            store.seed_loco(&self.cfg)?;
            let mut catalog = crate::loco::diagnostic(loco, "list_rooms").await?;
            catalog["transport"] = json!("loco");
            let registered = store.rooms()?;
            if let Some(rooms) = catalog["rooms"].as_array_mut() {
                for room in rooms {
                    let found = registered.iter().find(|r| {
                        r["chat_id"] == room["chat_id"]
                            && r["user_id"] == loco.expected_user_id
                            && r["account"] == self.cfg.kakao.account
                    });
                    room["registered"] = json!(found.is_some());
                    room["transport"] = json!("loco");
                    for field in ["approved", "yui", "yumi", "key"] {
                        room[field] = found.map(|r| r[field].clone()).unwrap_or(Value::Null);
                    }
                }
            }
            return Ok(catalog);
        }
        let receipt = self
            .native_request(&json!({"chat_name":"room-list","trigger_body":"room-list",
                "reply":crate::event::PREFIX,"list_rooms":true,"expires_at":crate::event::now()+75.0}))
            .await?;
        if receipt["status"] != "ready" {
            return Ok(json!({"status":receipt["status"],"reason":receipt["reason"],"rooms":[]}));
        }
        let names: Vec<String> = receipt["rooms"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| r["name"].as_str().map(str::to_owned))
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let registered: std::collections::HashSet<String> = store
            .rooms()?
            .into_iter()
            .filter(|r| r["approved"] == true)
            .filter_map(|r| r["title"].as_str().map(str::to_owned))
            .collect();
        let mut counts = std::collections::HashMap::new();
        for n in &names {
            *counts.entry(n.clone()).or_insert(0) += 1;
        }
        let mut seen = std::collections::HashSet::new();
        let rooms: Vec<Value> = names
            .into_iter()
            .filter(|n| seen.insert(n.clone()))
            .map(
                |n| json!({"name":n,"duplicate":counts[&n]>1,"registered":registered.contains(&n)}),
            )
            .collect();
        Ok(
            json!({"status":"ready","transport":"ax","rooms":rooms,"list_rows":receipt["list_rows"]}),
        )
    }
    /// Dashboard registration: proves the room name is unique in the chat list without opening the
    /// room or writing anything, and records the list size it held at.
    pub async fn verify_room(&self, store: &Store, key: &str) -> Result<Value> {
        let room = store
            .rooms()?
            .into_iter()
            .find(|r| r["key"] == key)
            .ok_or_else(|| anyhow::anyhow!("unknown_room"))?;
        if room["transport"] == "loco" {
            let loco = self
                .cfg
                .kakao
                .loco
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("loco_not_configured"))?;
            let catalog = crate::loco::diagnostic(loco, "list_rooms").await?;
            let found = catalog["status"] == "ready"
                && catalog["rooms"]
                    .as_array()
                    .is_some_and(|rs| rs.iter().any(|r| r["chat_id"] == room["chat_id"]));
            return Ok(
                json!({"status":if found {"ready"}else{"held"},"transport":"loco","chat_id":room["chat_id"],"reason":if found {Value::Null}else{json!("room_not_in_catalog")}}),
            );
        }
        let receipt = self
            .verify_name(room["title"].as_str().unwrap_or(""))
            .await?;
        if receipt["status"] == "ready"
            && let Some(rows) = receipt["list_rows"].as_i64()
        {
            let parts: Vec<String> = serde_json::from_str(key)?;
            let conversation = crate::event::Conversation {
                provider: parts[0].clone(),
                account: parts[1].clone(),
                id: parts[2].clone(),
            };
            store.note_room_verified(&conversation, rows)?;
        }
        Ok(receipt)
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
const ATTACHMENT_SETTLE: Duration = Duration::from_secs(2);
/// One KakaoTalk UI operation at a time: a busy notice, a probe and a reply never interleave.
fn ui_lock() -> &'static tokio::sync::Mutex<()> {
    static UI: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    UI.get_or_init(|| tokio::sync::Mutex::new(()))
}
impl Kakao {
    async fn native_request(&self, p: &Value) -> Result<Value> {
        self.native_request_inner(p, None).await
    }
    /// Only host-stamped worker/prepared Events reach this boundary. Recheck permissions
    /// after artifact staging and waiting for the global UI lock, not just before queuing.
    pub async fn authorized_native_request(
        &self,
        store: &Store,
        e: &Event,
        p: &Value,
    ) -> Result<Value> {
        self.native_request_inner(p, Some((store, e, true))).await
    }
    async fn native_request_inner(
        &self,
        p: &Value,
        scope: Option<(&Store, &Event, bool)>,
    ) -> Result<Value> {
        let _ui = ui_lock().lock().await;
        if let Some((store, e, stamp_required)) = scope
            && let Some(reason) = native_gate(&self.cfg, store, e, stamp_required)?
        {
            return Ok(no_input_held(&reason));
        }
        let request = nonce()?;
        let base = &self.cfg.kakao.sender_ipc;
        private_dir(base)?;
        let path = base.join("sender-requests").join(format!("{request}.json"));
        atomic(&path, p)?;
        let receipt = base.join("sender-receipts").join(format!("{request}.json"));
        if let Some((store, e, stamp_required)) = scope
            && let Some(reason) = native_gate(&self.cfg, store, e, stamp_required)?
        {
            let _ = std::fs::remove_file(&path);
            return Ok(no_input_held(&reason));
        }
        let result = self.invoke_sender(&request, &receipt).await;
        let _ = std::fs::remove_file(path);
        // An image is still uploading when its preview closes. Holding the queue briefly keeps the
        // next text from reaching KakaoTalk's server first, so each text stays next to its sticker.
        if result.as_ref().is_ok_and(|r| r["attachment_sent"] == true) {
            tokio::time::sleep(ATTACHMENT_SETTLE).await;
        }
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
fn no_input_held(reason: &str) -> Value {
    json!({"status":"held","reason":reason,"input_started":false,"side_effects_started":false,"text_sent":false,"attachment_sent":false})
}
fn native_gate(
    cfg: &Config,
    store: &Store,
    e: &Event,
    stamp_required: bool,
) -> Result<Option<String>> {
    // Store/host errors are known preflight failures here: the helper has not been invoked.
    match crate::capabilities::delivery_gate(cfg, store, e) {
        Ok(Some(reason)) => return Ok(Some(format!("delivery_{reason}"))),
        Err(_) => return Ok(Some("delivery_gate_unavailable".into())),
        Ok(None) => {}
    }
    if stamp_required && crate::daemon::validate_deferred_authorization(cfg, e).is_err() {
        return Ok(Some("authorization_changed_or_missing".into()));
    }
    Ok(None)
}

#[cfg(test)]
mod late_gate_tests {
    use super::*;
    use crate::{capabilities, event::now};
    #[tokio::test]
    async fn waiting_for_ui_lock_rechecks_permission_after_wait() {
        for revoke in ["room", "policy"] {
            let t = tempfile::TempDir::new().unwrap();
            let r = t.path();
            std::fs::write(
                r.join("policy.md"),
                "name: contact-other\nCurrent sharing policy\n",
            )
            .unwrap();
            let cfg:Config=serde_json::from_value(json!({"state":r.join("state"),"socket":r.join("hub.sock"),"app_server_socket":r.join("app.sock"),"contact_skill":r.join("policy.md"),"lookup_workdir":r,"external_auto_send":true,"kakao":{"enabled":true,"account":"owner","legacy_state":r.join("legacy"),"receiver_app":r.join("receiver.app"),"sender_app":r.join("sender.app"),"sender_ipc":r.join("ipc")}})).unwrap();
            let store = Store::open(cfg.state.clone()).unwrap();
            let mut e:Event=serde_json::from_value(json!({"conversation":{"provider":"kakao","account":"owner","id":"room-fixture"},"id":"event-fixture","body":"@[유이] task status","title":"fixture","occurred_at":now(),"source":"kakao_notification_store","metadata":{}})).unwrap();
            store.note_room_seen(&e.conversation, &e.title).unwrap();
            store
                .update_room(&e.conversation.key(), true, true, true)
                .unwrap();
            e.metadata["__hub_authorization_stamp"] =
                json!(capabilities::authorization_stamp(&cfg, &e).unwrap());
            let lock = ui_lock().lock().await;
            let started = std::sync::Arc::new(tokio::sync::Notify::new());
            let (task_cfg, task_store, task_event, signal) =
                (cfg.clone(), store.clone(), e.clone(), started.clone());
            let task = tokio::spawn(async move {
                signal.notify_one();
                Kakao { cfg: task_cfg }
                    .authorized_native_request(&task_store, &task_event, &json!({}))
                    .await
            });
            started.notified().await;
            if revoke == "room" {
                store.remove_room(&e.conversation.key()).unwrap();
            } else {
                std::fs::write(
                    &cfg.contact_skill,
                    "name: contact-other\nRevised sharing policy\n",
                )
                .unwrap();
            }
            drop(lock);
            let receipt = task.await.unwrap().unwrap();
            assert_eq!(receipt["status"], "held");
            assert_eq!(receipt["input_started"], false);
            assert!(
                !cfg.kakao.sender_ipc.exists(),
                "waiting reply must be gated before request write/helper invoke"
            );
        }
    }
    #[tokio::test]
    async fn loco_waiting_for_transport_lock_rechecks_policy_and_pause() {
        for revoke in ["room", "policy", "pause"] {
            let t = tempfile::TempDir::new().unwrap();
            let r = t.path();
            std::fs::write(
                r.join("policy.md"),
                "name: contact-other\nCurrent sharing policy\n",
            )
            .unwrap();
            let cfg:Config=serde_json::from_value(json!({"state":r.join("state"),"socket":r.join("hub.sock"),"app_server_socket":r.join("app.sock"),"contact_skill":r.join("policy.md"),"lookup_workdir":r,"external_auto_send":true,"kakao":{"enabled":true,"account":"owner","legacy_state":r.join("legacy"),"receiver_app":r.join("receiver.app"),"sender_app":r.join("sender.app"),"sender_ipc":r.join("ipc"),"loco":{"socket":r.join("loco.sock"),"expected_user_id":"123","mode":"active","rooms":{"room-fixture":"456"}}}})).unwrap();
            let store = Store::open(cfg.state.clone()).unwrap();
            let mut e:Event=serde_json::from_value(json!({"conversation":{"provider":"kakao","account":"owner","id":"room-fixture"},"id":"event-fixture","body":"@[유이] task status","title":"fixture","occurred_at":now(),"source":"kakao_loco","metadata":{}})).unwrap();
            store.note_room_seen(&e.conversation, &e.title).unwrap();
            store
                .update_room(&e.conversation.key(), true, true, true)
                .unwrap();
            e.metadata["__hub_authorization_stamp"] =
                json!(capabilities::authorization_stamp(&cfg, &e).unwrap());
            let lock = ui_lock().lock().await;
            let signal = std::sync::Arc::new(tokio::sync::Notify::new());
            let (c, s, event, ready) = (cfg.clone(), store.clone(), e.clone(), signal.clone());
            let task = tokio::spawn(async move {
                ready.notify_one();
                Kakao { cfg: c }
                    .authorized_loco_send(
                        &s,
                        &event.key(),
                        &event,
                        &json!({"reply":"fixture","expires_at":now()+75.0}),
                    )
                    .await
            });
            signal.notified().await;
            match revoke {
                "room" => {
                    store.remove_room(&e.conversation.key()).unwrap();
                }
                "policy" => {
                    std::fs::write(&cfg.contact_skill, "name: contact-other\nUpdated policy\n")
                        .unwrap()
                }
                _ => atomic(&cfg.state.join("control.json"), &json!({"paused":true})).unwrap(),
            }
            drop(lock);
            let result = task.await.unwrap().unwrap();
            assert_eq!(result["status"], "held");
            assert_eq!(result["side_effects_started"], false);
            assert_ne!(result["reason"], "loco_unavailable_before_dispatch");
            assert!(!cfg.kakao.sender_ipc.exists());
        }
    }
}
