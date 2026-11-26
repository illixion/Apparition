//! Loads the iOS Developer Disk Image from an Apple-provided Xcode installation.

use std::{io::Read, path::Path};
use sha2::{Digest, Sha384};

use idevice::{RsdService, lockdown::LockdownClient, mobile_image_mounter::ImageMounter, rsd::RsdHandshake, tcp::handle::AdapterHandle};
use rootcause::prelude::*;
use serde_json::{Value, json};

#[cfg(feature = "desktop")]
pub(crate) const BUNDLE_NAME: &str = "Apparition-iOS-DDI-v1.apddi";
pub(crate) const MAX_BUNDLE: usize = 64 * 1024 * 1024;
const MAGIC: &[u8] = b"APDDI1\0";
const LIMITS: [usize; 3] = [60 * 1024 * 1024, 256 * 1024, 3 * 1024 * 1024];
const KEYS: [&str; 2] = ["PersonalizedDMG", "LoadableTrustCache"];

fn bounded_file(path: &Path, limit: usize) -> Result<Vec<u8>, Report> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > limit {
        bail!("Invalid length for {}", path.display());
    }
    Ok(bytes)
}

fn identities(manifest: &plist::Value) -> impl Iterator<Item = &plist::Dictionary> {
    manifest.as_dictionary().and_then(|d| d.get("BuildIdentities"))
        .and_then(plist::Value::as_array).into_iter().flatten()
        .filter_map(|v| v.as_dictionary()?.get("Manifest")?.as_dictionary())
}

fn digest<'a>(entries: &'a plist::Dictionary, key: &str) -> Option<&'a [u8]> {
    entries.get(key)?.as_dictionary()?.get("Digest")?.as_data()
}

fn validate(image: &[u8], trust: &[u8], manifest: &[u8]) -> Result<(), Report> {
    let manifest: plist::Value = plist::from_bytes(manifest).context("Invalid Xcode DDI manifest")?;
    let image_hash = Sha384::digest(image);
    let trust_hash = Sha384::digest(trust);
    if !identities(&manifest).any(|m| {
        digest(m, KEYS[0]) == Some(image_hash.as_ref()) && digest(m, KEYS[1]) == Some(trust_hash.as_ref())
    }) {
        bail!("The Developer Disk Image and trust cache don't match the Xcode build manifest. Send the image from the Mac again.");
    }
    Ok(())
}

fn unpack(bundle: &[u8]) -> Result<[&[u8]; 3], Report> {
    let Some(mut rest) = bundle.strip_prefix(MAGIC).filter(|_| bundle.len() <= MAX_BUNDLE) else {
        bail!("Invalid Apparition Developer Disk Image bundle");
    };
    let mut parts = [&[][..]; 3];
    for (part, limit) in parts.iter_mut().zip(LIMITS) {
        let Some((length, remaining)) = rest.split_first_chunk::<4>() else { bail!("Truncated Developer Disk Image bundle"); };
        let length = u32::from_le_bytes(*length) as usize;
        if length == 0 || length > limit || length > remaining.len() {
            bail!("Invalid file length in the Developer Disk Image bundle");
        }
        (*part, rest) = remaining.split_at(length);
    }
    if !rest.is_empty() { bail!("Unexpected data in the Developer Disk Image bundle"); }
    validate(parts[0], parts[1], parts[2])?;
    Ok(parts)
}

#[cfg(all(feature = "desktop", target_os = "macos"))]
pub(crate) fn from_xcode() -> Result<Vec<u8>, Report> {
    const HELP: &str = "Install or update Xcode from the Mac App Store, open it to install its components, or run xcodebuild -runFirstLaunch -checkForNewerComponents on the Mac.";
    let output = std::process::Command::new("/usr/bin/xcrun")
        .args(["devicectl", "list", "preferredDDI", "--platform", "iOS", "--timeout", "15", "--json-output", "-"])
        .output().context(HELP)?;
    if !output.status.success() { bail!("Could not locate Xcode's iOS Developer Disk Image. {HELP}"); }
    let info: Value = serde_json::from_slice(&output.stdout).context(HELP)?;
    let path = info["result"]["platforms"]["iOS"].as_array().into_iter().flatten()
        .find(|entry| entry["ddiMetadata"]["isUsable"].as_bool() == Some(true))
        .and_then(|entry| entry["hostDDI"].as_str())
        .and_then(|url| reqwest::Url::parse(url).ok())
        .and_then(|url| url.to_file_path().ok())
        .ok_or_else(|| report!("Xcode has no usable iOS Developer Disk Image. {HELP}"))?;
    let restore = path.join("Restore").canonicalize().context(HELP)?;
    if !restore.starts_with("/Library/Developer") {
        bail!("Xcode's iOS DDI is outside /Library/Developer. {HELP}");
    }
    let manifest_path = restore.join("BuildManifest.plist").canonicalize().context(HELP)?;
    if !manifest_path.starts_with(&restore) { bail!("Xcode DDI manifest is outside its Restore folder"); }
    let manifest = bounded_file(&manifest_path, LIMITS[2])?;
    let decoded: plist::Value = plist::from_bytes(&manifest)?;
    // Older hardware identities use SHA-1 for the same physical payloads. Select
    // the SHA-384 paths and read each file once even if validation fails.
    let entry = identities(&decoded).find(|m| {
        KEYS.iter().all(|key| digest(m, key).is_some_and(|d| d.len() == 48))
    }).ok_or_else(|| report!("Xcode's iOS DDI lacks a PersonalizedDMG and trust cache. {HELP}"))?;
    let extract = |key, limit| -> Result<Vec<u8>, Report> {
        let relative = entry.get(key).and_then(plist::Value::as_dictionary)
            .and_then(|e| e.get("Info")).and_then(plist::Value::as_dictionary)
            .and_then(|i| i.get("Path")).and_then(plist::Value::as_string)
            .ok_or_else(|| report!("Xcode's DDI manifest is missing the path for {key}. {HELP}"))?;
        let path = restore.join(relative).canonicalize().context(HELP)?;
        if !path.starts_with(&restore) { bail!("Xcode DDI payload is outside its Restore folder"); }
        bounded_file(&path, limit)
    };
    let image = extract(KEYS[0], LIMITS[0])?;
    let trust = extract(KEYS[1], LIMITS[1])?;
    validate(&image, &trust, &manifest)?;
    let mut output = Vec::from(MAGIC);
    for file in [image, trust, manifest] {
        output.extend_from_slice(&(file.len() as u32).to_le_bytes());
        output.extend(file);
    }
    Ok(output)
}

#[cfg(feature = "desktop")]
pub(crate) async fn source(companion_dir: &Path) -> Result<Vec<u8>, Report> {
    #[cfg(target_os = "macos")]
    {
        let _ = companion_dir;
        return Ok(tokio::task::spawn_blocking(from_xcode).await??);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let reply = crate::companion_client::developer(companion_dir, "GET", "/ios-ddi", None, crate::companion_client::Upload::None).await
            .context("Pair this computer with a Mac running Apparition and allow developer access. Install Xcode on that Mac.")?;
        if reply.status != 200 {
            let hint = if reply.status == 503 { String::from_utf8_lossy(&reply.body).into_owned() } else { format!("HTTP {}", reply.status) };
            bail!("Couldn't get the Developer Disk Image from the Mac: {hint}. Update Apparition on the Mac and install Xcode's device support.");
        }
        unpack(&reply.body)?;
        Ok(reply.body)
    }
}

pub(crate) async fn mounted(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<bool, Report> {
    let mut mounter = ImageMounter::connect_rsd(adapter, handshake).await.context("image mounter connect failed")?;
    let images = mounter.copy_devices().await.context("listing mounted images failed")?;
    Ok(!images.is_empty())
}

/// The device's ECID, which TSS signs the image for: from the tunnel's own properties, else lockdown.
async fn chip_id(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<u64, Report> {
    if let Some(id) = handshake.properties.get("UniqueChipID").and_then(plist::Value::as_unsigned_integer) {
        return Ok(id);
    }
    let mut lockdown = LockdownClient::connect_rsd(adapter, handshake).await.context("lockdown connect failed")?;
    lockdown
        .get_value(Some("UniqueChipID"), None)
        .await
        .context("reading the device's ECID failed")?
        .as_unsigned_integer()
        .ok_or_else(|| report!("the device's ECID isn't a number"))
}

pub(crate) async fn mount(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, bundle: &[u8]) -> Result<(), Report> {
    let [image, trust, manifest] = unpack(bundle)?;
    let ecid = chip_id(adapter, handshake).await?;
    let mut mounter = ImageMounter::connect_rsd(adapter, handshake).await.context("image mounter connect failed")?;
    mounter
        .mount_personalized_rsd(adapter, handshake, image.to_vec(), trust.to_vec(), manifest, None, ecid)
        .await
        .context("mounting the Developer Disk Image failed")?;
    Ok(())
}

pub async fn ensure(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, bundle: Option<&Path>) -> Result<Value, Report> {
    if mounted(adapter, handshake).await? {
        return Ok(json!({ "mounted": "already" }));
    }
    let Some(bundle) = bundle.filter(|path| path.is_file()) else {
        return Ok(json!({ "mounted": false, "needsFiles": true }));
    };
    let bytes = bounded_file(bundle, MAX_BUNDLE).context("reading the Developer Disk Image from the Mac failed")?;
    mount(adapter, handshake, &bytes).await?;
    Ok(json!({ "mounted": "now" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(image: &[u8], trust: &[u8], manifest: &[u8]) -> Vec<u8> {
        let mut bin = MAGIC.to_vec();
        for part in [image, trust, manifest] {
            bin.extend_from_slice(&(part.len() as u32).to_le_bytes());
            bin.extend_from_slice(part);
        }
        bin
    }

    #[test]
    fn ddi_bundle_checks_contents_and_limits() {
        let image = b"developer image fixture";
        let trust = b"trust cache fixture";
        let entry = |data: &[u8]| plist::Value::Dictionary(
            [("Digest".to_owned(), plist::Value::Data(Sha384::digest(data).to_vec()))].into_iter().collect()
        );
        let assets = plist::Value::Dictionary([
            (KEYS[0].to_owned(), entry(image)),
            (KEYS[1].to_owned(), entry(trust)),
        ].into_iter().collect());
        let identity = plist::Value::Dictionary([("Manifest".to_owned(), assets)].into_iter().collect());
        let root = plist::Value::Dictionary([
            ("BuildIdentities".to_owned(), plist::Value::Array(vec![identity]))
        ].into_iter().collect());
        let mut manifest = Vec::new();
        plist::to_writer_binary(&mut manifest, &root).unwrap();

        let good = package(image, trust, &manifest);
        assert_eq!(unpack(&good).unwrap(), [image.as_slice(), trust.as_slice(), manifest.as_slice()]);
        assert!(unpack(&package(b"modified image", trust, &manifest)).is_err());
        assert!(unpack(&package(image, b"modified trust", &manifest)).is_err());
        assert!(unpack(&package(image, trust, b"garbled plist")).is_err());
        assert!(unpack(&good[..good.len() - 1]).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(unpack(&trailing).is_err());
        let mut oversized = good;
        oversized[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&((LIMITS[0] + 1) as u32).to_le_bytes());
        assert!(unpack(&oversized).is_err());
    }
}
