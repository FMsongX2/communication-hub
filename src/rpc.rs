use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::Path, time::Duration};
use tokio::net::UnixStream;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

#[derive(Debug)]
pub struct UsageLimit;
impl std::fmt::Display for UsageLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "account_usage_limit_exceeded")
    }
}
impl std::error::Error for UsageLimit {}
#[derive(Debug)]
pub struct Uncertain;
impl std::fmt::Display for Uncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model_dispatch_or_completion_uncertain")
    }
}
impl std::error::Error for Uncertain {}
pub fn usage_limit(v: &Value) -> bool {
    v["codexErrorInfo"] == "usageLimitExceeded"
}
pub struct Rpc {
    ws: WebSocketStream<UnixStream>,
    id: u64,
    pending: VecDeque<Value>,
}
impl Rpc {
    pub async fn connect(path: &Path) -> Result<Self> {
        let stream =
            tokio::time::timeout(Duration::from_secs(8), UnixStream::connect(path)).await??;
        let config = WebSocketConfig::default()
            .max_message_size(Some(1_000_000))
            .max_frame_size(Some(1_000_000));
        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(8),
            tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config)),
        )
        .await??;
        let mut rpc = Self {
            ws,
            id: 0,
            pending: VecDeque::new(),
        };
        rpc.call("initialize",json!({"clientInfo":{"name":"communication-hub","version":"0.1.0"},"capabilities":{"experimentalApi":true}})).await?;
        rpc.send(json!({"method":"initialized"})).await?;
        Ok(rpc)
    }
    pub async fn send(&mut self, v: Value) -> Result<()> {
        let text = serde_json::to_string(&v)?;
        if text.len() > 1_000_000 {
            bail!("oversized_rpc_request")
        }
        tokio::time::timeout(
            Duration::from_secs(8),
            self.ws.send(Message::Text(text.into())),
        )
        .await??;
        Ok(())
    }
    async fn read(&mut self) -> Result<Value> {
        loop {
            match self.ws.next().await {
                Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
                Some(Ok(Message::Ping(p))) => self.ws.send(Message::Pong(p)).await?,
                Some(Ok(Message::Close(_))) | None => bail!("app_server_disconnected"),
                Some(Err(e)) => return Err(e.into()),
                _ => {}
            }
        }
    }
    pub async fn receive(&mut self) -> Result<Value> {
        if let Some(v) = self.pending.pop_front() {
            return Ok(v);
        }
        self.read().await
    }
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        let id = self.id;
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        loop {
            let v = tokio::time::timeout_at(deadline, self.read()).await??;
            if v["id"] == id {
                if let Some(e) = v.get("error") {
                    if usage_limit(e) || usage_limit(&e["data"]) {
                        return Err(UsageLimit.into());
                    }
                    bail!("rpc_rejected:{method}:{}", e["code"])
                }
                return Ok(v.get("result").cloned().unwrap_or(json!({})));
            }
            if v.get("id").is_some() && v.get("method").is_some() {
                self.send(json!({"id":v["id"],"error":{"code":-32000,"message":"Interactive action requires owner review"}})).await?;
            } else {
                if self.pending.len() >= 4096 {
                    bail!("rpc_notification_buffer_full")
                }
                self.pending.push_back(v)
            }
        }
    }
}
