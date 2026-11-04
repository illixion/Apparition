//! `data/state.json` and the `/api/state` bundle (`store/server.js`: `state`, `stateBundle`).
//! The file is read as written, so a Mac switching from the Node store keeps its build history,
//! install ledger and OTA copies. Unknown keys round-trip.

use crate::{config::Config, git::GitCache, install_mode::Presence};
use serde_json::{Map, Value, json};
use std::{path::Path, sync::Mutex};

pub struct State {
    value: Mutex<Value>,
    pub git: GitCache,
    pub presence: Presence,
}

fn defaults() -> Map<String, Value> {
    let Value::Object(map) = json!({ "installs": {}, "deviceApps": {}, "devices": null, "builds": [], "nextBuildId": 1, "ota": {} }) else { unreachable!() };
    map
}

impl State {
    pub fn load(data_dir: &Path) -> State {
        let mut state = defaults();
        if let Some(Value::Object(file)) = std::fs::read_to_string(data_dir.join("state.json")).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            state.extend(file);
        }
        // Anything mid-flight when the last server died is failed now: its queue is gone.
        if let Some(builds) = state.get_mut("builds").and_then(Value::as_array_mut) {
            for build in builds.iter_mut().filter_map(Value::as_object_mut) {
                if matches!(build.get("status").and_then(Value::as_str), Some("running" | "queued")) {
                    build.insert("status".into(), "failed".into());
                    build.insert("error".into(), "server restarted during build".into());
                }
            }
        }
        State { value: Mutex::new(Value::Object(state)), git: GitCache::default(), presence: Presence::default() }
    }

    fn field(&self, key: &str) -> Value {
        self.value.lock().unwrap().get(key).cloned().unwrap_or(Value::Null)
    }

    /// What `GET /api/state` returns. Parts of the Node bundle whose features are not ported yet
    /// (Mac app builds, running debug apps, the build queue) are empty, not invented.
    pub fn bundle(&self, config: &Config, now: u64, host: &str) -> Value {
        let repos: Vec<Value> = config.raw.get("repos").and_then(Value::as_array).into_iter().flatten().map(|repo| {
            let path = repo.get("path").and_then(Value::as_str).unwrap_or("");
            json!({
                "name": repo.get("name"), "branches": repo.get("branches"), "platforms": repo.get("platforms"),
                "deviceBranches": repo.get("deviceBranches").cloned().unwrap_or_else(|| json!({})),
                "deployDefaults": config.deploy_defaults(repo),
                "macApps": [],
                "git": self.git.info(path),
            })
        }).collect();
        let builds: Vec<Value> = self.field("builds").as_array().map(|b| b.iter().rev().take(40).cloned().collect()).unwrap_or_default();
        json!({
            "now": now, "host": host, "repos": repos,
            "devices": self.field("devices"), "deviceApps": self.field("deviceApps"),
            "ledgerOnly": config.skipped_devices(),
            "installs": self.field("installs"),
            "appsOpen": self.presence.open(now),
            "ota": self.field("ota"),
            "debugApps": { "apps": [], "error": null },
            "authRequired": config.password_hash.is_some(),
            "builds": builds, "running": [], "queued": [],
        })
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into the buffer we own.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    if !ok {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
