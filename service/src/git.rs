//! What the state shows about each checkout: branch, last commit, whether tracked files are
//! dirty. Cached for ten seconds, as the Node store does.

use serde_json::{Value, json};
use std::{collections::HashMap, process::Command, sync::Mutex, time::{Duration, Instant}};

#[derive(Default)]
pub struct GitCache(Mutex<HashMap<String, (Instant, Value)>>);

fn git(path: &str, args: &[&str]) -> String {
    Command::new("git").arg("-C").arg(path).args(args).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// `git -C path args`, its trimmed output when it succeeded.
pub fn run(path: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(path).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl GitCache {
    pub fn info(&self, path: &str) -> Value {
        if let Some((at, info)) = self.0.lock().unwrap().get(path) {
            if at.elapsed() < Duration::from_secs(10) {
                return info.clone();
            }
        }
        let branch = git(path, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let log = git(path, &["log", "-1", "--format=%h%x09%s%x09%ct"]);
        let dirty = !git(path, &["status", "--porcelain", "--untracked-files=no"]).is_empty();
        let mut parts = log.splitn(3, '\t');
        let (commit, subject, ctime) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let info = json!({
            "branch": if branch.is_empty() { "?" } else { &branch },
            "commit": commit,
            "subject": subject,
            "commitTime": ctime.parse::<u64>().ok().map(|s| s * 1000),
            "dirty": dirty,
        });
        self.0.lock().unwrap().insert(path.to_string(), (Instant::now(), info.clone()));
        info
    }
}
