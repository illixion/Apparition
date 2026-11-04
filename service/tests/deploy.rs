//! A build sent from another computer: kept so that it is listed, described and served exactly
//! like one built on this Mac. The expected list entry, manifest and download headers are what the
//! old Node store (`store/lib/artifacts.js`) read back from the very build this test uploads,
//! captured when it was removed. The upload is the same bytes each run, so its SHA-256 is pinned
//! too; only the generated build id and timestamp vary.

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
async fn an_uploaded_ipa_is_kept_as_a_build_the_devices_apparition_can_read() {
    let work = tempfile::tempdir().unwrap();
    let dir = "rust";
    std::fs::create_dir_all(work.path().join(dir).join("data")).unwrap();
    std::fs::write(work.path().join(dir).join("config.json"), json!({ "port": 1, "buildManifestDir": "../builds", "repos": [] }).to_string()).unwrap();
    let rust = work.path().join("rust");
    let app = router(Settings { host_token: Some(HOST.into()), key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();

    let good = ipa(&[("Payload/Thing.app/Info.plist", &info("XROS", "com.example.thing")), ("Payload/Thing.app/Thing", b"\xcf\xfa\xed\xfe binary")]);
    assert_eq!(deploy(&app, "wrong", "", good.clone(), None).await.0, 401);
    let (status, reply) = deploy(&app, HOST, "?file=Thing.ipa&device=ixVision", good.clone(), Some("Gaming PC")).await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!(reply["platform"], "visionOS");
    assert_eq!(reply["install"]["message"], format!("Spatial Thing is on {}. Open Apparition on ixVision to install it.", apparition_service::HERE));
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

    // With a host token the web UI is off, so read it back through a service on the same data
    // and build store, as the Store's own page would.
    let reader = router(Settings { host_token: None, key_dir: rust.join("k"), browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(rust.join("config.json"), rust.join("data"), rust.join("p")) }).unwrap();
    let get = |uri: String| { let app = reader.clone(); async move {
        let res = app.oneshot(Request::builder().uri(uri).header("host", "localhost").body(Body::empty()).unwrap()).await.unwrap();
        (res.status().as_u16(), res.headers().clone(), axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap().to_vec())
    } };
    const SHA: &str = "2d384e7853a37d31088ef68366b300646073c66e16ce3c4d025d08868973eb7e";
    let app_info = json!({ "bundleId": "com.example.thing", "originalBundleId": "com.example.thing", "name": "Spatial Thing", "bundleName": "Thing.app", "minOSVersion": "26.0", "version": "1.2", "build": "42" });
    let artifact = json!({ "sha256": SHA, "bytes": 502, "kind": "ipa", "file": format!("{SHA}.ipa") });
    let origin = json!({ "kind": "upload", "computer": "Gaming PC", "file": "Thing.ipa", "device": "ixVision" });
    // Ids and timestamps vary per run: put this run's into the captured shape.
    let created = |v: &Value| v["createdAt"].clone();

    let (status, _, body) = get("/api/artifacts".into()).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let listed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(listed["builds"].as_array().unwrap().len(), 1);
    let entry = &listed["builds"][0];
    assert_eq!(entry["buildId"], id.as_str());
    assert_eq!(serde_json::to_string(entry).unwrap(), serde_json::to_string(&json!({
        "buildId": id, "createdAt": created(entry), "project": null, "platform": "visionOS", "configuration": null,
        "app": app_info, "artifact": artifact, "origin": origin, "source": null,
    })).unwrap(), "the list entry, key order included, as Node read it back");

    let (status, _, body) = get(format!("/api/artifacts/{id}/manifest")).await;
    assert_eq!(status, 200);
    let kept: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(serde_json::to_string(&kept).unwrap(), serde_json::to_string(&json!({
        "format": "apparition-build/1", "buildId": id, "keyId": null, "createdAt": created(&kept), "project": null, "platform": "visionOS", "configuration": null,
        "app": app_info, "artifact": artifact, "binaries": {}, "symbols": null, "source": null, "origin": origin,
    })).unwrap(), "the manifest, as Node read it back");
    assert!(created(&kept).as_str().is_some_and(|t| t.ends_with('Z')));

    let (status, headers, body) = get(format!("/api/artifacts/{id}/download")).await;
    assert_eq!((status, body.len()), (200, good.len()));
    assert_eq!(good.len(), 502);
    assert_eq!(headers["content-type"], "application/octet-stream");
    assert_eq!(headers["content-disposition"], format!("attachment; filename=\"{SHA}.ipa\""));
}
