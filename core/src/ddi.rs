//! Where the iOS Developer Disk Image comes from. On a Mac: the Xcode on it (which keeps Apple's
//! image in `/Library/Developer/DeveloperDiskImages` once it has finished its first launch). On
//! a PC: the Mac it is paired with. Then a copy downloaded earlier, and last a download from GitHub,
//! which only happens when the caller's `consent` says yes: the person is asked, since nothing
//! else here reaches that server.

use std::{io::Read, path::{Path, PathBuf}};
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
/// The files of Xcode's personalized iOS image, as a folder holds them and as the download serves them.
pub const FILES: [&str; 3] = ["Image.dmg", "Image.dmg.trustcache", "BuildManifest.plist"];
/// Xcode's image as a public mirror keeps it. The device mounts it only after Apple's server has
/// signed it for that device, so a changed file is refused rather than mounted.
pub const DOWNLOAD_URL: &str = "https://raw.githubusercontent.com/doronz88/DeveloperDiskImage/main/PersonalizedImages/Xcode_iOS_DDI_Personalized/";
pub const DOWNLOAD_HOST: &str = "GitHub (raw.githubusercontent.com)";
pub const DOWNLOAD_SIZE: &str = "about 18 MB";

/// Asked, with what went wrong with the Mac's own copy, before anything is downloaded.
pub type Consent<'a> = &'a (dyn Fn(&str) -> bool + Send + Sync);

/// No image came from Xcode or a paired Mac, and the download wasn't allowed. Callers ask the
/// person and try again with their answer; `needs_download` recognizes it.
#[derive(Debug)]
pub struct NeedsDownload(pub String);

impl std::fmt::Display for NeedsDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

impl std::error::Error for NeedsDownload {}

/// The reason, when `error` is [`NeedsDownload`].
pub fn needs_download(error: &Report) -> Option<String> {
    error.iter_reports().find_map(|cause| cause.downcast_current_context::<NeedsDownload>().map(|n| n.0.clone()))
}

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

/// The Xcode build the manifest belongs to, for messages.
fn build(manifest: &[u8]) -> String {
    plist::from_bytes::<plist::Value>(manifest).ok()
        .and_then(|m| m.as_dictionary()?.get("ProductBuildVersion")?.as_string().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
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

fn pack(parts: [&[u8]; 3]) -> Vec<u8> {
    let mut bundle = Vec::from(MAGIC);
    for part in parts {
        bundle.extend_from_slice(&(part.len() as u32).to_le_bytes());
        bundle.extend_from_slice(part);
    }
    bundle
}

/// The three files in `dir`, checked against each other, as one bundle.
pub fn from_files(dir: &Path) -> Result<Vec<u8>, Report> {
    let mut parts = Vec::new();
    for (name, limit) in FILES.iter().zip(LIMITS) {
        parts.push(bounded_file(&dir.join(name), limit)?);
    }
    validate(&parts[0], &parts[1], &parts[2])?;
    Ok(pack([&parts[0], &parts[1], &parts[2]]))
}

/// Xcode's iOS image: the folder Xcode's first launch fills from its own packages.
#[cfg(all(feature = "desktop", target_os = "macos"))]
const XCODE_IMAGE: &str = "/Library/Developer/DeveloperDiskImages/iOS_DDI";
/// What Xcode ships inside, before its first launch has installed it.
#[cfg(all(feature = "desktop", target_os = "macos"))]
const XCODE_CANDIDATE: &str = "/Library/Developer/CoreDevice/CandidateDDIs/iOS_DDI.dmg";

/// Reads the image from the folder `image` (`Restore/` inside), which must lie under `trusted`.
/// Read in place: asking `xcrun` instead would open macOS's install-developer-tools window on a
/// Mac that has none.
#[cfg(feature = "desktop")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn from_folder(image: &Path, candidate: &Path, trusted: &Path) -> Result<Vec<u8>, Report> {
    let Ok(restore) = image.join("Restore").canonicalize() else {
        if candidate.exists() {
            bail!("Xcode is installed on this Mac but hasn't finished installing its components. Open Xcode once, or run sudo xcodebuild -runFirstLaunch in Terminal.");
        }
        bail!("This Mac has no Xcode image for iOS devices. Install Xcode from the Mac App Store and open it once.");
    };
    if !restore.starts_with(trusted) {
        bail!("Xcode's iOS Developer Disk Image is outside {}", trusted.display());
    }
    let manifest_path = restore.join("BuildManifest.plist").canonicalize().context("Xcode's image has no BuildManifest.plist. Open Xcode once so it finishes installing its components.")?;
    if !manifest_path.starts_with(&restore) { bail!("Xcode DDI manifest is outside its Restore folder"); }
    let manifest = bounded_file(&manifest_path, LIMITS[2])?;
    let decoded: plist::Value = plist::from_bytes(&manifest)?;
    // Older hardware identities use SHA-1 for the same physical payloads. Select
    // the SHA-384 paths and read each file once even if validation fails.
    let entry = identities(&decoded).find(|m| {
        KEYS.iter().all(|key| digest(m, key).is_some_and(|d| d.len() == 48))
    }).ok_or_else(|| report!("Xcode's iOS image lacks a PersonalizedDMG and trust cache. Update Xcode and open it once."))?;
    let extract = |key, limit| -> Result<Vec<u8>, Report> {
        let relative = entry.get(key).and_then(plist::Value::as_dictionary)
            .and_then(|e| e.get("Info")).and_then(plist::Value::as_dictionary)
            .and_then(|i| i.get("Path")).and_then(plist::Value::as_string)
            .ok_or_else(|| report!("Xcode's DDI manifest is missing the path for {key}"))?;
        let path = restore.join(relative).canonicalize().context("Xcode's image is missing a file")?;
        if !path.starts_with(&restore) { bail!("Xcode DDI payload is outside its Restore folder"); }
        bounded_file(&path, limit)
    };
    let image = extract(KEYS[0], LIMITS[0])?;
    let trust = extract(KEYS[1], LIMITS[1])?;
    validate(&image, &trust, &manifest)?;
    Ok(pack([&image, &trust, &manifest]))
}

#[cfg(all(feature = "desktop", target_os = "macos"))]
pub(crate) fn from_xcode() -> Result<Vec<u8>, Report> {
    // Debug builds only: pretend Xcode's image lives elsewhere (or nowhere), to try a Mac without Xcode.
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("APPARITION_DDI_XCODE_DIR") {
        let dir = PathBuf::from(dir);
        let trusted = dir.parent().and_then(|p| p.canonicalize().ok()).unwrap_or_else(|| PathBuf::from("/nonexistent"));
        return from_folder(&dir, &dir.join("no-candidate"), &trusted);
    }
    from_folder(Path::new(XCODE_IMAGE), Path::new(XCODE_CANDIDATE), Path::new("/Library/Developer"))
}

/// Where a downloaded image is kept, so asking is needed once. `APPARITION_DDI_CACHE` moves it (tests).
#[cfg(feature = "desktop")]
pub(crate) fn cache_dir() -> PathBuf {
    match std::env::var_os("APPARITION_DDI_CACHE").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Apparition Installer").join("ddi"),
    }
}

#[cfg(feature = "desktop")]
fn cached(dir: &Path) -> Option<Vec<u8>> {
    let bytes = bounded_file(&dir.join(BUNDLE_NAME), MAX_BUNDLE).ok()?;
    unpack(&bytes).ok()?;
    Some(bytes)
}

/// What this computer has without asking anyone: Xcode's image on a Mac, else a download kept earlier.
#[cfg(feature = "desktop")]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn local() -> Result<Vec<u8>, Report> {
    #[cfg(target_os = "macos")]
    let from_xcode = from_xcode();
    #[cfg(not(target_os = "macos"))]
    let from_xcode: Result<Vec<u8>, Report> = Err(report!("This computer isn't a Mac."));
    match (from_xcode, cached(&cache_dir())) {
        (Ok(bundle), _) | (Err(_), Some(bundle)) => Ok(bundle),
        (Err(error), None) => Err(error),
    }
}

/// Downloads the image from [`DOWNLOAD_URL`], checks the files against each other and keeps them in `dir`.
#[cfg(feature = "desktop")]
async fn download(dir: &Path) -> Result<Vec<u8>, Report> {
    let client = isideload::tls::client_builder()
        .user_agent(concat!("Apparition/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(180))
        .build()?;
    let mut parts: Vec<Vec<u8>> = Vec::new();
    for (name, limit) in FILES.iter().zip(LIMITS) {
        let mut response = client.get(format!("{DOWNLOAD_URL}{name}")).send().await
            .context(format!("Couldn't reach {DOWNLOAD_HOST} to download {name}"))?;
        if !response.status().is_success() {
            bail!("Downloading {name} from {DOWNLOAD_HOST} failed (HTTP {})", response.status().as_u16());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context(format!("Downloading {name} was interrupted"))? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > limit { bail!("{name} from {DOWNLOAD_HOST} is larger than expected"); }
        }
        parts.push(bytes);
    }
    validate(&parts[0], &parts[1], &parts[2])?;
    let bundle = pack([&parts[0], &parts[1], &parts[2]]);
    std::fs::create_dir_all(dir)?;
    // Its own name, so a second run (the app and the command line) can't interleave writes.
    let partial = dir.join(format!("{BUNDLE_NAME}.{}.part", std::process::id()));
    std::fs::write(&partial, &bundle)?;
    std::fs::rename(&partial, dir.join(BUNDLE_NAME))?;
    Ok(bundle)
}

/// Where [`resolve`] found the image.
#[cfg(feature = "desktop")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Xcode,
    PairedMac,
    Cached,
    Downloaded,
}

#[cfg(feature = "desktop")]
impl Origin {
    pub fn describe(self) -> &'static str {
        match self {
            Origin::Xcode => "Xcode on this Mac",
            Origin::PairedMac => "the paired Mac",
            Origin::Cached => "a download kept earlier",
            Origin::Downloaded => "a download from GitHub, just now",
        }
    }
}

/// The image to hand a device or mount on it, and where it came from. Never downloads unless
/// `consent` agrees, and `consent` is only asked when Xcode, the paired Mac and an earlier download
/// all came up empty.
#[cfg(feature = "desktop")]
pub(crate) async fn resolve(companion_dir: &Path, consent: Consent<'_>) -> Result<(Origin, Vec<u8>), Report> {
    #[cfg(target_os = "macos")]
    let reason = {
        let _ = companion_dir;
        let (xcode, cache) = tokio::task::spawn_blocking(|| (from_xcode(), cached(&cache_dir()))).await?;
        match (xcode, cache) {
            (Ok(bundle), _) => return Ok((Origin::Xcode, bundle)),
            (Err(_), Some(bundle)) => return Ok((Origin::Cached, bundle)),
            (Err(error), None) => error.format_current_context().to_string(),
        }
    };
    #[cfg(not(target_os = "macos"))]
    let reason = match from_paired_mac(companion_dir).await {
        Ok(bundle) => return Ok((Origin::PairedMac, bundle)),
        Err(error) => match cached(&cache_dir()) {
            Some(bundle) => return Ok((Origin::Cached, bundle)),
            None => error,
        },
    };
    if consent(&reason) {
        return Ok((Origin::Downloaded, download(&cache_dir()).await?));
    }
    Err(report!("{reason}").context(NeedsDownload(reason)).into_dynamic())
}

#[cfg(feature = "desktop")]
pub(crate) async fn source(companion_dir: &Path, consent: Consent<'_>) -> Result<Vec<u8>, Report> {
    Ok(resolve(companion_dir, consent).await?.1)
}

/// The Xcode build a bundle belongs to, for `apparition ddi`.
#[cfg(feature = "desktop")]
pub fn bundle_build(bundle: &[u8]) -> String {
    unpack(bundle).map(|[_, _, manifest]| build(manifest)).unwrap_or_else(|_| "unknown".into())
}

/// What a PC asks its paired Mac for. Compiled on a Mac too, where the tests run it against a real channel.
#[cfg(feature = "desktop")]
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) async fn from_paired_mac(companion_dir: &Path) -> Result<Vec<u8>, String> {
    const HELP: &str = "Pair this computer with a Mac that has Xcode (apparition mac pair) and allow developer access there";
    let reply = crate::companion_client::developer(companion_dir, "GET", "/ios-ddi", None, crate::companion_client::Upload::None).await
        .map_err(|error| format!("{HELP}. {}", error.format_current_context()))?;
    if reply.status == 503 {
        return Err(format!("The paired Mac has no image to send: {}", String::from_utf8_lossy(&reply.body)));
    }
    if reply.status != 200 {
        // A Mac whose Apparition predates this route answers as it does any path it doesn't know.
        return Err(format!("The paired Mac's Apparition can't send the image (HTTP {}). Update Apparition on that Mac.", reply.status));
    }
    unpack(&reply.body).map_err(|error| error.format_current_context().to_string())?;
    Ok(reply.body)
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
        .context(format!("mounting the Developer Disk Image (Xcode build {}) failed. Check that Developer Mode is on and the device is unlocked; if this device's iOS is newer than that Xcode, update Xcode and send the image again", build(manifest)))?;
    Ok(())
}

/// `{mounted: "already"}`, `{mounted: "now"}` after mounting `bundle` (a handed-over file or a folder of
/// the three [`FILES`]), or `{mounted: false, needsFiles: true}` when none is mounted and there's nothing to mount.
pub async fn ensure(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, bundle: Option<&Path>) -> Result<Value, Report> {
    if mounted(adapter, handshake).await? {
        return Ok(json!({ "mounted": "already" }));
    }
    // A bundle a computer handed over (a file), or the three files downloaded on this device (a folder).
    let bytes = match bundle {
        Some(path) if path.is_file() => bounded_file(path, MAX_BUNDLE).context("reading the Developer Disk Image failed")?,
        Some(path) if path.is_dir() => from_files(path).context("the downloaded Developer Disk Image is damaged; delete it and download it again")?,
        _ => return Ok(json!({ "mounted": false, "needsFiles": true })),
    };
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

#[cfg(all(test, feature = "desktop"))]
mod folder_tests {
    use super::*;

    /// A folder laid out like Xcode's: `Restore/BuildManifest.plist` naming an image and a trust cache.
    fn xcode_folder(root: &Path, image: &[u8], trust: &[u8]) {
        let restore = root.join("Restore");
        std::fs::create_dir_all(restore.join("Firmware")).unwrap();
        std::fs::write(restore.join("img.dmg"), image).unwrap();
        std::fs::write(restore.join("Firmware/img.trustcache"), trust).unwrap();
        let entry = |path: &str, data: &[u8]| plist::Value::Dictionary([
            ("Digest".to_owned(), plist::Value::Data(Sha384::digest(data).to_vec())),
            ("Info".to_owned(), plist::Value::Dictionary([("Path".to_owned(), plist::Value::String(path.into()))].into_iter().collect())),
        ].into_iter().collect());
        let assets = plist::Value::Dictionary([
            (KEYS[0].to_owned(), entry("img.dmg", image)),
            (KEYS[1].to_owned(), entry("Firmware/img.trustcache", trust)),
        ].into_iter().collect());
        let identity = plist::Value::Dictionary([("Manifest".to_owned(), assets)].into_iter().collect());
        let root_plist = plist::Value::Dictionary([
            ("BuildIdentities".to_owned(), plist::Value::Array(vec![identity])),
            ("ProductBuildVersion".to_owned(), plist::Value::String("TEST1".into())),
        ].into_iter().collect());
        plist::to_file_binary(restore.join("BuildManifest.plist"), &root_plist).unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apparition-ddi-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn reads_the_folder_xcode_installed() {
        let root = temp("folder");
        xcode_folder(&root.join("iOS_DDI"), b"image bytes", b"trust bytes");
        let bundle = from_folder(&root.join("iOS_DDI"), &root.join("none.dmg"), &root).unwrap();
        let [image, trust, _] = unpack(&bundle).unwrap();
        assert_eq!((image, trust), (b"image bytes".as_slice(), b"trust bytes".as_slice()));
        // A file Xcode's manifest doesn't vouch for is refused.
        std::fs::write(root.join("iOS_DDI/Restore/img.dmg"), b"swapped").unwrap();
        assert!(from_folder(&root.join("iOS_DDI"), &root.join("none.dmg"), &root).is_err());
        // Outside the trusted prefix is refused too.
        xcode_folder(&root.join("elsewhere"), b"i", b"t");
        assert!(from_folder(&root.join("elsewhere"), &root.join("none.dmg"), &root.join("iOS_DDI")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn says_what_to_do_when_xcode_has_no_image() {
        let root = temp("missing");
        let none = from_folder(&root.join("iOS_DDI"), &root.join("candidate.dmg"), &root).unwrap_err().to_string();
        assert!(none.contains("Mac App Store"), "{none}");
        std::fs::write(root.join("candidate.dmg"), b"x").unwrap();
        let unfinished = from_folder(&root.join("iOS_DDI"), &root.join("candidate.dmg"), &root).unwrap_err().to_string();
        assert!(unfinished.contains("runFirstLaunch") && unfinished.contains("sudo"), "{unfinished}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keeps_a_download_only_while_it_checks_out() {
        let root = temp("cache");
        xcode_folder(&root.join("x"), b"image", b"trust");
        let bundle = from_folder(&root.join("x"), &root.join("none.dmg"), &root).unwrap();
        assert!(cached(&root).is_none());
        std::fs::write(root.join(BUNDLE_NAME), &bundle).unwrap();
        assert_eq!(cached(&root).as_deref(), Some(bundle.as_slice()));
        std::fs::write(root.join(BUNDLE_NAME), &bundle[..bundle.len() - 1]).unwrap();
        assert!(cached(&root).is_none(), "a truncated copy is not used");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_of_the_three_files_is_a_bundle_too() {
        let root = temp("files");
        xcode_folder(&root.join("x"), b"image", b"trust");
        let restore = root.join("x/Restore");
        let dir = root.join("dl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(restore.join("img.dmg"), dir.join(FILES[0])).unwrap();
        std::fs::copy(restore.join("Firmware/img.trustcache"), dir.join(FILES[1])).unwrap();
        std::fs::copy(restore.join("BuildManifest.plist"), dir.join(FILES[2])).unwrap();
        assert!(unpack(&from_files(&dir).unwrap()).is_ok());
        std::fs::write(dir.join(FILES[0]), b"tampered").unwrap();
        assert!(from_files(&dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn needs_download_is_recognized_through_the_error() {
        let error = report!("no image").context(NeedsDownload("no image".into())).into_dynamic();
        assert_eq!(needs_download(&error).as_deref(), Some("no image"));
        assert_eq!(needs_download(&report!("other").into_dynamic()), None);
    }

    /// A Mac with no Xcode image and no download yet (a clean VM), through the real consent flow. Opt in
    /// with APPARITION_TEST_NO_XCODE=1: it downloads from github.com once the consent says yes.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn without_xcode_it_asks_before_it_downloads() {
        if std::env::var("APPARITION_TEST_NO_XCODE").as_deref() != Ok("1") { return; }
        let cache = temp("noxcode");
        // SAFETY: this is the only test that sets this, and it runs on its own in the VM.
        unsafe { std::env::set_var("APPARITION_DDI_CACHE", &cache); }
        assert!(local().is_err(), "this Mac must have no Xcode image for this test");
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let no = |_: &str| { asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst); false };
        let refused = source(Path::new("/nonexistent"), &no).await.unwrap_err();
        let reason = needs_download(&refused).expect("a refusal is NeedsDownload");
        assert!(reason.contains("Mac App Store"), "{reason}");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!cache.join(BUNDLE_NAME).exists(), "nothing is downloaded before a yes");
        let yes = |_: &str| { asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst); true };
        let bundle = source(Path::new("/nonexistent"), &yes).await.unwrap();
        assert!(unpack(&bundle).is_ok());
        assert!(cache.join(BUNDLE_NAME).exists(), "the download is kept");
        // The next run uses the kept copy and doesn't ask.
        let before = asked.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(source(Path::new("/nonexistent"), &no).await.unwrap(), bundle);
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), before);
        let _ = std::fs::remove_dir_all(&cache);
    }
}
