//! Trace upload: the verifier on good and tampered archives, with the status and error text the
//! Node store (`store/server.js` `verifyTrace`) gave for each, captured when it was removed. The
//! archives are generated here (a fresh key each run, so only the outcomes are pinned, not bytes).
//! `tests/fixtures/node-traces/` is a trace directory exactly as the Node store stored one, so
//! reading, listing, zipping and deleting what it wrote stays covered.

use apparition_service::server::{Settings, router};
use axum::{Router, body::Body, http::Request};
use ring::{digest, rand::SystemRandom, signature::{Ed25519KeyPair, KeyPair}};
use serde_json::{Value, json};
use std::{io::Write, path::Path};
use tower::ServiceExt;

const TRACE_ID: &str = "11111111-2222-3333-4444-555555555555";

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

async fn rust_post(app: &Router, body: Vec<u8>) -> (u16, Value) {
    let res = app.clone().oneshot(Request::builder().method("POST").uri("/api/traces").header("host", "localhost").body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    (status, serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap())
}

/// What the stored trace answers on: the list, a text file, a binary file, traversal and missing
/// files, the zip, and delete.
async fn check_reads(app: &Router) {
    let get = |uri: String| { let app = app.clone(); async move {
        let res = app.oneshot(Request::builder().uri(&uri).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
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
    let res = app.clone().oneshot(Request::builder().method("POST").uri(format!("/api/traces/{TRACE_ID}/delete")).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(get(format!("/api/traces/{TRACE_ID}.zip")).await.0, 404);
}

fn service(work: &Path, keys: &Path) -> Router {
    std::fs::create_dir_all(work.join("data")).unwrap();
    std::fs::write(work.join("config.json"), json!({ "port": 1, "repos": [{ "name": "App", "path": "/nowhere", "branches": ["main"], "platforms": [{ "platform": "iOS", "bundleId": "com.x.app", "device": "iPhone" }] }] }).to_string()).unwrap();
    router(Settings { host_token: None, key_dir: keys.to_path_buf(), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(work.join("config.json"), work.join("data"), work.join("p")) }).unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() { copy_dir(&e.path(), &target) } else { std::fs::copy(e.path(), target).unwrap(); }
    }
}

#[tokio::test]
async fn a_trace_the_node_store_stored_reads_and_deletes_in_rust() {
    let work = tempfile::tempdir().unwrap();
    let app = service(work.path(), &work.path().join("keys"));
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/node-traces"), &work.path().join("data/traces"));
    let res = app.clone().oneshot(Request::builder().uri("/api/traces").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let listed: Value = serde_json::from_slice(&body).unwrap();
    let mut meta = listed["traces"][0].clone();
    assert!(meta["receivedAt"].is_u64());
    meta.as_object_mut().unwrap().remove("receivedAt");
    assert_eq!(meta, node_stored_meta(), "the list shows Node's meta.json as written");
    check_reads(&app).await;
}

/// The meta Node stored for the good archive of `the_verifier_gives_the_status_and_text_node_gave`,
/// without `receivedAt` (and with `bytes` as that archive's size, 937).
fn node_stored_meta() -> Value {
    json!({
        "id": TRACE_ID, "bytes": 937, "via": "app", "uploader": null, "createdAt": "2026-11-04T10:00:00Z", "session": "s1", "note": null,
        "app": { "bundleId": "com.x.app", "build": "42", "name": "My App" }, "device": { "name": "Vision Pro" },
        "build": { "keyId": "key1", "buildId": "key1", "repo": "App", "branch": "main", "platform": "iOS", "configuration": null, "installedTo": "iPhone", "builtAt": 5 },
        "files": [{ "path": "logs/app.log", "bytes": 360 }, { "path": "data.bin", "bytes": 100 }],
    })
}

#[tokio::test]
async fn the_verifier_gives_the_status_and_text_node_gave() {
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
    let app = service(&work.path().join("rust"), &keys);

    let with = |f: &dyn Fn(&mut Spec)| { let mut s = good_spec(); f(&mut s); archive(&key, s) };
    // (case, archive, status Node answered, its error text, or None for an accepted upload)
    let cases: Vec<(&str, Vec<u8>, u16, Option<&str>)> = vec![
        ("not a zip", b"hello".to_vec(), 400, Some("not a readable trace zip: no end-of-central-directory record")),
        ("unsigned", { let mut s = good_spec(); s.manifest.as_object_mut().unwrap().remove("signature"); archive(&key, s) }, 403, Some("unsigned trace: only apps built by build-and-sign carry a key; share this one as a file instead")),
        ("wrong format", with(&|s| s.manifest["format"] = json!("debugtrace/2")), 400, Some("unknown trace format \"debugtrace/2\"")),
        ("bad trace id", with(&|s| s.manifest["traceId"] = json!("nope")), 400, Some("manifest has no valid traceId")),
        ("unknown key", with(&|s| s.manifest["signature"]["keyId"] = json!("ghost")), 403, Some("signing key ghost is not in this store's ledger (built on another Mac, or the ledger was cleared)")),
        ("bad key id", with(&|s| s.manifest["signature"]["keyId"] = json!("../etc")), 403, Some("signing key ../etc is not in this store's ledger (built on another Mac, or the ledger was cleared)")),
        ("bad algorithm", with(&|s| s.manifest["signature"]["algorithm"] = json!("rsa")), 400, Some("unsupported signature rsa")),
        ("forged signature", archive(&key, Spec { sign_with: Some(other), ..good_spec() }), 403, Some("manifest.sig does not verify")),
        ("key for another app", with(&|s| s.manifest["signature"]["keyId"] = json!("wrongapp")), 403, Some("trace claims com.x.app, but key wrongapp belongs to com.other")),
        ("key for another build", with(&|s| s.manifest["signature"]["keyId"] = json!("wrongbuild")), 403, Some("trace claims build 42, but key wrongbuild belongs to build 41")),
        ("hash mismatch", with(&|s| s.manifest["files"][0]["sha256"] = json!(sha(b"other"))), 403, Some("logs/app.log does not match its manifest hash")),
        ("size mismatch", with(&|s| s.manifest["files"][1]["bytes"] = json!(99)), 400, Some("data.bin is 100 bytes, the manifest says 99")),
        ("missing file", with(&|s| { s.entries.pop(); }), 400, Some("manifest lists data.bin, the zip lacks it")),
        ("unlisted file", with(&|s| s.entries.push(("extra.txt".into(), b"x".to_vec(), false))), 400, Some("extra.txt is in the zip but not in the manifest")),
        ("traversal path", with(&|s| { s.manifest["files"][0]["path"] = json!("../x"); }), 400, Some("bad manifest entry \"../x\"")),
        ("duplicate listing", with(&|s| { let f = s.manifest["files"][0].clone(); s.manifest["files"].as_array_mut().unwrap().push(f); }), 400, Some("bad manifest entry \"logs/app.log\"")),
        ("lists the manifest", with(&|s| s.manifest["files"].as_array_mut().unwrap().push(json!({ "path": "manifest.json", "bytes": 1, "sha256": "x" }))), 400, Some("bad manifest entry \"manifest.json\"")),
        ("good", archive(&key, good_spec()), 201, None),
        ("good again is a duplicate", archive(&key, good_spec()), 200, None),
    ];
    for (name, body, status, error) in cases {
        let size = body.len();
        let (rs, rb) = rust_post(&app, body).await;
        assert_eq!(rs, status, "{name}: status ({rb})");
        match error {
            Some(text) => assert_eq!(rb["error"], text, "{name}: error text"),
            None => {
                let mut stored = rb["trace"].clone();
                let obj = stored.as_object_mut().unwrap();
                assert!(obj.remove("receivedAt").is_some_and(|t| t.is_u64()));
                let duplicate = obj.remove("duplicate");
                assert_eq!(duplicate.is_some(), status == 200, "{name}: only the repeat is flagged a duplicate");
                let mut expected = node_stored_meta();
                expected["bytes"] = json!(size);
                assert_eq!(size, 937, "{name}: the archive is the same size every run");
                assert_eq!(stored, expected, "{name}: stored meta");
            }
        }
    }
    check_reads(&app).await;
}
