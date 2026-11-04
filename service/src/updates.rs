//! Which installed apps are behind their sources, and why: the app's own branch, or a local
//! package it builds against (vergence for Hypnos, RAVESDK for most).
//!
//! A kept build's manifest records every source it was built from (`source.before.sources`: the
//! app and each local package, with its commit). So for an app on a device the planner finds the
//! kept build that is installed there, by its build ID from an install ledger or by the build
//! number the device reports, and compares each recorded commit with where that source is now.
//! A source that moved counts only when the commits change what the build compiles: for a
//! package, its manifest and its targets' folders (`Sources/` and any `path:`), less tests and
//! docs; for the app, anything but tests and docs. So a test-only commit to vergence leaves Hypnos
//! up to date, and a commit to its interpolator doesn't.
//!
//! Builds made in a build clone record the clone's paths; `real_path` maps them back to the
//! checkouts beside the app's, where the clone mirrors them.

use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

/// One app on one device that the Store builds: a platform row of a repo in config.json.
#[derive(Clone, Debug)]
pub struct Target {
    pub repo: String,
    pub repo_path: PathBuf,
    pub platform: String,
    pub device: String,
    pub bundle_id: String,
    /// The branch to follow when nothing installed says which (`deviceBranches`, else the
    /// repo's first branch).
    pub default_branch: String,
}

/// What is on a device, from the newest of: an install ledger (the Store's own, or the Node
/// store's), or the device's own inventory.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    pub build_no: String,
    pub build_id: Option<String>,
    pub branch: Option<String>,
    pub at: u64,
    /// "ledger" or "device".
    pub from: &'static str,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Reason {
    /// The source's folder name: the app's repo, or a package such as `vergence`.
    pub source: String,
    pub package: bool,
    /// Commits since the installed build that change the build.
    pub commits: usize,
    pub from: String,
    pub to: String,
    /// The newest such commit's subject.
    pub latest: String,
    /// When the newest such commit was made, in ms: the automatic mode waits for sources to settle.
    pub latest_at: u64,
    /// The installed commit is no longer in the source's history (a rebase or a reset).
    pub rewritten: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub repo: String,
    pub platform: String,
    pub device: String,
    pub bundle_id: String,
    pub branch: String,
    /// The app's commit on that branch now.
    pub head: String,
    pub installed: Option<Installed>,
    /// "current", "behind", "unknown" (can't tell; `note` says why) or "missing" (not installed).
    pub status: &'static str,
    pub reasons: Vec<Reason>,
    pub note: Option<String>,
    /// Commits since the installed build that change nothing the build compiles.
    pub ignored_commits: usize,
}

/// Kept-build manifests, each read once: a kept build never changes.
#[derive(Default)]
pub struct Manifests(Mutex<HashMap<String, Arc<Value>>>);

impl Manifests {
    /// Every kept build's manifest in `store`, newest first.
    pub fn all(&self, store: &Path) -> Vec<Arc<Value>> {
        let Ok(dir) = std::fs::read_dir(store) else { return Vec::new() };
        let ids: Vec<String> = dir.flatten().filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| crate::artifacts::valid_build_id(n)).collect();
        let mut cache = self.0.lock().unwrap();
        cache.retain(|id, _| ids.contains(id));
        for id in &ids {
            if cache.contains_key(id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(store.join(id).join("manifest.json")) else { continue };
            let Ok(manifest) = serde_json::from_str::<Value>(&text) else { continue };
            if manifest["buildId"].as_str() == Some(id) {
                cache.insert(id.clone(), Arc::new(manifest));
            }
        }
        let mut all: Vec<Arc<Value>> = cache.values().cloned().collect();
        all.sort_by(|a, b| b["createdAt"].as_str().unwrap_or("").cmp(a["createdAt"].as_str().unwrap_or("")));
        all
    }
}

/// Git, with each answer kept for one planning run: many apps share RAVESDK.
#[derive(Default)]
pub struct Git {
    answers: Mutex<HashMap<(PathBuf, Vec<String>), Option<String>>>,
}

impl Git {
    pub fn run(&self, dir: &Path, args: &[&str]) -> Option<String> {
        let key = (dir.to_path_buf(), args.iter().map(|a| a.to_string()).collect());
        if let Some(hit) = self.answers.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()
            .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string());
        self.answers.lock().unwrap().insert(key, out.clone());
        out
    }

    fn commit_of(&self, dir: &Path, rev: &str) -> Option<String> {
        self.run(dir, &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")]).filter(|s| !s.is_empty())
    }

    fn short(&self, dir: &Path, commit: &str) -> String {
        self.run(dir, &["rev-parse", "--short", commit]).unwrap_or_else(|| commit.chars().take(7).collect())
    }
}

/// A recorded source path, mapped from a build clone (`<root>/<repo>/<branch>/<rest>`) back to the
/// checkouts beside the app's (`<repo parent>/<rest>`).
pub fn real_path(recorded: &Path, clone_roots: &[PathBuf], repo_parent: &Path) -> PathBuf {
    for root in clone_roots {
        if let Ok(rest) = recorded.strip_prefix(root) {
            let rel: PathBuf = rest.components().skip(2).collect();
            if !rel.as_os_str().is_empty() {
                return repo_parent.join(rel);
            }
        }
    }
    recorded.to_path_buf()
}

/// Whether a changed file can change the app, by its path in its repo. `package`: the folders a
/// package's targets live in (from its Package.swift); None for the app's own repo, where only
/// tests and docs are ruled out.
pub fn affects_build(file: &str, package: Option<&[String]>) -> bool {
    let parts: Vec<&str> = file.split('/').collect();
    let name = parts.last().copied().unwrap_or("");
    let lower = name.to_ascii_lowercase();
    let doc = lower.ends_with(".md") || lower.ends_with(".markdown") || lower.starts_with("license") || lower == ".gitignore";
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let test_or_doc_dir = dirs.iter().any(|d| {
        matches!(*d, "Tests" | "tests" | "Docs" | "docs" | "Documentation" | ".github" | "Examples" | "Benchmarks") || d.ends_with("Tests")
    });
    if doc || test_or_doc_dir {
        return false;
    }
    match package {
        None => true,
        Some(folders) => {
            parts.len() == 1 && (name.starts_with("Package") && (name.ends_with(".swift") || name == "Package.resolved"))
                || folders.iter().any(|f| file.starts_with(&format!("{}/", f.trim_end_matches('/'))))
        }
    }
}

/// The folders a package's targets build from: `Sources` and every `path:` in Package.swift.
pub fn package_folders(manifest: &str) -> Vec<String> {
    let mut out = vec!["Sources".to_string()];
    let mut rest = manifest;
    while let Some(at) = rest.find("path:") {
        rest = &rest[at + 5..];
        let trimmed = rest.trim_start();
        if let Some(body) = trimmed.strip_prefix('"') {
            if let Some(end) = body.find('"') {
                let folder = body[..end].trim_start_matches("./").trim_end_matches('/').to_string();
                // `.package(path: "../X")` names another package, not a folder of this one.
                if !folder.is_empty() && !folder.starts_with("..") && !folder.starts_with('/') && !out.contains(&folder) {
                    out.push(folder);
                }
            }
        }
    }
    out
}

/// What changed in `dir` between `from` and `to` that changes the build, or None when nothing did.
/// `ignored` counts the commits that changed nothing the build compiles.
fn changes(git: &Git, dir: &Path, from: &str, to: &str, package: bool, ignored: &mut usize) -> Option<Reason> {
    let source = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if from == to {
        return None;
    }
    let reason = |commits, latest: String, latest_at, rewritten| Reason {
        source: source.clone(), package, commits, from: git.short(dir, from), to: git.short(dir, to), latest, latest_at, rewritten,
    };
    if git.commit_of(dir, from).is_none() || git.run(dir, &["merge-base", "--is-ancestor", from, to]).is_none() {
        let latest = git.run(dir, &["log", "-1", "--format=%ct%x09%s", to]).unwrap_or_default();
        let (at, subject) = latest.split_once('\t').unwrap_or(("0", ""));
        return Some(reason(0, subject.to_string(), at.parse::<u64>().unwrap_or(0) * 1000, true));
    }
    let folders = package.then(|| package_folders(&git.run(dir, &["show", &format!("{to}:Package.swift")]).unwrap_or_default()));
    let log = git.run(dir, &["log", "--format=%x00%h%x09%ct%x09%s", "--name-only", &format!("{from}..{to}")]).unwrap_or_default();
    let (mut count, mut newest): (usize, Option<(u64, String)>) = (0, None);
    for entry in log.split('\0').filter(|e| !e.trim().is_empty()) {
        let mut lines = entry.lines();
        let header = lines.next().unwrap_or("");
        let mut fields = header.splitn(3, '\t');
        let (_, at, subject) = (fields.next(), fields.next().unwrap_or("0"), fields.next().unwrap_or(""));
        let files: Vec<&str> = lines.map(str::trim).filter(|l| !l.is_empty()).collect();
        // A merge lists no files: count it when its parents' changes did.
        if files.iter().any(|f| affects_build(f, folders.as_deref())) {
            count += 1;
            if newest.is_none() {
                newest = Some((at.parse::<u64>().unwrap_or(0) * 1000, subject.to_string()));
            }
        } else if !files.is_empty() {
            *ignored += 1;
        }
    }
    let (latest_at, latest) = newest?;
    Some(reason(count, latest, latest_at, false))
}

/// The kept build that is installed: by build ID, else the newest kept build of that app with
/// that build number.
pub fn manifest_for<'a>(installed: &Installed, bundle_id: &str, manifests: &'a [Arc<Value>]) -> Option<&'a Arc<Value>> {
    if let Some(id) = &installed.build_id {
        if let Some(m) = manifests.iter().find(|m| m["buildId"].as_str() == Some(id)) {
            return Some(m);
        }
    }
    manifests.iter().find(|m| m.pointer("/app/bundleId").and_then(Value::as_str) == Some(bundle_id)
        && m.pointer("/app/build").and_then(Value::as_str) == Some(installed.build_no.as_str()))
}

/// The app commit of a build number: `<sha>[-<packages>][-dirty]`.
pub fn build_commit(build_no: &str) -> Option<&str> {
    let sha = build_no.split('-').next()?;
    ((7..=40).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit())).then_some(sha)
}

/// Plans one target. `clone_roots` are where build clones live (see `real_path`).
pub fn plan(target: &Target, installed: Option<&Installed>, manifests: &[Arc<Value>], clone_roots: &[PathBuf], git: &Git) -> Row {
    let repo_dir = target.repo_path.clone();
    let repo_parent = repo_dir.parent().map(Path::to_path_buf).unwrap_or_default();
    let manifest = installed.and_then(|i| manifest_for(i, &target.bundle_id, manifests));
    let sources: Vec<Value> = manifest.and_then(|m| m.pointer("/source/before/sources").or(m.pointer("/source/after/sources")))
        .and_then(Value::as_array).cloned().unwrap_or_default();
    // The app's own source: the deepest one holding the manifest's project.
    let project = manifest.and_then(|m| m["project"].as_str()).map(PathBuf::from);
    let app_source = project.as_ref().and_then(|p| sources.iter()
        .filter(|s| s["path"].as_str().is_some_and(|sp| p.starts_with(sp)))
        .max_by_key(|s| s["path"].as_str().map(str::len).unwrap_or(0)));
    let recorded_branch = app_source.and_then(|s| s["branch"].as_str()).filter(|b| !b.is_empty() && *b != "HEAD");
    let branch = installed.and_then(|i| i.branch.clone()).filter(|b| b != "current" && !b.is_empty())
        .or(recorded_branch.map(String::from)).unwrap_or_else(|| target.default_branch.clone());
    let head = git.commit_of(&repo_dir, &format!("refs/heads/{branch}"));
    let mut row = Row {
        repo: target.repo.clone(), platform: target.platform.clone(), device: target.device.clone(), bundle_id: target.bundle_id.clone(),
        branch: branch.clone(), head: head.as_deref().map(|h| git.short(&repo_dir, h)).unwrap_or_default(),
        installed: installed.cloned(), status: "current", reasons: Vec::new(), note: None, ignored_commits: 0,
    };
    let Some(installed) = installed else {
        row.status = "missing";
        return row;
    };
    let Some(head) = head else {
        row.status = "unknown";
        row.note = Some(format!("{} has no branch {branch}.", target.repo));
        return row;
    };
    let mut ignored = 0;
    if let (Some(_), Some(app)) = (manifest, app_source) {
        for source in &sources {
            let Some(recorded) = source["path"].as_str().map(PathBuf::from) else { continue };
            let is_app = source["path"] == app["path"];
            let real = if is_app { repo_dir.clone() } else { real_path(&recorded, clone_roots, &repo_parent) };
            let name = real.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Some(from) = source["commit"].as_str() else {
                row.note = Some(format!("{name} isn't in git, so its changes can't be followed."));
                continue;
            };
            let to = if is_app { Some(head.clone()) } else { git.commit_of(&real, "HEAD") };
            let Some(to) = to else {
                row.note = Some(format!("{name} is no longer at {}.", real.display()));
                continue;
            };
            if let Some(reason) = changes(git, &real, from, &to, !is_app, &mut ignored) {
                row.reasons.push(reason);
            }
        }
    } else if let Some(sha) = build_commit(&installed.build_no) {
        // A build this Store doesn't keep (Xcode, or kept builds pruned): only the app can be followed.
        match git.commit_of(&repo_dir, sha) {
            Some(from) => if let Some(reason) = changes(git, &repo_dir, &from, &head, false, &mut ignored) { row.reasons.push(reason) },
            None => row.reasons.push(Reason {
                source: repo_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), package: false, commits: 0,
                from: sha.to_string(), to: git.short(&repo_dir, &head), latest: String::new(), latest_at: 0, rewritten: true,
            }),
        }
        row.note = Some("The installed build isn't kept here, so its packages can't be compared.".into());
    } else {
        row.status = "unknown";
        row.note = Some(format!("The installed build number {} isn't a commit.", installed.build_no));
        return row;
    }
    row.ignored_commits = ignored;
    if !row.reasons.is_empty() {
        row.status = "behind";
    }
    row
}

/// Folds `.` and `..` out of a path without touching the disk.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, text: &str, message: &str) -> String {
        let path = dir.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    fn repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
    }

    #[test]
    fn test_and_doc_changes_never_count() {
        assert!(!affects_build("README.md", None));
        assert!(!affects_build("Tests/VergenceTests/X.swift", None));
        assert!(!affects_build("App/AppTests/X.swift", None));
        assert!(!affects_build("docs/plan.txt", None));
        assert!(affects_build("App/View.swift", None));
        assert!(affects_build("scripts/build-signing.conf", None), "an app's scripts can be its build");
        let folders = package_folders(r#"let p = Package(targets: [.target(name: "A"), .target(name: "B", path: "Lib/B"), .testTarget(name: "T", path: "Tests/T")], dependencies: [.package(path: "../X")])"#);
        assert_eq!(folders, ["Sources", "Lib/B", "Tests/T"]);
        assert!(affects_build("Sources/A/a.swift", Some(&folders)));
        assert!(affects_build("Lib/B/b.swift", Some(&folders)));
        assert!(affects_build("Package.swift", Some(&folders)));
        assert!(affects_build("Package.resolved", Some(&folders)));
        assert!(!affects_build("Tests/T/t.swift", Some(&folders)), "a test target's own path is still tests");
        assert!(!affects_build("scripts/build-android.sh", Some(&folders)), "a package's scripts aren't its targets");
        assert!(!affects_build("Tools/inject/main.swift", Some(&folders)));
    }

    #[test]
    fn a_clone_path_maps_back_to_the_checkouts() {
        let roots = [PathBuf::from("/data/clones")];
        let parent = Path::new("/Users/me/Projects");
        assert_eq!(real_path(Path::new("/data/clones/Hypnos/main/vergence"), &roots, parent), Path::new("/Users/me/Projects/vergence"));
        assert_eq!(real_path(Path::new("/data/clones/Hypnos/main/Hypnos"), &roots, parent), Path::new("/Users/me/Projects/Hypnos"));
        assert_eq!(real_path(Path::new("/Users/me/Projects/RAVESDK"), &roots, parent), Path::new("/Users/me/Projects/RAVESDK"));
    }

    #[test]
    fn build_numbers_name_the_app_commit() {
        assert_eq!(build_commit("b948ad6"), Some("b948ad6"));
        assert_eq!(build_commit("b948ad6-3fa1c2"), Some("b948ad6"));
        assert_eq!(build_commit("b948ad6-3fa1c2-dirty"), Some("b948ad6"));
        assert_eq!(build_commit("1.0"), None);
    }

    /// An app and a package beside it, a kept build of both, and the package moving on.
    #[test]
    fn a_package_change_that_compiles_makes_the_app_behind() {
        let root = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(root.path()).unwrap();
        let (app, kit) = (root.join("App"), root.join("Kit"));
        repo(&app);
        repo(&kit);
        let app_commit = commit(&app, "App/main.swift", "1", "app");
        let kit_commit = commit(&kit, "Package.swift", "let p = Package(targets: [.target(name: \"Kit\")])", "kit");
        let manifest = Arc::new(json!({
            "buildId": "b1", "createdAt": "2026-11-04T00:00:00Z", "project": app,
            "app": { "bundleId": "com.example.app", "build": "abc1234-000000" },
            "source": { "before": { "sources": [
                { "path": app, "commit": app_commit, "branch": "main", "dirty": false },
                { "path": kit, "commit": kit_commit, "branch": "main", "dirty": false },
            ] } },
        }));
        let target = Target { repo: "App".into(), repo_path: app.clone(), platform: "visionOS".into(), device: "AVP".into(), bundle_id: "com.example.app".into(), default_branch: "main".into() };
        let installed = Installed { build_no: "abc1234-000000".into(), build_id: None, branch: None, at: 1, from: "device" };
        let plan_now = || plan(&target, Some(&installed), &[manifest.clone()], &[], &Git::default());

        let row = plan_now();
        assert_eq!((row.status, row.branch.as_str()), ("current", "main"), "{row:?}");

        commit(&kit, "Tests/KitTests/t.swift", "t", "only tests");
        commit(&kit, "README.md", "r", "only docs");
        let row = plan_now();
        assert_eq!(row.status, "current", "{row:?}");
        assert_eq!(row.ignored_commits, 2);

        commit(&kit, "Sources/Kit/k.swift", "k", "Interpolate frames");
        let row = plan_now();
        assert_eq!(row.status, "behind");
        assert_eq!(row.reasons.len(), 1);
        let reason = &row.reasons[0];
        assert_eq!((reason.source.as_str(), reason.package, reason.commits, reason.latest.as_str()), ("Kit", true, 1, "Interpolate frames"));

        commit(&app, "App/main.swift", "2", "app change");
        let row = plan_now();
        assert_eq!(row.reasons.len(), 2, "{row:?}");
        assert!(row.reasons.iter().any(|r| r.source == "App" && !r.package));

        // Not installed: nothing to update.
        assert_eq!(plan(&target, None, &[manifest.clone()], &[], &Git::default()).status, "missing");
        // A build this Store doesn't keep: the app alone is followed.
        let other = Installed { build_no: app_commit[..7].to_string(), ..installed.clone() };
        let row = plan(&target, Some(&other), &[], &[], &Git::default());
        assert_eq!(row.status, "behind");
        assert!(row.note.as_deref().unwrap().contains("packages"));
    }
}
