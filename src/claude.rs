//! Yumi's backend: a pre-spawned `claude -p` process used for exactly one call.
//!
//! Start-up (auth, settings, model handshake) takes 1.5–2.7 s, so one idle process waits on stdin
//! ahead of the next call. Each process answers a single message and exits, keeping calls stateless;
//! user settings, hooks, MCP servers, skills, tools and session persistence are all off.
use crate::{
    config::{YumiConfig, private_dir},
    event::digest,
    rpc::{Uncertain, UsageLimit},
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio, sync::OnceLock, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

struct Spare {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    prompt_hash: String,
}
fn slot() -> &'static Mutex<Option<Spare>> {
    static SPARE: OnceLock<Mutex<Option<Spare>>> = OnceLock::new();
    SPARE.get_or_init(|| Mutex::new(None))
}
fn spawn(cfg: &YumiConfig, cwd: &Path, prompt: &str) -> Result<Spare> {
    // An empty private directory: no project CLAUDE.md, auto-memory or repository is in scope.
    private_dir(cwd)?;
    let mut child = Command::new(&cfg.claude_bin)
        .args([
            "-p",
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--tools",
            "",
            "--disable-slash-commands",
            "--no-session-persistence",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--model",
            &cfg.model,
            "--effort",
            &cfg.effort,
            "--system-prompt",
            prompt,
        ])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("claude_stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("claude_stdout"))?;
    Ok(Spare {
        child,
        stdin,
        lines: BufReader::new(stdout).lines(),
        prompt_hash: digest(prompt),
    })
}
/// Keeps one idle process for `prompt` ready; a changed prompt replaces the old one.
pub async fn warm(cfg: &YumiConfig, cwd: &Path, prompt: &str) {
    let mut spare = slot().lock().await;
    let fresh = spare
        .as_mut()
        .is_some_and(|s| s.prompt_hash == digest(prompt) && matches!(s.child.try_wait(), Ok(None)));
    if !fresh {
        *spare = spawn(cfg, cwd, prompt).ok();
    }
}
/// Sends one message and returns Yumi's final text. The process is discarded afterwards.
pub async fn ask(cfg: &YumiConfig, cwd: &Path, prompt: &str, message: &str) -> Result<String> {
    let ready = slot().lock().await.take().and_then(|mut s| {
        (s.prompt_hash == digest(prompt) && matches!(s.child.try_wait(), Ok(None))).then_some(s)
    });
    let mut spare = match ready {
        Some(s) => s,
        None => spawn(cfg, cwd, prompt)?,
    };
    let line =
        json!({"type":"user","message":{"role":"user","content":message}}).to_string() + "\n";
    spare.stdin.write_all(line.as_bytes()).await?;
    spare.stdin.flush().await?;
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        while let Some(line) = spare.lines.next_line().await? {
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if msg["type"] == "result" {
                return Ok::<_, anyhow::Error>(Some(msg));
            }
        }
        Ok(None)
    })
    .await;
    let _ = spare.child.start_kill();
    let msg = match result {
        Ok(Ok(Some(msg))) => msg,
        _ => return Err(Uncertain.into()),
    };
    let text = msg["result"].as_str().unwrap_or("").to_owned();
    if msg["is_error"] == true || msg["subtype"] != "success" {
        let lower = text.to_lowercase();
        if lower.contains("usage limit") || lower.contains("limit reached") {
            return Err(UsageLimit.into());
        }
        return Err(Uncertain.into());
    }
    Ok(text)
}
