//! The personalized Developer Disk Image, which debugserver (and so JIT) needs mounted on
//! iOS 17 and later. It stays mounted until the device restarts, so this only checks, and
//! mounts it when nothing is: the image's files come from the caller (`dir`), and Apple's TSS
//! server personalizes it for this device, as Xcode does.

use std::path::Path;

use idevice::{RsdService, lockdown::LockdownClient, mobile_image_mounter::ImageMounter, rsd::RsdHandshake, tcp::handle::AdapterHandle};
use rootcause::prelude::*;
use serde_json::{Value, json};

/// The files of Xcode's personalized iOS disk image, as they sit in `dir`.
pub const FILES: [&str; 3] = ["Image.dmg", "Image.dmg.trustcache", "BuildManifest.plist"];

/// Whether a disk image is mounted: CopyDevices lists one entry per mounted image.
async fn mounted(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<bool, Report> {
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

/// `{mounted: "already"}` when an image is mounted, `{mounted: "now"}` after mounting the one in
/// `dir`, or `{mounted: false, needsFiles: true}` when none is and `dir` (None, or missing a file)
/// can't supply it.
pub async fn ensure(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, dir: Option<&Path>) -> Result<Value, Report> {
    if mounted(adapter, handshake).await? {
        return Ok(json!({ "mounted": "already" }));
    }
    let Some(dir) = dir.filter(|dir| FILES.iter().all(|name| dir.join(name).is_file())) else {
        return Ok(json!({ "mounted": false, "needsFiles": true }));
    };
    let image = std::fs::read(dir.join(FILES[0])).context("reading Image.dmg failed")?;
    let trust_cache = std::fs::read(dir.join(FILES[1])).context("reading the trust cache failed")?;
    let manifest = std::fs::read(dir.join(FILES[2])).context("reading BuildManifest.plist failed")?;
    let ecid = chip_id(adapter, handshake).await?;
    let mut mounter = ImageMounter::connect_rsd(adapter, handshake).await.context("image mounter connect failed")?;
    mounter
        .mount_personalized_rsd(adapter, handshake, image, trust_cache, &manifest, None, ecid)
        .await
        .context("mounting the Developer Disk Image failed")?;
    Ok(json!({ "mounted": "now" }))
}
