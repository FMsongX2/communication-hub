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
pub(crate) fn protected(path: &Path) -> bool {
    path.components().any(|x|matches!(x,Component::Normal(s) if ["PRIVATE",".ssh",".passwords",".codex",".claude",".env",".git"].iter().any(|v|s==*v)))
}
pub(crate) fn sensitive(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.contains("/Users/") || regex::Regex::new(r#"(?i)-----BEGIN .*PRIVATE KEY-----|"(?:api_key|access_token|password|secret_key)"\s*:\s*"[^"\s]+""#).unwrap().is_match(&text)
}
const ENTRY_LIMIT: usize = 4096;
const FILE_LIMIT: u64 = 32 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 128 * 1024 * 1024;
fn tar_header(bytes: &[u8]) -> bool {
    if bytes.get(257..262) == Some(b"ustar") {
        return true;
    }
    // Old V7 tar has no ustar marker. Recognize a valid first header checksum as well.
    let Some(header) = bytes.get(..512) else {
        return false;
    };
    let field = String::from_utf8_lossy(&header[148..156]);
    let field = field.trim_matches([' ', '\0']);
    if field.is_empty() || !field.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
        return false;
    }
    let checksum: u64 = header
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(*b)
            }
        })
        .sum();
    u64::from_str_radix(field, 8).ok() == Some(checksum)
}
fn opaque_container(name: &str, bytes: &[u8]) -> bool {
    let name = name.trim_end_matches(['.', ' ']).to_ascii_lowercase();
    let extension = Path::new(&name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let known_extension = [
        "zip", "zipx", "gz", "gzip", "tgz", "taz", "bz", "bz2", "bzip", "bzip2", "tbz", "tbz2",
        "tar", "xz", "txz", "zst", "zstd", "tzst", "lz4", "lzma", "tlz", "lz", "lzo", "7z", "rar",
        "z", "cab", "cpio", "ar", "a", "lib", "iso", "dmg",
    ]
    .contains(&extension);
    let skippable_frame =
        bytes.len() >= 4 && (0x50..=0x5f).contains(&bytes[0]) && bytes[1..4] == [0x2a, 0x4d, 0x18]; // Zstandard and LZ4 share this framing.
    known_extension
        || tar_header(bytes)
        || skippable_frame
        || [
            b"PK\x03\x04".as_slice(),
            b"PK\x05\x06",
            b"PK\x07\x08",
            b"\x1f\x8b",
            b"\x1f\x9d",
            b"7z\xbc\xaf\x27\x1c",
            b"Rar!",
            b"\xfd7zXZ\0",
            b"BZh",
            b"\x28\xb5\x2f\xfd",
            b"\x04\x22\x4d\x18",
            b"\x02\x21\x4c\x18",
            b"MSCF",
            b"!<arch>\n",
            b"070701",
            b"070702",
            b"070707",
        ]
        .iter()
        .any(|magic| bytes.starts_with(magic))
}
fn payload_safe(name: &str, bytes: &[u8]) -> Result<()> {
    if sensitive(bytes) {
        bail!("protected_information_in_artifact")
    }
    // Nested containers would hide their payload from this scanner. Register their expanded
    // directory instead; the legacy compound bundle already expands its result ZIP.
    if opaque_container(name, bytes) {
        bail!("nested_or_uninspected_archive")
    }
    Ok(())
}
fn member_safe(name: &str) -> bool {
    let path = Path::new(name);
    !name.is_empty()
        && !name.contains(['\\', ':', '\0'])
        && !path.is_absolute()
        && !path
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::RootDir))
        && !protected(path)
}
fn add_payload(
    payloads: &mut Vec<(String, Vec<u8>)>,
    name: String,
    bytes: Vec<u8>,
    total: &mut u64,
) -> Result<()> {
    if !member_safe(&name) || payloads.iter().any(|(n, _)| n == &name) {
        bail!("unsafe_or_duplicate_archive_member")
    }
    *total += bytes.len() as u64;
    if bytes.len() as u64 > FILE_LIMIT || *total > TOTAL_LIMIT || payloads.len() >= ENTRY_LIMIT {
        bail!("archive_exceeds_size_budget")
    }
    payload_safe(&name, &bytes)?;
    payloads.push((name, bytes));
    Ok(())
}
fn walk_payloads(
    root: &Path,
    dir: &Path,
    depth: usize,
    payloads: &mut Vec<(String, Vec<u8>)>,
    total: &mut u64,
    visited: &mut usize,
) -> Result<()> {
    if depth > 16 {
        bail!("directory_too_deep")
    }
    for entry in fs::read_dir(dir)? {
        *visited += 1;
        if *visited > ENTRY_LIMIT {
            bail!("too_many_directory_entries")
        }
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root)?;
        if protected(relative) || metadata.file_type().is_symlink() {
            bail!("protected_or_linked_artifact")
        }
        if metadata.is_dir() {
            walk_payloads(root, &path, depth + 1, payloads, total, visited)?;
        } else if metadata.is_file() && metadata.len() <= FILE_LIMIT {
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(FILE_LIMIT + 1)
                .read_to_end(&mut bytes)?;
            add_payload(
                payloads,
                relative.to_string_lossy().into_owned(),
                bytes,
                total,
            )?;
        } else {
            bail!("unsupported_or_oversized_file")
        }
    }
    Ok(())
}
// Original delivery is an opt-in room capability, never a path supplied by an agent.
fn prepare_original(cfg: &Config, key: &str, id: &str, b: &Value) -> Result<Value> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    const LIMIT: u64 = 20 * 1024 * 1024;
    let input = Path::new(
        b["source"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing_bundle_source"))?,
    );
    if !input.is_absolute()
        || protected(input)
        || input
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        bail!("protected_or_linked_artifact")
    }
    // Reject symlinks in every supplied component, including parent directory aliases.
    let mut component_path = std::path::PathBuf::new();
    for component in input.components() {
        component_path.push(component.as_os_str());
        if fs::symlink_metadata(&component_path)?
            .file_type()
            .is_symlink()
        {
            bail!("protected_or_linked_artifact")
        }
    }
    let source = fs::canonicalize(input)?;
    if protected(&source) || b["kind"] != "file" {
        bail!("invalid_original_source")
    }
    let filename = b["filename"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing_bundle_filename"))?;
    if !member_safe(filename)
        || filename.contains('/')
        || filename.len() > 200
        || filename.chars().any(char::is_control)
    {
        bail!("invalid_bundle_filename")
    }
    let extension = Path::new(filename)
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("");
    let source_extension = source.extension().and_then(|v| v.to_str()).unwrap_or("");
    if !["pdf", "png", "json", "txt"].contains(&extension) || extension != source_extension {
        bail!("unsupported_or_mismatched_original_type")
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&source)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > LIMIT {
        bail!("artifact_too_large_or_not_file")
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > LIMIT {
        bail!("artifact_changed_or_too_large")
    }
    payload_safe(filename, &bytes)?;
    match extension {
        "pdf" if bytes.starts_with(b"%PDF-") && bytes.windows(5).any(|w| w == b"%%EOF") => {}
        "png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {}
        "json" => {
            let _: Value = serde_json::from_slice(&bytes)?;
        }
        "txt"
            if std::str::from_utf8(&bytes).is_ok()
                && !bytes.contains(&0)
                && !bytes.starts_with(b"%PDF-")
                && !bytes.starts_with(b"\x89PNG") => {}
        _ => bail!("original_signature_mismatch"),
    }
    let job = cfg.kakao.sender_ipc.join("file-jobs").join(key);
    // Check existing staging ancestors before private_dir could follow a link and chmod its target.
    for ancestor in job.ancestors().take_while(|p| {
        *p != cfg
            .kakao
            .sender_ipc
            .as_path()
            .parent()
            .unwrap_or(Path::new("/"))
    }) {
        if let Ok(meta) = fs::symlink_metadata(ancestor)
            && meta.file_type().is_symlink()
        {
            bail!("linked_staging_directory")
        }
    }
    private_dir(&job)?;
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|v| v.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid_bundle_filename"))?;
    let name = format!("{stem}-{}.{extension}", &key[..8]);
    let output = job.join(&name);
    let temporary = job.join(format!(".{}.tmp", crate::adapters::nonce()?));
    let mut staged = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    staged.write_all(&bytes)?;
    staged.sync_all()?;
    // Rename replaces a leaf symlink instead of following it. Source bytes are unchanged.
    fs::rename(&temporary, &output)?;
    fs::set_permissions(&output, fs::Permissions::from_mode(0o600))?;
    fs::File::open(&job)?.sync_all()?;
    Ok(
        json!({"path":output,"filename":name,"bytes":bytes.len(),"sha256":digest(&bytes),
        "entries":1,"bundle_id":id,"delivery":"original"}),
    )
}
fn prepare_generic(cfg: &Config, key: &str, id: &str, b: &Value) -> Result<Value> {
    let input = Path::new(
        b["source"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing_bundle_source"))?,
    );
    if !input.is_absolute()
        || protected(input)
        || fs::symlink_metadata(input)?.file_type().is_symlink()
    {
        bail!("protected_or_linked_artifact")
    }
    let source = fs::canonicalize(input)?;
    if protected(&source) {
        bail!("protected_artifact")
    }
    let filename = b["filename"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing_bundle_filename"))?;
    if !member_safe(filename)
        || filename.contains('/')
        || !filename.ends_with(".zip")
        || filename.len() > 200
    {
        bail!("invalid_bundle_filename")
    }
    let mut payloads = Vec::new();
    let mut total = 0;
    match b["kind"].as_str() {
        Some("directory") if source.is_dir() => {
            walk_payloads(&source, &source, 0, &mut payloads, &mut total, &mut 0)?
        }
        Some("file") if source.is_file() => {
            if fs::metadata(&source)?.len() > FILE_LIMIT {
                bail!("artifact_too_large")
            }
            let mut bytes = Vec::new();
            fs::File::open(&source)?
                .take(FILE_LIMIT + 1)
                .read_to_end(&mut bytes)?;
            add_payload(
                &mut payloads,
                source.file_name().unwrap().to_string_lossy().into_owned(),
                bytes,
                &mut total,
            )?;
        }
        Some("zip") if source.is_file() => {
            if fs::metadata(&source)?.len() > TOTAL_LIMIT {
                bail!("artifact_too_large")
            }
            let mut old = ZipArchive::new(fs::File::open(&source)?)?;
            if old.len() > ENTRY_LIMIT {
                bail!("too_many_archive_entries")
            }
            for i in 0..old.len() {
                let mut entry = old.by_index(i)?;
                if !member_safe(entry.name())
                    || entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
                {
                    bail!("unsafe_archive_member")
                }
                if entry.is_dir() {
                    continue;
                }
                if entry.size() > FILE_LIMIT {
                    bail!("archive_exceeds_size_budget")
                }
                let name = entry.name().to_owned();
                let mut bytes = Vec::new();
                entry
                    .by_ref()
                    .take(FILE_LIMIT + 1)
                    .read_to_end(&mut bytes)?;
                add_payload(&mut payloads, name, bytes, &mut total)?;
            }
        }
        _ => bail!("invalid_bundle_kind_or_source"),
    }
    if payloads.is_empty() {
        bail!("empty_artifact_bundle")
    }
    payloads.sort_by(|a, b| a.0.cmp(&b.0));
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
pub fn prepare(cfg: &Config, e: &Event, key: &str, id: &str) -> Result<Value> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid_job_key")
    }
    let all = bundles(cfg, e)?;
    let b = all
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("unauthorized_attachment_bundle"))?;
    match b.get("delivery") {
        None => {}
        Some(Value::String(mode)) if mode == "zip" => {}
        Some(Value::String(mode)) if mode == "original" => {
            if cfg.kakao.loco_target(&e.conversation).is_none() {
                bail!("original_requires_active_loco_room")
            }
            return prepare_original(cfg, key, id, b);
        }
        _ => bail!("unknown_attachment_delivery"),
    }
    let path = |field: &str| -> Result<std::path::PathBuf> {
        Ok(fs::canonicalize(b[field].as_str().ok_or_else(|| {
            anyhow::anyhow!("invalid_bundle_config")
        })?)?)
    };
    if b.get("kind").is_some() {
        return prepare_generic(cfg, key, id, b);
    }
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
    let mut payloads = Vec::new();
    let mut total = 0u64;
    add_payload(
        &mut payloads,
        taxonomy.file_name().unwrap().to_string_lossy().into_owned(),
        raw,
        &mut total,
    )?;
    for i in 0..old.len() {
        let mut entry = old.by_index(i)?;
        let name = entry.name().to_owned();
        if !member_safe(&name) || entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            bail!("unsafe_archive_member")
        }
        if entry.is_dir() {
            continue;
        }
        if entry.size() > FILE_LIMIT {
            bail!("archive_exceeds_size_budget")
        }
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take(FILE_LIMIT + 1)
            .read_to_end(&mut bytes)?;
        add_payload(
            &mut payloads,
            format!("assign-context/{name}"),
            bytes,
            &mut total,
        )?;
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
