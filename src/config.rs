use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub state: PathBuf,
    pub socket: PathBuf,
    pub app_server_socket: PathBuf,
    pub contact_skill: PathBuf,
    pub lookup_workdir: PathBuf,
    pub kakao: KakaoConfig,
    pub external_auto_send: bool,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default = "yes")]
    pub dispatch_enabled: bool,
    #[serde(default)]
    pub dashboard: Option<DashboardConfig>,
    #[serde(default)]
    pub intro_text: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub expressions: Option<ExpressionConfig>,
    /// Enables `[유미]` calls answered by Claude. Absent means Yumi tags are ignored.
    #[serde(default)]
    pub yumi: Option<YumiConfig>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YumiConfig {
    /// The `claude` CLI, logged in with the owner's account.
    pub claude_bin: PathBuf,
    /// Yumi's generated global persona (the Claude Code `CLAUDE.md`), loaded on every call.
    pub persona: PathBuf,
    #[serde(default = "default_yumi_model")]
    pub model: String,
    #[serde(default = "default_yumi_effort")]
    pub effort: String,
}
fn default_yumi_model() -> String {
    "claude-sonnet-5-5".into()
}
fn default_yumi_effort() -> String {
    "low".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpressionConfig {
    pub catalog: PathBuf,
    #[serde(default)]
    pub emoticons: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    pub port: u16,
    #[serde(default)]
    pub store_body: bool,
}
fn yes() -> bool {
    true
}
fn default_model() -> String {
    "gpt-6-luna".into()
}
fn default_effort() -> String {
    "medium".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KakaoConfig {
    pub enabled: bool,
    pub account: String,
    pub legacy_state: PathBuf,
    pub receiver_app: PathBuf,
    pub sender_app: PathBuf,
    pub sender_ipc: PathBuf,
    /// Probe the target while the model runs. Only pays off with a sender that reuses the probe's
    /// chat-list scan; with an older sender it delays the final send instead.
    #[serde(default)]
    pub prewarm: bool,
    /// Kakao posts no notification for the room window in front, so the sender app (which already
    /// holds Accessibility) also runs a read-only watch of that room and forwards tagged messages.
    #[serde(default)]
    pub watch_open_room: bool,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        if c.kakao.account.is_empty()
            || c.model.trim().is_empty()
            || c.effort.trim().is_empty()
            || !c.socket.is_absolute()
            || !c.state.is_absolute()
        {
            bail!("invalid hub configuration")
        }
        Ok(c)
    }
    pub fn descriptors(&self) -> Value {
        json!([
            {"provider":"kakao","enabled":self.kakao.enabled,"kind":"chat","transport":"macos_notification_and_ax","capabilities":["receive_calls","reply_text","approved_bundle_attachment","approved_sticker_attachment"],"sticker_catalog_configured":self.expressions.is_some(),"sticker_formats":["PNG"],"attachment_live_verified":false,"uses_pointer":false},
            {"provider":"discord","enabled":false,"kind":"chat","status":"adapter_not_implemented","capabilities":[]},
            {"provider":"slack","enabled":false,"kind":"chat","status":"adapter_not_implemented","capabilities":[]},
            {"provider":"notion","enabled":false,"kind":"documents_and_comments","status":"adapter_not_implemented","capabilities":[]}
        ])
    }
    pub fn validate_channel(&self, provider: &str, account: &str) -> Result<()> {
        if provider != "kakao" || !self.kakao.enabled || account != self.kakao.account {
            bail!("adapter_disabled_or_account_not_configured")
        }
        Ok(())
    }
}
pub fn default_config_path() -> PathBuf {
    home().join("Library/Application Support/CommunicationHub/config.json")
}
pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}
pub fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn atomic(path: &Path, value: &Value) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    private_dir(path.parent().unwrap())?;
    let tmp = path.with_extension(format!("{}.tmp", crate::adapters::nonce()?));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(value)?)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub fn json_file(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}
