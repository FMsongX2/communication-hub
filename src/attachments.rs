use crate::{
    config::{Config, json_file, private_dir},
    event::{Event, digest},
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path},
};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

pub fn bundles(cfg: &Config, e: &Event) -> Result<Value> {
    cfg.validate_channel(&e.conversation.provider, &e.conversation.account)?;
    Ok(
        json_file(&cfg.kakao.legacy_state.join("attachment-bundles.json"))?["rooms"]
            [&e.conversation.id]
            .as_object()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or(json!({})),
    )
}
fn protected(path: &Path) -> bool {
    path.components().any(|x|matches!(x,Component::Normal(s) if ["PRIVATE",".ssh",".passwords",".codex",".claude",".env",".git"].iter().any(|v|s==*v)))
}
fn sensitive(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.contains("/Users/") || regex::Regex::new(r#"(?i)-----BEGIN .*PRIVATE KEY-----|"(?:api_key|access_token|password|secret_key)"\s*:\s*"[^"\s]+""#).unwrap().is_match(&text)
}
pub fn prepare(cfg: &Config, e: &Event, key: &str, id: &str) -> Result<Value> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid_job_key")
    }
    let all = bundles(cfg, e)?;
    let b = all
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("unauthorized_attachment_bundle"))?;
    let path = |field: &str| -> Result<std::path::PathBuf> {
        Ok(fs::canonicalize(b[field].as_str().ok_or_else(|| {
            anyhow::anyhow!("invalid_bundle_config")
        })?)?)
    };
    let taxonomy = path(if b.get("json_source").is_some() {
        "json_source"
    } else {
        "taxonomy"
    })?;
    let archive = path("result_zip")?;
    if protected(&taxonomy) || protected(&archive) || !taxonomy.is_file() || !archive.is_file() {
        bail!("protected_or_missing_artifact")
    }
    if fs::metadata(&taxonomy)?.len() > 16 * 1024 * 1024 {
        bail!("taxonomy_too_large")
    }
    let raw = fs::read(&taxonomy)?;
    let parsed: Value = serde_json::from_slice(&raw)?;
    let expected = b["expected_issue_count"].as_u64().or_else(|| {
        if b.get("json_source").is_none() {
            Some(152)
        } else {
            None
        }
    });
    if expected
        .is_some_and(|count| parsed["issues"].as_array().map(Vec::len) != Some(count as usize))
    {
        bail!("taxonomy_snapshot_changed")
    }
    if sensitive(&raw) {
        bail!("protected_information_in_taxonomy")
    }
    let filename = b["filename"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing_bundle_filename"))?;
    if filename.contains(['/', '\\']) || !filename.ends_with(".zip") || filename.len() > 200 {
        bail!("invalid_bundle_filename")
    }
    let mut old = ZipArchive::new(fs::File::open(&archive)?)?;
    if old.len() > 4096 {
        bail!("too_many_archive_entries")
    }
    let mut payloads = vec![(
        taxonomy.file_name().unwrap().to_string_lossy().into_owned(),
        raw,
    )];
    let mut total = 0u64;
    let mut actual_total = 0u64;
    for i in 0..old.len() {
        let mut entry = old.by_index(i)?;
        let name = entry.name().to_owned();
        let member = Path::new(&name);
        if member.is_absolute()
            || name.contains('\\')
            || name.contains(':')
            || member
                .components()
                .any(|p| matches!(p, Component::ParentDir))
            || protected(member)
            || entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
        {
            bail!("unsafe_archive_member")
        }
        total += entry.size();
        if entry.size() > 32 * 1024 * 1024 || total > 128 * 1024 * 1024 {
            bail!("archive_exceeds_size_budget")
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        actual_total += bytes.len() as u64;
        if bytes.len() > 32 * 1024 * 1024 || actual_total > 128 * 1024 * 1024 {
            bail!("expanded_archive_exceeds_size_budget")
        }
        if sensitive(&bytes) {
            bail!("protected_information_in_artifact")
        }
        payloads.push((format!("assign-context/{name}"), bytes));
    }
    let job = cfg.kakao.sender_ipc.join("file-jobs").join(key);
    private_dir(&job)?;
    let name = format!(
        "{}-{}.zip",
        filename.strip_suffix(".zip").unwrap(),
        &key[..8]
    );
    let output = job.join(&name);
    let mut writer = ZipWriter::new(fs::File::create(&output)?);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(6));
    for (name, bytes) in &payloads {
        writer.start_file(name, options)?;
        writer.write_all(bytes)?;
    }
    writer.finish()?.sync_all()?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&output, fs::Permissions::from_mode(0o600))?;
    let mut check = ZipArchive::new(fs::File::open(&output)?)?;
    for i in 0..check.len() {
        std::io::copy(&mut check.by_index(i)?, &mut std::io::sink())?;
    }
    Ok(
        json!({"path":output,"filename":name,"bytes":fs::metadata(&output)?.len(),"sha256":digest(fs::read(&output)?),"entries":payloads.len(),"bundle_id":id}),
    )
}
