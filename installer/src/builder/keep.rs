//! Keeping a build: its dSYMs in the symbol archive (`store/lib/symbols.sh`) and the signed
//! artifact with its manifest in the build store (`store/lib/artifacts.js` `publish`), in the
//! `apparition-build/1` format the Store and the device's Apparition read.

use std::{collections::{BTreeMap, BTreeSet}, fs, path::{Path, PathBuf}, process::Command};

use serde_json::{Value, json};

use super::{bundle, file_digest, utc_now};

const MACHO_MAGIC: [[u8; 4]; 8] = [
    [0xfe, 0xed, 0xfa, 0xce], [0xce, 0xfa, 0xed, 0xfe], [0xfe, 0xed, 0xfa, 0xcf], [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe], [0xbe, 0xba, 0xfe, 0xca], [0xca, 0xfe, 0xba, 0xbf], [0xbf, 0xba, 0xfe, 0xca],
];

fn is_macho(file: &Path) -> bool {
    let mut header = [0u8; 4];
    fs::File::open(file).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut header)).is_ok() && MACHO_MAGIC.contains(&header)
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            files_under(&entry.path(), out);
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
}

/// `dwarfdump --uuid`'s lines: (UUID, architecture, path).
fn dwarfdump(path: &Path) -> Result<Vec<(String, String, PathBuf)>, String> {
    let output = Command::new("dwarfdump").arg("--uuid").arg(path).stdin(std::process::Stdio::null()).output()
        .map_err(|e| format!("dwarfdump: {e}"))?;
    if !output.status.success() {
        return Err(format!("dwarfdump failed on {}", path.display()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().filter_map(|line| {
        let rest = line.strip_prefix("UUID: ")?;
        let (uuid, rest) = rest.split_once(' ')?;
        let rest = rest.strip_prefix('(')?;
        let (arch, path) = rest.split_once(") ")?;
        (uuid.len() == 36).then(|| (uuid.to_uppercase(), arch.to_string(), PathBuf::from(path)))
    }).collect())
}

/// Every Mach-O binary in `app`, by UUID: `artifacts.js`'s `uuidIndex`.
pub fn uuid_index(app: &Path) -> Result<BTreeMap<String, Value>, String> {
    let mut files = Vec::new();
    files_under(app, &mut files);
    let mut index = BTreeMap::new();
    for file in files.into_iter().filter(|f| is_macho(f)) {
        let (sha256, bytes) = file_digest(&file)?;
        for (uuid, arch, _) in dwarfdump(&file)? {
            index.insert(uuid, json!({ "arch": arch, "binary": file.strip_prefix(app).unwrap_or(&file), "sha256": sha256, "bytes": bytes }));
        }
    }
    Ok(index)
}

pub struct Symbols<'a> {
    pub archive: &'a Path,
    pub products: &'a Path,
    pub app: &'a Path,
    pub build_id: &'a str,
    pub key_id: &'a str,
    pub bundle_id: &'a str,
    pub device: &'a str,
    pub build: &'a str,
    pub platform: &'a str,
    pub configuration: &'a str,
}

fn copy_tree(source: &Path, into: &Path) -> Result<(), String> {
    // A clone on APFS: instant, and no extra space.
    let cloned = Command::new("/bin/cp").arg("-cR").arg(source).arg(into).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    if cloned { return Ok(()) }
    let status = Command::new("/bin/cp").arg("-R").arg(source).arg(into).status().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("couldn't copy {}", source.display())) }
}

/// Keeps the dSYMs of this build, those matching the app's own binaries, as an immutable set
/// `<archive>/<bundle id>@<build id>/` with a `uuids.json` index. Returns that index's path,
/// or None when the build made no dSYMs.
pub fn archive_dsyms(s: Symbols) -> Result<Option<PathBuf>, String> {
    let mut sources: Vec<PathBuf> = fs::read_dir(s.products).map_err(|e| e.to_string())?.filter_map(Result::ok).map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "dSYM")).collect();
    sources.sort();
    if sources.is_empty() {
        println!("WARNING: no dSYMs found; this build cannot be symbolicated");
        return Ok(None);
    }
    let mut wanted = BTreeSet::new();
    let mut binaries = Vec::new();
    files_under(s.app, &mut binaries);
    for binary in binaries.into_iter().filter(|f| is_macho(f)) {
        wanted.extend(dwarfdump(&binary)?.into_iter().map(|(uuid, _, _)| uuid));
    }
    let plain = |text: &str, extra: char| !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == extra);
    if !plain(s.bundle_id, '.') || !plain(s.build_id, '-') {
        return Err("invalid symbol archive identity".into());
    }
    fs::create_dir_all(s.archive).map_err(|e| e.to_string())?;
    let slot = s.archive.join(format!("{}@{}", s.bundle_id, s.build_id));
    let stage = s.archive.join(format!(".incoming-{}", super::random_hex(8)));
    fs::create_dir(&stage).map_err(|e| e.to_string())?;
    let result = (|| {
        let mut uuids = BTreeMap::new();
        let stage_real = fs::canonicalize(&stage).map_err(|e| e.to_string())?;
        for source in &sources {
            if !dwarfdump(source)?.iter().any(|(uuid, _, _)| wanted.contains(uuid)) {
                continue;
            }
            copy_tree(source, &stage)?;
            let copied = stage.join(source.file_name().unwrap_or_default());
            for (uuid, arch, dwarf) in dwarfdump(&copied)? {
                if !wanted.contains(&uuid) {
                    continue;
                }
                let dwarf = fs::canonicalize(&dwarf).map_err(|_| "missing archived DWARF file".to_string())?;
                let relative = dwarf.strip_prefix(&stage_real).map_err(|_| "archived DWARF file outside the archive".to_string())?.to_path_buf();
                let (sha256, bytes) = file_digest(&dwarf)?;
                uuids.entry(uuid).or_insert(json!({ "arch": arch, "dwarf": relative, "sha256": sha256, "bytes": bytes }));
            }
        }
        if uuids.is_empty() {
            return Err("no binary UUIDs found in the dSYMs".to_string());
        }
        let count = uuids.len();
        let record = json!({
            "format": "apparition-symbols/1", "buildId": s.build_id, "keyId": s.key_id, "bundleId": s.bundle_id,
            "device": s.device, "build": s.build, "platform": s.platform, "configuration": s.configuration,
            "archivedAt": utc_now().seconds, "uuids": uuids,
        });
        fs::write(stage.join("uuids.json"), serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        if slot.exists() {
            let old: Value = serde_json::from_slice(&fs::read(slot.join("uuids.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            if ["buildId", "bundleId", "build", "uuids"].iter().any(|k| old.get(k) != record.get(k)) {
                return Err("refusing to overwrite a different symbol set for this build id".into());
            }
        } else {
            fs::rename(&stage, &slot).map_err(|e| e.to_string())?;
        }
        println!("==> Kept immutable dSYMs for build {} ({count} UUIDs)", s.build_id);
        Ok(Some(slot.join("uuids.json")))
    })();
    if stage.exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

pub struct Publish<'a> {
    pub store: &'a Path,
    /// The signed app bundle the artifact holds.
    pub app: &'a Path,
    /// The signed IPA (or for a Mac app, the .app).
    pub artifact: &'a Path,
    pub project: &'a Path,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub build_id: &'a str,
    pub key_id: Option<&'a str>,
    pub platform: &'a str,
    pub configuration: &'a str,
    pub original_bundle_id: &'a str,
    pub symbols: Option<&'a Path>,
    /// For an unsigned build, the profile the device should sign it with (`signing_hint`).
    pub signing: Option<Value>,
}

/// Copies the artifact into the build store under its digest and writes its manifest, all
/// staged and renamed into place, so a reader never sees half a build. Returns the manifest's path.
pub fn publish(p: Publish) -> Result<PathBuf, String> {
    if p.build_id.is_empty() || p.build_id.len() > 64 || !p.build_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("invalid build identity".into());
    }
    let info = bundle::read_info(&bundle::info_path(p.app))?;
    {
        #[allow(unused_mut)]
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(p.store).map_err(|e| format!("can't create the build store {}: {e}", p.store.display()))?;
    }
    let destination = p.store.join(p.build_id);
    if destination.exists() {
        return Err("refusing to replace an existing build manifest".into());
    }
    let stage = p.store.join(format!(".incoming-{}", super::random_hex(8)));
    fs::create_dir(&stage).map_err(|e| e.to_string())?;
    let result = (|| {
        let staged = stage.join("artifact");
        let mut kind = if p.platform == "macOS" { "app.zip" } else { "ipa" };
        if p.artifact.is_dir() {
            kind = "app.zip";
            let status = Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(p.artifact).arg(&staged).status().map_err(|e| e.to_string())?;
            if !status.success() { return Err("ditto failed while keeping the build".to_string()) }
        } else {
            fs::copy(p.artifact, &staged).map_err(|e| format!("couldn't copy the build into the store: {e}"))?;
        }
        let (sha256, bytes) = file_digest(&staged)?;
        let name = format!("{sha256}.{kind}");
        fs::rename(&staged, stage.join(&name)).map_err(|e| e.to_string())?;
        let mut symbols = Value::Null;
        if let Some(path) = p.symbols.filter(|s| s.is_file()) {
            let record: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            if record["buildId"] != p.build_id || record["bundleId"].as_str() != bundle::string(&info, "CFBundleIdentifier").as_deref() {
                return Err("symbol identity does not match the build".into());
            }
            symbols = json!({ "path": path, "uuids": record["uuids"] });
        }
        let source = match (&p.before, &p.after) {
            (Some(before), Some(after)) => json!({ "before": before, "after": after, "stable": before["fingerprint"] == after["fingerprint"] }),
            _ => Value::Null,
        };
        let bundle_id = bundle::string(&info, "CFBundleIdentifier");
        let manifest = json!({
            "format": "apparition-build/1", "buildId": p.build_id, "keyId": p.key_id, "createdAt": utc_now().millis,
            "project": std::path::absolute(p.project).map_err(|e| e.to_string())?, "platform": p.platform, "configuration": p.configuration,
            "app": {
                "bundleId": bundle_id,
                "originalBundleId": if p.original_bundle_id.is_empty() { bundle_id.clone() } else { Some(p.original_bundle_id.to_string()) },
                "name": bundle::string(&info, "CFBundleDisplayName").or_else(|| bundle::string(&info, "CFBundleName")),
                "bundleName": p.app.file_name().map(|n| n.to_string_lossy().into_owned()),
                "minOSVersion": bundle::string(&info, "MinimumOSVersion").or_else(|| bundle::string(&info, "LSMinimumSystemVersion")),
                "version": bundle::string(&info, "CFBundleShortVersionString"),
                "build": bundle::string(&info, "CFBundleVersion"),
            },
            "artifact": { "sha256": sha256, "bytes": bytes, "kind": kind, "file": name },
            "binaries": uuid_index(p.app)?,
            "symbols": symbols,
            "source": source,
            "origin": { "kind": if p.before.is_some() && p.after.is_some() { "build-and-sign" } else { "resign" } },
            "signing": p.signing,
        });
        let mut file = super::private_file(&stage.join("manifest.json"))?;
        std::io::Write::write_all(&mut file, serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?.as_bytes()).map_err(|e| e.to_string())?;
        drop(file);
        fs::rename(&stage, &destination).map_err(|e| e.to_string())?;
        Ok(destination.join("manifest.json"))
    })();
    if stage.exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}
