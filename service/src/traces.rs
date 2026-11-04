//! Trace upload and storage (`store/server.js`: `verifyTrace`, `receiveTrace`, `listTraces`).
//!
//! Apps upload a DebugTrace archive with no session, so the signature check is the whole gate and
//! this port is as strict as the Node one: a non-zip64 archive, no encryption, stored or deflate
//! entries only, every path relative and plain, every file listed in the signed manifest with its
//! size and SHA-256, and nothing in the zip the manifest does not list. The signing key must be
//! one `build-and-sign` recorded in the key ledger, for this bundle id and build.

use flate2::read::DeflateDecoder;
use ring::{digest, signature};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

pub const MAX_UPLOAD: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 512;
/// The most a trace's files may inflate to, together: each is capped at its listed size, and
/// this caps the sum, so a small zip can't fill memory.
pub const MAX_INFLATED: usize = 256 * 1024 * 1024;
const KEEP: usize = 300;

#[derive(Debug, PartialEq)]
pub struct TraceError(pub u16, pub String);

impl TraceError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        TraceError(status, message.into())
    }
}

fn bad(message: impl std::fmt::Display) -> TraceError {
    TraceError::new(400, format!("not a readable trace zip: {message}"))
}

/// Where `build-and-sign` records each build's signing key: `DEBUGTRACE_KEY_DIR`, else
/// `$XDG_STATE_HOME/debugtrace/keys`, else `~/.local/state/debugtrace/keys`.
pub fn default_key_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DEBUGTRACE_KEY_DIR") {
        return dir.into();
    }
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local/state"));
    state.join("debugtrace/keys")
}

fn u16_at(b: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as usize)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[derive(Debug)]
struct Entry {
    name: String,
    method: usize,
    csize: usize,
    usize: usize,
    offset: usize,
}

/// The central directory of a non-zip64 archive.
fn zip_entries(buf: &[u8]) -> Result<Vec<Entry>, TraceError> {
    let mut found = None;
    if buf.len() >= 22 {
        let lowest = (buf.len() - 22).saturating_sub(0xffff);
        let mut i = buf.len() - 22;
        loop {
            if u32_at(buf, i) == Some(0x0605_4b50) {
                found = Some(i);
                break;
            }
            if i == lowest {
                break;
            }
            i -= 1;
        }
    }
    let eocd = found.ok_or_else(|| bad("no end-of-central-directory record"))?;
    let count = u16_at(buf, eocd + 10).ok_or_else(|| bad("corrupt central directory"))?;
    let cd_size = u32_at(buf, eocd + 12).ok_or_else(|| bad("corrupt central directory"))? as usize;
    let mut p = u32_at(buf, eocd + 16).ok_or_else(|| bad("corrupt central directory"))? as usize;
    if count > MAX_FILES {
        return Err(bad(format!("{count} entries")));
    }
    if p + cd_size > eocd {
        return Err(bad("central directory out of bounds"));
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if p + 46 > eocd || u32_at(buf, p) != Some(0x0201_4b50) {
            return Err(bad("corrupt central directory"));
        }
        let flags = u16_at(buf, p + 8).unwrap();
        let method = u16_at(buf, p + 10).unwrap();
        let csize = u32_at(buf, p + 20).unwrap();
        let usize_ = u32_at(buf, p + 24).unwrap();
        let (nlen, xlen, clen) = (u16_at(buf, p + 28).unwrap(), u16_at(buf, p + 30).unwrap(), u16_at(buf, p + 32).unwrap());
        let offset = u32_at(buf, p + 42).unwrap();
        let name_bytes = buf.get(p + 46..p + 46 + nlen).ok_or_else(|| bad("corrupt central directory"))?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        if flags & 1 != 0 {
            return Err(bad(format!("{name} is encrypted")));
        }
        if method != 0 && method != 8 {
            return Err(bad(format!("{name} uses compression method {method}")));
        }
        if csize == u32::MAX || usize_ == u32::MAX || offset == u32::MAX {
            return Err(bad("zip64 is not supported"));
        }
        entries.push(Entry { name, method, csize: csize as usize, usize: usize_ as usize, offset: offset as usize });
        p += 46 + nlen + xlen + clen;
    }
    Ok(entries)
}

/// One entry's bytes, which must be exactly `expected` long. `expected` is the manifest's own
/// claim as JSON (None when absent), compared and reported the way the Node store does.
fn zip_read(buf: &[u8], entry: &Entry, expected: Option<u64>) -> Result<Vec<u8>, TraceError> {
    let corrupt = || TraceError::new(400, format!("corrupt entry {}", entry.name));
    let p = entry.offset;
    if p + 30 > buf.len() || u32_at(buf, p) != Some(0x0403_4b50) {
        return Err(corrupt());
    }
    let start = p + 30 + u16_at(buf, p + 26).ok_or_else(corrupt)? + u16_at(buf, p + 28).ok_or_else(corrupt)?;
    if start + entry.csize > buf.len() {
        return Err(TraceError::new(400, format!("entry {} runs past the end of the zip", entry.name)));
    }
    let raw = &buf[start..start + entry.csize];
    if expected != Some(entry.usize as u64) {
        let said = expected.map_or("undefined".to_string(), |e| e.to_string());
        return Err(TraceError::new(400, format!("{} is {} bytes, the manifest says {said}", entry.name, entry.usize)));
    }
    if entry.method == 0 {
        return Ok(raw.to_vec());
    }
    let limit = entry.usize.max(1) as u64;
    let mut out = Vec::new();
    // one byte past the limit tells "inflates to more than listed" from "exactly listed"
    DeflateDecoder::new(raw).take(limit + 1).read_to_end(&mut out)
        .map_err(|e| TraceError::new(400, format!("{} does not inflate to its listed size: {e}", entry.name)))?;
    if out.len() as u64 > limit {
        return Err(TraceError::new(400, format!("{} does not inflate to its listed size: ERR_BUFFER_TOO_LARGE", entry.name)));
    }
    Ok(out)
}

/// Archive paths as the manifest lists them: relative, no `.`/`..` segments, nothing a
/// filesystem would read differently.
pub fn safe_trace_path(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.starts_with('/') && !name.contains(['\\', '\0'])
        && name.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != ".." && crate::plain_segment(seg))
}

fn js(value: Option<&Value>) -> String {
    value.map_or("undefined".to_string(), Value::to_string)
}

fn string_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(value, |v, k| v.get(k))?.as_str()
}

fn trace_key(key_dir: &Path, key_id: &str) -> Option<Value> {
    let valid = !key_id.is_empty() && key_id.len() <= 64 && key_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !valid {
        return None;
    }
    serde_json::from_str(&fs::read_to_string(key_dir.join(format!("{key_id}.json"))).ok()?).ok()
}

pub struct Verified {
    pub manifest: Value,
    pub key: Value,
    pub files: Vec<(String, Vec<u8>)>,
}

pub fn verify(buf: &[u8], key_dir: &Path) -> Result<Verified, TraceError> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let entries = zip_entries(buf)?;
    let mut by_name: HashMap<&str, &Entry> = HashMap::new();
    for e in &entries {
        if !safe_trace_path(&e.name) {
            return Err(TraceError::new(400, format!("unsafe path in zip: {}", Value::String(e.name.clone()))));
        }
        if by_name.insert(&e.name, e).is_some() {
            return Err(TraceError::new(400, format!("duplicate entry {}", e.name)));
        }
    }
    let manifest_entry = by_name.get("manifest.json").ok_or_else(|| TraceError::new(400, "no manifest.json — not a DebugTrace archive"))?;
    if manifest_entry.usize > 4 * 1024 * 1024 {
        return Err(TraceError::new(400, "manifest.json is implausibly large"));
    }
    let manifest_bytes = zip_read(buf, manifest_entry, Some(manifest_entry.usize as u64))?;
    let manifest: Value = serde_json::from_str(&String::from_utf8_lossy(&manifest_bytes)).map_err(|_| TraceError::new(400, "manifest.json is not JSON"))?;
    if manifest.get("format").and_then(Value::as_str) != Some("debugtrace/1") {
        return Err(TraceError::new(400, format!("unknown trace format {}", js(manifest.get("format")))));
    }
    let trace_id = manifest.get("traceId").and_then(Value::as_str).unwrap_or("");
    if trace_id.len() != 36 || !trace_id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-') {
        return Err(TraceError::new(400, "manifest has no valid traceId"));
    }
    let signature_info = manifest.get("signature").filter(|s| !s.is_null() && s.as_bool() != Some(false));
    let (Some(signature_info), Some(sig_entry)) = (signature_info, by_name.get("manifest.sig")) else {
        return Err(TraceError::new(403, "unsigned trace: only apps built by apparition build carry a key; share this one as a file instead"));
    };
    if signature_info.get("algorithm").and_then(Value::as_str) != Some("ed25519") {
        return Err(TraceError::new(400, format!("unsupported signature {}", js(signature_info.get("algorithm")).trim_matches('"'))));
    }
    let key_id = signature_info.get("keyId").and_then(Value::as_str).unwrap_or("");
    let key = trace_key(key_dir, key_id).ok_or_else(|| TraceError::new(403, format!(
        "signing key {} is not in this store's ledger (built on another Mac, or the ledger was cleared)", js(signature_info.get("keyId")).trim_matches('"'))))?;
    let public = STANDARD.decode(key.get("publicKey").and_then(Value::as_str).unwrap_or("")).unwrap_or_default();
    let sig = zip_read(buf, sig_entry, Some(64))?;
    if signature::UnparsedPublicKey::new(&signature::ED25519, &public).verify(&manifest_bytes, &sig).is_err() {
        return Err(TraceError::new(403, "manifest.sig does not verify"));
    }
    let claimed_bundle = string_at(&manifest, &["app", "bundleId"]);
    if claimed_bundle != string_at(&key, &["bundleId"]) {
        return Err(TraceError::new(403, format!("trace claims {}, but key {} belongs to {}",
            claimed_bundle.unwrap_or("undefined"), key_id, string_at(&key, &["bundleId"]).unwrap_or("undefined"))));
    }
    if let Some(build) = key.get("build").filter(|b| !b.is_null() && b.as_str() != Some("")) {
        if manifest.get("app").and_then(|a| a.get("build")) != Some(build) {
            return Err(TraceError::new(403, format!("trace claims build {}, but key {} belongs to build {}",
                js(manifest.get("app").and_then(|a| a.get("build"))).trim_matches('"'), key_id, build.as_str().map_or(build.to_string(), String::from))));
        }
    }
    let listed = manifest.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut expected: HashSet<String> = ["manifest.json".to_string(), "manifest.sig".to_string()].into();
    let mut files = Vec::new();
    let mut inflated = 0usize;
    for f in &listed {
        let path = f.get("path").and_then(Value::as_str).unwrap_or("");
        if !safe_trace_path(path) || expected.contains(path) {
            return Err(TraceError::new(400, format!("bad manifest entry {}", js(f.get("path")))));
        }
        expected.insert(path.to_string());
        let entry = by_name.get(path).ok_or_else(|| TraceError::new(400, format!("manifest lists {path}, the zip lacks it")))?;
        inflated = inflated.saturating_add(entry.usize);
        if inflated > MAX_INFLATED {
            return Err(TraceError::new(413, format!("the trace's files come to more than {} MB", MAX_INFLATED >> 20)));
        }
        let bytes = f.get("bytes").and_then(Value::as_u64);
        let data = zip_read(buf, entry, bytes)?;
        let sha: String = digest::digest(&digest::SHA256, &data).as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if Some(data.len() as u64) != bytes || f.get("sha256").and_then(Value::as_str) != Some(&sha) {
            return Err(TraceError::new(403, format!("{path} does not match its manifest hash")));
        }
        files.push((path.to_string(), data));
    }
    // In archive order, as `Map` keys iterate in Node.
    for e in &entries {
        if !expected.contains(&e.name) {
            return Err(TraceError::new(400, format!("{} is in the zip but not in the manifest", e.name)));
        }
    }
    Ok(Verified { manifest, key, files })
}

fn read_meta(dir: &Path, id: &str) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(dir.join(id).join("meta.json")).ok()?).ok()
}

/// Every stored trace's meta, newest first. Partial or foreign directories are skipped.
pub fn list(dir: &Path) -> Vec<Value> {
    let mut out: Vec<Value> = fs::read_dir(dir).into_iter().flatten().flatten()
        .filter_map(|e| read_meta(dir, e.file_name().to_str()?)).collect();
    out.sort_by(|a, b| b["receivedAt"].as_f64().partial_cmp(&a["receivedAt"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn prune(dir: &Path) {
    for t in list(dir).iter().skip(KEEP) {
        if let Some(id) = t["id"].as_str().filter(|id| safe_trace_path(id) && !id.contains('/')) {
            let _ = fs::remove_dir_all(dir.join(id));
        }
    }
}

/// What the upload route needs from the request and the config.
pub struct Upload<'a> {
    pub dir: &'a Path,
    pub key_dir: &'a Path,
    pub now_ms: u64,
    pub via_web: bool,
    pub uploader: Option<&'a str>,
    /// Store repo name for a bundle id, when config.json knows it.
    pub repo_for_bundle: &'a dyn Fn(&str) -> Option<String>,
}

pub fn receive(buf: &[u8], up: &Upload) -> Result<(u16, Value), TraceError> {
    let Verified { manifest, key, files } = verify(buf, up.key_dir)?;
    let id = manifest["traceId"].as_str().unwrap().to_string();
    if let Some(mut meta) = read_meta(up.dir, &id) {
        meta["duplicate"] = Value::Bool(true);
        return Ok((200, meta));
    }
    let io = |e: std::io::Error| TraceError::new(500, format!("could not store the trace: {e}"));
    fs::create_dir_all(up.dir).map_err(io)?;
    // Written under a temporary name and renamed, so a half-written trace never lists.
    let tmp = up.dir.join(format!(".incoming-{id}"));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(tmp.join("files")).map_err(io)?;
    fs::write(tmp.join("trace.zip"), buf).map_err(io)?;
    for (path, data) in &files {
        let dest = tmp.join("files").join(path);
        fs::create_dir_all(dest.parent().unwrap()).map_err(io)?;
        fs::write(dest, data).map_err(io)?;
    }
    let bundle = key["bundleId"].as_str().unwrap_or("");
    let mut build = Map::new();
    let key_id = key["keyId"].clone();
    build.insert("keyId".into(), key_id.clone());
    build.insert("buildId".into(), key.get("buildId").filter(|v| !v.is_null()).cloned().unwrap_or(key_id));
    build.insert("repo".into(), (up.repo_for_bundle)(bundle).map_or(Value::Null, Value::String));
    for (name, from) in [("branch", "branch"), ("platform", "platform"), ("configuration", "configuration"), ("installedTo", "device"), ("builtAt", "createdAt")] {
        build.insert(name.into(), key.get(from).filter(|v| !v.is_null()).cloned().unwrap_or(Value::Null));
    }
    let meta = json!({
        "id": id, "receivedAt": up.now_ms, "bytes": buf.len(),
        "via": if up.via_web { "web" } else { "app" },
        "uploader": up.uploader,
        "createdAt": manifest.get("createdAt"), "session": manifest.get("session"),
        "note": manifest.get("note").filter(|n| !n.is_null() && n.as_str() != Some("")).cloned().unwrap_or(Value::Null),
        "app": manifest.get("app"), "device": manifest.get("device"),
        "build": Value::Object(build),
        "files": files.iter().map(|(p, d)| json!({ "path": p, "bytes": d.len() })).collect::<Vec<_>>(),
    });
    fs::write(tmp.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).map_err(io)?;
    fs::rename(&tmp, up.dir.join(&id)).map_err(io)?;
    prune(up.dir);
    Ok((201, meta))
}
