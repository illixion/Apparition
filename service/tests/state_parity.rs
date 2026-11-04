//! `GET /api/state` from the Rust service against the same answer from the real Node store,
//! started on an identical config, state file and git checkout. Skipped without `node` or `git`.


use apparition_service::server::{Settings, router};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::{io::{Read, Write}, net::TcpStream, path::Path, process::{Child, Command, Stdio}, time::{Duration, Instant}};
use tower::ServiceExt;

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill(); // the Node store this test started: its own pid
        let _ = self.0.wait();
    }
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").arg("-C").arg(dir).args(args).env("GIT_AUTHOR_NAME", "t").env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t").env("GIT_COMMITTER_EMAIL", "t@t").stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success();
    assert!(ok, "git {args:?}");
}

fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    head.contains(" 200 ").then(|| body.to_string())
}

#[tokio::test]
async fn state_matches_the_node_store() {
    if !have("node") || !have("git") {
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

    let store = Path::new(env!("CARGO_MANIFEST_DIR")).join("../store");
    let _node = Kill(Command::new("node").arg("server.js").current_dir(&store)
        .env("STORE_CONFIG", dir.path().join("config.json")).env("STORE_DATA", dir.path().join("data"))
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let start = Instant::now();
    let theirs = loop {
        if let Some(body) = http_get(port, "/api/state") {
            break serde_json::from_str::<Value>(&body).unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the Node store did not start");
        std::thread::sleep(Duration::from_millis(200));
    };

    // `now` and `host` are the clock and machine; `devices`, `deviceApps` and `debugApps` come from
    // devicectl and Bonjour on the Node side, which the Rust service does not have yet.
    let fields = |v: &Value| -> Vec<String> { let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect(); k.sort(); k };
    assert_eq!(fields(&ours), fields(&theirs), "the same set of fields");
    for key in ["repos", "ledgerOnly", "installs", "ota", "builds", "running", "queued", "authRequired", "appsOpen", "host"] {
        assert_eq!(ours[key], theirs[key], "field {key}");
    }
    let repo = &ours["repos"][0];
    assert_eq!(repo["deployDefaults"], json!({ "launch": true, "mcp": false, "jit": false, "buildDebug": false, "inject": false }));
    assert_eq!(repo["git"]["subject"], "First\tcommit".split('\t').next().unwrap(), "git log is split on tabs");
    assert_eq!(repo["git"]["dirty"], true);
    assert_eq!(ours["builds"].as_array().unwrap().len(), 40);
    assert_eq!(ours["builds"][0]["status"], "failed", "mid-flight builds are failed on start");
    assert_eq!(ours["ledgerOnly"], json!(["Apple TV"]));
}
