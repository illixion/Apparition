//! Updates end to end, with a stand-in `apparition` and fake devices: a package the app builds
//! against moves, the Store sees the app behind on both devices that run it, builds it once in a
//! clean clone and installs that build on both, and the app is current again.

use apparition_service::{debug::Fixed, devices::{App, Device, Devices, Fut, Installed}, server::{Settings, router}};
use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use std::{fs, path::{Path, PathBuf}, process::Command, sync::{Arc, Mutex}, time::{Duration, Instant}};
use tower::ServiceExt;

const HOST: &str = "host-token-host-token-host-token-host-token";
const BUNDLE: &str = "com.example.app";

#[derive(Default)]
struct Fake {
    signed: Mutex<Vec<(String, PathBuf)>>,
}

impl Devices for Fake {
    fn list(&self) -> Fut<'_, Vec<Device>> {
        Box::pin(async { Ok(vec![
            Device { udid: "PHONE-1".into(), name: "ixPhone".into(), platform: "iOS".into(), model: String::new() },
            Device { udid: "PAD-2".into(), name: "YPad".into(), platform: "iOS".into(), model: String::new() },
        ]) })
    }
    fn apps(&self, _device: String) -> Fut<'_, Vec<App>> {
        Box::pin(async { Ok(vec![App { bundle_id: BUNDLE.into(), name: "App".into(), version: Some("1.0".into()), build: Some("first-build".into()) }]) })
    }
    fn install(&self, _device: String, _ipa: PathBuf) -> Fut<'_, Installed> {
        Box::pin(async { Err("an update must not be signed again".to_string()) })
    }
    fn install_signed(&self, device: String, ipa: PathBuf) -> Fut<'_, Installed> {
        Box::pin(async move {
            self.signed.lock().unwrap().push((device.clone(), ipa));
            Ok(Installed { bundle_id: BUNDLE.into(), device, seconds: 0.1, queued: false, signed_with: None })
        })
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args)
        .env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(dir: &Path, file: &str, text: &str) {
    let path = dir.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", &format!("change {file}")]);
}

/// `apparition build`, as far as the queue sees it: `--plan` resolves the iPad row to the iOS
/// build, and a build keeps a manifest of the clone it ran in.
const FAKE_CLI: &str = r#"#!/bin/sh
set -e
[ "$1" = build ] || exit 2
if [ "$2" = --plan ]; then
  case "$PLATFORM" in iPad) p=iOS; d=YPad ;; *) p=$PLATFORM; d=ixPhone ;; esac
  echo "{\"platform\":\"$p\",\"bundleId\":\"com.example.app\",\"scheme\":\"App\",\"device\":\"$d\",\"profileDevices\":[\"$d\"]}"
  exit 0
fi
app=$(pwd -P); kit=$(cd ../Kit && pwd -P)
id=$(date +%s)$$
build="$(git rev-parse --short HEAD)-$(git -C ../Kit rev-parse --short=6 HEAD)"
dir="$APPARITION_BUILD_DIR/$id"; mkdir -p "$dir"
printf 'ipa %s' "$build" > "$dir/tmp"
sha=$(shasum -a 256 "$dir/tmp" | cut -d' ' -f1); bytes=$(wc -c < "$dir/tmp" | tr -d ' ')
mv "$dir/tmp" "$dir/$sha.ipa"
cat > "$dir/manifest.json" <<EOF
{"format":"apparition-build/1","buildId":"$id","createdAt":"2026-11-04T12:00:00.000Z","project":"$app","platform":"iOS",
 "app":{"bundleId":"com.example.app","build":"$build"},
 "artifact":{"kind":"ipa","sha256":"$sha","bytes":$bytes,"file":"$sha.ipa"},
 "source":{"before":{"sources":[
   {"path":"$app","commit":"$(git rev-parse HEAD)","branch":"$(git rev-parse --abbrev-ref HEAD)","dirty":false},
   {"path":"$kit","commit":"$(git -C ../Kit rev-parse HEAD)","branch":"main","dirty":false}]}}}
EOF
echo "==> Building for $PLATFORM"
echo "  Build ID:    $id"
"#;

struct Rig {
    app: Router,
    fake: Arc<Fake>,
    kit: PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let (projects, data, builds) = (root.join("Projects"), root.join("data"), root.join("builds"));
    let (app, kit) = (projects.join("App"), projects.join("Kit"));
    for (repo, manifest) in [(&app, "let p = Package(dependencies: [.package(path: \"../Kit\")])"), (&kit, "let p = Package(targets: [.target(name: \"Kit\")])")] {
        fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q", "-b", "main"]);
        commit(repo, "Package.swift", manifest);
    }
    // The build on both devices now: the app and Kit as they are.
    let first = builds.join("first");
    fs::create_dir_all(&first).unwrap();
    fs::write(first.join("x.ipa"), "first").unwrap();
    fs::write(first.join("manifest.json"), json!({
        "format": "apparition-build/1", "buildId": "first", "createdAt": "2026-11-04T10:00:00.000Z", "project": app, "platform": "iOS",
        "app": { "bundleId": BUNDLE, "build": "first-build" },
        "artifact": { "kind": "ipa", "sha256": "0".repeat(64), "bytes": 5, "file": "x.ipa" },
        "source": { "before": { "sources": [
            { "path": app, "commit": git(&app, &["rev-parse", "HEAD"]), "branch": "main", "dirty": false },
            { "path": kit, "commit": git(&kit, &["rev-parse", "HEAD"]), "branch": "main", "dirty": false },
        ] } },
    }).to_string()).unwrap();
    let cli = root.join("apparition");
    fs::write(&cli, FAKE_CLI).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::PermissionsExt::set_mode(&mut fs::metadata(&cli).unwrap().permissions(), 0o755);
    Command::new("chmod").arg("755").arg(&cli).status().unwrap();
    fs::write(root.join("config.json"), json!({
        "buildManifestDir": "builds", "apparitionCli": cli,
        "repos": [{ "name": "App", "path": app, "branches": ["main"], "platforms": [
            { "platform": "iOS", "bundleId": BUNDLE, "device": "ixPhone" },
            { "platform": "iPad", "bundleId": BUNDLE, "device": "YPad" },
        ] }],
    }).to_string()).unwrap();
    let fake = Arc::new(Fake::default());
    let app = router(Settings {
        host_token: Some(HOST.into()), key_dir: root.join("k"), browser: Arc::new(Fixed(vec![])),
        devices: Some(fake.clone() as Arc<dyn Devices>),
        ..Settings::new(root.join("config.json"), data, root.join("p"))
    }).unwrap();
    Rig { app, fake, kit, _dir: dir }
}

async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let res = app.clone().oneshot(Request::builder().method(method).uri(uri).header("host", "localhost")
        .header("authorization", format!("Bearer {HOST}")).header("content-type", "application/json")
        .body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 22).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn statuses(report: &Value) -> Vec<(String, String)> {
    report["rows"].as_array().unwrap().iter().map(|r| (r["device"].as_str().unwrap().to_string(), r["status"].as_str().unwrap().to_string())).collect()
}

#[tokio::test]
async fn a_package_change_rebuilds_the_app_once_for_both_devices() {
    let r = rig();
    let (status, report) = call(&r.app, "POST", "/api/service/v1/updates/check", Value::Null).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(statuses(&report), [("ixPhone".into(), "current".into()), ("YPad".into(), "current".into())], "{report:#}");

    commit(&r.kit, "Tests/KitTests/t.swift", "only a test");
    let (_, report) = call(&r.app, "GET", "/api/service/v1/updates", Value::Null).await;
    assert_eq!(report["rows"][0]["status"], "current", "a test-only commit changes nothing: {report:#}");

    commit(&r.kit, "Sources/Kit/Interpolator.swift", "frames");
    let (_, report) = call(&r.app, "GET", "/api/service/v1/updates", Value::Null).await;
    assert_eq!(statuses(&report), [("ixPhone".into(), "behind".into()), ("YPad".into(), "behind".into())], "{report:#}");
    assert_eq!(report["rows"][0]["reasons"][0]["source"], "Kit");
    let groups = report["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "the iPhone and iPad rows are one iOS build: {report:#}");
    assert_eq!(groups[0]["devices"], json!(["ixPhone", "YPad"]));

    let (status, started) = call(&r.app, "POST", "/api/service/v1/updates/run", json!({})).await;
    assert_eq!(status, 200, "{started}");
    let jobs = started["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let id = jobs[0]["id"].as_u64().unwrap();
    assert!(id >= 1_000_000);
    // Asked again while it runs: nothing more is queued.
    let (_, again) = call(&r.app, "POST", "/api/service/v1/updates/run", json!({})).await;
    assert_eq!(again["jobs"], json!([]));

    let start = Instant::now();
    let job = loop {
        let (_, report) = call(&r.app, "GET", "/api/service/v1/updates", Value::Null).await;
        let job = report["builds"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).cloned().unwrap();
        if !["queued", "running"].contains(&job["status"].as_str().unwrap()) {
            break job;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the build didn't finish: {job}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let (_, log) = call(&r.app, "GET", &format!("/api/service/v1/builds/{id}/log"), Value::Null).await;
    assert_eq!(job["status"], "success", "{job:#}\n{}", log["chunk"].as_str().unwrap_or(""));
    assert!(log["chunk"].as_str().unwrap().contains("Kit: change Sources/Kit/Interpolator.swift"), "{log}");
    let signed = r.fake.signed.lock().unwrap().clone();
    assert_eq!(signed.iter().map(|(d, _)| d.as_str()).collect::<Vec<_>>(), ["PHONE-1", "PAD-2"], "one build, installed on both");
    assert_eq!(signed[0].1, signed[1].1);
    assert_eq!(job["installs"].as_array().unwrap().iter().filter(|i| i["ok"] == json!(true)).count(), 2);

    // What it installed is what both devices now run, and that build is current.
    let (_, report) = call(&r.app, "GET", "/api/service/v1/updates", Value::Null).await;
    assert_eq!(statuses(&report), [("ixPhone".into(), "current".into()), ("YPad".into(), "current".into())], "{report:#}");
    assert_eq!(report["rows"][0]["installed"]["buildId"], job["buildId"]);

    // The page's state lists the queue's build.
    let (_, state) = call(&r.app, "GET", "/api/service/v1/state", Value::Null).await;
    assert_eq!(state["builds"][0]["id"], json!(id));
}
