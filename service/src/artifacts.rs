//! Kept builds: `<store>/<buildId>/manifest.json` plus the artifact it names
//! (`store/lib/artifacts.js`, the read side). The manifest carries its artifact's SHA-256 in the
//! file name, so a listing never has to hash a multi-gigabyte IPA.

use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}};

pub fn valid_build_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `APPARITION_BUILD_DIR`, else `$XDG_STATE_HOME/apparition/builds`, else `~/.local/state/...`.
pub fn default_store() -> PathBuf {
    if let Some(dir) = std::env::var_os("APPARITION_BUILD_DIR") {
        return dir.into();
    }
    let state = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    state.join("apparition/builds")
}

pub struct Kept {
    pub manifest: Value,
    pub file: PathBuf,
}

/// The build's manifest and artifact path, refusing anything that is not a well-formed kept build.
pub fn read_build(id: &str, store: &Path) -> Result<Kept, String> {
    if !valid_build_id(id) {
        return Err("invalid build identity".into());
    }
    let dir = store.join(id);
    let manifest: Value = serde_json::from_str(&fs::read_to_string(dir.join("manifest.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let invalid = || "invalid build manifest".to_string();
    let artifact = manifest.get("artifact").filter(|a| a.is_object()).ok_or_else(invalid)?;
    let kind = artifact.get("kind").and_then(Value::as_str).filter(|k| ["ipa", "app.zip"].contains(k)).ok_or_else(invalid)?;
    let sha = artifact.get("sha256").and_then(Value::as_str).filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).ok_or_else(invalid)?;
    let bytes = artifact.get("bytes").and_then(Value::as_u64).ok_or_else(invalid)?;
    if manifest.get("format").and_then(Value::as_str) != Some("apparition-build/1")
        || manifest.get("buildId").and_then(Value::as_str) != Some(id)
        || artifact.get("file").and_then(Value::as_str) != Some(&format!("{sha}.{kind}"))
    {
        return Err(invalid());
    }
    let file = dir.join(format!("{sha}.{kind}"));
    if fs::metadata(&file).map_err(|e| e.to_string())?.len() != bytes {
        return Err("artifact integrity check failed".into());
    }
    Ok(Kept { manifest, file })
}

/// The newest `limit` kept builds, by directory modification time. An incomplete or corrupt one
/// does not hide the others.
pub fn list_builds(store: &Path, limit: usize) -> Vec<Value> {
    let Ok(read) = fs::read_dir(store) else { return Vec::new() };
    let mut newest: Vec<(String, std::time::SystemTime)> = read.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.metadata().ok()?.modified().ok()?)))
        .filter(|(name, _)| valid_build_id(name)).collect();
    newest.sort_by(|a, b| b.1.cmp(&a.1));
    let mut builds = Vec::new();
    for (id, _) in newest {
        if builds.len() >= limit {
            break;
        }
        let Ok(Kept { manifest: m, .. }) = read_build(&id, store) else { continue };
        let source = m.get("source").filter(|s| !s.is_null()).map(|s| json!({
            "stable": s.get("stable"), "anyDirty": s.pointer("/before/anyDirty"), "fingerprint": s.pointer("/before/fingerprint"),
        }));
        builds.push(json!({
            "buildId": m["buildId"], "createdAt": m.get("createdAt"), "project": m.get("project"),
            "platform": m.get("platform"), "configuration": m.get("configuration"), "app": m.get("app"),
            "artifact": m["artifact"], "source": source,
        }));
    }
    builds
}
