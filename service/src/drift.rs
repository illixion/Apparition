//! Whether what a device runs still matches the sources on this Mac.
//!
//! A kept build's manifest records, for each checkout it was built from (the app, its local
//! packages such as RAVESDK and RAVEEngine, submodules), the commit and a digest of the
//! uncommitted changes (`workingTreeSha256`, `installer/src/builder/provenance.rs`). This module
//! takes that state again now and says which sources moved, so a dirty tree that was built is
//! "up to date" and one edited since is "out of date", naming the checkout.
//!
//! The digest is the installer's byte for byte (same git commands, same order); the shared literal
//! in both test suites keeps them in step.
//!
//! Cost: a clean checkout needs one `rev-parse` and one `git status`, which read the index's stat
//! cache. A dirty one needs its digest, which reads only the changed and untracked files, and that
//! is memoized on (HEAD, status output, size and mtime of each listed file), so an unchanged dirty
//! tree is never hashed twice. Nothing walks the tree.

use serde_json::{Value, json};
use std::{collections::HashMap, fs, path::Path, process::Command, sync::Mutex, time::{Duration, Instant, SystemTime}};

#[derive(Default)]
pub struct DriftCache {
    /// (path, key) -> digest, so an unchanged dirty tree is hashed once.
    digests: Mutex<HashMap<String, (String, String)>>,
    /// build id -> (when, answer): a 10 second cache like the git info's.
    answers: Mutex<HashMap<String, (Instant, Value)>>,
}

fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn mode(meta: &fs::Metadata) -> u32 {
    std::os::unix::fs::MetadataExt::mode(meta)
}
#[cfg(not(unix))]
fn mode(_: &fs::Metadata) -> u32 {
    0
}

fn file_hex(file: &Path, meta: &fs::Metadata) -> Option<String> {
    if meta.file_type().is_symlink() {
        return Some(fs::read_link(file).ok()?.to_string_lossy().into_owned());
    }
    let mut f = fs::File::open(file).ok()?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buffer).ok()?;
        if n == 0 { break }
        hash.update(&buffer[..n]);
    }
    Some(hex(hash.finish().as_ref()))
}

/// `source_state`'s `workingTreeSha256`, from the pieces already read.
fn working_tree_digest(root: &Path, status: &[u8], has_head: bool) -> Option<String> {
    let changes = if has_head {
        git(root, &["diff", "--binary", "--no-ext-diff", "HEAD", "--"])?
    } else {
        [git(root, &["diff", "--binary", "--no-ext-diff", "--cached", "--"])?, git(root, &["diff", "--binary", "--no-ext-diff", "--"])?].concat()
    };
    let list = git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut untracked: Vec<String> = list.split(|b| *b == 0).filter(|n| !n.is_empty()).map(|n| String::from_utf8_lossy(n).into_owned()).collect();
    untracked.sort();
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(status);
    hash.update(&changes);
    for name in &untracked {
        let file = root.join(name);
        let meta = fs::symlink_metadata(&file).ok()?;
        hash.update(format!("\0{name}\0{}\0", mode(&meta)).as_bytes());
        hash.update(file_hex(&file, &meta)?.as_bytes());
    }
    Some(hex(hash.finish().as_ref()))
}

/// Size and mtime of every file `git status` lists: what changes when its contents do.
fn stat_key(root: &Path, status: &[u8]) -> String {
    let mut key = String::new();
    // `XY path\0`, and a rename or copy is followed by its origin as a bare `path\0`.
    for entry in status.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let text = String::from_utf8_lossy(entry);
        let path = if text.len() > 3 && text.as_bytes()[2] == b' ' { &text[3..] } else { &text[..] };
        if let Ok(meta) = fs::symlink_metadata(root.join(path)) {
            let modified = meta.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
            key.push_str(&format!("{path}\0{}\0{modified}\n", meta.len()));
        }
    }
    key
}

impl DriftCache {
    /// One checkout's commit, whether it is dirty, and the digest of its changes, or None when it
    /// is not a checkout any more.
    fn current(&self, path: &str) -> Option<(String, bool, String)> {
        let root = Path::new(path);
        let head = git(root, &["rev-parse", "--verify", "HEAD"]).map(|h| String::from_utf8_lossy(&h).trim().to_string());
        let status = git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        let dirty = !status.is_empty() || head.is_none();
        let empty = hex(ring::digest::digest(&ring::digest::SHA256, b"").as_ref());
        if !dirty {
            return Some((head.unwrap_or_default(), false, empty));
        }
        let key = format!("{}\n{}\n{}", head.clone().unwrap_or_default(), String::from_utf8_lossy(&status), stat_key(root, &status));
        if let Some((k, digest)) = self.digests.lock().unwrap().get(path) {
            if *k == key {
                return Some((head.unwrap_or_default(), true, digest.clone()));
            }
        }
        let digest = working_tree_digest(root, &status, head.is_some())?;
        self.digests.lock().unwrap().insert(path.to_string(), (key, digest.clone()));
        Some((head.unwrap_or_default(), true, digest))
    }

    /// `{ state: "same" | "changed" | "unknown", changed: [{ path, why }], dirty: [...] }` for a
    /// kept build's manifest against the sources now. `dirty` is the build's own record of the
    /// sources that had uncommitted changes when it was made (what stamped it -dirty: `path`,
    /// `kind`, and for a checkout its `changes`, `git status --short` lines, and `changesTotal`). Unknown when the manifest has no usable snapshot, the
    /// sources moved while it built, or a checkout it names is gone (a build clone).
    pub fn compare(&self, build_id: &str, manifest: &Value) -> Value {
        if let Some((at, answer)) = self.answers.lock().unwrap().get(build_id) {
            if at.elapsed() < Duration::from_secs(10) {
                return answer.clone();
            }
        }
        let answer = self.compare_uncached(manifest);
        self.answers.lock().unwrap().insert(build_id.to_string(), (Instant::now(), answer.clone()));
        answer
    }

    fn compare_uncached(&self, manifest: &Value) -> Value {
        let sources = manifest.pointer("/source/before/sources").and_then(Value::as_array);
        let dirty: Vec<Value> = sources.into_iter().flatten().filter(|s| s["dirty"].as_bool() == Some(true)).map(|s| json!({
            "path": s["path"], "kind": s["kind"], "changes": s["changes"], "changesTotal": s["changesTotal"],
        })).collect();
        let unknown = |why: &str| json!({ "state": "unknown", "why": why, "changed": [], "dirty": dirty });
        let Some(sources) = sources else { return unknown("no source snapshot") };
        if manifest.pointer("/source/stable").and_then(Value::as_bool) == Some(false) {
            return unknown("the sources changed while it built");
        }
        let mut changed = Vec::new();
        for source in sources {
            if source["kind"] != "git" {
                continue;
            }
            let path = source["path"].as_str().unwrap_or_default();
            let Some((commit, dirty, digest)) = self.current(path) else { return unknown("a source checkout is gone") };
            let built_commit = source["commit"].as_str().unwrap_or_default();
            let short = |c: &str| c.chars().take(7).collect::<String>();
            if commit != built_commit {
                changed.push(json!({ "path": path, "why": format!("commit {} → {}", short(built_commit), short(&commit)) }));
            } else if dirty != source["dirty"].as_bool().unwrap_or(false) || digest != source["workingTreeSha256"].as_str().unwrap_or_default() {
                changed.push(json!({ "path": path, "why": if dirty { "uncommitted changes differ from the build" } else { "the build's uncommitted changes were committed or discarded" } }));
            }
        }
        json!({ "state": if changed.is_empty() { "same" } else { "changed" }, "changed": changed, "dirty": dirty })
    }
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

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.swift"), "let a = 1\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "one"]);
        dir
    }

    fn manifest_of(cache: &DriftCache, dir: &Path) -> Value {
        let path = fs::canonicalize(dir).unwrap().to_string_lossy().into_owned();
        let (commit, dirty, digest) = cache.current(&path).unwrap();
        json!({ "source": { "stable": true, "before": { "sources": [
            { "kind": "git", "path": path, "commit": commit, "dirty": dirty, "workingTreeSha256": digest }
        ] } } })
    }

    #[test]
    fn a_built_dirty_tree_is_the_same_until_it_is_edited() {
        let dir = repo();
        let cache = DriftCache::default();
        fs::write(dir.path().join("a.swift"), "let a = 2\n").unwrap();
        fs::write(dir.path().join("new.swift"), "let n = 1\n").unwrap();
        let manifest = manifest_of(&cache, dir.path());
        assert_eq!(cache.compare_uncached(&manifest)["state"], "same");
        fs::write(dir.path().join("new.swift"), "let n = 2\n").unwrap();
        let answer = cache.compare_uncached(&manifest);
        assert_eq!(answer["state"], "changed");
        assert!(answer["changed"][0]["why"].as_str().unwrap().contains("uncommitted"));
    }

    #[test]
    fn the_builds_dirty_sources_are_passed_on() {
        let cache = DriftCache::default();
        let manifest = json!({ "source": { "before": { "sources": [
            { "kind": "git", "path": "/nonexistent/DebugTrace", "commit": "a", "dirty": true,
              "changes": ["?? build/sym-motion/"], "changesTotal": 1 },
            { "kind": "git", "path": "/nonexistent/App", "commit": "b", "dirty": false },
        ] } } });
        let answer = cache.compare_uncached(&manifest);
        assert_eq!(answer["dirty"], json!([{ "path": "/nonexistent/DebugTrace", "kind": "git",
            "changes": ["?? build/sym-motion/"], "changesTotal": 1 }]), "even when the comparison itself is unknown");
    }

    #[test]
    fn a_new_commit_is_named() {
        let dir = repo();
        let cache = DriftCache::default();
        let manifest = manifest_of(&cache, dir.path());
        assert_eq!(cache.compare_uncached(&manifest)["state"], "same");
        fs::write(dir.path().join("b.swift"), "b").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "two"]);
        let answer = cache.compare_uncached(&manifest);
        assert_eq!(answer["state"], "changed");
        assert!(answer["changed"][0]["why"].as_str().unwrap().starts_with("commit "));
    }

    #[test]
    fn a_gone_checkout_or_missing_snapshot_is_unknown() {
        let cache = DriftCache::default();
        assert_eq!(cache.compare_uncached(&json!({}))["state"], "unknown");
        let gone = json!({ "source": { "before": { "sources": [{ "kind": "git", "path": "/nonexistent/x", "commit": "a", "dirty": false }] } } });
        assert_eq!(cache.compare_uncached(&gone)["state"], "unknown");
    }

    #[test]
    fn the_digest_is_the_installers() {
        // The same fixture and literal sit in installer/src/builder/provenance.rs's tests.
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"]);
        fs::write(dir.path().join("a.txt"), "tracked\n").unwrap();
        run(dir.path(), &["add", "."]);
        run(dir.path(), &["commit", "-q", "-m", "one"]);
        fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
        fs::write(dir.path().join("u.txt"), "untracked\n").unwrap();
        let path = fs::canonicalize(dir.path()).unwrap().to_string_lossy().into_owned();
        let (_, dirty, digest) = DriftCache::default().current(&path).unwrap();
        assert!(dirty);
        assert_eq!(digest, PARITY_DIGEST);
    }

    /// The installer's test pins the same value.
    const PARITY_DIGEST: &str = "341b584c4c0b33f65b0c48f0d0f67255c66335070e3d9f1172cc5494a72505da";
}
