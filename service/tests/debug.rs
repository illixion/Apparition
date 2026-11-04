//! Debug routes: discovery from Bonjour services and launch records, and the proxy to a (fake)
//! DebugTrace app. The advertisement tag is checked against the Python tool's own function.

#![cfg(target_os = "macos")]

use apparition_service::{debug::{Fixed, Service, advertisement_tag}, server::{Settings, router}};
use axum::{Json, Router, body::Body, extract::State, http::{HeaderMap, Request, StatusCode}, response::IntoResponse, routing::{get, post}};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, process::Command, sync::{Arc, Mutex}};
use tower::ServiceExt;

const TOKEN: &str = "command-token-for-build-one";

#[test]
fn the_advertisement_tag_matches_the_python_tool() {
    let tool = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../DebugTrace/Tools/debugtrace-mcp");
    if !tool.exists() { return; }
    let script = "import importlib.machinery as m, importlib.util as u, sys\nl=m.SourceFileLoader('d',sys.argv[1]);s=u.spec_from_loader('d',l);mod=u.module_from_spec(s);l.exec_module(mod)\nprint(mod.advertisement_tag(sys.argv[2],sys.argv[3]))";
    let Ok(out) = Command::new("python3").args(["-I", "-c", script]).arg(&tool).args([TOKEN, "nonce-123"]).output() else { return };
    if !out.status.success() { return; }
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), advertisement_tag(TOKEN, "nonce-123"));
}

#[derive(Default)]
struct Seen {
    calls: Vec<(String, String, Value)>,
}

async fn fake_help(State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap) -> impl IntoResponse {
    seen.lock().unwrap().calls.push(("help".into(), format!("{}|{}", headers["authorization"].to_str().unwrap(), headers["x-debugtrace-client"].to_str().unwrap()), Value::Null));
    Json(json!({ "ok": true, "endpoints": ["_info", "_logs"] }))
}

async fn fake_call(State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap, Json(body): Json<Value>) -> impl IntoResponse {
    seen.lock().unwrap().calls.push(("call".into(), headers["authorization"].to_str().unwrap().into(), body.clone()));
    match body["name"].as_str().unwrap() {
        "_screenshot" => ([("content-type", "image/png")], vec![137u8, 80, 78, 71]).into_response(),
        "_logs" => {
            let after = body["arguments"]["afterSequence"].as_i64().unwrap();
            if after == 0 {
                Json(json!({ "ok": true, "data": { "entries": [{ "seq": 1, "m": "hello" }, { "seq": 2, "m": "world" }], "lastSequence": 2, "truncated": false } })).into_response()
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Json(json!({ "ok": true, "data": { "entries": [{ "seq": 3, "m": "later" }], "lastSequence": 3, "truncated": false } })).into_response()
            }
        }
        name => Json(json!({ "ok": true, "data": { "echo": name, "args": body["arguments"] } })).into_response(),
    }
}

struct Rig {
    app: Router,
    seen: Arc<Mutex<Seen>>,
    dir: tempfile::TempDir,
}

async fn rig(console_record: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let fake = Router::new().route("/", get(fake_help)).route("/_call", post(fake_call)).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });

    let keys = dir.path().join("state/keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("build1.json"), json!({ "keyId": "build1", "commandToken": TOKEN, "bundleId": "com.x.app", "device": "Vision Pro", "build": "9", "project": "/p/MyApp" }).to_string()).unwrap();
    std::fs::write(keys.join("build2.json"), json!({ "keyId": "build2", "bundleId": "com.x.other", "device": "iPhone" }).to_string()).unwrap();
    std::fs::write(keys.join("..bad.json"), "{}").unwrap();
    let panels = dir.path().join("state/panels/com.x.app");
    std::fs::create_dir_all(&panels).unwrap();
    std::fs::write(panels.join("hdr.json"), json!({ "title": "HDR", "updatedAt": "2026-11-01", "controls": [{ "id": "peak" }] }).to_string()).unwrap();
    std::fs::write(panels.join("newer.json"), json!({ "title": "N", "updatedAt": "2026-11-03", "controls": [] }).to_string()).unwrap();
    std::fs::write(panels.join("broken.json"), "{").unwrap();
    std::fs::write(panels.join("nocontrols.json"), "{\"title\":\"x\"}").unwrap();
    std::fs::write(panels.join("Bad Name.json"), json!({ "controls": [] }).to_string()).unwrap();

    let txt = |pairs: &[(&str, &str)]| -> HashMap<String, String> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    let services = vec![
        Service { instance: "app".into(), host: "127.0.0.1".into(), port, txt: txt(&[("n", "nonce-123"), ("t", &advertisement_tag(TOKEN, "nonce-123")), ("name", "My App")]) },
        Service { instance: "stranger".into(), host: "10.9.9.9".into(), port: 1, txt: txt(&[("n", "x"), ("t", "forged"), ("name", "Stranger")]) },
        Service { instance: "old".into(), host: "10.9.9.8".into(), port: 2, txt: txt(&[("keyId", "build2"), ("name", "Other")]) },
    ];
    if console_record {
        let sessions = dir.path().join("state/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("com.x.app@Vision Pro.json"), json!({ "bundleId": "com.x.app", "device": "Vision Pro", "keyId": "build1", "url": format!("http://127.0.0.1:{port}"), "pid": 999_999, "projectRoot": "/p/MyApp", "injectDir": "/p/MyApp/build/inject", "startedAt": "2026-11-04T09:00:00Z" }).to_string()).unwrap();
    }
    std::fs::write(dir.path().join("config.json"), json!({ "port": 1, "repos": [] }).to_string()).unwrap();
    let app = router(Settings {
        key_dir: keys, debug_state_dir: dir.path().join("state"), browser: Arc::new(Fixed(services)), client_name: "Test page".into(),
        ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("p"))
    }).unwrap();
    Rig { app, seen, dir }
}

async fn send(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri).header("host", "localhost");
    let body = match body { Some(b) => { req = req.header("content-type", "application/json"); Body::from(b.to_string()) } None => Body::empty() };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().into())).collect();
    (status, headers, axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec())
}

fn json_of(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap()
}

#[tokio::test]
async fn apps_are_recognised_by_tag_or_key_id_and_never_show_a_token() {
    let r = rig(false).await;
    let (status, _, body) = send(&r.app, "GET", "/api/debug/apps", None).await;
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(!text.contains(TOKEN), "the token never reaches the page");
    let v: Value = json_of(&body);
    assert_eq!(v["clientName"], "Test page");
    let apps = v["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 3);
    let mine = apps.iter().find(|a| a["id"] == "build1").unwrap();
    assert_eq!((mine["appName"].as_str(), mine["bundleId"].as_str(), mine["device"].as_str(), mine["attachable"].as_bool(), mine["source"].as_str()), (Some("My App"), Some("com.x.app"), Some("Vision Pro"), Some(true), Some("bonjour")));
    let stranger = apps.iter().find(|a| a["appName"] == "Stranger").unwrap();
    assert_eq!(stranger["attachable"], false);
    assert_eq!(stranger["reason"], "built on another Mac, or before per-build tokens: not in this Mac's key ledger");
    let old = apps.iter().find(|a| a["id"] == "build2").unwrap();
    assert_eq!(old["attachable"], false);
    assert_eq!(old["reason"], "this build has no command token; rebuild it with build-and-sign");
}

#[tokio::test]
async fn a_call_reaches_the_app_with_the_ledger_token_and_the_client_name() {
    let r = rig(false).await;
    let (status, _, body) = send(&r.app, "POST", "/api/debug/build1/call", Some(json!({ "endpoint": "_info", "arguments": { "a": 1 } }))).await;
    assert_eq!(status, 200);
    assert_eq!(json_of(&body)["data"], json!({ "echo": "_info", "args": { "a": 1 } }));
    let calls = r.seen.lock().unwrap().calls.clone();
    assert_eq!(calls[0].1, format!("Bearer {TOKEN}"));
    assert_eq!(calls[0].2, json!({ "name": "_info", "arguments": { "a": 1 } }));

    let (status, headers, body) = send(&r.app, "POST", "/api/debug/build1/call", Some(json!({ "endpoint": "_screenshot" }))).await;
    assert_eq!((status.as_u16(), headers.iter().find(|(k, _)| k == "content-type").unwrap().1.as_str(), body), (200, "image/png", vec![137, 80, 78, 71]));

    let (_, _, body) = send(&r.app, "GET", "/api/debug/build1/help", None).await;
    assert_eq!(json_of(&body)["endpoints"][1], "_logs");
    assert_eq!(r.seen.lock().unwrap().calls.last().unwrap().1, format!("Bearer {TOKEN}|Test page"));

    for bad in [json!({}), json!({ "endpoint": "not valid!" }), json!({ "endpoint": 5 }), json!({ "endpoint": "../x" })] {
        assert_eq!(send(&r.app, "POST", "/api/debug/build1/call", Some(bad)).await.0, 400);
    }
    let (status, _, body) = send(&r.app, "POST", "/api/debug/ghost/call", Some(json!({ "endpoint": "_info" }))).await;
    assert_eq!((status.as_u16(), json_of(&body)["error"]["message"].as_str().unwrap().contains("no longer running")), (404, true));
    let (status, _, body) = send(&r.app, "POST", "/api/debug/build2/call", Some(json!({ "endpoint": "_info" }))).await;
    assert_eq!((status.as_u16(), json_of(&body)["error"]["message"].as_str()), (403, Some("this build has no command token; rebuild it with build-and-sign")));
}

#[tokio::test]
async fn an_app_that_stopped_answering_is_a_502_with_a_hint() {
    let r = rig(false).await;
    // point the one recognised app at a closed port by pulling the fake's listener away
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let keys = r.dir.path().join("state/keys");
    let app = router(Settings {
        key_dir: keys, debug_state_dir: r.dir.path().join("state"), client_name: "Test page".into(),
        browser: Arc::new(Fixed(vec![Service { instance: "app".into(), host: "127.0.0.1".into(), port, txt: [("n".to_string(), "n1".to_string()), ("t".to_string(), advertisement_tag(TOKEN, "n1"))].into() }])),
        ..Settings::new(r.dir.path().join("config.json"), r.dir.path().join("data2"), r.dir.path().join("p"))
    }).unwrap();
    let (status, _, body) = send(&app, "POST", "/api/debug/build1/call", Some(json!({ "endpoint": "_info" }))).await;
    assert_eq!(status, 502);
    let v = json_of(&body);
    assert_eq!(v["error"]["code"], "unreachable");
    assert_eq!(v["error"]["hint"], "iOS suspends apps in the background; bring it to the foreground");
}

#[tokio::test]
async fn panels_come_from_disk_newest_first_and_skip_the_malformed() {
    let r = rig(false).await;
    let (status, _, body) = send(&r.app, "GET", "/api/debug/build1/panels", None).await;
    assert_eq!(status, 200);
    let v = json_of(&body);
    let names: Vec<&str> = v["panels"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["newer", "hdr"]);
    assert_eq!(v["bundleId"], "com.x.app");
}

#[tokio::test]
async fn a_launch_record_with_a_dead_console_is_revived_by_its_advertisement() {
    let r = rig(true).await;
    let (_, _, body) = send(&r.app, "GET", "/api/debug/apps", None).await;
    let apps = json_of(&body)["apps"].as_array().unwrap().clone();
    assert_eq!(apps.iter().filter(|a| a["bundleId"] == "com.x.app").count(), 1, "one app, not the record and the advertisement");
    // the recovered record's launch details ride along (visible to MCP, not the page): the call still works
    let (status, ..) = send(&r.app, "POST", "/api/debug/build1/call", Some(json!({ "endpoint": "_info" }))).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn the_log_stream_sends_a_backlog_then_new_lines() {
    let r = rig(false).await;
    let res = r.app.clone().oneshot(Request::builder().uri("/api/debug/build1/logs?level=info&contains=hi").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    let mut stream = res.into_body().into_data_stream();
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !(text.contains("event: backlog") && text.contains("event: lines")) {
        let chunk = tokio::time::timeout_at(deadline, stream.next()).await.expect("events arrive").expect("stream open").unwrap();
        text += &String::from_utf8_lossy(&chunk);
    }
    assert!(text.contains("event: status\ndata: {\"state\":\"live\"}"), "{text}");
    assert!(text.contains("hello") && text.contains("later"));
    let log_call = r.seen.lock().unwrap().calls.iter().find(|c| c.2["name"] == "_logs").cloned().unwrap();
    assert_eq!(log_call.2["arguments"]["level"], "info");
    assert_eq!(log_call.2["arguments"]["contains"], "hi");
    assert_eq!(send(&r.app, "GET", "/api/debug/build1/logs?level=loud", None).await.0, 400);
}

/// Someone else on the network copies a build's advertisement: its key id alone (what older
/// builds advertised, and what the apps list shows), or its whole tag while the app runs. Either
/// way no token leaves this Mac.
#[tokio::test]
async fn a_copied_advertisement_is_never_sent_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let hits = Arc::new(Mutex::new(0usize));
    let counter = hits.clone();
    let thief = Router::new().fallback(move || { let counter = counter.clone(); async move { *counter.lock().unwrap() += 1; "stolen" } });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, thief).await.unwrap() });
    let keys = dir.path().join("state/keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("build1.json"), json!({ "keyId": "build1", "commandToken": TOKEN, "bundleId": "com.x.app", "device": "Vision Pro" }).to_string()).unwrap();
    std::fs::write(dir.path().join("config.json"), "{}").unwrap();
    let txt = |pairs: &[(&str, &str)]| -> HashMap<String, String> { pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
    let tag = advertisement_tag(TOKEN, "n1");
    for (services, reason) in [
        (vec![Service { instance: "000".into(), host: "127.0.0.1".into(), port, txt: txt(&[("keyId", "build1")]) }], "advertises only its key id"),
        (vec![
            Service { instance: "real".into(), host: "127.0.0.1".into(), port: 1, txt: txt(&[("n", "n1"), ("t", &tag)]) },
            Service { instance: "000".into(), host: "127.0.0.1".into(), port, txt: txt(&[("n", "n1"), ("t", &tag)]) },
        ], "more than one server"),
    ] {
        let app = router(Settings {
            key_dir: keys.clone(), debug_state_dir: dir.path().join("state"), browser: Arc::new(Fixed(services)), client_name: "Test".into(),
            ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("p"))
        }).unwrap();
        let (_, _, body) = send(&app, "GET", "/api/debug/apps", None).await;
        let apps = json_of(&body);
        let apps = apps["apps"].as_array().unwrap();
        assert!(!apps.is_empty() && apps.iter().all(|a| a["attachable"] == false && a["reason"].as_str().unwrap().contains(reason)), "{apps:?}");
        let (status, _, _) = send(&app, "POST", "/api/debug/build1/call", Some(json!({ "endpoint": "_info" }))).await;
        assert_eq!(status, 403);
        let (status, _, _) = send(&app, "GET", "/api/debug/build1/help", None).await;
        assert_eq!(status, 403);
    }
    assert_eq!(*hits.lock().unwrap(), 0, "the copy was contacted");
}
