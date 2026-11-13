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
        ("unsigned", { let mut s = good_spec(); s.manifest.as_object_mut().unwrap().remove("signature"); archive(&key, s) }, 403, Some("unsigned trace: only apps built by apparition build carry a key; share this one as a file instead")),
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

// ---- encrypted problem reports (.trace) ----

/// The throwaway key of `tests/fixtures/report` (see its README).
const REPORT_KEY: &str = "a5d2ee9eadd96f4a";
const REPORT_ID: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

fn report_fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/report").join(name)
}

/// A Store whose report keys are `report_keys`, with `config` on top of the usual one repo.
fn report_service(work: &Path, keys: &Path, report_keys: &Path, config: Value) -> Router {
    std::fs::create_dir_all(work.join("data")).unwrap();
    let mut whole = json!({ "port": 1, "repos": [{ "name": "App", "path": "/nowhere", "branches": ["main"], "platforms": [{ "platform": "iOS", "bundleId": "com.x.app", "device": "iPhone" }] }] });
    whole.as_object_mut().unwrap().extend(config.as_object().unwrap().clone());
    std::fs::write(work.join("config.json"), whole.to_string()).unwrap();
    router(Settings {
        host_token: None, key_dir: keys.to_path_buf(), report_key_dir: report_keys.to_path_buf(),
        browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(work.join("config.json"), work.join("data"), work.join("p"))
    }).unwrap()
}

async fn post(app: &Router, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (u16, Value) {
    let mut request = Request::builder().method("POST").uri(uri).header("host", "localhost");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let res = app.clone().oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    (status, serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap_or(Value::Null))
}

/// The public half of a report key, and the directory that holds its private half.
fn report_key(dir: &Path, secret: [u8; 32]) -> [u8; 32] {
    use base64::{Engine, engine::general_purpose::STANDARD};
    std::fs::create_dir_all(dir).unwrap();
    let secret = x25519_dalek::StaticSecret::from(secret);
    let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
    std::fs::write(dir.join(format!("{}.key", apparition_service::reports::key_id(&public))), format!("{}\n", STANDARD.encode(secret.to_bytes()))).unwrap();
    public
}

#[tokio::test]
async fn a_report_cryptokit_sealed_is_opened_and_kept_as_an_unsigned_report() {
    let work = tempfile::tempdir().unwrap();
    let app = report_service(work.path(), &work.path().join("keys"), &report_fixture("keys"), json!({}));
    let sealed = std::fs::read(report_fixture("cryptokit.trace")).unwrap();
    let zip = std::fs::read(report_fixture("report.zip")).unwrap();

    let (status, body) = post(&app, "/api/traces/report", &[], sealed.clone()).await;
    assert_eq!(status, 201, "{body}");
    let mut stored = body["trace"].clone();
    assert!(stored.as_object_mut().unwrap().remove("receivedAt").is_some_and(|t| t.is_u64()));
    assert_eq!(stored, json!({
        "id": REPORT_ID, "bytes": zip.len(), "via": "report", "uploader": null,
        "createdAt": "2026-11-13T09:00:05Z", "session": "s-report", "note": "It stopped refreshing after the update.",
        "app": { "bundleId": "com.example.publicapp", "name": "Public App", "version": "1.0.0", "build": "298", "platform": "visionOS" },
        "device": { "model": "RealityDevice14,1", "os": "visionOS 27.1.0" },
        // Nothing vouches for the build: no ledger key signed this.
        "build": { "keyId": null, "buildId": null, "repo": null, "branch": null, "platform": null, "configuration": null, "installedTo": null, "builtAt": null },
        "files": [{ "path": "README.md", "bytes": 57 }, { "path": "info.json", "bytes": 199 }, { "path": "logs.txt", "bytes": 730 },
                  { "path": "breadcrumbs.jsonl", "bytes": 108 }, { "path": "note.txt", "bytes": 39 }],
        "report": { "keyId": REPORT_KEY, "signed": false },
    }));

    // Unpacked like any trace, and the zip kept is the one that was inside.
    let dir = work.path().join("data/traces").join(REPORT_ID);
    assert_eq!(std::fs::read(dir.join("trace.zip")).unwrap(), zip);
    assert!(std::fs::read_to_string(dir.join("files/logs.txt")).unwrap().starts_with("# the app's own log"));
    let res = app.clone().oneshot(Request::builder().uri("/api/traces").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    let listed: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(listed["traces"][0]["report"], json!({ "keyId": REPORT_KEY, "signed": false }));

    let (status, body) = post(&app, "/api/traces/report", &[], sealed.clone()).await;
    assert_eq!((status, &body["trace"]["duplicate"]), (200, &json!(true)));

    // The route apps upload to takes neither form of it: its gate is a ledger signature.
    let (status, body) = post(&app, "/api/traces", &[], sealed).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().starts_with("this is an encrypted report (.trace), which only a person at this Store can add"), "{body}");
    let (status, body) = post(&app, "/api/traces", &[], zip).await;
    assert_eq!((status, body["error"].as_str().unwrap()), (403, "unsigned trace: only apps built by apparition build carry a key; share this one as a file instead"));
}

/// One stored entry per pair, the way DebugTrace's report sheet writes the attachable form.
fn wrapper(entries: &[(&str, &[u8])]) -> Vec<u8> {
    zip_of(&entries.iter().map(|(name, data)| (name.to_string(), data.to_vec(), false)).collect::<Vec<_>>())
}

/// GitHub takes no `.trace` file on an issue, so a person attaches `<name>.trace.zip`: one entry,
/// the `.trace`. It is a report however it is named, and nothing else shaped like a zip is.
#[tokio::test]
async fn a_report_wrapped_in_a_zip_is_taken_like_the_bare_file() {
    use apparition_service::reports::{is_report, unwrap};
    let sealed = std::fs::read(report_fixture("cryptokit.trace")).unwrap();
    let wrapped = wrapper(&[("Public-App-trace-20261113-090005-aaaaaaaa.trace", &sealed)]);
    assert_eq!(unwrap(&wrapped).as_deref(), Some(&sealed[..]));
    assert!(is_report(&wrapped));
    for shape in [
        wrapper(&[("a.trace", &sealed), ("b.trace", &sealed)]),
        wrapper(&[("a.txt", &sealed)]),
        wrapper(&[("dir/a.trace", &sealed)]),
        wrapper(&[(".trace", &sealed)]),
        wrapper(&[("a.trace", b"not encrypted")]),
        std::fs::read(report_fixture("report.zip")).unwrap(),
    ] {
        assert!(unwrap(&shape).is_none() && !is_report(&shape));
    }

    let work = tempfile::tempdir().unwrap();
    let app = report_service(work.path(), &work.path().join("keys"), &report_fixture("keys"), json!({}));
    // The app route refuses it as it refuses the bare file.
    let (status, body) = post(&app, "/api/traces", &[], wrapped.clone()).await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().starts_with("this is an encrypted report"), "{body}");
    let (status, body) = post(&app, "/api/traces/report", &[], wrapped).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["trace"]["id"], json!(REPORT_ID));
    assert_eq!(body["trace"]["report"], json!({ "keyId": REPORT_KEY, "signed": false }));
}

#[tokio::test]
async fn only_a_signed_in_page_adds_a_report() {
    let work = tempfile::tempdir().unwrap();
    let hash = apparition_service::auth::hash_password("correct horse").unwrap();
    let app = report_service(work.path(), &work.path().join("keys"), &report_fixture("keys"), json!({ "passwordHash": hash }));
    let sealed = std::fs::read(report_fixture("cryptokit.trace")).unwrap();

    let (status, body) = post(&app, "/api/traces/report", &[], sealed.clone()).await;
    assert_eq!((status, &body["error"]), (401, &json!("sign in first")));
    let (status, _) = post(&app, "/api/traces/report", &[("cookie", "store_session=bogus")], sealed.clone()).await;
    assert_eq!(status, 401);
    // Not through the native clients' API either, whatever they hold.
    let (status, _) = post(&app, "/api/service/v1/traces/report", &[("authorization", "Bearer abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNO12")], sealed.clone()).await;
    assert_eq!(status, 401);
    assert!(!work.path().join("data/traces").join(REPORT_ID).exists());

    let res = app.clone().oneshot(Request::builder().method("POST").uri("/api/login").header("host", "localhost")
        .body(Body::from(json!({ "password": "correct horse" }).to_string())).unwrap()).await.unwrap();
    let cookie = res.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_string();
    // Another site's page can't post one with the person's session.
    let (status, _) = post(&app, "/api/traces/report", &[("cookie", &cookie), ("origin", "https://evil.test")], sealed.clone()).await;
    assert_eq!(status, 403);
    let (status, body) = post(&app, "/api/service/v1/traces/report", &[("cookie", &cookie)], sealed.clone()).await;
    assert_ne!(status, 201, "{body}");
    let (status, body) = post(&app, "/api/traces/report", &[("cookie", &cookie)], sealed).await;
    assert_eq!(status, 201, "{body}");
}

#[tokio::test]
async fn a_report_is_held_to_every_rule_but_the_signature() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let rng = SystemRandom::new();
    let key = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
    let other = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref()).unwrap();
    let work = tempfile::tempdir().unwrap();
    let keys = work.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    std::fs::write(keys.join("key1.json"), json!({ "keyId": "key1", "publicKey": STANDARD.encode(key.public_key().as_ref()), "bundleId": "com.x.app", "build": "42", "branch": "main", "platform": "iOS", "device": "iPhone", "createdAt": 5 }).to_string()).unwrap();
    let report_keys = work.path().join("report-keys");
    let public = report_key(&report_keys, [5u8; 32]);
    let stranger = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from([6u8; 32])).to_bytes();
    let app = report_service(&work.path().join("store"), &keys, &report_keys, json!({}));
    let seal = |zip: Vec<u8>| apparition_service::reports::seal(&zip, &public).unwrap();

    // What a public copy sends: no signature at all.
    let unsigned = |f: &dyn Fn(&mut Spec)| {
        let mut s = good_spec();
        s.manifest.as_object_mut().unwrap().remove("signature");
        f(&mut s);
        let mut entries = vec![("manifest.json".to_string(), serde_json::to_vec(&s.manifest).unwrap(), true)];
        entries.extend(s.entries);
        zip_of(&entries)
    };
    let signed = |f: &dyn Fn(&mut Spec)| { let mut s = good_spec(); f(&mut s); archive(&key, s) };
    let id = |n: u8| format!("{n:08x}-2222-3333-4444-555555555555");

    // (case, body, status, error or None, and for an accepted one whether a ledger key signed it)
    let cases: Vec<(&str, Vec<u8>, u16, Option<&str>, bool)> = vec![
        ("a zip, not a report", unsigned(&|_| {}), 400, Some("not an encrypted report: no magic. A report is a .trace file, or the .trace.zip that holds one; upload a trace .zip as a trace instead"), false),
        ("sealed to a key this Mac lacks", apparition_service::reports::seal(&unsigned(&|_| {}), &stranger).unwrap(), 403, None, false),
        ("cut short", { let mut b = seal(unsigned(&|_| {})); b.truncate(b.len() - 9); b }, 400, Some("the report did not decrypt: it was changed or cut short. Ask for the original file again"), false),
        ("not a zip inside", seal(b"hello".to_vec()), 400, Some("not a readable trace zip: no end-of-central-directory record"), false),
        ("hash mismatch", seal(unsigned(&|s| s.manifest["files"][0]["sha256"] = json!(sha(b"other")))), 403, Some("logs/app.log does not match its manifest hash"), false),
        ("size mismatch", seal(unsigned(&|s| s.manifest["files"][1]["bytes"] = json!(99))), 400, Some("data.bin is 100 bytes, the manifest says 99"), false),
        ("missing file", seal(unsigned(&|s| { s.entries.pop(); })), 400, Some("manifest lists data.bin, the zip lacks it"), false),
        ("unlisted file", seal(unsigned(&|s| s.entries.push(("extra.txt".into(), b"x".to_vec(), false)))), 400, Some("extra.txt is in the zip but not in the manifest"), false),
        ("traversal path", seal(unsigned(&|s| { s.manifest["files"][0]["path"] = json!("../x"); })), 400, Some("bad manifest entry \"../x\""), false),
        ("wrong format", seal(unsigned(&|s| s.manifest["format"] = json!("debugtrace/2"))), 400, Some("unknown trace format \"debugtrace/2\""), false),
        ("bad trace id", seal(unsigned(&|s| s.manifest["traceId"] = json!("nope"))), 400, Some("manifest has no valid traceId"), false),
        // A signature naming a key in this Mac's ledger is checked, so a build of this Mac's is held to it.
        ("forged against a ledger key", seal(archive(&key, Spec { sign_with: Some(other), ..good_spec() })), 403, Some("manifest.sig does not verify"), false),
        ("a ledger key for another build", seal(signed(&|s| s.manifest["app"]["build"] = json!("43"))), 403, Some("trace claims build 43, but key key1 belongs to build 42"), false),
        ("unsigned", seal(unsigned(&|s| s.manifest["traceId"] = json!(id(1)))), 201, None, false),
        ("signed by a key this Mac never saw", seal(signed(&|s| { s.manifest["traceId"] = json!(id(2)); s.manifest["signature"]["keyId"] = json!("ghost"); })), 201, None, false),
        ("says signed, carries no signature", seal(unsigned(&|s| { s.manifest["traceId"] = json!(id(3)); s.manifest["signature"] = json!({ "algorithm": "ed25519", "keyId": "key1" }); })), 201, None, false),
        ("an algorithm nothing here checks", seal(signed(&|s| { s.manifest["traceId"] = json!(id(4)); s.manifest["signature"]["algorithm"] = json!("rsa"); })), 201, None, false),
        ("signed by a ledger key", seal(signed(&|s| s.manifest["traceId"] = json!(id(5)))), 201, None, true),
    ];
    for (name, body, status, error, by_ledger) in cases {
        let (rs, rb) = post(&app, "/api/traces/report", &[], body).await;
        assert_eq!(rs, status, "{name}: status ({rb})");
        if let Some(text) = error {
            assert_eq!(rb["error"], text, "{name}: error text");
        } else if status == 201 {
            let trace = &rb["trace"];
            assert_eq!(trace["via"], "report", "{name}");
            assert_eq!(trace["report"], json!({ "keyId": apparition_service::reports::key_id(&public), "signed": by_ledger }), "{name}");
            let build = if by_ledger {
                json!({ "keyId": "key1", "buildId": "key1", "repo": "App", "branch": "main", "platform": "iOS", "configuration": null, "installedTo": "iPhone", "builtAt": 5 })
            } else {
                json!({ "keyId": null, "buildId": null, "repo": null, "branch": null, "platform": null, "configuration": null, "installedTo": null, "builtAt": null })
            };
            assert_eq!(trace["build"], build, "{name}: what the ledger says of the build, or nothing");
        } else {
            let message = rb["error"].as_str().unwrap();
            assert!(message.starts_with(&format!("this report is encrypted to key {}, which is not in ", apparition_service::reports::key_id(&stranger))), "{name}: {message}");
            assert!(message.ends_with("Open it on the Mac that holds that key (`debugtrace-decrypt keys` lists a Mac's keys)"), "{name}: {message}");
        }
    }
    assert_eq!(apparition_service::traces::list(&work.path().join("store/data/traces")).len(), 5);
}
