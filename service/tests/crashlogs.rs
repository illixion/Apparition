//! Crash logs against the real Node store: the list (including forgetting an entry whose file was
//! moved away), the file routes, and the MetricKit refs and linking that tie a log to a trace.


mod common;

use apparition_service::{crashlogs::{CrashLogs, Ref, crash_refs}, server::{Settings, router}};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use tower::ServiceExt;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn entry(id: &str, at: u64, pid: i64, file: &str, symbolicated: Option<&str>) -> Value {
    json!({ "id": id, "device": "AVP", "bundleId": "com.x.app", "appName": "App", "version": "1.0", "build": "7", "os": "visionOS 26",
        "exception": "EXC_BREAKPOINT SIGTRAP", "pid": pid, "capturedAt": at, "pulledAt": at + 5, "file": file, "symbolicated": symbolicated, "traceId": null })
}

fn seed(dir: &Path) {
    let logs = dir.join("data/crashlogs");
    write(&logs.join("AVP/a-aaaaaaaa.ips"), "{\"h\":1}\n{\"b\":2}\n");
    write(&logs.join("AVP/a-aaaaaaaa.txt"), "symbolicated text <script>");
    write(&logs.join("AVP/b-bbbbbbbb.ips"), "{}\n{}\n");
    // c's .ips is gone: forgotten, and its text with it
    write(&logs.join("AVP/c-cccccccc.txt"), "orphan text");
    let index = json!({ "entries": [
        entry("aaaaaaaa11", 2000, 11, "AVP/a-aaaaaaaa.ips", Some("AVP/a-aaaaaaaa.txt")),
        entry("bbbbbbbb22", 3000, 22, "AVP/b-bbbbbbbb.ips", None),
        entry("cccccccc33", 1000, 33, "AVP/c-cccccccc.ips", Some("AVP/c-cccccccc.txt")),
    ], "pull": { "AVP": { "since": 5, "error": null } } });
    write(&logs.join("index.json"), &index.to_string());
}

#[tokio::test]
async fn crash_logs_match_the_node_store() {
    let work = tempfile::tempdir().unwrap();
    let port = common::free_port();
    for d in ["rust", "node"] {
        let dir = work.path().join(d);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("config.json"), json!({ "port": port, "repos": [] }).to_string()).unwrap();
        seed(&dir);
    }
    let rust = work.path().join("rust");
    let app = router(Settings { host_token: None, key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();
    let node = work.path().join("node");
    let Some(node) = common::NodeStore::start(&node.join("config.json"), &node.join("data"), port, &[]) else { return };

    for uri in ["/api/crashlogs", "/api/crashlogs/aaaaaaaa11/ips", "/api/crashlogs/aaaaaaaa11/symbolicated", "/api/crashlogs/bbbbbbbb22/symbolicated",
        "/api/crashlogs/cccccccc33/ips", "/api/crashlogs/deadbeef/ips", "/api/crashlogs/aaaaaaaa11/other"] {
        let res = app.clone().oneshot(Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
        let rs = res.status().as_u16();
        let ctype = res.headers().get("content-type").map(|v| v.to_str().unwrap().to_string());
        let body = String::from_utf8(axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
        let (ns, nb) = node.get(uri);
        if uri.ends_with("other") { assert_ne!(rs, 200); continue; }
        assert_eq!(rs, ns, "{uri}: status");
        if let Some(c) = &ctype { if rs == 200 && !uri.ends_with("crashlogs") { assert_eq!(c, "text/plain; charset=utf-8"); } }
        if uri.ends_with("crashlogs") || rs != 200 {
            assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), serde_json::from_str::<Value>(&nb).unwrap(), "{uri}");
        } else {
            assert_eq!(body, nb, "{uri}");
        }
    }
    assert!(!rust.join("data/crashlogs/AVP/c-cccccccc.txt").exists(), "the orphan's text is removed with its entry");
}

#[test]
fn metrickit_refs_and_linking_match_the_node_store() {
    let work = tempfile::tempdir().unwrap();
    let traces = work.path().join("traces");
    let metas = vec![json!({ "id": "t-new", "app": { "bundleId": "com.x.app" } }), json!({ "id": "t-old", "app": { "bundleId": "com.x.app" } }), json!({ "id": "t-none", "app": {} })];
    write(&traces.join("t-new/files/crashes/a.json"), &json!({ "report": { "timeRange": { "begin": "2026-10-09T08:00:00Z" }, "environment": { "pid": 11, "bundleIdentifier": "com.x.app" }, "crashDiagnostic": { "x": 1 } } }).to_string());
    write(&traces.join("t-old/files/crashes/b.json"), &json!({ "report": { "timeStampBegin": "2026-10-09T08:00:00.500+02:00", "crashDiagnostics": [{ "diagnosticMetaData": { "pid": 22, "bundleIdentifier": "com.x.app" } }, { "diagnosticMetaData": { "pid": 23 } }] } }).to_string());
    write(&traces.join("t-old/files/crashes/broken.json"), "{nope");
    write(&traces.join("t-none/files/crashes/c.json"), &json!({ "report": { "crashDiagnostic": {}, "timeRange": {} } }).to_string());

    let ours = crash_refs(&traces, &metas);
    let script = "const {crashRefs}=require(process.argv[1]);console.log(JSON.stringify(crashRefs(process.argv[2],JSON.parse(process.argv[3]))))";
    let crashlogs_js = Path::new(env!("CARGO_MANIFEST_DIR")).join("../store/lib/crashlogs.js");
    let out = Command::new("node").arg("-e").arg(script).arg("--").arg(&crashlogs_js).arg(&traces).arg(Value::Array(metas).to_string()).output();
    if let Some(out) = out.ok().filter(|o| o.status.success()) {
        let theirs: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        let mine: Vec<Value> = ours.iter().map(|r| json!({ "traceId": r.trace_id, "bundleId": r.bundle_id, "pid": r.pid, "at": r.at, "tolerance": r.tolerance })).collect();
        assert_eq!(mine, theirs);
    }
    assert!(ours.contains(&Ref { trace_id: "t-new".into(), bundle_id: Some("com.x.app".into()), pid: 11, at: 1_791_532_800_000, tolerance: 600_000 }));
    assert_eq!(ours.len(), 3, "the exact one, and two from the old layout; a pid-less or time-less one is no ref");

    // linking: same app, same pid, within tolerance
    let dir = work.path().join("crashlogs");
    write(&dir.join("x.ips"), "{}");
    write(&dir.join("index.json"), &json!({ "entries": [
        entry("one", 1_791_532_800_000 + 60_000, 11, "x.ips", None),
        entry("two", 1_791_532_800_000 + 11 * 60_000, 11, "x.ips", None),
        entry("three", 1_791_532_800_000, 99, "x.ips", None),
    ] }).to_string());
    let logs = CrashLogs::new(dir);
    assert_eq!(logs.link(&ours), 1, "only the log inside the ten-minute window with the right pid");
    assert_eq!(logs.get("one").unwrap()["traceId"], "t-new");
    assert!(logs.get("two").unwrap()["traceId"].is_null());
    assert_eq!(logs.link(&ours), 0, "already linked");
}
