//! `data/state.json` and the `/api/state` bundle. The Store owns the file: it keeps what is
//! installed where (`installs`), what each device reported having (`deviceApps`), the device list
//! and the OTA copies (`ota`). A Mac that ran the Node store before keeps its history, since the
//! file is read as written; unknown keys round-trip.

use crate::{config::Config, git::GitCache, install_mode::Presence};
use serde_json::{Map, Value, json};
use std::{path::{Path, PathBuf}, sync::Mutex};

pub struct State {
    value: Mutex<Value>,
    file: PathBuf,
    pub git: GitCache,
    pub presence: Presence,
}

fn defaults() -> Map<String, Value> {
    let Value::Object(map) = json!({ "installs": {}, "deviceApps": {}, "devices": null, "builds": [], "nextBuildId": 1, "ota": {} }) else { unreachable!() };
    map
}

impl State {
    pub fn load(data_dir: &Path) -> State {
        let file = data_dir.join("state.json");
        let mut state = defaults();
        if let Some(Value::Object(saved)) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()) {
            state.extend(saved);
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
        State { value: Mutex::new(Value::Object(state)), file, git: GitCache::default(), presence: Presence::default() }
    }

    pub fn field(&self, key: &str) -> Value {
        self.value.lock().unwrap().get(key).cloned().unwrap_or(Value::Null)
    }

    /// Changes the state and writes it (atomically: a temporary file, then a rename).
    pub fn update<R>(&self, change: impl FnOnce(&mut Map<String, Value>) -> R) -> R {
        let mut guard = self.value.lock().unwrap();
        let Some(map) = guard.as_object_mut() else { unreachable!("the state is an object") };
        let out = change(map);
        if let Ok(text) = serde_json::to_string_pretty(&*guard) {
            let tmp = self.file.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.file);
            }
        }
        out
    }

    /// What `device` has installed from this Store: the ledger entry for `bundle_id`.
    pub fn installed(&self, device: &str, bundle_id: &str) -> Option<Value> {
        self.value.lock().unwrap().pointer(&format!("/installs/{}/{}", escape(device), escape(bundle_id))).cloned()
    }

    /// Records what a device runs now (the install ledger).
    pub fn record_install(&self, device: &str, bundle_id: &str, entry: Value) {
        self.update(|s| {
            let installs = s.entry("installs").or_insert_with(|| json!({}));
            if !installs.is_object() { *installs = json!({}); }
            let device = installs.as_object_mut().unwrap().entry(device).or_insert_with(|| json!({}));
            if !device.is_object() { *device = json!({}); }
            device[bundle_id] = entry;
        });
    }

    /// What `GET /api/state` returns. `mac_apps(repo)` is that repo's Mac downloads.
    pub fn bundle(&self, config: &Config, now: u64, host: &str, mac_apps: &dyn Fn(&str) -> Value) -> Value {
        let repos: Vec<Value> = config.raw.get("repos").and_then(Value::as_array).into_iter().flatten().map(|repo| {
            let path = repo.get("path").and_then(Value::as_str).unwrap_or("");
            let name = repo.get("name").and_then(Value::as_str).unwrap_or("");
            json!({
                "name": repo.get("name"), "branches": repo.get("branches"), "platforms": repo.get("platforms"),
                "deviceBranches": repo.get("deviceBranches").cloned().unwrap_or_else(|| json!({})),
                "deployDefaults": config.deploy_defaults(repo),
                "macApps": mac_apps(name),
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

/// A JSON pointer segment.
fn escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

#[cfg(windows)]
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

#[cfg(unix)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_are_written_and_read_back_by_the_next_process() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::load(dir.path());
        state.record_install("ixPhone", "com.example.app", json!({ "buildNo": "abc1234", "confirmed": true }));
        assert_eq!(State::load(dir.path()).installed("ixPhone", "com.example.app").unwrap()["buildNo"], "abc1234");
        // Device and bundle names containing "/" and "~" are still found.
        state.record_install("a/b~c", "x", json!({ "buildNo": "z" }));
        assert_eq!(state.installed("a/b~c", "x").unwrap()["buildNo"], "z");
    }

    #[test]
    fn unknown_keys_survive_a_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), r#"{"somethingNew":{"a":1},"builds":[{"id":3,"status":"running"}]}"#).unwrap();
        let state = State::load(dir.path());
        state.update(|s| { s.insert("devices".into(), json!({ "list": [] })); });
        let again: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("state.json")).unwrap()).unwrap();
        assert_eq!(again["somethingNew"]["a"], 1);
        assert_eq!(again["builds"][0]["status"], "failed", "a build that was running when the last process died has failed");
    }
}
