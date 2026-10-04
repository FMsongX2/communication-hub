use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "[System-유이] : ";
/// Which sister a call addresses. Yui answers through the Codex backend, Yumi through Claude.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    #[default]
    Yui,
    Yumi,
}
impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Yui, Agent::Yumi];
    pub fn name(self) -> &'static str {
        match self {
            Agent::Yui => "유이",
            Agent::Yumi => "유미",
        }
    }
    pub fn tag(self) -> &'static str {
        match self {
            Agent::Yui => "[유이]",
            Agent::Yumi => "[유미]",
        }
    }
    pub fn initial_tag(self) -> &'static str {
        match self {
            Agent::Yui => "@[유이]",
            Agent::Yumi => "@[유미]",
        }
    }
    pub fn prefix(self) -> &'static str {
        match self {
            Agent::Yui => PREFIX,
            Agent::Yumi => "[System-유미] : ",
        }
    }
    /// The room-registry switch that enables this sister.
    pub fn name_key(self) -> &'static str {
        match self {
            Agent::Yui => "yui",
            Agent::Yumi => "yumi",
        }
    }
    fn is_yui(&self) -> bool {
        *self == Agent::Yui
    }
}
pub fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}
/// The wire prefix belongs to the transport, not model generation.
pub fn format_reply(body: &str) -> Result<String> {
    format_reply_as(Agent::Yui, body)
}
pub fn format_reply_as(agent: Agent, body: &str) -> Result<String> {
    let prefix = agent.prefix();
    let mut text = body.trim();
    while let Some(rest) = text.strip_prefix(prefix.trim_end()) {
        text = rest.trim_start();
    }
    if text.is_empty() {
        bail!("empty_reply")
    }
    let reply = format!("{prefix}{text}");
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
    /// One notification can call both sisters; each answer is its own event.
    #[serde(default, skip_serializing_if = "Agent::is_yui")]
    pub agent: Agent,
}
impl Event {
    pub fn key(&self) -> String {
        let mut identity = json!([
            self.conversation.provider,
            self.conversation.account,
            self.conversation.id,
            self.id
        ]);
        // Yui keys stay unchanged so earlier journals still deduplicate.
        if !self.agent.is_yui() {
            identity.as_array_mut().unwrap().push(json!(self.agent));
        }
        digest(serde_json::to_vec(&identity).unwrap())
    }
    /// The sisters this event's body calls, in a stable order.
    pub fn called_agents(&self) -> Vec<Agent> {
        Agent::ALL
            .into_iter()
            .filter(|a| self.body.contains(a.tag()))
            .collect()
    }
    pub fn for_agent(&self, agent: Agent) -> Self {
        Self {
            agent,
            ..self.clone()
        }
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
            agent: Agent::Yui,
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
        if [
            "[System-유이]",
            "[System-유미]",
            "[System-유니]",
            "KAKAO_YUI_REVIEW_PACKET",
        ]
        .iter()
        .any(|s| self.body.contains(s))
        {
            bail!("agent_echo")
        }
        if !self.body.contains(self.agent.tag()) {
            bail!("keyword_absent")
        }
        if !self.body.contains(self.agent.initial_tag()) && !initialized {
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
        Self::parse_as(Agent::Yui, text, allowed)
    }
    pub fn parse_as(agent: Agent, text: &str, allowed: &Value) -> Result<Self> {
        let mut p: Self = if text.trim_start().starts_with('{') {
            serde_json::from_str(text)?
        } else {
            Self {
                reply: text.trim().into(),
                bundle_id: None,
                sticker_id: None,
            }
        };
        p.reply = format_reply_as(agent, &p.reply)?;
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
