//! Kept builds: list, manifest and download from Rust against the real Node store, on one build
//! store holding good, corrupt and half-written builds.

#![cfg(target_os = "macos")]

mod common;

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
    std::fs::File::open(&dir).unwrap().set_modified(when).unwrap();
}

#[tokio::test]
async fn artifacts_match_the_node_store() {
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
    let port = common::free_port();
    for dir in ["rust", "node"] {
        std::fs::create_dir_all(work.path().join(dir).join("data")).unwrap();
        std::fs::write(work.path().join(dir).join("config.json"), json!({ "port": port, "buildManifestDir": "../builds", "repos": [] }).to_string()).unwrap();
    }
    let rust = work.path().join("rust");
    let app = router(Settings { host_token: None, key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])), ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();
    let node = work.path().join("node");
    let Some(node) = common::NodeStore::start(&node.join("config.json"), &node.join("data"), port, &[]) else { return };

    for uri in ["/api/artifacts", "/api/artifacts/newest/manifest", "/api/artifacts/older-1/manifest", "/api/artifacts/wrong-id/manifest",
        "/api/artifacts/short-file/manifest", "/api/artifacts/bad-kind/manifest", "/api/artifacts/missing/manifest", "/api/artifacts/newest/download"] {
        let res = app.clone().oneshot(Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
        let rs = res.status().as_u16();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let (ns, nb) = node.get(uri);
        assert_eq!(rs, ns, "{uri}: status");
        if uri.ends_with("download") {
            assert_eq!(body.len(), 2006);
            assert_eq!(headers["content-length"], "2006");
            assert!(headers["content-disposition"].to_str().unwrap().ends_with(".ipa\""));
        } else {
            let (r, n): (Value, Value) = (serde_json::from_slice(&body).unwrap(), serde_json::from_str(&nb).unwrap());
            assert_eq!(r, n, "{uri}");
        }
    }
    let res = app.clone().oneshot(Request::builder().uri("/api/artifacts/..%2fx/manifest").header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
    assert_ne!(res.status(), 200, "a traversal id is never served (404 on Node, 501 here until every route is ported)");
    let list: Value = serde_json::from_str(&node.get("/api/artifacts").1).unwrap();
    let ids: Vec<&str> = list["builds"].as_array().unwrap().iter().map(|b| b["buildId"].as_str().unwrap()).collect();
    assert_eq!(ids, ["newest", "older-1"], "corrupt and half-written builds are skipped, newest first");
}
