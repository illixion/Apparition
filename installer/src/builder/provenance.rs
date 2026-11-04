//! What a build was made from, as `store/lib/artifacts.js` records it (`snapshot`, `stamp`): each
//! git checkout or plain directory the project builds from (its local Swift packages too,
//! transitively), whether it had uncommitted changes, a digest of those changes, the resolved
//! package versions and the toolchain. Taken before and after the build, so a manifest can say
//! whether the sources stayed the same while it built.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::{Value, json};

use super::{hex_sha256, file_digest};

/// Directories never walked for packages or a directory digest.
const SKIP_MANIFESTS: &[&str] = &[".build", "build", "DerivedData", "node_modules"];
const SKIP_DIRECTORY_STATE: &[&str] = &[".git", ".build", "build", "DerivedData", "node_modules", "target", ".DS_Store"];
const SKIP_RESOLVED: &[&str] = &["build", "DerivedData", "node_modules", "target", "vendor", "repos"];

fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn git_required(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    git(dir, args).ok_or_else(|| "git failed while recording build provenance".to_string())
}

fn trimmed(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u32 {
    std::os::unix::fs::MetadataExt::mode(meta)
}
#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

/// The content of a file for a digest: a symlink's target, else the file's SHA-256.
fn content(file: &Path, meta: &fs::Metadata) -> Result<String, String> {
    if meta.file_type().is_symlink() {
        Ok(fs::read_link(file).map_err(|e| e.to_string())?.to_string_lossy().into_owned())
    } else {
        Ok(file_digest(file)?.0)
    }
}

/// One checkout or directory: `artifacts.js`'s `sourceState`.
pub fn source_state(directory: &Path) -> Result<Value, String> {
    let Some(top) = git(directory, &["rev-parse", "--show-toplevel"]) else {
        let root = fs::canonicalize(directory).map_err(|e| e.to_string())?;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut files = 0u64;
        fn visit(dir: &Path, relative: &str, hash: &mut ring::digest::Context, files: &mut u64) -> Result<(), String> {
            let mut entries: Vec<_> = fs::read_dir(dir).map_err(|e| e.to_string())?.filter_map(Result::ok).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                if SKIP_DIRECTORY_STATE.contains(&name.as_str()) {
                    continue;
                }
                let path = entry.path();
                let relative = if relative.is_empty() { name } else { format!("{relative}/{name}") };
                let kind = entry.file_type().map_err(|e| e.to_string())?;
                if kind.is_dir() {
                    visit(&path, &relative, hash, files)?;
                } else if kind.is_file() || kind.is_symlink() {
                    let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                    hash.update(format!("{relative}\0{}\0", mode(&meta)).as_bytes());
                    hash.update(content(&path, &meta)?.as_bytes());
                    *files += 1;
                }
            }
            Ok(())
        }
        visit(&root, "", &mut hash, &mut files)?;
        return Ok(json!({
            "kind": "directory", "path": root, "commit": null, "branch": null, "dirty": true,
            "workingTreeSha256": hex(hash.finish().as_ref()), "files": files, "submodules": [],
        }));
    };
    let root = PathBuf::from(trimmed(top));
    let revision = git(&root, &["rev-parse", "--verify", "HEAD"]).map(trimmed);
    let status = git_required(&root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let changes = match revision {
        Some(_) => git_required(&root, &["diff", "--binary", "--no-ext-diff", "HEAD", "--"])?,
        None => [git_required(&root, &["diff", "--binary", "--no-ext-diff", "--cached", "--"])?, git_required(&root, &["diff", "--binary", "--no-ext-diff", "--"])?].concat(),
    };
    let untracked_list = git_required(&root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked: Vec<String> = untracked_list.split(|b| *b == 0).filter(|n| !n.is_empty()).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    untracked.sort();
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(&status);
    hash.update(&changes);
    for name in &untracked {
        let file = root.join(name);
        let meta = fs::symlink_metadata(&file).map_err(|e| e.to_string())?;
        hash.update(format!("\0{name}\0{}\0", mode(&meta)).as_bytes());
        hash.update(content(&file, &meta)?.as_bytes());
    }
    let branch = git(&root, &["symbolic-ref", "-q", "--short", "HEAD"]).map(trimmed);
    let stage = git_required(&root, &["ls-files", "--stage", "-z"])?;
    let submodules: Vec<String> = stage.split(|b| *b == 0).map(String::from_utf8_lossy)
        .filter(|row| row.starts_with("160000 "))
        .filter_map(|row| row.split_once('\t').map(|(_, path)| root.join(path).to_string_lossy().into_owned()))
        .collect();
    Ok(json!({
        "kind": "git", "path": fs::canonicalize(&root).map_err(|e| e.to_string())?,
        "commit": revision, "branch": branch, "dirty": !status.is_empty() || revision.is_none(),
        "workingTreeSha256": hex(hash.finish().as_ref()), "submodules": submodules,
    }))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every `Package.resolved` under `directory` (six levels down at most), with its digest.
fn resolved_packages(directory: &Path) -> Vec<Value> {
    let mut files = BTreeSet::new();
    fn visit(dir: &Path, depth: usize, files: &mut BTreeSet<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(kind) = entry.file_type() else { continue };
            if name == "Package.resolved" && kind.is_file() {
                files.insert(entry.path());
            }
            if kind.is_dir() && depth < 6 && !name.starts_with('.') && !SKIP_RESOLVED.contains(&name.as_str()) {
                visit(&entry.path(), depth + 1, files);
            }
        }
    }
    visit(directory, 0, &mut files);
    files.into_iter().filter_map(|file| {
        let (sha256, bytes) = file_digest(&file).ok()?;
        Some(json!({ "path": file.strip_prefix(directory).unwrap_or(&file), "sha256": sha256, "bytes": bytes }))
    }).collect()
}

/// The local package directories the Xcode projects and Swift package manifests under `top`
/// reference by relative path (`projects.js`'s `localPackageRefs`).
pub fn local_package_refs(top: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name == "Package.swift" {
                for relative in package_swift_paths(&fs::read_to_string(&path).unwrap_or_default()) {
                    out.push(normalize(&dir.join(relative)));
                }
            } else if name.ends_with(".xcodeproj") {
                for relative in pbxproj_paths(&fs::read_to_string(path.join("project.pbxproj")).unwrap_or_default()) {
                    out.push(normalize(&dir.join(relative)));
                }
            } else if depth < 4 && !name.starts_with('.') && !SKIP_MANIFESTS.contains(&name.as_str())
                && entry.file_type().is_ok_and(|k| k.is_dir() || k.is_symlink())
            {
                walk(&path, depth + 1, out);
            }
        }
    }
    walk(top, 0, &mut out);
    out
}

/// `path.resolve`: `..` and `.` folded away without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => { out.pop(); }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `.package(path: "…")` and `.package(name: "…", path: "…")` in a manifest.
fn package_swift_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(".package(") {
        rest = &rest[at + ".package(".len()..];
        let mut cursor = rest.trim_start();
        if let Some(after) = cursor.strip_prefix("name:") {
            let after = after.trim_start();
            let Some(after) = after.strip_prefix('"') else { continue };
            let Some(end) = after.find('"') else { continue };
            let after = after[end + 1..].trim_start();
            let Some(after) = after.strip_prefix(',') else { continue };
            cursor = after.trim_start();
        }
        let Some(after) = cursor.strip_prefix("path:") else { continue };
        let Some(after) = after.trim_start().strip_prefix('"') else { continue };
        if let Some(end) = after.find('"').filter(|&end| end > 0) {
            found.push(after[..end].to_string());
        }
    }
    found
}

/// `isa = XCLocalSwiftPackageReference; relativePath = …;` in a project file.
fn pbxproj_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let marker = "isa = XCLocalSwiftPackageReference;";
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        rest = &rest[at + marker.len()..];
        let Some(after) = rest.trim_start().strip_prefix("relativePath = ") else { continue };
        let after = after.strip_prefix('"').unwrap_or(after);
        let end = after.find(|c| c == '"' || c == ';' || c == '\n').unwrap_or(after.len());
        if end > 0 && after[end..].trim_start_matches('"').starts_with(';') {
            found.push(after[..end].to_string());
        }
    }
    found
}

/// The toolchain, with the confs' environment (`DEVELOPER_DIR`).
fn toolchain(environment: &[(String, String)]) -> Result<Value, String> {
    let run = |program: &str, args: &[&str]| -> Result<String, String> {
        let output = Command::new(program).args(args).envs(environment.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output()
            .map_err(|e| format!("{program}: {e}"))?;
        if !output.status.success() {
            return Err(format!("{program} failed while recording build provenance"));
        }
        Ok(trimmed(output.stdout))
    };
    Ok(json!({ "xcode": run("xcodebuild", &["-version"])?, "swift": run("xcrun", &["swiftc", "--version"])? }))
}

/// `snapshot(project, { argumentsFile })`: the sources `project` builds from, now.
pub fn snapshot(project: &Path, arguments: Option<&[u8]>, environment: Option<&[(String, String)]>) -> Result<Value, String> {
    let mut seen = BTreeSet::new();
    let mut roots: BTreeMap<String, Value> = BTreeMap::new();
    let mut missing = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::from([normalize(&std::path::absolute(project).map_err(|e| e.to_string())?)]);
    while let Some(directory) = queue.pop_front() {
        let Ok(canonical) = fs::canonicalize(&directory) else {
            missing.push(directory.to_string_lossy().into_owned());
            continue;
        };
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let mut source = source_state(&canonical)?;
        let path = source["path"].as_str().unwrap_or_default().to_string();
        if !roots.contains_key(&path) {
            source["resolvedPackages"] = Value::Array(resolved_packages(Path::new(&path)));
            for submodule in source["submodules"].as_array().cloned().unwrap_or_default() {
                if let Some(s) = submodule.as_str() {
                    queue.push_back(PathBuf::from(s));
                }
            }
            roots.insert(path, source);
        }
        queue.extend(local_package_refs(&canonical));
    }
    missing.sort();
    let sources: Vec<Value> = roots.into_values().collect();
    let any_dirty = sources.iter().any(|s| s["dirty"].as_bool() == Some(true));
    let builder = std::env::current_exe().ok().and_then(|exe| {
        let (sha256, bytes) = file_digest(&exe).ok()?;
        Some(json!([{ "file": "apparition", "sha256": sha256, "bytes": bytes }]))
    }).unwrap_or(json!([]));
    let mut result = json!({
        "scope": "git-or-directory-and-resolved-packages", "builder": builder, "sources": sources,
        "missing": missing, "anyDirty": any_dirty,
        "argumentsSha256": arguments.map(hex_sha256),
    });
    if let Some(environment) = environment {
        result["toolchain"] = toolchain(environment)?;
    }
    result["fingerprint"] = Value::String(hex_sha256(serde_json::to_string(&result).map_err(|e| e.to_string())?.as_bytes()));
    Ok(result)
}

/// `stamp`: the short commit the project was at, `-dirty` with uncommitted changes, or empty
/// for a project outside git.
pub fn stamp(snapshot: &Value, project: &Path) -> Result<String, String> {
    let directory = fs::canonicalize(project).map_err(|e| e.to_string())?;
    let source = snapshot["sources"].as_array().into_iter().flatten()
        .filter(|s| s["path"].as_str().is_some_and(|p| directory.starts_with(p)))
        .max_by_key(|s| s["path"].as_str().map(str::len).unwrap_or(0))
        .ok_or("the source snapshot is missing the project")?;
    let Some(commit) = source["commit"].as_str() else { return Ok(String::new()) };
    let short = git(project, &["rev-parse", "--short", commit]).map(trimmed).ok_or("cannot resolve the captured commit")?;
    Ok(if source["dirty"].as_bool() == Some(true) { format!("{short}-dirty") } else { short })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "Test").env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test").env("GIT_COMMITTER_EMAIL", "test@example.com")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn package_references_are_found_in_both_kinds_of_manifest() {
        let swift = r#"
            dependencies: [
                .package(path: "../RAVESDK"),
                .package(name: "DebugTrace", path: "../../DebugTrace"),
                .package(url: "https://example.com/x.git", from: "1.0.0"),
            ]"#;
        assert_eq!(package_swift_paths(swift), ["../RAVESDK", "../../DebugTrace"]);
        let pbxproj = "A1 /* XCLocalSwiftPackageReference \"x\" */ = {\n\t\t\tisa = XCLocalSwiftPackageReference;\n\t\t\trelativePath = ../RAVEEngine;\n\t\t};\n\
                       B2 = {\n\t\t\tisa = XCLocalSwiftPackageReference;\n\t\t\trelativePath = \"../Some Package\";\n\t\t};";
        assert_eq!(pbxproj_paths(pbxproj), ["../RAVEEngine", "../Some Package"]);
    }

    #[test]
    fn a_checkout_and_its_local_package_are_both_recorded_and_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("App");
        let package = dir.path().join("Kit");
        fs::create_dir_all(app.join("Sources")).unwrap();
        fs::create_dir_all(&package).unwrap();
        fs::write(app.join("Package.swift"), "let p = Package(dependencies: [.package(path: \"../Kit\")])").unwrap();
        fs::write(app.join("Sources/main.swift"), "print(1)").unwrap();
        fs::write(package.join("Package.resolved"), "{}").unwrap();
        run(&app, &["init", "-q", "-b", "main"]);
        run(&app, &["add", "."]);
        run(&app, &["commit", "-q", "-m", "one"]);

        let clean = snapshot(&app, Some(b"args"), None).unwrap();
        // Kit is a plain directory, which always counts as changed.
        assert_eq!(clean["anyDirty"], serde_json::json!(true));
        let sources = clean["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 2, "{clean:#}");
        let kit = sources.iter().find(|s| s["kind"] == "directory").unwrap();
        assert_eq!(kit["resolvedPackages"][0]["path"], "Package.resolved");
        // The app's own checkout is clean.
        assert_eq!(sources.iter().find(|s| s["kind"] == "git").unwrap()["dirty"], serde_json::json!(false));
        assert_eq!(clean["argumentsSha256"], serde_json::json!(hex_sha256(b"args")));
        let short = trimmed(git(&app, &["rev-parse", "--short", "HEAD"]).unwrap());
        assert_eq!(stamp(&clean, &app).unwrap(), short);
        assert_eq!(snapshot(&app, Some(b"args"), None).unwrap()["fingerprint"], clean["fingerprint"], "the same sources give the same fingerprint");

        fs::write(app.join("Sources/main.swift"), "print(2)").unwrap();
        let edited = snapshot(&app, Some(b"args"), None).unwrap();
        assert_ne!(edited["fingerprint"], clean["fingerprint"]);
        assert_eq!(stamp(&edited, &app).unwrap(), format!("{short}-dirty"));
    }

    #[test]
    fn a_project_outside_git_has_no_stamp() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let snap = snapshot(dir.path(), None, None).unwrap();
        assert_eq!(snap["sources"][0]["kind"], "directory");
        assert_eq!(snap["sources"][0]["files"], serde_json::json!(1));
        assert_eq!(stamp(&snap, dir.path()).unwrap(), "");
    }
}
