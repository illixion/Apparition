//! Copies an installed app's data container off a device and back, through house_arrest over
//! Apparition's own tunnel, so a build tool can back an app's data up, reinstall it under
//! another signature and put the data back without CoreDevice (whose tunnel makes Xcode's
//! DeviceHub wake a sleeping Apple TV).
//!
//! The layout on this computer mirrors the container: `<dir>/Documents/...` and
//! `<dir>/Library/...`. What `migrate::skipped` leaves behind (caches, saved state) is left
//! here too, on the way off and in the count. As with `migrate`, only a development-signed app's
//! container is vended.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use idevice::{
    RsdService,
    afc::{AfcClient, opcode::AfcFopenMode},
    house_arrest::HouseArrestClient,
};
use rootcause::prelude::*;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::migrate::{CHUNK, ROOTS, skipped};

async fn open(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
) -> Result<AfcClient, Report> {
    Ok(HouseArrestClient::connect_rsd(adapter, handshake)
        .await
        .context("house_arrest connect failed")?
        .vend_container(bundle_id)
        .await
        .context(format!("{bundle_id}'s data isn't open to this computer (is it installed, and development-signed?)"))?)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackupManifest {
    format: String,
    bundle_id: String,
    created_at: u64,
}

/// Normalizes a caller-supplied container path and limits browsing to the data folders the
/// existing migration tool copies. In particular, caches and traversal paths are never exposed.
fn checked_device_path(path: &str) -> Result<String, Report> {
    let path = path.trim_start_matches('/');
    let mut components = path.split('/');
    let root = components.next().unwrap_or("");
    if !matches!(root, "Documents" | "Library") {
        bail!("Choose a path under Documents or Library");
    }
    if components.clone().any(|part| {
        part.is_empty() || part == "." || part == ".." || part.contains('\\') || part.contains('\0')
    }) {
        bail!("The container path isn't valid");
    }
    let normalized = format!("/{path}");
    if skipped(&normalized) {
        bail!("This data folder is excluded from Apparition's app-data tools");
    }
    Ok(normalized)
}

/// Lists regular files and folders from an app's development-readable container.
pub async fn browse(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
    path: &str,
) -> Result<Value, Report> {
    let path = checked_device_path(path)?;
    let mut afc = open(adapter, handshake, bundle_id).await?;
    let names = match afc.list_dir(path.clone()).await {
        Ok(names) => names,
        // The app may never have created one of the two standard data folders.
        Err(_) if ROOTS.contains(&path.as_str()) => Vec::new(),
        Err(error) => return Err(report!("Couldn't list {path}: {error}").into()),
    };
    let mut entries = Vec::new();
    for name in names.into_iter().filter(|n| n != "." && n != "..") {
        if local_name(&name).is_none() {
            continue;
        }
        let Ok(child) = checked_device_path(&format!("{path}/{name}")) else {
            continue;
        };
        let Ok(info) = afc.get_file_info(child.clone()).await else {
            continue;
        };
        let kind = match info.st_ifmt.as_str() {
            "S_IFDIR" => "directory",
            "S_IFREG" => "file",
            _ => continue,
        };
        entries.push(json!({ "name": name, "path": child, "kind": kind, "bytes": info.size }));
    }
    entries.sort_by(|a, b| {
        let directory = |v: &Value| v["kind"].as_str() == Some("directory");
        directory(b)
            .cmp(&directory(a))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(json!({ "path": path, "entries": entries }))
}

/// Copies one regular file from an app container to a local, caller-owned path.
pub async fn export_file(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
    device_path: &str,
    destination: &Path,
) -> Result<Value, Report> {
    let device_path = checked_device_path(device_path)?;
    if device_path == "/Documents" || device_path == "/Library" {
        bail!("Choose a file to export");
    }
    let mut afc = open(adapter, handshake, bundle_id).await?;
    let info = afc
        .get_file_info(device_path.clone())
        .await
        .context("Couldn't read this item")?;
    if info.st_ifmt != "S_IFREG" {
        bail!("This item isn't a regular file");
    }
    let bytes = pull_file(&mut afc, &device_path, destination, info.size as u64).await?;
    Ok(json!({ "bytes": bytes }))
}

/// Streams a complete, cache-free Documents/Library backup to a ZIP on local storage.
pub async fn export_backup(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
    destination: &Path,
) -> Result<Value, Report> {
    let mut afc = open(adapter, handshake, bundle_id).await?;
    let mut failed = Vec::new();
    let found = walk(&mut afc, &mut failed).await?;
    let file = std::fs::File::create(destination).context("Couldn't create the backup archive")?;
    let mut archive = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    archive
        .start_file("ApparitionBackup.json", options)
        .context("Couldn't write backup metadata")?;
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let manifest = BackupManifest {
        format: "apparition-app-data-v1".into(),
        bundle_id: bundle_id.into(),
        created_at,
    };
    archive
        .write_all(&serde_json::to_vec(&manifest)?)
        .context("Couldn't write backup metadata")?;

    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut folders = 0u64;
    for (device_path, is_dir, size) in found {
        let relative = device_path.trim_start_matches('/');
        if relative.contains('\\') {
            failed.push(format!("{device_path}: unsupported path separator"));
            continue;
        }
        if is_dir {
            archive
                .add_directory(format!("{relative}/"), options)
                .context("Couldn't add a folder to the backup")?;
            folders += 1;
            continue;
        }
        archive
            .start_file(relative, options)
            .context("Couldn't add a file to the backup")?;
        let mut reader = afc.open(device_path, AfcFopenMode::RdOnly).await?;
        let mut copied = 0u64;
        loop {
            let chunk = reader.read_n(CHUNK).await?;
            if chunk.is_empty() {
                break;
            }
            archive
                .write_all(&chunk)
                .context("Couldn't write a file to the backup")?;
            copied += chunk.len() as u64;
        }
        reader.close().await?;
        if copied != size {
            bail!("A file changed during backup: copied {copied} of {size} bytes");
        }
        files += 1;
        bytes += copied;
    }
    archive
        .finish()
        .context("Couldn't finish the backup archive")?;
    Ok(json!({ "files": files, "bytes": bytes, "folders": folders, "failed": failed }))
}

fn backup_path(name: &str) -> Result<Option<String>, Report> {
    if name == "ApparitionBackup.json" {
        return Ok(None);
    }
    if name.contains('\\') || name.contains('\0') {
        bail!("The backup contains an invalid path");
    }
    let path = name.strip_suffix('/').unwrap_or(name);
    if path.starts_with('/') {
        bail!("The backup contains a path outside the app data folders");
    }
    let enclosed: Vec<_> = path.split('/').collect();
    if enclosed.iter().any(|part| part.is_empty() || *part == "." || *part == "..") {
        bail!("The backup contains an invalid path");
    }
    if enclosed.len() < 2 || !matches!(enclosed[0], "Documents" | "Library") {
        bail!("The backup contains a path outside Documents and Library");
    }
    let normalized = format!("/{}", enclosed.join("/"));
    if skipped(&normalized) {
        bail!("The backup contains an excluded cache path");
    }
    Ok(Some(normalized))
}

/// Validates an Apparition backup's app identity and safe paths, then merges it into that app's
/// development-readable container. Existing files with the same names are replaced.
pub async fn restore_backup(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
    archive_path: &Path,
) -> Result<Value, Report> {
    let file = std::fs::File::open(archive_path).context("Couldn't open the selected backup")?;
    let mut archive = ZipArchive::new(file).context("This isn't a readable ZIP backup")?;
    let mut manifest = None;
    let mut seen_paths = std::collections::HashSet::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .context("Couldn't read a backup entry")?;
        let name = entry
            .name()
            .context("The backup contains an invalid entry name")?
            .to_string();
        let is_directory = entry.is_dir();
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind == 0o120000 {
                bail!("The backup contains a symbolic link");
            }
            if kind != 0 && kind != if is_directory { 0o040000 } else { 0o100000 } {
                bail!("The backup contains a special file");
            }
        }
        let Some(path) = backup_path(&name)? else {
            if manifest.is_some() {
                bail!("The backup contains duplicate metadata");
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("Couldn't read backup metadata")?;
            manifest = Some(
                serde_json::from_slice::<BackupManifest>(&bytes)
                    .context("Backup metadata is invalid")?,
            );
            continue;
        };
        if !seen_paths.insert(path) {
            bail!("The backup contains a duplicate path");
        }
    }
    let manifest =
        manifest.ok_or_else(|| report!("This ZIP isn't an Apparition app-data backup"))?;
    if manifest.format != "apparition-app-data-v1" {
        bail!("This backup format isn't supported");
    }
    if manifest.bundle_id != bundle_id {
        bail!(
            "This backup belongs to {}, not {bundle_id}",
            manifest.bundle_id
        );
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging =
        std::env::temp_dir().join(format!("apparition-restore-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&staging).context("Couldn't create a temporary restore folder")?;
    let unpacked = (|| -> Result<(), Report> {
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .context("Couldn't read a backup entry")?;
            let name = entry
                .name()
                .context("The backup contains an invalid entry name")?
                .to_string();
            let Some(path) = backup_path(&name)? else {
                continue;
            };
            let relative = path.trim_start_matches('/');
            let destination = staging.join(relative);
            if entry.is_dir() {
                std::fs::create_dir_all(&destination)?;
            } else {
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut output = std::fs::File::create(destination)?;
                std::io::copy(&mut entry, &mut output)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = unpacked {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    let result = push(adapter, handshake, bundle_id, &staging).await;
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// Lists entries from an app container through the on-device Tools bridge.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_container_browse(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    bundle_id: *const std::ffi::c_char,
    path: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = unsafe { crate::text(bundle_id) }?;
        let path = unsafe { crate::text(path) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(crate::stop::stoppable(browse(
            adapter, handshake, &bundle_id, &path,
        )))
    })())
}

/// Exports one regular app-data file to a caller-owned local path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_container_export_file(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    bundle_id: *const std::ffi::c_char,
    device_path: *const std::ffi::c_char,
    destination: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = unsafe { crate::text(bundle_id) }?;
        let device_path = unsafe { crate::text(device_path) }?;
        let destination = unsafe { crate::text(destination) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(crate::stop::stoppable(export_file(
            adapter,
            handshake,
            &bundle_id,
            &device_path,
            Path::new(&destination),
        )))
    })())
}

/// Creates a complete cache-free app-data archive at the caller-owned local path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_container_export_backup(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    bundle_id: *const std::ffi::c_char,
    destination: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = unsafe { crate::text(bundle_id) }?;
        let destination = unsafe { crate::text(destination) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(crate::stop::stoppable(export_backup(
            adapter,
            handshake,
            &bundle_id,
            Path::new(&destination),
        )))
    })())
}

/// Validates and restores a matching Apparition app-data archive into the selected container.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_container_restore_backup(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    bundle_id: *const std::ffi::c_char,
    archive_path: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = unsafe { crate::text(bundle_id) }?;
        let archive_path = unsafe { crate::text(archive_path) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(crate::stop::stoppable(restore_backup(
            adapter,
            handshake,
            &bundle_id,
            Path::new(&archive_path),
        )))
    })())
}


/// A name from the device that is safe to use as one path component here.
fn local_name(name: &str) -> Option<&str> {
    (!name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')).then_some(name)
}

/// Every folder and file under the container's data folders, as (device path, is folder),
/// skipped ones left out. A data folder the app never made is simply absent.
async fn walk(afc: &mut AfcClient, failed: &mut Vec<String>) -> Result<Vec<(String, bool, u64)>, Report> {
    let mut found = Vec::new();
    let mut folders: Vec<String> = ROOTS.iter().map(|r| r.to_string()).collect();
    while let Some(dir) = folders.pop() {
        let names = match afc.list_dir(dir.clone()).await {
            Ok(names) => names,
            Err(_) if ROOTS.contains(&dir.as_str()) => continue,
            Err(error) => {
                failed.push(format!("{dir}: {error}"));
                continue;
            }
        };
        for name in names.into_iter().filter(|n| n != "." && n != "..") {
            let path = format!("{dir}/{name}");
            if skipped(&path) || local_name(&name).is_none() {
                continue;
            }
            match afc.get_file_info(path.clone()).await {
                Ok(info) if info.st_ifmt == "S_IFDIR" => {
                    found.push((path.clone(), true, 0));
                    folders.push(path);
                }
                Ok(info) if info.st_ifmt == "S_IFREG" => found.push((path, false, info.size as u64)),
                Ok(_) => {} // links and the like stay behind, as in migrate
                Err(error) => failed.push(format!("{path}: {error}")),
            }
        }
    }
    Ok(found)
}

/// How many folders and files the app's data holds (skipped ones left out): `{folders, files, bytes}`.
pub async fn count(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
) -> Result<Value, Report> {
    let mut afc = open(adapter, handshake, bundle_id).await?;
    let mut failed = Vec::new();
    let found = walk(&mut afc, &mut failed).await?;
    let folders = found.iter().filter(|f| f.1).count();
    Ok(json!({
        "folders": folders,
        "files": found.len() - folders,
        "bytes": found.iter().map(|f| f.2).sum::<u64>(),
        "failed": failed,
    }))
}

/// Copies the app's Documents and Library into `dir` (which must be empty or missing).
/// Returns `{files, bytes, folders, failed, ms}`; a non-empty `failed` means the copy is incomplete.
pub async fn pull(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
    dir: &Path,
) -> Result<Value, Report> {
    let started = Instant::now();
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        bail!("{} isn't empty", dir.display());
    }
    let mut afc = open(adapter, handshake, bundle_id).await?;
    let mut failed = Vec::new();
    let found = walk(&mut afc, &mut failed).await?;
    for root in ROOTS {
        std::fs::create_dir_all(dir.join(root.trim_start_matches('/')))?;
    }
    let (mut files, mut bytes, mut folders) = (0u64, 0u64, 0u64);
    for (device_path, is_dir, size) in found {
        let local: PathBuf = dir.join(device_path.trim_start_matches('/'));
        if is_dir {
            std::fs::create_dir_all(&local)?;
            folders += 1;
            continue;
        }
        match pull_file(&mut afc, &device_path, &local, size).await {
            Ok(n) => {
                files += 1;
                bytes += n;
            }
            Err(error) => failed.push(format!("{device_path}: {error}")),
        }
    }
    Ok(json!({ "files": files, "bytes": bytes, "folders": folders, "failed": failed, "ms": started.elapsed().as_millis() as u64 }))
}

async fn pull_file(afc: &mut AfcClient, from: &str, to: &Path, size: u64) -> Result<u64, Report> {
    let mut reader = afc.open(from.to_string(), AfcFopenMode::RdOnly).await?;
    let mut writer = tokio::fs::File::create(to).await?;
    let mut copied = 0u64;
    loop {
        let chunk = reader.read_n(CHUNK).await?;
        if chunk.is_empty() {
            break;
        }
        writer.write_all(&chunk).await?;
        copied += chunk.len() as u64;
    }
    reader.close().await?;
    writer.flush().await?;
    if copied != size {
        bail!("copied {copied} of {size} bytes");
    }
    Ok(copied)
}

/// Copies `dir/Documents` and `dir/Library` (as `pull` laid them out) into the app's container,
/// over what is there. Returns `{files, bytes, folders, failed, ms}`.
pub async fn push(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
    dir: &Path,
) -> Result<Value, Report> {
    let started = Instant::now();
    let mut afc = open(adapter, handshake, bundle_id).await?;
    let (mut files, mut bytes, mut folders, mut failed) = (0u64, 0u64, 0u64, Vec::<String>::new());
    let mut pending: Vec<(PathBuf, String)> = Vec::new();
    for root in ROOTS {
        let local = dir.join(root.trim_start_matches('/'));
        if local.is_dir() {
            let _ = afc.mk_dir(root.to_string()).await;
            pending.push((local, root.to_string()));
        }
    }
    while let Some((local_dir, device_dir)) = pending.pop() {
        for entry in std::fs::read_dir(&local_dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
            let device_path = format!("{device_dir}/{name}");
            if skipped(&device_path) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                // mk_dir fails on a folder that exists, which is fine.
                let _ = afc.mk_dir(device_path.clone()).await;
                folders += 1;
                pending.push((entry.path(), device_path));
            } else if kind.is_file() {
                match push_file(&mut afc, &entry.path(), &device_path).await {
                    Ok(n) => {
                        files += 1;
                        bytes += n;
                    }
                    Err(error) => failed.push(format!("{device_path}: {error}")),
                }
            }
        }
    }
    Ok(json!({ "files": files, "bytes": bytes, "folders": folders, "failed": failed, "ms": started.elapsed().as_millis() as u64 }))
}

async fn push_file(afc: &mut AfcClient, from: &Path, to: &str) -> Result<u64, Report> {
    // A chunk at a time: an emulator's Documents can hold multi-gigabyte disc images.
    let mut reader = tokio::fs::File::open(from).await?;
    let mut writer = afc.open(to.to_string(), AfcFopenMode::WrOnly).await?;
    let mut buffer = vec![0u8; CHUNK];
    let mut copied = 0u64;
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        writer.write_entire(&buffer[..n]).await?;
        copied += n as u64;
    }
    writer.close().await?;
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_cannot_climb_out_of_the_backup() {
        assert_eq!(local_name("a.plist"), Some("a.plist"));
        for bad in ["", ".", "..", "a/b", "x\0y"] {
            assert_eq!(local_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn container_paths_stay_in_documents_and_library() {
        assert_eq!(checked_device_path("/Documents").unwrap(), "/Documents");
        assert_eq!(checked_device_path("Library/Preferences").unwrap(), "/Library/Preferences");
        for bad in [
            "/",
            "/private/var/mobile/Library",
            "/Documents/../Library",
            "/Documents//file",
            "/Documents/./file",
            "/Documents/file\\name",
            "/Library/Caches/database",
            "/Library/Saved Application State/window.plist",
        ] {
            assert!(checked_device_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn restore_paths_reject_traversal_and_normalized_duplicates() {
        assert_eq!(backup_path("Documents/config/settings.plist").unwrap(), Some("/Documents/config/settings.plist".into()));
        assert_eq!(backup_path("Library/Preferences/").unwrap(), Some("/Library/Preferences".into()));
        for bad in [
            "/Documents/config.plist",
            "Documents/../private",
            "Documents/./config.plist",
            "Documents//config.plist",
            "Documents\\private",
            "Library/Caches/private.db",
            "Library/Saved Application State/state.plist",
            "Other/file",
        ] {
            assert!(backup_path(bad).is_err(), "{bad:?}");
        }
    }
}
