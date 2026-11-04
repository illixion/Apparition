//! Trace upload: the Rust verifier against the real Node store, on the same archives and the same
//! key ledger. Every archive, valid or tampered, must get the same status and the same error text
//! from both. Skipped without `node`.

#![cfg(target_os = "macos")]

use apparition_service::server::{Settings, router};
use axum::{Router, body::Body, http::Request};
use ring::{digest, rand::SystemRandom, signature::{Ed25519KeyPair, KeyPair}};
use serde_json::{Value, json};
use std::{io::{Read, Write}, net::TcpStream, path::Path, process::{Child, Command, Stdio}, time::{Duration, Instant}};
use tower::ServiceExt;

const TRACE_ID: &str = "11111111-2222-3333-4444-555555555555";

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill(); // the Node store this test started: its own pid
        let _ = self.0.wait();
    }
}

fn sha(data: &[u8]) -> String {
    digest::digest(&digest::SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

struct Spec {
    manifest: Value,
    /// Extra or replacement zip entries: (name, bytes, deflate)
    entries: Vec<(String, Vec<u8>, bool)>,
    sign_with: Option<Ed25519KeyPair>,
}

fn zip_of(entries: &[(String, Vec<u8>, bool)]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data, deflate) in entries {
        let method = if *deflate { zip::CompressionMethod::Deflated } else { zip::CompressionMethod::Stored };
        w.start_file(name, zip::write::SimpleFileOptions::default().compression_method(method)).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A trace archive: `manifest` is signed by `key` (or by `sign_with` to forge it).
fn archive(key: &Ed25519KeyPair, mut spec: Spec) -> Vec<u8> {
    let manifest_bytes = serde_json::to_vec(&spec.manifest).unwrap();
    let signer = spec.sign_with.take();
    let sig = signer.as_ref().unwrap_or(key).sign(&manifest_bytes);
    let mut entries = vec![("manifest.json".to_string(), manifest_bytes, true), ("manifest.sig".to_string(), sig.as_ref().to_vec(), false)];
    entries.extend(spec.entries);
    zip_of(&entries)
}

fn good_spec() -> Spec {
    let log = b"line one\nline two\n".repeat(20);
    let blob = vec![7u8; 100];
    Spec {
        manifest: json!({
            "format": "debugtrace/1", "traceId": TRACE_ID, "createdAt": "2026-11-04T10:00:00Z", "session": "s1",
            "app": { "bundleId": "com.x.app", "build": "42", "name": "My App" }, "device": { "name": "Vision Pro" },
            "signature": { "algorithm": "ed25519", "keyId": "key1" },
            "files": [{ "path": "logs/app.log", "bytes": log.len(), "sha256": sha(&log) }, { "path": "data.bin", "bytes": 100, "sha256": sha(&blob) }],
        }),
        entries: vec![("logs/app.log".into(), log, true), ("data.bin".into(), blob, false)],
        sign_with: None,
    }
}

fn http_post(port: u16, path: &str, body: &[u8]) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    write!(stream, "POST {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n", body.len()).ok()?;
    stream.write_all(body).ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let (head, body) = raw.split_once("\r\n\r\n")?;
    Some((head.split(' ').nth(1)?.parse().ok()?, body.to_string()))
}

async fn rust_post(app: &Router, body: Vec<u8>) -> (u16, Value) {
    let res = app.clone().oneshot(Request::builder().method("POST").uri("/api/traces").header("host", "x").body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    (status, serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap())
}

#[tokio::test]
async fn the_rust_verifier_agrees_with_the_node_store_on_good_and_bad_archives() {
    if Command::new("node").arg("--version").stdout(Stdio::null()).status().is_err() {
        return;
    }
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let other = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
    use base64::{Engine, engine::general_purpose::STANDARD};

    let work = tempfile::tempdir().unwrap();
    let keys = work.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    let ledger = |id: &str, bundle: &str, build: Option<&str>| {
        let mut k = json!({ "keyId": id, "publicKey": STANDARD.encode(key.public_key().as_ref()), "bundleId": bundle, "branch": "main", "platform": "iOS", "device": "iPhone", "createdAt": 5 });
        if let Some(b) = build { k["build"] = json!(b); }
        std::fs::write(keys.join(format!("{id}.json")), k.to_string()).unwrap();
    };
    ledger("key1", "com.x.app", Some("42"));
    ledger("wrongapp", "com.other", None);
    ledger("wrongbuild", "com.x.app", Some("41"));

    let config = |dir: &Path, port: u16| {
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("config.json"), json!({ "port": port, "repos": [{ "name": "App", "path": "/nowhere", "branches": ["main"], "platforms": [{ "platform": "iOS", "bundleId": "com.x.app", "device": "iPhone" }] }] }).to_string()).unwrap();
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (rust_dir, node_dir) = (work.path().join("rust"), work.path().join("node"));
    config(&rust_dir, 1);
    config(&node_dir, port);
    let app = router(Settings { host_token: None, key_dir: keys.clone(), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(rust_dir.join("config.json"), rust_dir.join("data"), rust_dir.join("p")) }).unwrap();
    let store = Path::new(env!("CARGO_MANIFEST_DIR")).join("../store");
    let _node = Kill(Command::new("node").arg("server.js").current_dir(&store)
        .env("STORE_CONFIG", node_dir.join("config.json")).env("STORE_DATA", node_dir.join("data")).env("DEBUGTRACE_KEY_DIR", &keys)
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let start = Instant::now();
    while http_post(port, "/api/traces", b"").is_none() {
        assert!(start.elapsed() < Duration::from_secs(20), "the Node store did not start");
        std::thread::sleep(Duration::from_millis(200));
    }

    let with = |f: &dyn Fn(&mut Spec)| { let mut s = good_spec(); f(&mut s); archive(&key, s) };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not a zip", b"hello".to_vec()),
        ("unsigned", { let mut s = good_spec(); s.manifest.as_object_mut().unwrap().remove("signature"); archive(&key, s) }),
        ("wrong format", with(&|s| s.manifest["format"] = json!("debugtrace/2"))),
        ("bad trace id", with(&|s| s.manifest["traceId"] = json!("nope"))),
        ("unknown key", with(&|s| s.manifest["signature"]["keyId"] = json!("ghost"))),
        ("bad key id", with(&|s| s.manifest["signature"]["keyId"] = json!("../etc"))),
        ("bad algorithm", with(&|s| s.manifest["signature"]["algorithm"] = json!("rsa"))),
        ("forged signature", archive(&key, Spec { sign_with: Some(other), ..good_spec() })),
        ("key for another app", with(&|s| s.manifest["signature"]["keyId"] = json!("wrongapp"))),
        ("key for another build", with(&|s| s.manifest["signature"]["keyId"] = json!("wrongbuild"))),
        ("hash mismatch", with(&|s| s.manifest["files"][0]["sha256"] = json!(sha(b"other")))),
        ("size mismatch", with(&|s| s.manifest["files"][1]["bytes"] = json!(99))),
        ("missing file", with(&|s| { s.entries.pop(); })),
        ("unlisted file", with(&|s| s.entries.push(("extra.txt".into(), b"x".to_vec(), false)))),
        ("traversal path", with(&|s| { s.manifest["files"][0]["path"] = json!("../x"); })),
        ("duplicate listing", with(&|s| { let f = s.manifest["files"][0].clone(); s.manifest["files"].as_array_mut().unwrap().push(f); })),
        ("lists the manifest", with(&|s| s.manifest["files"].as_array_mut().unwrap().push(json!({ "path": "manifest.json", "bytes": 1, "sha256": "x" })))),
        ("good", archive(&key, good_spec())),
        ("good again is a duplicate", archive(&key, good_spec())),
    ];
    for (name, body) in cases {
        let (rs, rb) = rust_post(&app, body.clone()).await;
        let (ns, nb) = http_post(port, "/api/traces", &body).expect("node answered");
        let nb: Value = serde_json::from_str(&nb).unwrap_or(Value::Null);
        assert_eq!(rs, ns as u16, "{name}: status ({rb} vs {nb})");
        if rs >= 400 {
            assert_eq!(rb["error"], nb["error"], "{name}: error text");
        } else {
            let strip = |v: &Value| { let mut t = v["trace"].clone(); t.as_object_mut().unwrap().remove("receivedAt"); t };
            assert_eq!(strip(&rb), strip(&nb), "{name}: stored meta");
            assert_eq!(rb["trace"]["build"]["repo"], "App");
        }
    }

    // What was stored reads back, and the list and files routes work.
    let get = |uri: String| { let app = app.clone(); async move {
        let res = app.oneshot(Request::builder().uri(&uri).header("host", "x").body(Body::empty()).unwrap()).await.unwrap();
        (res.status().as_u16(), res.headers().clone(), axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec())
    } };
    let (status, _, body) = get("/api/traces".into()).await;
    assert_eq!(status, 200);
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["traces"][0]["id"], TRACE_ID);
    let (status, headers, body) = get(format!("/api/traces/{TRACE_ID}/files/logs/app.log")).await;
    assert_eq!((status, headers["content-type"].to_str().unwrap()), (200, "text/plain; charset=utf-8"));
    assert_eq!(body.len(), 360);
    let (_, headers, _) = get(format!("/api/traces/{TRACE_ID}/files/data.bin")).await;
    assert!(headers["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"data.bin\""));
    for bad in [format!("/api/traces/{TRACE_ID}/files/..%2f..%2fconfig.json"), format!("/api/traces/{TRACE_ID}/files/nope.txt")] {
        assert_eq!(get(bad).await.0, 404);
    }
    let (status, headers, _) = get(format!("/api/traces/{TRACE_ID}.zip")).await;
    assert_eq!((status, headers["content-disposition"].to_str().unwrap()), (200, "attachment; filename=\"My-App-trace-11111111.zip\""));
    let res = app.clone().oneshot(Request::builder().method("POST").uri(format!("/api/traces/{TRACE_ID}/delete")).header("host", "x").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(get(format!("/api/traces/{TRACE_ID}.zip")).await.0, 404);
}
