//! Builds that arrive from another computer: an IPA made elsewhere (xtool on Linux or Windows,
//! Xcode on another Mac) sent here to be installed. It is kept like a build made on this Mac
//! (`artifacts.rs`, the `apparition-build/1` manifest `store/lib/artifacts.js` writes), so the
//! device's Apparition lists it and installs it with its own signing, as it does a Store build.
//! Nothing here signs or runs the upload.

use std::{fs, io::Read, path::{Path, PathBuf}};

use futures_util::StreamExt;
use ring::digest;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::artifacts::valid_build_id;

/// The largest IPA accepted.
pub const MAX_DEPLOY: u64 = 4 << 30;
/// The largest Info.plist read from it.
const MAX_INFO_PLIST: u64 = 1 << 20;

pub struct Refused {
    pub status: u16,
    pub error: String,
}

fn refused(status: u16, error: impl Into<String>) -> Refused {
    Refused { status, error: error.into() }
}

/// Who sent it and how they named it, for the manifest's `origin`.
pub struct Origin {
    /// The paired computer's name, as the companion channel labelled the request.
    pub computer: Option<String>,
    /// The file name it had there.
    pub file: Option<String>,
    /// The device it is meant for, if the sender named one.
    pub device: Option<String>,
}

/// Streams `body` into the build store as a kept build and returns its manifest.
pub async fn receive(store: &Path, body: axum::body::Body, origin: Origin) -> Result<Value, Refused> {
    fs::create_dir_all(store).map_err(|e| refused(500, format!("can't create the build store: {e}")))?;
    let stage = store.join(format!(".incoming-{}", crate::auth::new_token()));
    let result = receive_into(store, &stage, body, origin).await;
    let _ = fs::remove_dir_all(&stage);
    result
}

async fn receive_into(store: &Path, stage: &Path, body: axum::body::Body, origin: Origin) -> Result<Value, Refused> {
    {
        #[allow(unused_mut)]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(stage).map_err(|e| refused(500, e.to_string()))?;
    }
    let staged = stage.join("artifact");
    let mut file = tokio::fs::File::create(&staged).await.map_err(|e| refused(500, e.to_string()))?;
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut bytes = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| refused(400, format!("the upload broke off: {e}")))?;
        bytes += chunk.len() as u64;
        if bytes > MAX_DEPLOY {
            return Err(refused(413, format!("the app is larger than {} GB", MAX_DEPLOY >> 30)));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await.map_err(|e| refused(500, e.to_string()))?;
    }
    file.flush().await.map_err(|e| refused(500, e.to_string()))?;
    drop(file);
    if bytes == 0 {
        return Err(refused(400, "send the .ipa as the request body"));
    }
    let sha: String = hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let (stage, store, staged) = (stage.to_path_buf(), store.to_path_buf(), staged.clone());
    tokio::task::spawn_blocking(move || publish(&store, &stage, &staged, &sha, bytes, origin))
        .await
        .map_err(|_| refused(500, "internal error"))?
}

/// What the IPA's app says it is.
struct App {
    info: plist::Dictionary,
    bundle_name: String,
}

/// The one app in `Payload/`, and its Info.plist.
fn read_app(ipa: &Path) -> Result<App, Refused> {
    let not_ipa = || refused(400, "that isn't an .ipa: it needs one Payload/<Name>.app with an Info.plist");
    let mut zip = zip::ZipArchive::new(fs::File::open(ipa).map_err(|e| refused(500, e.to_string()))?).map_err(|_| not_ipa())?;
    let mut found: Option<(usize, String)> = None;
    for index in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(index) else { continue };
        let Ok(name) = entry.name().map(|n| n.into_owned()) else { continue };
        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() == 3 && parts[0] == "Payload" && parts[1].ends_with(".app") && parts[1].len() > 4 && parts[2] == "Info.plist" {
            if found.is_some() {
                return Err(refused(400, "the .ipa holds more than one app"));
            }
            found = Some((index, parts[1].to_string()));
        }
    }
    let (index, bundle_name) = found.ok_or_else(not_ipa)?;
    let entry = zip.by_index(index).map_err(|_| not_ipa())?;
    if entry.size() > MAX_INFO_PLIST {
        return Err(refused(400, "the app's Info.plist is too large"));
    }
    let mut data = Vec::new();
    entry.take(MAX_INFO_PLIST).read_to_end(&mut data).map_err(|_| not_ipa())?;
    let info = plist::Value::from_reader(std::io::Cursor::new(data)).ok().and_then(|v| v.into_dictionary()).ok_or_else(not_ipa)?;
    Ok(App { info, bundle_name })
}

/// The platform an app is built for, as the Store names it (`iOS`, `visionOS`, `tvOS`).
fn platform(info: &plist::Dictionary) -> Result<&'static str, Refused> {
    let supported: Vec<&str> = info.get("CFBundleSupportedPlatforms").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_string()).collect()).unwrap_or_default();
    for platform in &supported {
        match *platform {
            "iPhoneOS" => return Ok("iOS"),
            "XROS" => return Ok("visionOS"),
            "AppleTVOS" => return Ok("tvOS"),
            "iPhoneSimulator" | "XRSimulator" | "AppleTVSimulator" => {
                return Err(refused(400, "that's a simulator build; build it for a device"));
            }
            "MacOSX" => return Err(refused(400, "a Mac app can't be installed from an .ipa here")),
            _ => {}
        }
    }
    let families: Vec<i64> = info.get("UIDeviceFamily").and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_signed_integer()).collect()).unwrap_or_default();
    Ok(if families.contains(&7) { "visionOS" } else if families.contains(&3) { "tvOS" } else { "iOS" })
}

fn text(info: &plist::Dictionary, key: &str) -> Option<String> {
    info.get(key).and_then(|v| v.as_string()).map(|s| s.chars().filter(|c| !c.is_control()).take(200).collect())
}

/// A label from the sender: kept short and printable.
fn label(value: Option<String>) -> Value {
    value.map(|v| v.chars().filter(|c| !c.is_control()).take(120).collect::<String>()).filter(|v| !v.is_empty()).map_or(Value::Null, Value::String)
}

fn publish(store: &Path, stage: &Path, staged: &Path, sha: &str, bytes: u64, origin: Origin) -> Result<Value, Refused> {
    let app = read_app(staged)?;
    let platform = platform(&app.info)?;
    let bundle_id = text(&app.info, "CFBundleIdentifier").filter(|b| crate::debug::valid_bundle_id(b))
        .ok_or_else(|| refused(400, "the app's Info.plist has no valid CFBundleIdentifier"))?;
    let now = crate::auth::system_ms();
    let build_id = format!("upload-{}-{}", now, &crate::auth::new_token().replace(['_', '-'], "")[..8]);
    debug_assert!(valid_build_id(&build_id));
    let name = format!("{sha}.ipa");
    fs::rename(staged, stage.join(&name)).map_err(|e| refused(500, e.to_string()))?;
    let manifest = json!({
        "format": "apparition-build/1", "buildId": build_id, "keyId": null, "createdAt": iso(now),
        "project": null, "platform": platform, "configuration": null,
        "app": {
            "bundleId": bundle_id, "originalBundleId": bundle_id,
            "name": text(&app.info, "CFBundleDisplayName").or_else(|| text(&app.info, "CFBundleName")),
            "bundleName": app.bundle_name, "minOSVersion": text(&app.info, "MinimumOSVersion"),
            "version": text(&app.info, "CFBundleShortVersionString"), "build": text(&app.info, "CFBundleVersion"),
        },
        "artifact": { "sha256": sha, "bytes": bytes, "kind": "ipa", "file": name },
        "binaries": {}, "symbols": null, "source": null,
        "origin": { "kind": "upload", "computer": label(origin.computer), "file": label(origin.file), "device": label(origin.device) },
    });
    {
        use std::io::Write;
        let mut file = crate::private_file().write(true).create_new(true).open(stage.join("manifest.json")).map_err(|e| refused(500, e.to_string()))?;
        file.write_all(serde_json::to_string_pretty(&manifest).unwrap_or_default().as_bytes()).map_err(|e| refused(500, e.to_string()))?;
    }
    let destination: PathBuf = store.join(&build_id);
    if destination.exists() {
        return Err(refused(500, "refusing to replace an existing build manifest"));
    }
    fs::rename(stage, &destination).map_err(|e| refused(500, e.to_string()))?;
    Ok(manifest)
}

/// Milliseconds since 1970 as `2026-11-04T09:00:00.000Z`, the way Node's `toISOString` writes it.
fn iso(ms: u64) -> String {
    let (secs, millis) = (ms / 1000, ms % 1000);
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso_matches_node() {
        assert_eq!(super::iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(super::iso(1_762_247_045_123), "2025-11-04T09:04:05.123Z");
        assert_eq!(super::iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
