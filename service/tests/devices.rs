//! What the Mac does with devices itself: list the ones it is paired with, read their apps, and
//! install a build that arrived from another computer, through a (fake) backend standing in for
//! the core's pairings.

#![cfg(target_os = "macos")]

use apparition_service::{debug::Fixed, devices::{App, Device, Devices, Fut, Installed}, server::{Settings, router}};
use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use std::{io::Write, path::PathBuf, sync::{Arc, Mutex}};
use tower::ServiceExt;

const HOST: &str = "host-token-host-token-host-token-host-token";

#[derive(Default)]
struct Fake {
    installs: Mutex<Vec<(String, PathBuf, u64)>>,
    fail: Mutex<Option<String>>,
}

impl Devices for Fake {
    fn list(&self) -> Fut<'_, Vec<Device>> {
        Box::pin(async { Ok(vec![
            Device { udid: "AAAA-1".into(), name: "ixPhone".into(), platform: "iOS".into(), model: "iPhone17,1".into() },
            Device { udid: "BBBB-2".into(), name: "ixVision".into(), platform: "visionOS".into(), model: "RealityDevice14,1".into() },
            Device { udid: "CCCC-3".into(), name: "Living Room".into(), platform: "tvOS".into(), model: "AppleTV14,1".into() },
        ]) })
    }
    fn apps(&self, device: String) -> Fut<'_, Vec<App>> {
        Box::pin(async move {
            assert_eq!(device, "AAAA-1", "the route passes the UDID it resolved");
            Ok(vec![App { bundle_id: "com.example.thing".into(), name: "Thing".into(), version: Some("1.2".into()), build: Some("42".into()) }])
        })
    }
    fn install(&self, device: String, ipa: PathBuf) -> Fut<'_, Installed> {
        Box::pin(async move {
            if let Some(why) = self.fail.lock().unwrap().clone() { return Err(why); }
            let size = std::fs::metadata(&ipa).map_err(|e| e.to_string())?.len();
            self.installs.lock().unwrap().push((device.clone(), ipa, size));
            Ok(Installed { bundle_id: "com.example.thing".into(), device: "ixPhone".into(), seconds: 1.5, queued: false, signed_with: Some("Apple Development: Someone".into()) })
        })
    }
}

fn ipa(platform: &str) -> Vec<u8> {
    let info = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.example.thing</string>
<key>CFBundleDisplayName</key><string>Thing</string>
<key>CFBundleShortVersionString</key><string>1.2</string>
<key>CFBundleVersion</key><string>42</string>
<key>CFBundleSupportedPlatforms</key><array><string>{platform}</string></array>
</dict></plist>"#);
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("Payload/Thing.app/Info.plist", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(info.as_bytes()).unwrap();
    zip.finish().unwrap().into_inner()
}

struct Rig {
    app: Router,
    fake: Arc<Fake>,
    _dir: tempfile::TempDir,
}

fn rig(devices: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), json!({ "buildManifestDir": "builds", "repos": [] }).to_string()).unwrap();
    let fake = Arc::new(Fake::default());
    let app = router(Settings {
        host_token: Some(HOST.into()), key_dir: dir.path().join("k"), browser: Arc::new(Fixed(vec![])),
        devices: devices.then(|| fake.clone() as Arc<dyn Devices>),
        ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("p"))
    }).unwrap();
    Rig { app, fake, _dir: dir }
}

async fn call(app: &Router, method: &str, uri: &str, bearer: &str, body: Vec<u8>) -> (u16, Value) {
    let res = app.clone().oneshot(Request::builder().method(method).uri(uri).header("host", "localhost")
        .header("authorization", format!("Bearer {bearer}")).body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn devices_and_their_apps_are_listed_for_the_bearer_only() {
    let r = rig(true);
    assert_eq!(call(&r.app, "GET", "/api/service/v1/devices", "wrong", vec![]).await.0, 401);
    let (status, body) = call(&r.app, "GET", "/api/service/v1/devices", HOST, vec![]).await;
    assert_eq!(status, 200);
    assert_eq!(body["devices"][0], json!({ "udid": "AAAA-1", "name": "ixPhone", "platform": "iOS", "model": "iPhone17,1" }));
    assert_eq!(body["devices"].as_array().unwrap().len(), 3);

    let (status, body) = call(&r.app, "GET", "/api/service/v1/devices/ixphone/apps", HOST, vec![]).await;
    assert_eq!((status, body["device"]["name"].as_str(), body["apps"][0]["bundleId"].as_str(), body["apps"][0]["build"].as_str()), (200, Some("ixPhone"), Some("com.example.thing"), Some("42")));
    let (status, body) = call(&r.app, "GET", "/api/service/v1/devices/ghost/apps", HOST, vec![]).await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("not paired with a device named 'ghost'"), "{body}");

    let none = rig(false);
    let (status, body) = call(&none.app, "GET", "/api/service/v1/devices", HOST, vec![]).await;
    assert_eq!(status, 501, "{body}");
}

#[tokio::test]
async fn a_deploy_can_ask_the_mac_to_finish_the_install_itself() {
    let r = rig(true);
    let (status, reply) = call(&r.app, "POST", "/api/service/v1/deploy?file=Thing.ipa&device=ixPhone&install=auto", HOST, ipa("iPhoneOS")).await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!((reply["install"]["via"].as_str(), reply["install"]["installed"].as_bool()), (Some("store"), Some(true)));
    assert_eq!(reply["install"]["message"], "Thing is installed on ixPhone.");
    assert_eq!(reply["install"]["signedWith"], "Apple Development: Someone");
    let installs = r.fake.installs.lock().unwrap().clone();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0].0, "AAAA-1", "the backend gets the UDID");
    assert!(installs[0].1.extension().is_some_and(|e| e == "ipa") && installs[0].2 > 0, "{installs:?}");

    // Without install=, nothing is installed and the device's Apparition is asked, as before.
    let (_, reply) = call(&r.app, "POST", "/api/service/v1/deploy?device=ixPhone", HOST, ipa("iPhoneOS")).await;
    assert_eq!(reply["install"]["message"], "Thing is on the Mac. Open Apparition on ixPhone to install it.");
    assert_eq!(r.fake.installs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn when_the_mac_cannot_install_the_build_is_kept_for_the_device() {
    let r = rig(true);
    // The backend refuses (say, no certificate covers the device): the build stays, and the answer
    // says what happened and still points at the device's Apparition.
    *r.fake.fail.lock().unwrap() = Some("No imported profile can sign this app on this device".into());
    let (status, reply) = call(&r.app, "POST", "/api/service/v1/deploy?device=ixPhone&install=1", HOST, ipa("iPhoneOS")).await;
    assert_eq!(status, 201, "{reply}");
    assert_eq!((reply["install"]["via"].as_str(), reply["install"]["installed"].as_bool()), (Some("device"), Some(false)));
    let message = reply["install"]["message"].as_str().unwrap();
    assert!(message.contains("couldn't install it itself: No imported profile can sign this app on this device.") && message.ends_with("Open Apparition on ixPhone to install it."), "{message}");
    assert!(reply["buildId"].as_str().is_some());

    // A device this Mac isn't paired with, a platform that doesn't fit, and no device at all.
    *r.fake.fail.lock().unwrap() = None;
    for (query, ipa_for, expect) in [
        ("device=ghost&install=1", "iPhoneOS", "not paired with a device named 'ghost'"),
        ("device=Living%20Room&install=1", "iPhoneOS", "the build is for iOS, and Living Room runs tvOS"),
        ("install=1", "iPhoneOS", "Name a device to install it from the Mac"),
    ] {
        let (status, reply) = call(&r.app, "POST", &format!("/api/service/v1/deploy?{query}"), HOST, ipa(ipa_for)).await;
        assert_eq!(status, 201, "{reply}");
        assert_eq!(reply["install"]["installed"], false, "{query}");
        assert!(reply["install"]["message"].as_str().unwrap().contains(expect), "{query}: {reply}");
    }
    assert!(r.fake.installs.lock().unwrap().is_empty(), "nothing was installed on a wrong device");

    // An iOS build does run on a visionOS device.
    let (_, reply) = call(&r.app, "POST", "/api/service/v1/deploy?device=ixVision&install=1", HOST, ipa("iPhoneOS")).await;
    assert_eq!(reply["install"]["installed"], true, "{reply}");
}

#[tokio::test]
async fn a_kept_build_is_installed_by_id() {
    let r = rig(true);
    let (_, sent) = call(&r.app, "POST", "/api/service/v1/deploy", HOST, ipa("iPhoneOS")).await;
    let id = sent["buildId"].as_str().unwrap().to_string();
    assert_eq!(call(&r.app, "POST", &format!("/api/service/v1/builds/{id}/install?device=ixPhone"), "wrong", vec![]).await.0, 401);
    let (status, body) = call(&r.app, "POST", &format!("/api/service/v1/builds/{id}/install?device=ixPhone"), HOST, vec![]).await;
    assert_eq!((status, body["installed"]["bundleId"].as_str(), body["installed"]["seconds"].as_f64()), (200, Some("com.example.thing"), Some(1.5)), "{body}");
    assert_eq!(call(&r.app, "POST", &format!("/api/service/v1/builds/{id}/install"), HOST, vec![]).await.0, 400);
    let (status, _) = call(&r.app, "POST", "/api/service/v1/builds/nope/install?device=ixPhone", HOST, vec![]).await;
    assert_eq!(status, 404);
    assert_eq!(call(&r.app, "POST", "/api/service/v1/builds/../x/install?device=ixPhone", HOST, vec![]).await.0 / 100, 4);
    assert_eq!(r.fake.installs.lock().unwrap().len(), 1);
}
