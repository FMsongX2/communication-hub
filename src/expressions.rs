//! Operator-owned catalogs. Models select IDs, never arbitrary filesystem paths.
use crate::{
    config::{Config, private_dir},
    event::{Event, Plan, digest},
    store::Store,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path},
};

const MAX_IMAGE: u64 = 8 * 1024 * 1024;
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Sticker {
    pub id: String,
    pub category: String,
    pub visual_meaning: String,
    pub suitable_situations: Vec<String>,
    pub avoid_context: Vec<String>,
    pub random_eligible: bool,
    pub format: String,
    pub sha256: String,
    pub archive: String,
    pub archive_path: String,
}
#[derive(Deserialize)]
struct Catalog {
    items: Vec<Sticker>,
}
fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn catalog(cfg: &Config) -> Result<Vec<Sticker>> {
    let Some(c) = &cfg.expressions else {
        return Ok(vec![]);
    };
    if fs::metadata(&c.catalog)?.len() > 4 * 1024 * 1024 {
        bail!("catalog_too_large")
    }
    let cat: Catalog = serde_json::from_slice(&fs::read(&c.catalog)?)?;
    if cat.items.len() > 4096 {
        bail!("catalog_too_many_items")
    }
    let mut ids = HashSet::new();
    let valid_id = regex::Regex::new(r"^[A-Za-z0-9_-]{1,40}$")?;
    for row in &cat.items {
        if !valid_id.is_match(&row.id)
            || !ids.insert(&row.id)
            || !safe_relative(&row.archive)
            || Path::new(&row.archive).components().count() != 1
            || !row.archive.ends_with(".zip")
            || !safe_relative(&row.archive_path)
            || row.sha256.len() != 64
            || !row.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || !matches!(row.format.as_str(), "PNG" | "GIF")
        {
            bail!("invalid_sticker_catalog")
        }
    }
    Ok(cat.items)
}
/// Bound input cost. Lexical ranking only nominates candidates: the model still checks meaning,
/// relationship and all avoid conditions. Restricted memes are not automatically nominated.
pub fn candidates(cfg: &Config, store: &Store, e: &Event) -> Result<Value> {
    let history = store.expression_history(&e.conversation)?;
    let recent: HashSet<_> = history.iter().map(|(sha, _)| sha.as_str()).collect();
    let families: HashSet<_> = history.iter().take(6).map(|(_, f)| f.as_str()).collect();
    let body = e
        .body
        .replace("@[유이]", "")
        .replace("[유이]", "")
        .to_lowercase();
    let chars: Vec<_> = body.chars().collect();
    let grams: HashSet<String> = chars
        .windows(2)
        .filter(|w| w.iter().all(|c| c.is_alphanumeric()))
        .map(|w| w.iter().collect())
        .collect();
    let mut seen = HashSet::new();
    let mut scored = Vec::new();
    for row in catalog(cfg)? {
        if !row.random_eligible
            || recent.contains(row.sha256.as_str())
            || !seen.insert(row.sha256.clone())
            || row.format == "GIF" && !cfg.expressions.as_ref().is_some_and(|c| c.gif_verified)
        {
            continue;
        }
        let meaning = format!(
            "{} {} {}",
            row.category,
            row.visual_meaning,
            row.suitable_situations.join(" ")
        )
        .to_lowercase();
        let score = grams
            .iter()
            .filter(|g| meaning.contains(g.as_str()))
            .count() as i32
            * 10
            - if families.contains(row.category.as_str()) {
                5
            } else {
                0
            };
        let tie = digest(format!("{}:{}", e.key(), row.id));
        scored.push((score, tie, row));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    // Keep strong lexical matches, then diversify the remaining category coverage.
    let mut picked = Vec::new();
    let mut counts = std::collections::HashMap::new();
    for (score, _, row) in scored {
        let count = counts.entry(row.category.clone()).or_insert(0);
        if score <= 0 && *count >= 3 {
            continue;
        }
        *count += 1;
        picked.push(
            json!({"id":row.id,"family":row.category,"meaning":row.visual_meaning,
            "suitable":row.suitable_situations,"avoid":row.avoid_context,"format":row.format}),
        );
        if picked.len() == 32 {
            break;
        }
    }
    Ok(json!(picked))
}
pub fn validate_plan(plan: &Plan, offered: &Value) -> Result<()> {
    if plan.bundle_id.is_some() && plan.sticker_id.is_some() {
        bail!("multiple_attachment_types")
    }
    if plan.sticker_id.as_ref().is_some_and(|id| {
        !offered
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v["id"] == *id))
    }) {
        bail!("sticker_not_offered")
    }
    Ok(())
}
pub fn emoticons(cfg: &Config) -> Result<Value> {
    let Some(path) = cfg.expressions.as_ref().and_then(|c| c.emoticons.as_ref()) else {
        return Ok(Value::Null);
    };
    if fs::metadata(path)?.len() > 128 * 1024 {
        bail!("emoticons_too_large")
    }
    let v: Value = serde_json::from_slice(&fs::read(path)?)?;
    let items = v["items"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("emoticons_invalid"))?;
    if items.len() > 256
        || items
            .iter()
            .any(|i| i["text"].as_str().is_none_or(|s| s.chars().count() > 100))
    {
        bail!("emoticons_invalid")
    }
    Ok(json!(items))
}
pub fn prepare(cfg: &Config, key: &str, id: &str) -> Result<Value> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid_job_key")
    }
    let row = catalog(cfg)?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| anyhow::anyhow!("unknown_sticker"))?;
    if !row.random_eligible
        || row.format == "GIF" && !cfg.expressions.as_ref().is_some_and(|c| c.gif_verified)
    {
        bail!("sticker_not_enabled")
    }
    let root = fs::canonicalize(cfg.expressions.as_ref().unwrap().catalog.parent().unwrap())?;
    let archive = fs::canonicalize(root.join(&row.archive))?;
    if archive.parent() != Some(root.as_path()) {
        bail!("unsafe_sticker_archive")
    }
    let mut zip = zip::ZipArchive::new(fs::File::open(archive)?)?;
    let mut member = zip.by_name(&row.archive_path)?;
    let suffix = Path::new(&row.archive_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if member.is_dir()
        || member.size() > MAX_IMAGE
        || member.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
        || suffix.to_uppercase() != row.format
    {
        bail!("invalid_sticker_member")
    }
    let mut data = Vec::new();
    member.by_ref().take(MAX_IMAGE + 1).read_to_end(&mut data)?; // CRC verified on EOF.
    if data.len() as u64 > MAX_IMAGE || digest(&data) != row.sha256 {
        bail!("sticker_hash_mismatch")
    }
    let valid = match suffix {
        "png" => data.starts_with(b"\x89PNG\r\n\x1a\n"),
        "gif" => data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"),
        _ => false,
    };
    if !valid {
        bail!("sticker_signature_mismatch")
    }
    private_dir(&cfg.kakao.sender_ipc)?;
    let ipc = fs::canonicalize(&cfg.kakao.sender_ipc)?;
    let jobs = ipc.join("file-jobs");
    if fs::symlink_metadata(&jobs).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("unsafe_jobs_directory")
    }
    private_dir(&jobs)?;
    if fs::canonicalize(&jobs)?.parent() != Some(ipc.as_path()) {
        bail!("unsafe_jobs_directory")
    }
    let dir = jobs.join(key);
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("unsafe_job_directory")
    }
    private_dir(&dir)?;
    if fs::canonicalize(&dir)?.parent() != Some(jobs.as_path()) {
        bail!("unsafe_job_directory")
    }
    let path = dir.join(format!("{}.{}", row.id, suffix));
    let tmp = dir.join(format!("{}.tmp", crate::adapters::nonce()?));
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    output.write_all(&data)?;
    output.sync_all()?;
    fs::rename(&tmp, &path)?;
    fs::File::open(&dir)?.sync_all()?;
    Ok(
        json!({"path":path,"sticker_id":row.id,"sha256":row.sha256,"family":row.category,"format":row.format,"bytes":data.len()}),
    )
}
