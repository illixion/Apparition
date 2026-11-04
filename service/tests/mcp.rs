//! The MCP server over HTTP, against a fake DebugTrace app, and its tool schemas and panel
//! validation against the Python `debugtrace-mcp` it replaces.

#![cfg(target_os = "macos")]

use apparition_service::{debug::{Fixed, Service, advertisement_tag, request_proof}, server::{Settings, router}};
use axum::{Json, Router, body::{Body, Bytes}, extract::State, http::{HeaderMap, Method, Request, Uri}, response::IntoResponse, routing::{get, post}};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{path::Path, process::Command, sync::{Arc, Mutex}};
use tower::ServiceExt;

const HOST: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNO12";
const TOKEN: &str = "command-token-for-build-one";
const PYTHON_TOOL: &str = "../../DebugTrace/Tools/debugtrace-mcp";

type Calls = Arc<Mutex<Vec<Value>>>;

async fn fake_root(State(calls): State<Calls>, uri: axum::http::Uri) -> impl IntoResponse {
    let endpoint = uri.query().and_then(|q| q.strip_prefix("endpoint=")).map(|v| v.replace("%20", " ").replace("%2F", "/"));
    calls.lock().unwrap().push(json!({ "help": endpoint }));
    Json(json!({ "ok": true, "endpoints": ["_info"] }))
}

async fn fake_nonce() -> impl IntoResponse {
    Json(json!({ "nonce": "n0", "expiresIn": 30 }))
}

async fn fake_call(State(calls): State<Calls>, headers: HeaderMap, method: Method, uri: Uri, raw: Bytes) -> impl IntoResponse {
    // A launch record is a trusted address and gets the bearer; an advertisement's gets a proof.
    let header = headers["authorization"].to_str().unwrap();
    let proof = format!("DebugTrace-HMAC nonce=n0, mac={}", request_proof(TOKEN, "n0", method.as_str(), uri.path_and_query().map_or("/", |p| p.as_str()), &raw));
    assert!(header == format!("Bearer {TOKEN}") || header == proof, "unexpected authorization: {header}");
    let body: Value = serde_json::from_slice(&raw).unwrap();
    calls.lock().unwrap().push(body.clone());
    match body["name"].as_str().unwrap() {
        "_info" => Json(json!({ "ok": true, "data": { "app": { "bundleId": "com.x.app" }, "process": { "pid": 5 } } })).into_response(),
        "_screenshot" => ([("content-type", "image/png")], vec![1u8, 2, 3]).into_response(),
        "_file" => ([("content-type", "application/zip")], vec![9u8; 10]).into_response(),
        "_big" => Json(json!({ "ok": true, "data": { "entries": (0..4000).map(|i| format!("log line number {i} with padding padding padding")).collect::<Vec<_>>(), "lastSequence": 4000, "returned": 4000 } })).into_response(),
        "_bigobject" => Json(json!({ "ok": true, "data": { "blob": "x".repeat(70_000), "other": 1 } })).into_response(),
        "_trace" => Json(json!({ "ok": true, "data": { "download": "/trace.zip", "filename": "app-trace.zip", "traceId": "t1", "signed": true, "files": [{ "path": "a.log" }] } })).into_response(),
        "_patches" => Json(json!({ "ok": true, "data": { "patches": [] } })).into_response(),
        "_inject" => Json(json!({ "ok": true, "data": { "output": "42" } })).into_response(),
        "_fail" => (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": { "message": "nope" } }))).into_response(),
        name => Json(json!({ "ok": true, "data": { "echo": name, "args": body["arguments"] } })).into_response(),
    }
}

struct Rig {
    app: Router,
    calls: Calls,
    dir: tempfile::TempDir,
}

async fn rig(services: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let calls: Calls = Arc::default();
    let fake = Router::new().route("/_nonce", get(fake_nonce)).route("/", get(fake_root)).route("/_call", post(fake_call)).route("/trace.zip", get(|| async { vec![0x50u8, 0x4b, 3, 4] })).with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
    let state = dir.path().join("state");
    let keys = state.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("build1.json"), json!({ "keyId": "build1", "commandToken": TOKEN, "bundleId": "com.x.app", "device": "Vision Pro", "build": "9" }).to_string()).unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let list = if services { vec![Service { instance: "app".into(), host: "127.0.0.1".into(), port,
        txt: [("n".to_string(), "n1".to_string()), ("t".to_string(), advertisement_tag(TOKEN, "n1")), ("name".to_string(), "My App".to_string())].into() }] } else { vec![] };
    if services {
        // the launch record supplies the project root, as build-and-sign --run --mcp writes it
        let sessions = state.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("a.json"), json!({ "bundleId": "com.x.app", "device": "Vision Pro", "keyId": "build1", "url": format!("http://127.0.0.1:{port}"), "pid": 999_999, "projectRoot": project, "startedAt": "t0" }).to_string()).unwrap();
    }
    std::fs::write(dir.path().join("config.json"), "{}").unwrap();
    let app = router(Settings {
        host_token: Some(HOST.into()), key_dir: keys, debug_state_dir: state, browser: Arc::new(Fixed(list)), client_name: "Test".into(),
        ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("p"))
    }).unwrap();
    Rig { app, calls, dir }
}

async fn rpc(app: &Router, message: Value) -> (u16, Value) {
    let res = app.clone().oneshot(Request::builder().method("POST").uri("/mcp").header("host", "localhost").header("authorization", format!("Bearer {HOST}"))
        .header("content-type", "application/json").body(Body::from(message.to_string())).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
}

async fn tool(app: &Router, name: &str, args: Value) -> Value {
    let (_, v) = rpc(app, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } })).await;
    v["result"].clone()
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"].as_str().unwrap_or("").to_string()
}

#[tokio::test]
async fn the_protocol_basics_and_the_bearer() {
    let r = rig(false).await;
    let (status, v) = rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })).await;
    assert_eq!(status, 200);
    assert_eq!((v["result"]["protocolVersion"].as_str(), v["result"]["serverInfo"]["name"].as_str()), (Some("2025-03-26"), Some("debugtrace-apps")));
    assert!(v["result"]["instructions"].as_str().unwrap().contains("Call `apps` to see what is running"));
    let (_, v) = rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "1999" } })).await;
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" })).await.1["result"], json!({}));
    assert_eq!(rpc(&r.app, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await.0, 202, "a notification gets no body");
    let (_, v) = rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 4, "method": "nope" })).await;
    assert_eq!(v["error"]["code"], -32601);
    let (_, v) = rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "nope" } })).await;
    assert_eq!(v["error"]["code"], -32602);
    let (_, v) = rpc(&r.app, json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }])).await;
    assert_eq!(v["error"]["message"], "batches are not supported");

    for (auth, expect) in [(None, 401), (Some("Bearer wrong"), 401)] {
        let mut b = Request::builder().method("POST").uri("/mcp").header("host", "localhost");
        if let Some(a) = auth { b = b.header("authorization", a); }
        assert_eq!(r.app.clone().oneshot(b.body(Body::from("{}")).unwrap()).await.unwrap().status(), expect);
    }
    let res = r.app.clone().oneshot(Request::builder().uri("/mcp").header("host", "localhost").header("authorization", format!("Bearer {HOST}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 405);
}

#[tokio::test]
async fn tool_schemas_and_instructions_are_the_python_tools() {
    let r = rig(false).await;
    let (_, v) = rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).await;
    let script = "import importlib.machinery as m, importlib.util as u, sys, json\nl=m.SourceFileLoader('d',sys.argv[1]);s=u.spec_from_loader('d',l);mod=u.module_from_spec(s);l.exec_module(mod)\nprint(json.dumps({'tools':mod.TOOLS,'instructions':mod.INSTRUCTIONS}))";
    let Some(out) = Command::new("python3").args(["-I", "-c", script, PYTHON_TOOL]).output().ok().filter(|o| o.status.success()) else { return };
    let python: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["result"]["tools"], python["tools"], "the Rust tool list drifted from debugtrace-mcp: regenerate service/src/mcp_tools.json");
    let (_, init) = rpc(&r.app, json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": {} })).await;
    assert_eq!(init["result"]["instructions"], python["instructions"]);
}

#[tokio::test]
async fn apps_help_and_call_with_the_forgiving_arguments() {
    let r = rig(true).await;
    let apps = tool(&r.app, "apps", json!({})).await;
    let text = text_of(&apps);
    assert!(!text.contains(TOKEN));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["apps"][0]["bundleId"], "com.x.app");
    assert_eq!(v["apps"][0]["inject"]["ready"], "unknown", "found by Bonjour, no inject directory in its launch record");

    // one app running: `app` may be left out; a flattened argument is the endpoint's own
    let res = tool(&r.app, "call", json!({ "endpoint": "echoer", "level": "error" })).await;
    let v: Value = serde_json::from_str(&text_of(&res)).unwrap();
    assert_eq!(v["data"], json!({ "echo": "echoer", "args": { "level": "error" } }));
    assert!(res["content"][1]["text"].as_str().unwrap().contains("treated as the endpoint's arguments"));
    // aliases and a JSON string
    let res = tool(&r.app, "call", json!({ "bundleId": "zzz", "name": "e", "args": "{\"a\":1}" })).await;
    assert!(text_of(&res).contains("no running app matches 'zzz'"), "{res}");
    let res = tool(&r.app, "call", json!({ "app": "my app", "name": "e", "args": "{\"a\":1}" })).await;
    assert_eq!(serde_json::from_str::<Value>(&text_of(&res)).unwrap()["data"]["args"], json!({ "a": 1 }));
    let notes = res["content"][1]["text"].as_str().unwrap();
    assert!(notes.contains("`name` was accepted as `endpoint`") && notes.contains("`args` was accepted as `arguments`") && notes.contains("JSON string"));
    // errors say what to do
    let res = tool(&r.app, "call", json!({ "arguments": {} })).await;
    assert_eq!((res["isError"].clone(), text_of(&res)), (json!(true), "`endpoint` is required\nhint: call `help` to list the app's endpoints".to_string()));
    let res = tool(&r.app, "call", json!({ "endpoint": "e", "arguments": { "a": 1 }, "extra": 2 })).await;
    assert!(text_of(&res).contains("put the endpoint's parameters inside `arguments`"));
    let res = tool(&r.app, "call", json!({ "endpoint": "e", "params": {}, "args": {} })).await;
    assert!(text_of(&res).contains("`arguments` was given twice"), "{}", text_of(&res));
    let res = tool(&r.app, "help", json!({ "endpointt": "x" })).await;
    assert!(text_of(&res).contains("did you mean `endpoint`?"), "{}", text_of(&res));
    let res = tool(&r.app, "call", json!({ "endpoint": "_fail" })).await;
    assert_eq!(res["isError"], true);

    let res = tool(&r.app, "help", json!({ "endpoint": "a b/c" })).await;
    assert!(text_of(&res).contains("\"endpoints\""));
    assert_eq!(r.calls.lock().unwrap().iter().find_map(|c| c.get("help").cloned()).unwrap(), "a b/c");

    let img = tool(&r.app, "call", json!({ "endpoint": "_screenshot" })).await;
    assert_eq!(img["content"][0], json!({ "type": "image", "data": "AQID", "mimeType": "image/png" }));
    let file = tool(&r.app, "call", json!({ "endpoint": "_file" })).await;
    let saved = serde_json::from_str::<Value>(&text_of(&file)).unwrap();
    let path = saved["savedTo"].as_str().unwrap();
    assert!(path.starts_with(r.dir.path().join("project/build/debugtrace").to_str().unwrap()) && path.ends_with("_file.zip"), "{path}");
    assert_eq!(std::fs::read(path).unwrap(), vec![9u8; 10]);
}

#[tokio::test]
async fn a_reply_too_big_for_a_tool_result_goes_to_a_file() {
    let r = rig(true).await;
    let res = tool(&r.app, "call", json!({ "endpoint": "_big" })).await;
    let v: Value = serde_json::from_str(&text_of(&res)).unwrap();
    assert!(v["note"].as_str().unwrap().contains("4000 lines, too many to return inline"));
    assert_eq!(v["newest"].as_array().unwrap().len(), 40);
    assert_eq!(v["lastSequence"], 4000);
    let saved = std::fs::read_to_string(v["savedTo"].as_str().unwrap()).unwrap();
    assert_eq!(saved.lines().count(), 4000);
    let res = tool(&r.app, "call", json!({ "endpoint": "_bigobject" })).await;
    let v: Value = serde_json::from_str(&text_of(&res)).unwrap();
    assert_eq!(v["dataKeys"], json!(["blob", "other"]));
    assert!(std::fs::metadata(v["savedTo"].as_str().unwrap()).unwrap().len() > 70_000);
}

#[tokio::test]
async fn a_trace_is_captured_and_saved_next_to_the_project() {
    let r = rig(true).await;
    let res = tool(&r.app, "trace", json!({ "note": "why", "logMinutes": 5 })).await;
    let v: Value = serde_json::from_str(&text_of(&res)).unwrap();
    assert_eq!((v["traceId"].as_str(), v["signed"].as_bool(), v["bytes"].as_u64()), (Some("t1"), Some(true), Some(4)));
    assert!(std::fs::read(v["savedTo"].as_str().unwrap()).unwrap().starts_with(b"PK"));
    let call = r.calls.lock().unwrap().iter().find(|c| c["name"] == "_trace").cloned().unwrap();
    assert_eq!(call["arguments"], json!({ "note": "why", "logMinutes": 5 }));
}

#[tokio::test]
async fn panels_are_written_listed_and_removed() {
    let r = rig(true).await;
    let ok = tool(&r.app, "panel", json!({ "name": "hdr-tuning", "title": "HDR", "controls": [{ "kind": "slider", "tunable": "hdr.peakNits" }, { "kind": "button", "endpoint": "reset", "label": "Reset" }] })).await;
    let v: Value = serde_json::from_str(&text_of(&ok)).unwrap();
    assert_eq!((v["ok"].clone(), v["controls"].clone(), v["bundleId"].clone()), (json!(true), json!(2), json!("com.x.app")));
    // the Debug routes see what the tool wrote
    let res = r.app.clone().oneshot(Request::builder().uri("/api/debug/build1/panels").header("host", "localhost").header("authorization", format!("Bearer {HOST}")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 404, "the page routes are behind the web UI, which a hosted store keeps off");
    let listed: Value = serde_json::from_str(&text_of(&tool(&r.app, "panel", json!({})).await)).unwrap();
    assert_eq!(listed["panels"][0]["name"], "hdr-tuning");
    let removed: Value = serde_json::from_str(&text_of(&tool(&r.app, "panel", json!({ "name": "hdr-tuning", "remove": true })).await)).unwrap();
    assert_eq!(removed["removed"], true);
    assert_eq!(serde_json::from_str::<Value>(&text_of(&tool(&r.app, "panel", json!({ "name": "hdr-tuning", "remove": true })).await)).unwrap()["removed"], false);
    // a panel for an app that is not running, by bundle id
    let ok = tool(&r.app, "panel", json!({ "app": "com.other.thing", "name": "p", "controls": [{ "kind": "toggle", "tunable": "x" }] })).await;
    assert_eq!(serde_json::from_str::<Value>(&text_of(&ok)).unwrap()["bundleId"], "com.other.thing");
    let _ = &r.dir;
}

#[tokio::test]
async fn panel_validation_says_what_the_python_tool_says() {
    let cases = [
        json!({ "name": "BAD", "controls": [{ "kind": "slider", "tunable": "t" }] }),
        json!({ "name": "ok", "controls": [] }),
        json!({ "name": "ok", "controls": ["x"] }),
        json!({ "name": "ok", "controls": [{ "kind": "knob", "tunable": "t" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "slider" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "slider", "tunable": "t", "endpoint": "e" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "button", "tunable": "t" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "slider", "endpoint": "e" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "slider", "endpoint": "e", "argument": "a" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "choice", "endpoint": "e", "argument": "a" }] }),
        json!({ "name": "ok", "controls": [{ "kind": "button", "endpoint": "e", "arguments": [] }] }),
        json!({ "name": "ok", "controls": [{ "kind": "toggle", "tunable": "t" }], "bogus": 1 }),
        json!({ "controls": [{ "kind": "toggle", "tunable": "t" }] }),
    ];
    let script = "import importlib.machinery as m, importlib.util as u, sys, json\nl=m.SourceFileLoader('d',sys.argv[1]);s=u.spec_from_loader('d',l);mod=u.module_from_spec(s);l.exec_module(mod)\nr=mod.handle({'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'panel','arguments':json.loads(sys.argv[2])}})['result']\nprint(json.dumps({'text':r['content'][0]['text'],'err':bool(r.get('isError'))}))";
    let r = rig(false).await;
    for args in cases {
        let mut args = args;
        args["app"] = json!("com.x.app");
        let state = r.dir.path().join("pystate");
        let Some(out) = Command::new("python3").args(["-I", "-c", script, PYTHON_TOOL, &args.to_string()])
            .env("DEBUGTRACE_STATE_DIR", &state).env("DEBUGTRACE_BROWSE_SECONDS", "0").env("DEBUGTRACE_KEY_DIR", state.join("nokeys"))
            .output().ok().filter(|o| o.status.success()) else { return };
        let python: Value = serde_json::from_slice(&out.stdout).unwrap();
        let ours = tool(&r.app, "panel", args.clone()).await;
        assert_eq!((text_of(&ours), ours["isError"] == true), (python["text"].as_str().unwrap().to_string(), python["err"].as_bool().unwrap()), "{args}");
    }
    let _: &Path = r.dir.path();
}

/// Endpoint names become file names when a reply is saved, so the MCP tool takes only real
/// endpoint names, as the web route does.
#[tokio::test]
async fn an_endpoint_name_cannot_leave_the_project() {
    let r = rig(true).await;
    for endpoint in ["../../../Library/LaunchAgents/x", "_file/../../x", "a b"] {
        let result = tool(&r.app, "call", json!({ "app": "com.x.app", "endpoint": endpoint })).await;
        assert_eq!(result["isError"], true, "{endpoint}: {result}");
        assert!(text_of(&result).contains("isn't an endpoint name"), "{result}");
    }
    assert!(!r.calls.lock().unwrap().iter().any(|c| c["name"].as_str().is_some_and(|n| n.contains('/'))));
}

/// `inject` drives the external patch tool from the project root and sends what it builds to the
/// app: reset when the app has no patches, patch, one `_inject` per library, then commit.
#[tokio::test]
async fn inject_runs_the_patch_tool_and_sends_its_libraries() {
    let r = rig(true).await;
    let dir = r.dir.path();
    let (inject_dir, log) = (dir.join("inject"), dir.join("tool.log"));
    std::fs::create_dir_all(&inject_dir).unwrap();
    // The launch record names the injection directory, as `build-and-sign --run --mcp --inject` writes it.
    let record = dir.join("state/sessions/a.json");
    let mut session: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    session["injectDir"] = json!(inject_dir);
    std::fs::write(&record, session.to_string()).unwrap();
    let tool_path = dir.join("fake-inject");
    std::fs::write(&tool_path, format!("#!/bin/bash\necho \"$PWD :: $*\" >> {log}\ncase \"$1\" in\n  patch) printf 'PATCH' > {dylib}\n    echo '{{\"ok\":true,\"dylibs\":[{{\"path\":\"{dylib}\",\"label\":\"p1\",\"module\":\"M\"}}],\"commit\":[\"A.swift\"],\"record\":\"rec.json\",\"files\":{{\"A.swift\":{{\"patched\":[\"f()\"],\"warnings\":[\"reordered\"]}}}}}}';;\n  *) echo '{{\"ok\":true}}';;\nesac\n",
        log = log.display(), dylib = dir.join("p1.dylib").display())).unwrap();
    std::fs::set_permissions(&tool_path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    unsafe { std::env::set_var("DEBUGTRACE_INJECT_TOOL", &tool_path) };

    let result = tool(&r.app, "inject", json!({ "app": "com.x.app", "files": ["A.swift"], "run": "return 1", "allowViewTypeChange": true })).await;
    assert!(result["isError"].is_null(), "{result}");
    let summary: Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(summary["applied"], json!(["p1"]));
    assert_eq!(summary["output"], "42");
    assert_eq!(summary["patched"], json!(["A.swift: f()"]));
    assert_eq!(summary["warnings"], json!(["A.swift: reordered"]));

    let sent: Vec<Value> = r.calls.lock().unwrap().iter().filter(|c| c["name"] == "_inject").cloned().collect();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["arguments"]["label"], "p1");
    assert_eq!(STANDARD.decode(sent[0]["arguments"]["patch"].as_str().unwrap()).unwrap(), b"PATCH");

    let lines = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = lines.lines().collect();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines[0].ends_with(&format!(":: reset --dir {}", inject_dir.display())), "{}", lines[0]);
    assert!(lines[0].starts_with(&std::fs::canonicalize(dir.join("project")).unwrap().display().to_string()), "runs from the project root: {}", lines[0]);
    assert!(lines[1].ends_with(&format!(":: patch --dir {} --file A.swift --run return 1 --allow-view-type-change", inject_dir.display())), "{}", lines[1]);
    assert!(lines[2].ends_with(&format!(":: commit --dir {} --file A.swift --record rec.json", inject_dir.display())), "{}", lines[2]);

    // Only Swift sources, and nothing the tool could read as an option.
    for bad in ["../../etc/passwd", "--evil.swift", "Notes.txt"] {
        let refused = tool(&r.app, "inject", json!({ "app": "com.x.app", "files": [bad] })).await;
        assert_eq!(refused["isError"], true, "{bad}");
        assert!(text_of(&refused).contains("is not a Swift source file"), "{refused}");
    }
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 3, "a refused call runs nothing");

    // A second client is turned away while one patch holds the ledger.
    use std::os::fd::AsRawFd;
    let held = std::fs::OpenOptions::new().create(true).append(true).open(inject_dir.join("transaction.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let busy = tool(&r.app, "inject", json!({ "app": "com.x.app" })).await;
    assert!(text_of(&busy).contains("another patch is in progress"), "{busy}");
    drop(held);
}

#[tokio::test]
async fn inject_needs_a_build_made_for_patching() {
    let r = rig(true).await;
    let result = tool(&r.app, "inject", json!({ "app": "com.x.app" })).await;
    assert_eq!(result["isError"], true);
    assert!(text_of(&result).contains("was not launched for live patching"), "{result}");
}
