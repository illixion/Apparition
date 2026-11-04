//! `GET /api/state` from the Rust service against what the Node store answered for the same config,
//! state file and git checkout, captured when it was removed (see `NODE_STATE` below), and against
//! a `state.json` as the Node store wrote it (`tests/fixtures/node-state/`). Skipped without `git`.

use apparition_service::server::{Settings, router};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::{path::Path, process::{Command, Stdio}};
use tower::ServiceExt;

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", "2026-11-01T10:00:00Z").env("GIT_COMMITTER_DATE", "2026-11-01T10:00:00Z").env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success();
    assert!(ok, "git {args:?}");
}

#[tokio::test]
async fn state_matches_what_the_node_store_answered() {
    if !have("git") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "First\tcommit"]);
    std::fs::write(repo.join("a.txt"), "changed").unwrap(); // a dirty tracked file

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let config = json!({
        "port": port, "bundlePrefix": ["com.x"],
        "deployDefaults": { "mcp": false, "jit": "yes" },
        "ledgerOnlyDevices": ["Apple TV", "Quiet TV"], "quietAppsDevices": ["Quiet TV"],
        "repos": [{
            "name": "App", "path": repo, "branches": ["main", "dev"],
            "platforms": [{ "platform": "iOS", "bundleId": "com.x.app", "device": "iPhone" }],
            "deviceBranches": { "iPhone": "dev" },
            "deployDefaults": { "launch": true, "inject": "no" },
        }],
    });
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    let builds: Vec<Value> = (1..=45).map(|id| json!({ "id": id, "repo": "App", "status": if id == 44 { "running" } else if id == 45 { "queued" } else { "done" } })).collect();
    let state = json!({
        "installs": { "iPhone": { "com.x.app": { "repo": "App", "commit": "abc", "at": 1 } } },
        "ota": { "com.x.app|iOS": { "slug": "app", "size": 7 } },
        "builds": builds, "nextBuildId": 46, "futureKey": [1],
    });
    std::fs::create_dir(dir.path().join("data")).unwrap();
    std::fs::write(dir.path().join("data/state.json"), state.to_string()).unwrap();

    let app = router(Settings { host_token: None, key_dir: dir.path().join("keys"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("none")) }).unwrap();
    let res = app.oneshot(Request::builder().uri("/api/state").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    let ours: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap()).unwrap();

    // What the Node store answered on this exact setup (store/server.js `GET /api/state`, run on the
    // same config, state file and a git checkout with the commit dates pinned above), captured at
    // removal time, minus `now` (the clock), `host` (the machine) and `devices`, which Node filled
    // from devicectl. The commit `f2ce1c2` is deterministic: fixed tree, message, author and dates.
    let mut builds = vec![
        json!({ "id": 45, "repo": "App", "status": "failed", "error": "server restarted during build" }),
        json!({ "id": 44, "repo": "App", "status": "failed", "error": "server restarted during build" }),
    ];
    builds.extend((6..=43).rev().map(|id| json!({ "id": id, "repo": "App", "status": "done" })));
    let expected = json!({
        "repos": [{
            "name": "App", "branches": ["main", "dev"],
            "platforms": [{ "platform": "iOS", "bundleId": "com.x.app", "device": "iPhone" }],
            "deviceBranches": { "iPhone": "dev" },
            "deployDefaults": { "launch": true, "mcp": false, "jit": false, "buildDebug": false, "inject": false },
            "macApps": [],
            "git": { "branch": "main", "commit": "f2ce1c2", "subject": "First", "commitTime": null, "dirty": true },
        }],
        "deviceApps": {}, "ledgerOnly": ["Apple TV"],
        "installs": { "iPhone": { "com.x.app": { "repo": "App", "commit": "abc", "at": 1 } } },
        "appsOpen": [], "ota": { "com.x.app|iOS": { "slug": "app", "size": 7 } },
        "debugApps": { "apps": [], "error": null }, "authRequired": false,
        "builds": builds, "running": [], "queued": [],
    });
    let mut got = ours.clone();
    let fields = got.as_object_mut().unwrap();
    for gone in ["now", "host", "devices"] {
        assert!(fields.contains_key(gone), "{gone}");
        fields.remove(gone);
    }
    let names = |v: &Value| -> Vec<String> { let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect(); k.sort(); k };
    assert_eq!(names(&got), names(&expected), "the same set of fields as Node");
    for key in names(&expected) {
        assert_eq!(got[&key], expected[&key], "field {key}");
    }
    assert_eq!(ours["devices"], Value::Null, "no device list until the first refresh");
    assert_eq!(ours["builds"].as_array().unwrap().len(), 40);
    assert_eq!(ours["builds"][0]["status"], "failed", "mid-flight builds are failed on start");
}

/// A `state.json` exactly as the Node store left it after running (one-space indent, the device
/// cache, a field this build does not know), with the device names anonymised.
#[tokio::test]
async fn a_state_file_written_by_the_node_store_is_served_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), json!({ "port": 1, "repos": [{ "name": "App", "path": dir.path(), "branches": ["main"], "platforms": [{ "platform": "iOS", "bundleId": "com.x.app", "device": "iPhone" }] }] }).to_string()).unwrap();
    std::fs::create_dir(dir.path().join("data")).unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/node-state/state.json");
    std::fs::copy(&fixture, dir.path().join("data/state.json")).unwrap();
    let node: Value = serde_json::from_slice(&std::fs::read(&fixture).unwrap()).unwrap();

    let app = router(Settings { host_token: None, key_dir: dir.path().join("keys"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("none")) }).unwrap();
    let res = app.oneshot(Request::builder().uri("/api/state").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    let ours: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap()).unwrap();
    for key in ["installs", "ota", "devices", "deviceApps"] {
        assert_eq!(ours[key], node[key], "{key} as Node wrote it");
    }
    // 45 builds were stored (the last two failed by the Node restart); the bundle shows the newest 40.
    assert_eq!(node["builds"].as_array().unwrap().len(), 45);
    let shown: Vec<u64> = ours["builds"].as_array().unwrap().iter().map(|b| b["id"].as_u64().unwrap()).collect();
    assert_eq!(shown, (6..=45).rev().collect::<Vec<u64>>());
    assert_eq!(ours["builds"][0]["error"], "server restarted during build");
    assert!(ours["running"].as_array().unwrap().is_empty() && ours["queued"].as_array().unwrap().is_empty());
    assert_eq!(node["nextBuildId"], 46);
}
