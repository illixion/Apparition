//! Kept builds: list, manifest and download from Rust on one build store holding good, corrupt and
//! half-written builds. The expected replies are what the Node store (`store/lib/artifacts.js`)
//! answered on this same store, captured when it was removed; the digests are the ones `keep`
//! derives from each build id.

use apparition_service::server::{Settings, router};
use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::{path::Path, time::{Duration, SystemTime}};
use tower::ServiceExt;

fn keep(store: &Path, id: &str, age_secs: u64, mutate: impl FnOnce(&mut Value, &Path)) {
    let dir = store.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let sha = format!("{:064x}", id.bytes().fold(7u128, |a, b| a * 31 + b as u128));
    let data = vec![1u8; 2000 + id.len()];
    let mut manifest = json!({
        "format": "apparition-build/1", "buildId": id, "createdAt": "2026-11-01T00:00:00Z", "project": "/p/App",
        "platform": "iOS", "configuration": "Debug", "app": { "bundleId": "com.x.app", "version": "1.0", "build": "7" },
        "artifact": { "sha256": sha, "bytes": data.len(), "kind": "ipa", "file": format!("{sha}.ipa") },
        "binaries": {}, "source": { "before": { "anyDirty": true, "fingerprint": "f1" }, "after": {}, "stable": true },
    });
    std::fs::write(dir.join(format!("{sha}.ipa")), &data).unwrap();
    mutate(&mut manifest, &dir);
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
    let when = SystemTime::now() - Duration::from_secs(age_secs);
    directory(&dir).set_modified(when).unwrap();
}

const SHA_NEWEST: &str = "0000000000000000000000000000000000000000000000000000000233c79aad";
const SHA_OLDER: &str = "0000000000000000000000000000000000000000000000000000004484f817d1";

fn summary(id: &str, sha: &str, bytes: u64, source: Value) -> Value {
    json!({ "buildId": id, "createdAt": "2026-11-01T00:00:00Z", "project": "/p/App", "platform": "iOS", "configuration": "Debug",
        "app": { "bundleId": "com.x.app", "version": "1.0", "build": "7" },
        "artifact": { "sha256": sha, "bytes": bytes, "kind": "ipa", "file": format!("{sha}.ipa") }, "origin": null, "source": source })
}

fn manifest(id: &str, sha: &str, bytes: u64, source: Value) -> Value {
    json!({ "format": "apparition-build/1", "buildId": id, "createdAt": "2026-11-01T00:00:00Z", "project": "/p/App", "platform": "iOS", "configuration": "Debug",
        "app": { "bundleId": "com.x.app", "version": "1.0", "build": "7" },
        "artifact": { "sha256": sha, "bytes": bytes, "kind": "ipa", "file": format!("{sha}.ipa") }, "binaries": {}, "source": source })
}

#[tokio::test]
async fn artifacts_answer_as_the_node_store_did() {
    let work = tempfile::tempdir().unwrap();
    let store = work.path().join("builds");
    keep(&store, "newest", 10, |_, _| {});
    keep(&store, "older-1", 500, |m, _| { m["source"] = Value::Null; });
    keep(&store, "wrong-id", 300, |m, _| m["buildId"] = json!("someone-else"));
    keep(&store, "short-file", 200, |_, dir| {
        for e in std::fs::read_dir(dir).unwrap().flatten() { if e.path().extension().is_some_and(|x| x == "ipa") { std::fs::write(e.path(), b"x").unwrap(); } }
    });
    keep(&store, "bad-kind", 100, |m, _| m["artifact"]["kind"] = json!("exe"));
    std::fs::create_dir_all(store.join(".incoming-abc")).unwrap();
    std::fs::create_dir_all(store.join("empty-dir")).unwrap();
    let rust = work.path().join("rust");
    std::fs::create_dir_all(rust.join("data")).unwrap();
    std::fs::write(rust.join("config.json"), json!({ "port": 1, "buildManifestDir": "../builds", "repos": [] }).to_string()).unwrap();
    let app = router(Settings { host_token: None, key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();

    let source = json!({ "stable": true, "anyDirty": true, "fingerprint": "f1" });
    let not_found = json!({ "error": "Build artifact not found or invalid." });
    for (uri, status, expected) in [
        ("/api/artifacts", 200, json!({ "builds": [summary("newest", SHA_NEWEST, 2006, source), summary("older-1", SHA_OLDER, 2007, Value::Null)] })),
        ("/api/artifacts/newest/manifest", 200, manifest("newest", SHA_NEWEST, 2006, json!({ "before": { "anyDirty": true, "fingerprint": "f1" }, "after": {}, "stable": true }))),
        ("/api/artifacts/older-1/manifest", 200, manifest("older-1", SHA_OLDER, 2007, Value::Null)),
        ("/api/artifacts/wrong-id/manifest", 404, not_found.clone()),
        ("/api/artifacts/short-file/manifest", 404, not_found.clone()),
        ("/api/artifacts/bad-kind/manifest", 404, not_found.clone()),
        ("/api/artifacts/missing/manifest", 404, not_found),
    ] {
        let res = app.clone().oneshot(Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status().as_u16(), status, "{uri}: status");
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        // Compared as values, key order included: serde_json is built with preserve_order.
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected, "{uri}");
        assert_eq!(serde_json::to_string(&serde_json::from_slice::<Value>(&body).unwrap()).unwrap(), serde_json::to_string(&expected).unwrap(), "{uri}: key order");
    }
    let res = app.clone().oneshot(Request::builder().uri("/api/artifacts/newest/download").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), 200);
    let headers = res.headers().clone();
    assert_eq!(axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().len(), 2006);
    assert_eq!(headers["content-length"], "2006");
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["content-disposition"], format!("attachment; filename=\"{SHA_NEWEST}.ipa\""));
    let res = app.clone().oneshot(Request::builder().uri("/api/artifacts/..%2fx/manifest").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_ne!(res.status(), 200, "a traversal id is never served (404 on Node, 501 here until every route is ported)");
}

/// A directory opened so its times can be set: Windows opens one only with backup semantics, and
/// sets a time only through a handle opened for writing.
fn directory(dir: &std::path::Path) -> std::fs::File {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        return std::fs::OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir).unwrap();
    }
    #[cfg(not(windows))]
    std::fs::File::open(dir).unwrap()
}
