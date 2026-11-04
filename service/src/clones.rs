//! Build clones: where the Store builds a commit of a branch without touching the checkout, laid
//! out like the checkout's parent folder so the apps' relative package references
//! (`../../RAVESDK`) resolve.
//!
//! `<clones>/<repo>/<branch>/` holds the app as `<Repo>/` and each local package it builds
//! against beside it, transitively. A slot is kept between builds, so each one only checks out the
//! new commits and Xcode's DerivedData (in the clone's `build/`) builds incrementally; a slot
//! unused for a week goes (`reap`).
//!
//! Everything is clean, committed state: the app at the commit asked for, each package at its
//! checkout's HEAD, never an edit someone has not committed yet. The gitignored files a build
//! needs come from the checkout each time: `build-signing.conf`, `scripts/build-signing.conf` and
//! whatever the repo's `.worktree-include` lists.

use crate::updates::normalize;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

const SKIP_MANIFESTS: &[&str] = &[".build", "build", "DerivedData", "node_modules"];

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "._-".contains(c) { out.push(c) } else if !out.ends_with('-') { out.push('-') }
    }
    out
}

pub fn slot(clones: &Path, repo: &Path, branch: &str) -> PathBuf {
    clones.join(slug(&repo.file_name().unwrap_or_default().to_string_lossy())).join(slug(branch))
}

fn git(dir: &Path, args: &[&str], log: &mut dyn Write) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let _ = writeln!(log, "    git {}: {error}", args.join(" "));
        return Err(format!("git {} in {} failed: {error}", args.first().unwrap_or(&""), dir.display()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `cp -cR` (an APFS clone: instant, sharing blocks) of `from` into `to`, leaving out `exclude`
/// (paths relative to `from`, from the repo's `cloneExclude`): folders above an excluded path are
/// copied entry by entry, everything else whole.
pub fn clone_tree(from: &Path, to: &Path, exclude: &[String], rel: &str) -> Result<(), String> {
    let under = |x: &String| if rel.is_empty() { true } else { x.starts_with(&format!("{rel}/")) };
    if !exclude.iter().any(under) {
        let status = Command::new("cp").arg("-cR").arg(from).arg(to).stdin(Stdio::null()).stderr(Stdio::piped()).output()
            .map_err(|e| format!("cp: {e}"))?;
        if !status.status.success() {
            return Err(format!("couldn't copy {}: {}", from.display(), String::from_utf8_lossy(&status.stderr).trim()));
        }
        return Ok(());
    }
    fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(from).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if exclude.contains(&child) || child == ".git/index.lock" {
            continue;
        }
        clone_tree(&entry.path(), &to.join(&name), exclude, &child)?;
    }
    Ok(())
}

/// `.package(path: "…")` and `.package(name: "…", path: "…")` in a manifest.
pub fn package_swift_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(".package(") {
        rest = &rest[at + ".package(".len()..];
        let mut cursor = rest.trim_start();
        if let Some(after) = cursor.strip_prefix("name:") {
            let Some(after) = after.trim_start().strip_prefix('"') else { continue };
            let Some(end) = after.find('"') else { continue };
            let Some(after) = after[end + 1..].trim_start().strip_prefix(',') else { continue };
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
pub fn pbxproj_paths(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let marker = "isa = XCLocalSwiftPackageReference;";
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        rest = &rest[at + marker.len()..];
        let Some(after) = rest.trim_start().strip_prefix("relativePath = ") else { continue };
        let after = after.strip_prefix('"').unwrap_or(after);
        let end = after.find(['"', ';', '\n']).unwrap_or(after.len());
        if end > 0 && after[end..].trim_start_matches('"').starts_with(';') {
            found.push(after[..end].to_string());
        }
    }
    found
}

/// The local packages the projects and manifests under `top` reference, as absolute paths.
pub fn package_refs(top: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name == "Package.swift" {
                out.extend(package_swift_paths(&fs::read_to_string(&path).unwrap_or_default()).iter().map(|r| normalize(&dir.join(r))));
            } else if name.ends_with(".xcodeproj") {
                out.extend(pbxproj_paths(&fs::read_to_string(path.join("project.pbxproj")).unwrap_or_default()).iter().map(|r| normalize(&dir.join(r))));
            } else if depth < 4 && !name.starts_with('.') && !SKIP_MANIFESTS.contains(&name.as_str()) && entry.file_type().is_ok_and(|k| k.is_dir()) {
                walk(&path, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(top, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Gitignored files a build needs, relative to the repo: the signing confs and `.worktree-include`.
fn ignored_inputs(repo: &Path) -> Vec<String> {
    let mut out: Vec<String> = ["build-signing.conf", "scripts/build-signing.conf"].iter().map(|s| s.to_string()).collect();
    for line in fs::read_to_string(repo.join(".worktree-include")).unwrap_or_default().lines() {
        let line = line.trim().trim_start_matches("./");
        if !line.is_empty() && !line.starts_with('#') && !line.split('/').any(|p| p == "..") {
            out.push(line.to_string());
        }
    }
    out
}

/// Readies the slot for a build of `rev` of `branch` and returns the app's folder in it.
pub fn prepare(clones: &Path, repo: &Path, branch: &str, rev: &str, exclude: &[String], log: &mut dyn Write) -> Result<PathBuf, String> {
    let slot = slot(clones, repo, branch);
    let name = repo.file_name().ok_or("the repo has no folder name")?;
    let app = slot.join(name);
    let parent = repo.parent().ok_or("the repo has no parent folder")?;
    fs::create_dir_all(&slot).map_err(|e| e.to_string())?;
    if !app.join(".git").exists() {
        let _ = writeln!(log, "==> Making a build clone of {} (once; later builds reuse it)", repo.display());
        if app.exists() {
            fs::remove_dir_all(&app).map_err(|e| format!("couldn't clear {}: {e}", app.display()))?;
        }
        clone_tree(repo, &app, exclude, "")?;
    }
    // Into a ref of its own, so the clone's checked-out branch never blocks the fetch.
    git(&app, &["fetch", "-q", "--no-tags", &repo.to_string_lossy(), &format!("+refs/heads/{branch}:refs/remotes/store/{branch}")], log)?;
    git(&app, &["checkout", "-q", "-f", "-B", branch, rev], log)?;
    git(&app, &["clean", "-q", "-fd"], log)?;
    if app.join(".gitmodules").is_file() {
        git(&app, &["submodule", "update", "-q", "--init", "--recursive", "--force"], log)?;
    }
    for file in ignored_inputs(repo) {
        let (from, to) = (repo.join(&file), app.join(&file));
        if from.exists() {
            if to.is_dir() { let _ = fs::remove_dir_all(&to); } else { let _ = fs::remove_file(&to); }
            if let Some(dir) = to.parent() { fs::create_dir_all(dir).map_err(|e| e.to_string())?; }
            clone_tree(&from, &to, &[], "")?;
        }
    }
    // The packages, transitively, each at its checkout's HEAD.
    let mut queue = package_refs(&app);
    let mut done: Vec<PathBuf> = Vec::new();
    while let Some(package) = queue.pop() {
        if done.contains(&package) || package.starts_with(&app) {
            continue;
        }
        done.push(package.clone());
        let rel = package.strip_prefix(&slot).map_err(|_| format!("a package reference reaches outside the projects folder: {}", package.display()))?;
        let real = parent.join(rel);
        if !real.is_dir() {
            return Err(format!("the app builds against {}, which isn't there", real.display()));
        }
        let head = git(&real, &["rev-parse", "HEAD"], log).ok();
        match head {
            Some(head) => {
                if !package.join(".git").exists() {
                    if let Some(dir) = package.parent() { fs::create_dir_all(dir).map_err(|e| e.to_string())?; }
                    let _ = fs::remove_dir_all(&package);
                    let out = Command::new("git").args(["clone", "-q", "--shared", "--no-checkout"]).arg(&real).arg(&package)
                        .stdin(Stdio::null()).output().map_err(|e| format!("git: {e}"))?;
                    if !out.status.success() {
                        return Err(format!("couldn't clone {}: {}", real.display(), String::from_utf8_lossy(&out.stderr).trim()));
                    }
                }
                git(&package, &["fetch", "-q", "--no-tags", &real.to_string_lossy(), "HEAD"], log)?;
                git(&package, &["checkout", "-q", "-f", "--detach", &head], log)?;
                git(&package, &["clean", "-q", "-fd"], log)?;
                let _ = writeln!(log, "    {} at {}", rel.display(), &head[..head.len().min(10)]);
            }
            None => {
                // Not in git: as it is.
                let _ = fs::remove_dir_all(&package);
                if let Some(dir) = package.parent() { fs::create_dir_all(dir).map_err(|e| e.to_string())?; }
                clone_tree(&real, &package, &[], "")?;
                let _ = writeln!(log, "    {} (not in git, copied as it is)", rel.display());
            }
        }
        queue.extend(package_refs(&package));
    }
    touch(&slot);
    Ok(app)
}

fn touch(dir: &Path) {
    let _ = fs::File::open(dir).and_then(|f| f.set_modified(SystemTime::now()));
}

/// Deletes slots unused for `max_age`, never one in `busy`.
pub fn reap(clones: &Path, max_age: Duration, busy: &[PathBuf]) -> Vec<PathBuf> {
    let mut gone = Vec::new();
    for repo in fs::read_dir(clones).into_iter().flatten().flatten() {
        for branch in fs::read_dir(repo.path()).into_iter().flatten().flatten() {
            let dir = branch.path();
            let old = branch.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > max_age);
            if old && !busy.contains(&dir) && fs::remove_dir_all(&dir).is_ok() {
                gone.push(dir);
            }
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn a_slot_holds_committed_app_and_packages_and_the_ignored_conf() {
        let projects = tempfile::tempdir().unwrap();
        let projects = fs::canonicalize(projects.path()).unwrap();
        let (app, kit, base) = (projects.join("App"), projects.join("Kit"), projects.join("Base"));
        for (dir, text) in [(&app, "let p = Package(dependencies: [.package(path: \"../Kit\")])"), (&kit, "let p = Package(dependencies: [.package(name: \"Base\", path: \"../Base\")])"), (&base, "let p = Package()")] {
            fs::create_dir_all(dir).unwrap();
            run(dir, &["init", "-q", "-b", "main"]);
            write(&dir.join("Package.swift"), text);
            write(&dir.join(".gitignore"), "build/\nscripts/build-signing.conf\n");
            run(dir, &["add", "-A"]);
            run(dir, &["commit", "-q", "-m", "one"]);
        }
        write(&app.join("scripts/build-signing.conf"), "SCHEME_NAME=App");
        let first = run(&app, &["rev-parse", "HEAD"]);
        // Uncommitted edits in the checkouts never reach the clone.
        write(&app.join("Package.swift"), "edited");
        write(&kit.join("Sources/Kit/wip.swift"), "wip");
        let clones = projects.join("clones");
        let mut log = Vec::new();
        let built = prepare(&clones, &app, "main", &first, &[], &mut log).unwrap();
        assert_eq!(built, clones.join("App/main/App"));
        assert!(fs::read_to_string(built.join("Package.swift")).unwrap().contains(".package"), "the committed manifest");
        assert_eq!(fs::read_to_string(built.join("scripts/build-signing.conf")).unwrap(), "SCHEME_NAME=App");
        assert!(clones.join("App/main/Kit/Package.swift").is_file());
        assert!(!clones.join("App/main/Kit/Sources/Kit/wip.swift").exists(), "{}", String::from_utf8_lossy(&log));
        assert!(clones.join("App/main/Base/Package.swift").is_file(), "packages of packages too");
        assert_eq!(run(&built, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");

        // The next build checks out the new commits in the same slot, keeping its build folder.
        write(&built.join("build/DerivedData/cache"), "warm");
        run(&app, &["checkout", "-q", "--", "Package.swift"]);
        write(&app.join("App.swift"), "two");
        run(&app, &["add", "-A"]);
        run(&app, &["commit", "-q", "-m", "two"]);
        write(&kit.join("Sources/Kit/k.swift"), "k");
        run(&kit, &["add", "Sources/Kit/k.swift"]);
        run(&kit, &["commit", "-q", "-m", "kit two"]);
        let second = run(&app, &["rev-parse", "HEAD"]);
        let built = prepare(&clones, &app, "main", &second, &[], &mut log).unwrap();
        assert!(built.join("App.swift").is_file());
        assert!(clones.join("App/main/Kit/Sources/Kit/k.swift").is_file());
        assert_eq!(fs::read_to_string(built.join("build/DerivedData/cache")).unwrap(), "warm");
    }

    #[test]
    fn references_are_read_from_projects_and_manifests() {
        assert_eq!(package_swift_paths(r#".package(path: "../A"), .package(name: "B", path: "../../B"), .package(url: "x", from: "1")"#), ["../A", "../../B"]);
        assert_eq!(pbxproj_paths("isa = XCLocalSwiftPackageReference;\n\t\t\trelativePath = ../../RAVESDK;\n"), ["../../RAVESDK"]);
        assert_eq!(slug("feature/native mac"), "feature-native-mac");
    }
}
