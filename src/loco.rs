//! Loopback Unix transport to the optional Kakao protocol sidecar. No UI fallback on uncertainty.
use crate::config::{LocoConfig, LocoMode};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

/// The sidecar's policy schema bounds the complete effective allowlist.
pub const MAX_POLICY_ROOMS: usize = 100;
pub const MAX_ROOM_ID_BYTES: usize = 256;
#[derive(Debug)]
pub struct RoomLimitReached;
impl std::fmt::Display for RoomLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("room_limit_reached")
    }
}
impl std::error::Error for RoomLimitReached {}
pub fn room_limit_error(error: &anyhow::Error) -> bool {
    error.is::<RoomLimitReached>()
}
/// SQLite is authoritative; static config participates only in the one-time migration.
pub fn target(
    cfg: &crate::config::Config,
    store: &crate::store::Store,
    c: &crate::event::Conversation,
) -> Result<Option<(LocoConfig, String)>> {
    let Some(mut loco) = cfg.kakao.loco.clone() else {
        return Ok(None);
    };
    let Some(binding) = store.loco_binding(cfg, c)? else {
        return Ok(None);
    };
    if loco.mode != LocoMode::Active
        || binding["deleted"] == true
        || store.room(c)?.is_none_or(|r| r["approved"] != true)
    {
        return Ok(None);
    }
    let chat_id = binding["chat_id"].as_str().unwrap().to_owned();
    loco.rooms = std::collections::BTreeMap::from([(c.id.clone(), chat_id.clone())]);
    Ok(Some((loco, chat_id)))
}
pub fn target_from_disk(
    cfg: &crate::config::Config,
    c: &crate::event::Conversation,
) -> Result<Option<(LocoConfig, String)>> {
    if cfg.kakao.loco.is_none() {
        return Ok(None);
    }
    target(cfg, &crate::store::Store::open(cfg.state.clone())?, c)
}
pub fn managed(
    cfg: &crate::config::Config,
    store: &crate::store::Store,
    c: &crate::event::Conversation,
) -> Result<bool> {
    let Some(loco) = &cfg.kakao.loco else {
        return Ok(false);
    };
    if loco.mode != LocoMode::Active || c.provider != "kakao" || c.account != cfg.kakao.account {
        return Ok(false);
    }
    store.seed_loco(cfg)?;
    if store.db()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM loco_bindings WHERE conversation=?)",
        [c.key()],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(true);
    }
    // Native events carry the numeric chat ID even when an older binding used an alias.
    let mut q = store.db()?;
    let tx = q.transaction()?;
    let mut rows = tx.prepare("SELECT conversation FROM loco_bindings WHERE chat_id=?")?;
    let keys = rows
        .query_map(rusqlite::params![c.id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for key in keys {
        let [provider, account, _]: [String; 3] = serde_json::from_str(&key)?;
        if provider == c.provider && account == c.account {
            return Ok(true);
        }
    }
    Ok(false)
}
pub async fn refresh(cfg: &LocoConfig) -> Result<Value> {
    diagnostic(cfg, "refresh_policy").await
}
pub fn numeric_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 20
        && !id.starts_with('0')
        && id.bytes().all(|b| b.is_ascii_digit())
        && id.parse::<u64>().is_ok()
}
fn uncertain(reason: &str) -> Value {
    json!({"status":"sending_uncertain","reason":reason,"transport":"loco"})
}
pub fn held(reason: &str) -> Value {
    json!({"status":"held","reason":reason,"transport":"loco","text_sent":false,
        "attachment_sent":false,"input_started":false,"side_effects_started":false})
}
async fn exchange(mut stream: UnixStream, method: &str, params: Value) -> Result<Value> {
    let id = crate::adapters::nonce()?;
    if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
        bail!("loco_peer_uid_mismatch")
    }
    let mut bytes = serde_json::to_vec(&json!({"id":id,"method":method,"params":params}))?;
    if bytes.len() > crate::daemon::LIMIT {
        bail!("oversized_loco_request")
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            bail!("loco_eof")
        }
        let end = buf
            .iter()
            .position(|b| *b == b'\n')
            .map_or(buf.len(), |n| n + 1);
        if frame.len() + end > crate::daemon::LIMIT {
            bail!("oversized_loco_response")
        }
        frame.extend_from_slice(&buf[..end]);
        reader.consume(end);
        if frame.last() == Some(&b'\n') {
            break;
        }
    }
    let v: Value = serde_json::from_slice(&frame)?;
    if v["id"] != id || v["ok"] != true || !v["result"].is_object() {
        bail!("invalid_loco_response")
    }
    Ok(v["result"].clone())
}
pub async fn diagnostic(cfg: &LocoConfig, method: &str) -> Result<Value> {
    cfg.validate()?;
    if !matches!(method, "status" | "list_rooms" | "refresh_policy") {
        bail!("invalid_loco_diagnostic")
    }
    let result = tokio::time::timeout(
        Duration::from_secs(if method == "refresh_policy" { 5 } else { 30 }),
        async {
            let stream = UnixStream::connect(&cfg.socket).await?;
            exchange(stream, method, json!({})).await
        },
    )
    .await??;
    if result["user_id"] != cfg.expected_user_id {
        bail!("loco_account_mismatch")
    }
    if method == "status"
        && (result["mode"] != json!(cfg.mode)
            || result["status"] == "ready" && result["connected"] != true)
    {
        bail!("loco_readiness_mismatch")
    }
    if !matches!(result["status"].as_str(), Some("ready" | "held")) {
        bail!("invalid_loco_status")
    }
    if method == "list_rooms" && result["status"] == "ready" {
        let rooms = result["rooms"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("invalid_loco_rooms"))?;
        if rooms
            .iter()
            .any(|r| r["chat_id"].as_str().is_none_or(|id| !numeric_id(id)))
        {
            bail!("invalid_loco_rooms")
        }
    }
    Ok(result)
}
pub async fn send(cfg: &LocoConfig, params: Value) -> Value {
    if cfg.validate().is_err() || cfg.mode != LocoMode::Active {
        return held("loco_not_active");
    }
    let Some(chat_id) = params["chat_id"].as_str() else {
        return held("loco_invalid_chat_id");
    };
    if !cfg.rooms.values().any(|id| id == chat_id)
        || params["expected_user_id"] != cfg.expected_user_id
    {
        return held("loco_target_not_bound");
    }
    let stream = match tokio::time::timeout(
        Duration::from_secs(5),
        UnixStream::connect(&cfg.socket),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return held("loco_unavailable_before_dispatch"),
    };
    // From the first write attempt onward, even a broken response is an uncertain external write.
    let attachment = params["attachment_path"].is_string();
    match tokio::time::timeout(
        Duration::from_secs(90),
        exchange(stream, "send", params.clone()),
    )
    .await
    {
        Ok(Ok(mut result)) => {
            validate_receipt(&mut result, cfg, chat_id, attachment);
            result
        }
        _ => uncertain("loco_dispatch_or_receipt_uncertain"),
    }
}
pub fn validate_receipt(result: &mut Value, cfg: &LocoConfig, chat_id: &str, attachment: bool) {
    let positive = |key: &str| result[key].as_str().is_some_and(numeric_id);
    let valid = result["transport"] == "loco"
        && result["user_id"] == cfg.expected_user_id
        && result["chat_id"] == chat_id
        && (result["text_sent"] != true || positive("text_log_id"))
        && (result["attachment_sent"] != true || attachment && positive("attachment_log_id"))
        && match result["status"].as_str() {
            Some("sent_verified") => {
                result["text_sent"] == true
                    && positive("text_log_id")
                    && (!attachment
                        || result["attachment_sent"] == true && positive("attachment_log_id"))
            }
            Some("partial_file_held") => {
                attachment
                    && result["text_sent"] == true
                    && positive("text_log_id")
                    && result["attachment_sent"] == false
            }
            Some("held") => [
                "text_sent",
                "attachment_sent",
                "input_started",
                "side_effects_started",
            ]
            .iter()
            .all(|k| result[*k] == false),
            Some("sending_uncertain") => true,
            _ => false,
        };
    if !valid {
        *result = uncertain("invalid_loco_receipt");
    }
}
