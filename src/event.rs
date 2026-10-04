use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "[System-유이] : ";
pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
/// The wire prefix belongs to the transport, not model generation.
pub fn format_reply(body: &str) -> Result<String> {
    let mut text = body.trim();
    while let Some(rest) = text.strip_prefix(PREFIX.trim_end()) {
        text = rest.trim_start();
    }
    if text.is_empty() {
        bail!("empty_reply")
    }
    let reply = format!("{PREFIX}{text}");
    if reply.chars().count() >= 8192 {
        bail!("reply_too_long")
    }
    Ok(reply)
}
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conversation {
    pub provider: String,
    pub account: String,
    pub id: String,
}
impl Conversation {
    pub fn key(&self) -> String {
        serde_json::to_string(&json!([self.provider, self.account, self.id])).unwrap()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub conversation: Conversation,
    pub id: String,
    pub body: String,
    pub title: String,
    pub occurred_at: f64,
    pub source: String,
    #[serde(default)]
    pub metadata: Value,
}
impl Event {
    pub fn key(&self) -> String {
        digest(
            serde_json::to_vec(&json!([
                self.conversation.provider,
                self.conversation.account,
                self.conversation.id,
                self.id
            ]))
            .unwrap(),
        )
    }
    pub fn legacy_key(&self) -> String {
        digest(serde_json::to_vec(&json!([self.conversation.id, self.id])).unwrap())
    }
    pub fn from_kakao(v: &Value, account: &str) -> Result<Self> {
        let string = |k| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("missing event field: {k}"))
        };
        Ok(Self {
            conversation: Conversation {
                provider: "kakao".into(),
                account: account.into(),
                id: string("room_id")?,
            },
            id: string("message_id")?,
            body: string("body")?,
            title: v["chat_name"].as_str().unwrap_or("").into(),
            occurred_at: v["occurred_at"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("invalid event timestamp"))?,
            source: string("source")?,
            metadata: json!({"sender_identity":"unverified","actual_mention_verified":false}),
        })
    }
    pub fn validate(&self, at: f64, initialized: bool) -> Result<()> {
        if self.conversation.provider != "kakao" || self.source != "kakao_notification_store" {
            bail!("unsupported_event_source")
        }
        if self.conversation.id.is_empty()
            || self.conversation.account.is_empty()
            || self.id.is_empty()
        {
            bail!("missing_identity")
        }
        if !self.occurred_at.is_finite()
            || self.occurred_at < at - 300.0
            || self.occurred_at > at + 5.0
        {
            bail!("stale_or_invalid_time")
        }
        if self.body.is_empty()
            || self.body.chars().count() > 8192
            || self.body.ends_with(['…'])
            || self.body.ends_with("...")
        {
            bail!("missing_or_incomplete_body")
        }
        if ["[System-유이]", "[System-유니]", "KAKAO_YUI_REVIEW_PACKET"]
            .iter()
            .any(|s| self.body.contains(s))
        {
            bail!("agent_echo")
        }
        if !self.body.contains("[유이]") {
            bail!("keyword_absent")
        }
        if !self.body.contains("@[유이]") && !initialized {
            bail!("room_not_initialized")
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub reply: String,
    pub bundle_id: Option<String>,
    #[serde(default)]
    pub sticker_id: Option<String>,
}
impl Plan {
    pub fn parse(text: &str, allowed: &Value) -> Result<Self> {
        let mut p: Self = if text.trim_start().starts_with('{') {
            serde_json::from_str(text)?
        } else {
            Self {
                reply: text.trim().into(),
                bundle_id: None,
                sticker_id: None,
            }
        };
        p.reply = format_reply(&p.reply)?;
        if p.bundle_id
            .as_ref()
            .is_some_and(|id| allowed.get(id).is_none())
        {
            bail!("unauthorized_attachment_bundle")
        }
        if p.bundle_id.is_some() && p.sticker_id.is_some() {
            bail!("multiple_attachment_types")
        }
        Ok(p)
    }
}
