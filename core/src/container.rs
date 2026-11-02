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
}
