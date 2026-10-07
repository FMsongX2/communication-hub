//! Loopback Unix transport to the optional Kakao protocol sidecar. No UI fallback on uncertainty.
use crate::config::{LocoConfig, LocoMode};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

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
    if !matches!(method, "status" | "list_rooms") {
        bail!("invalid_loco_diagnostic")
    }
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let stream = UnixStream::connect(&cfg.socket).await?;
        exchange(stream, method, json!({})).await
    })
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
