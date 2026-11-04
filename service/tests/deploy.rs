//! A build sent from another computer: kept so that the Node store (what the devices' Apparition
//! talks to today) lists, describes and serves it exactly like one built on this Mac.

#![cfg(target_os = "macos")]

mod common;

use apparition_service::server::{Settings, router};
use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use std::io::Write;
use tower::ServiceExt;

const HOST: &str = "host-token-host-token-host-token-host-token";

fn ipa(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in entries {
        zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(data).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

fn info(platforms: &str, bundle: &str) -> Vec<u8> {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{bundle}</string>
<key>CFBundleDisplayName</key><string>Spatial Thing</string>
<key>CFBundleShortVersionString</key><string>1.2</string>
<key>CFBundleVersion</key><string>42</string>
<key>MinimumOSVersion</key><string>26.0</string>
<key>CFBundleSupportedPlatforms</key><array><string>{platforms}</string></array>
</dict></plist>"#).into_bytes()
}

async fn deploy(app: &Router, bearer: &str, query: &str, body: Vec<u8>, computer: Option<&str>) -> (u16, Value) {
    let mut req = Request::builder().method("POST").uri(format!("/api/service/v1/deploy{query}")).header("host", "localhost")
        .header("authorization", format!("Bearer {bearer}"));
    if let Some(c) = computer { req = req.header("x-apparition-computer", c); }
    let res = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn an_uploaded_ipa_is_kept_as_a_build_the_node_store_serves() {
    let work = tempfile::tempdir().unwrap();
    let port = common::free_port();
    for dir in ["rust", "node"] {
        std::fs::create_dir_all(work.path().join(dir).join("data")).unwrap();
        std::fs::write(work.path().join(dir).join("config.json"), json!({ "port": port, "buildManifestDir": "../builds", "repos": [] }).to_string()).unwrap();
    }
    let rust = work.path().join("rust");
    let app = router(Settings { host_token: Some(HOST.into()), key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();

    let good = ipa(&[("Payload/Thing.app/Info.plist", &info("XROS", "com.example.thing")), ("Payload/Thing.app/Thing", b"\xcf\xfa\xed\xfe binary")]);
    assert_eq!(deploy(&app, "wrong", "", good.clone(), None).await.0, 401);
    let (status, reply) = deploy(&app, HOST, "?file=Thing.ipa&device=ixVision", good.clone(), Some("Gaming PC")).await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!(reply["platform"], "visionOS");
    assert_eq!(reply["install"]["message"], "Spatial Thing is on the Mac. Open Apparition on ixVision to install it.");
    let id = reply["buildId"].as_str().unwrap().to_string();
    assert!(id.starts_with("upload-") && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "{id}");

    for (body, expect) in [
        (Vec::new(), "send the .ipa"),
        (b"not a zip".to_vec(), "isn't an .ipa"),
        (ipa(&[("Payload/A.app/Info.plist", &info("iPhoneOS", "a.b")), ("Payload/B.app/Info.plist", &info("iPhoneOS", "a.c"))]), "more than one app"),
        (ipa(&[("Payload/A.app/Info.plist", &info("iPhoneSimulator", "a.b"))]), "simulator build"),
        (ipa(&[("Payload/A.app/Info.plist", &info("iPhoneOS", "../../evil"))]), "CFBundleIdentifier"),
        (ipa(&[("Payload/A.app/Frameworks/X.framework/Info.plist", &info("iPhoneOS", "a.b"))]), "isn't an .ipa"),
    ] {
        let (status, reply) = deploy(&app, HOST, "", body, None).await;
        assert_eq!(status, 400, "{expect}: {reply}");
        assert!(reply["error"].as_str().unwrap().contains(expect), "{expect}: {reply}");
    }
    let builds: Vec<_> = std::fs::read_dir(work.path().join("builds")).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    assert_eq!(builds, [id.clone()], "refused uploads leave nothing behind");

    let manifest = |res: (u16, String)| -> Value { assert_eq!(res.0, 200, "{}", res.1); serde_json::from_str(&res.1).unwrap() };
    let Some(node) = common::NodeStore::start(&work.path().join("node/config.json"), &work.path().join("node/data"), port, &[]) else { return };
    let listed = manifest(node.get("/api/artifacts"));
    assert_eq!(listed["builds"][0]["buildId"], id.as_str());
    assert_eq!(listed["builds"][0]["app"]["bundleId"], "com.example.thing");
    let kept = manifest(node.get(&format!("/api/artifacts/{id}/manifest")));
    assert_eq!(kept["origin"], json!({ "kind": "upload", "computer": "Gaming PC", "file": "Thing.ipa", "device": "ixVision" }));
    assert_eq!(kept["app"]["version"], "1.2");
    let (status, body) = node.get(&format!("/api/artifacts/{id}/download"));
    assert_eq!((status, body.len()), (200, String::from_utf8_lossy(&good).len()));
}
