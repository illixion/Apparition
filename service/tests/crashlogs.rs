//! Crash logs against what the Node store kept: the list (including forgetting an entry whose file
//! was moved away), the file routes, and the MetricKit refs and linking that tie a log to a trace.
//!
//! `tests/fixtures/crashlogs/` is a `data/crashlogs` tree (`index.json` pretty-printed the way
//! `store/lib/crashlogs.js` writes it). The expected replies below, and the MetricKit refs further
//! down, are what the Node store answered on that tree, captured when it was removed.

use apparition_service::{crashlogs::{CrashLogs, Ref, crash_refs}, server::{Settings, router}};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn entry(id: &str, at: u64, pid: i64, file: &str, symbolicated: Option<&str>) -> Value {
    json!({ "id": id, "device": "AVP", "bundleId": "com.x.app", "appName": "App", "version": "1.0", "build": "7", "os": "visionOS 26",
        "exception": "EXC_BREAKPOINT SIGTRAP", "pid": pid, "capturedAt": at, "pulledAt": at + 5, "file": file, "symbolicated": symbolicated, "traceId": null })
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() { copy_dir(&e.path(), &target) } else { std::fs::copy(e.path(), target).unwrap(); }
    }
}

#[tokio::test]
async fn crash_logs_read_what_the_node_store_kept() {
    let work = tempfile::tempdir().unwrap();
    let rust = work.path();
    std::fs::write(rust.join("config.json"), json!({ "port": 1, "repos": [] }).to_string()).unwrap();
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/crashlogs"), &rust.join("data/crashlogs"));
    let app = router(Settings { host_token: None, key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();

    // (uri, status, text of a plain-text body or the JSON of an error / the list), as Node answered
    let unknown = json!({ "error": "unknown crash log" });
    let list = json!({ "crashLogs": [
        entry("bbbbbbbb22", 3000, 22, "AVP/b-bbbbbbbb.ips", None),
        entry("aaaaaaaa11", 2000, 11, "AVP/a-aaaaaaaa.ips", Some("AVP/a-aaaaaaaa.txt")),
    ], "pull": { "AVP": { "since": 5, "error": null } } });
    for (uri, status, body) in [
        ("/api/crashlogs", 200, Err(list)),
        ("/api/crashlogs/aaaaaaaa11/ips", 200, Ok("{\"h\":1}\n{\"b\":2}\n")),
        ("/api/crashlogs/aaaaaaaa11/symbolicated", 200, Ok("symbolicated text <script>")),
        ("/api/crashlogs/bbbbbbbb22/symbolicated", 404, Err(unknown.clone())),
        ("/api/crashlogs/cccccccc33/ips", 404, Err(unknown.clone())),
        ("/api/crashlogs/deadbeef/ips", 404, Err(unknown.clone())),
    ] {
        let res = app.clone().oneshot(Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status().as_u16(), status, "{uri}: status");
        let ctype = res.headers().get("content-type").map(|v| v.to_str().unwrap().to_string());
        let text = String::from_utf8(axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
        match body {
            Ok(expected) => {
                assert_eq!(ctype.as_deref(), Some("text/plain; charset=utf-8"), "{uri}");
                assert_eq!(text, expected, "{uri}");
            }
            Err(expected) => assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), expected, "{uri}"),
        }
    }
    let res = app.clone().oneshot(Request::builder().uri("/api/crashlogs/aaaaaaaa11/other").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_ne!(res.status(), 200);
    assert!(!rust.join("data/crashlogs/AVP/c-cccccccc.txt").exists(), "the orphan's text is removed with its entry");
}

#[test]
fn metrickit_refs_and_linking_match_what_the_node_store_computed() {
    let work = tempfile::tempdir().unwrap();
    let traces = work.path().join("traces");
    let metas: Vec<Value> = vec![json!({ "id": "t-new", "app": { "bundleId": "com.x.app" } }), json!({ "id": "t-old", "app": { "bundleId": "com.x.app" } }), json!({ "id": "t-none", "app": {} })];
    write(&traces.join("t-new/files/crashes/a.json"), &json!({ "report": { "timeRange": { "begin": "2026-10-09T08:00:00Z" }, "environment": { "pid": 11, "bundleIdentifier": "com.x.app" }, "crashDiagnostic": { "x": 1 } } }).to_string());
    write(&traces.join("t-old/files/crashes/b.json"), &json!({ "report": { "timeStampBegin": "2026-10-09T08:00:00.500+02:00", "crashDiagnostics": [{ "diagnosticMetaData": { "pid": 22, "bundleIdentifier": "com.x.app" } }, { "diagnosticMetaData": { "pid": 23 } }] } }).to_string());
    write(&traces.join("t-old/files/crashes/broken.json"), "{nope");
    write(&traces.join("t-none/files/crashes/c.json"), &json!({ "report": { "crashDiagnostic": {}, "timeRange": {} } }).to_string());

    let ours = crash_refs(&traces, &metas);
    // `crashRefs` of store/lib/crashlogs.js on these traces and metas, captured at removal time.
    let theirs: Vec<Value> = serde_json::from_str(r#"[
        {"traceId":"t-new","bundleId":"com.x.app","pid":11,"at":1791532800000,"tolerance":600000},
        {"traceId":"t-old","bundleId":"com.x.app","pid":22,"at":1791525600500,"tolerance":172800000},
        {"traceId":"t-old","bundleId":"com.x.app","pid":23,"at":1791525600500,"tolerance":172800000}]"#).unwrap();
    let mine: Vec<Value> = ours.iter().map(|r| json!({ "traceId": r.trace_id, "bundleId": r.bundle_id, "pid": r.pid, "at": r.at, "tolerance": r.tolerance })).collect();
    assert_eq!(mine, theirs);
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
