//! Deploys and delivery end to end, with a stand-in `apparition` that keeps a real (tiny) IPA:
//! the page's deploy, the OTA links installd can fetch without a session, and the AltStore feed
//! whose download builds the commit it names.

use apparition_service::{debug::Fixed, server::{Settings, router}};
use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use std::{fs, io::Write, path::{Path, PathBuf}, process::Command, sync::Arc, time::{Duration, Instant}};
use tower::ServiceExt;

const BUNDLE: &str = "com.example.app";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args)
        .env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A tiny but real IPA: one app with an Info.plist and an executable.
fn make_ipa(path: &Path) {
    let plist = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{BUNDLE}</string>
<key>CFBundleDisplayName</key><string>Example</string>
<key>CFBundleName</key><string>Example</string>
<key>CFBundleExecutable</key><string>Example</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>7</string>
<key>MinimumOSVersion</key><string>17.0</string>
</dict></plist>"#);
    let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("Payload/Example.app/Info.plist", options).unwrap();
    zip.write_all(plist.as_bytes()).unwrap();
    zip.start_file("Payload/Example.app/Example", options).unwrap();
    zip.write_all(b"not really a Mach-O").unwrap();
    zip.finish().unwrap();
}

/// `apparition build`: `--plan` resolves the row, a build keeps the template IPA under a manifest
/// that names the commit it was built from.
fn fake_cli(ipa: &Path) -> String {
    format!(r#"#!/bin/sh
set -e
[ "$1" = build ] || exit 2
if [ "$2" = --plan ]; then
  echo "{{\"platform\":\"$PLATFORM\",\"bundleId\":\"{BUNDLE}\",\"scheme\":\"App\"}}"
  exit 0
fi
echo "args: $*"
app=$(pwd -P)
id=$(date +%s)$$
build="$(git rev-parse --short HEAD)"
dir="$APPARITION_BUILD_DIR/$id"; mkdir -p "$dir"
cp "{ipa}" "$dir/tmp"
sha=$(shasum -a 256 "$dir/tmp" | cut -d' ' -f1); bytes=$(wc -c < "$dir/tmp" | tr -d ' ')
mv "$dir/tmp" "$dir/$sha.ipa"
cat > "$dir/manifest.json" <<EOF
{{"format":"apparition-build/1","buildId":"$id","createdAt":"2026-11-04T12:00:00.000Z","project":"$app","platform":"iOS",
 "app":{{"bundleId":"{BUNDLE}","name":"Example","version":"1.0","build":"$build"}},
 "artifact":{{"kind":"ipa","sha256":"$sha","bytes":$bytes,"file":"$sha.ipa"}},
 "source":{{"stable":true,"before":{{"fingerprint":"x","anyDirty":false,"sources":[{{"path":"$app","commit":"$(git rev-parse HEAD)","branch":"main","dirty":false}}]}}}}}}
EOF
echo "  Build ID:    $id"
"#, ipa = ipa.display())
}

struct Rig {
    app: Router,
    repo: PathBuf,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(dir.path()).unwrap();
    let repo = root.join("Projects").join("App");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("main.swift"), "print(1)").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let ipa = root.join("template.ipa");
    make_ipa(&ipa);
    let cli = root.join("apparition");
    fs::write(&cli, fake_cli(&ipa)).unwrap();
    Command::new("chmod").arg("755").arg(&cli).status().unwrap();
    fs::write(root.join("config.json"), json!({
        "buildManifestDir": "builds", "apparitionCli": cli, "bundlePrefix": "com.example",
        "altSource": { "enabled": true, "holdSeconds": 30 },
        "repos": [{ "name": "App", "path": repo, "branches": ["main"], "platforms": [
            { "platform": "iOS", "bundleId": BUNDLE, "device": "ixPhone" },
        ] }],
    }).to_string()).unwrap();
    let app = router(Settings {
        key_dir: root.join("k"), browser: Arc::new(Fixed(vec![])),
        ..Settings::new(root.join("config.json"), root.join("data"), root.join("p"))
    }).unwrap();
    Rig { app, repo, _dir: dir }
}

/// `host` decides who is asking: localhost is the signed-in page, anything else has no session.
async fn call(app: &Router, method: &str, uri: &str, host: &str, body: Value) -> (u16, Vec<u8>) {
    let res = app.clone().oneshot(Request::builder().method(method).uri(uri).header("host", host)
        .header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = res.status().as_u16();
    (status, axum::body::to_bytes(res.into_body(), 1 << 24).await.unwrap().to_vec())
}

async fn page(app: &Router, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let (status, bytes) = call(app, method, uri, "localhost", body).await;
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn finished(app: &Router, id: u64) -> Value {
    let start = Instant::now();
    loop {
        let (_, state) = page(app, "GET", "/api/state", Value::Null).await;
        let job = state["builds"].as_array().unwrap().iter().find(|b| b["id"] == json!(id)).cloned().unwrap();
        if !["queued", "running"].contains(&job["status"].as_str().unwrap()) {
            return job;
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the build didn't finish: {job}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.find('/').map_or(String::new(), |i| rest[i..].to_string())
}

#[tokio::test]
async fn a_deploy_that_only_builds_is_kept_for_ota_and_noted_as_a_guess() {
    let r = rig();
    let (status, started) = page(&r.app, "POST", "/api/build", json!({ "repo": "App", "targets": [{ "device": "ixPhone" }], "install": "none" })).await;
    assert_eq!(status, 200, "{started}");
    let id = started["jobs"][0]["id"].as_u64().unwrap();
    assert_eq!(started["jobs"][0]["installVia"], "none");
    let job = finished(&r.app, id).await;
    let (_, log) = page(&r.app, "GET", &format!("/api/builds/{id}/log"), Value::Null).await;
    assert_eq!(job["status"], "success", "{job:#}\n{}", log["chunk"].as_str().unwrap_or(""));
    // Built in the checkout, as the branch is the one it has out; nothing was deployed.
    let chunk = log["chunk"].as_str().unwrap();
    assert!(chunk.contains("in the checkout") && chunk.contains("--no-deploy"), "{chunk}");

    let (_, state) = page(&r.app, "GET", "/api/state", Value::Null).await;
    let entry = &state["installs"]["ixPhone"][BUNDLE];
    assert_eq!(entry["confirmed"], false, "an OTA-only build is a guess: {entry}");
    assert_eq!(entry["commit"], git(&r.repo, &["rev-parse", "--short", "HEAD"]));
    let ota = state["ota"][format!("{BUNDLE}|iOS")].clone();
    assert_eq!(ota["name"], "Example");

    // installd has no cookie: the page hands out signed links, and only those work.
    let slug = ota["slug"].as_str().unwrap();
    let (_, link) = page(&r.app, "GET", &format!("/api/ota/{slug}/link"), Value::Null).await;
    let manifest_path = path_of(link["manifestUrl"].as_str().unwrap());
    let (status, _) = call(&r.app, "GET", &format!("/ota/{slug}/manifest.plist"), "store.example", Value::Null).await;
    assert_eq!(status, 302, "an unsigned request is sent to sign in");
    let (status, plist) = call(&r.app, "GET", &manifest_path, "store.example", Value::Null).await;
    assert_eq!(status, 200);
    let plist = String::from_utf8(plist).unwrap();
    assert!(plist.contains(BUNDLE) && plist.contains(&format!("/ota/{slug}.ipa?t=")), "{plist}");
    let ipa_url = plist.split("<string>").find(|s| s.starts_with("http")).unwrap().split("</string>").next().unwrap().replace("&amp;", "&");
    let (status, ipa) = call(&r.app, "GET", &path_of(&ipa_url), "store.example", Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(&ipa[..2], b"PK");
}

#[tokio::test]
async fn an_altstore_download_builds_the_commit_it_names_and_answers_with_the_ipa() {
    let r = rig();
    let (_, links) = page(&r.app, "GET", "/api/altsource", Value::Null).await;
    assert_eq!(links["enabled"], true, "{links}");
    let feed_url = links["combined"]["url"].as_str().unwrap().to_string();
    let (status, _) = call(&r.app, "GET", "/altsource/source.json?k=wrong", "store.example", Value::Null).await;
    assert_eq!(status, 403);
    let (status, feed) = call(&r.app, "GET", &path_of(&feed_url), "store.example", Value::Null).await;
    assert_eq!(status, 200);
    let feed: Value = serde_json::from_slice(&feed).unwrap();
    let app = &feed["apps"][0];
    assert_eq!(app["bundleIdentifier"], BUNDLE, "{feed:#}");
    let download = app["versions"][0]["downloadURL"].as_str().or(app["downloadURL"].as_str()).unwrap_or_else(|| panic!("no download link in {app:#}"));
    let head = git(&r.repo, &["log", "-1", "--format=%h"]);
    assert!(path_of(download).contains(&head), "the listing is at the branch head {head}: {download}");

    // Nothing is built yet: the request waits for the build, then is answered with the IPA.
    let (status, ipa) = call(&r.app, "GET", &path_of(download), "store.example", Value::Null).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&ipa));
    assert_eq!(&ipa[..2], b"PK");
    // It built in a clean clone at that commit, and is not noted as installed anywhere.
    let (_, state) = page(&r.app, "GET", "/api/state", Value::Null).await;
    assert_eq!(state["installs"], json!({}));
    let job = state["builds"][0].clone();
    assert_eq!(job["kind"], "altsource");
    assert_eq!(job["status"], "success", "{job:#}");
    // Asked again, it is the kept copy, with no new build.
    let (status, again) = call(&r.app, "GET", &path_of(download), "store.example", Value::Null).await;
    assert_eq!((status, again.len()), (200, ipa.len()));
    let (_, state) = page(&r.app, "GET", "/api/state", Value::Null).await;
    assert_eq!(state["builds"].as_array().unwrap().len(), 1);
}
