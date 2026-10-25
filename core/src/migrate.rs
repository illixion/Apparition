//! Moves an app's data to the same app installed under another bundle ID.
//!
//! Signing an app with another team's certificate changes its bundle ID (the Apple ID adds the
//! team ID to it, an imported certificate keeps it), and a bundle ID is what names the app's data
//! container: the new copy starts empty. house_arrest vends a development-signed app's whole
//! container over AFC, so the data is copied from one container to the other through the
//! tunnel, file by file with nothing kept on this device. Copying changes nothing in the old
//! copy; the caller removes it once everything arrived.
//!
//! What can't move: the Keychain, app group containers and anything the system keeps by bundle ID
//! outside the container (notification permissions, for one). An app that is signed for
//! distribution isn't development-signed, and house_arrest refuses its container.

use std::time::Instant;

use idevice::{
    RsdService,
    afc::{AfcClient, opcode::AfcFopenMode},
    house_arrest::HouseArrestClient,
};
use rootcause::prelude::*;
use serde_json::{Value, json};

/// How much is read from the old copy before it is written to the new one.
const CHUNK: usize = 1024 * 1024;

/// A copy that fails on this many files is stopped: the connection is gone, or the new copy
/// refuses writes, and the rest would fail the same way.
const MAX_FAILURES: usize = 25;

/// The container's folders that hold the app's data; `tmp` and `SystemData` are the system's.
const ROOTS: [&str; 2] = ["/Documents", "/Library"];

/// Whether `path` (in the old container) is left behind: what the system or the app rebuilds
/// itself, and would only cost time to copy.
fn skipped(path: &str) -> bool {
    ["/Library/Caches", "/Library/Saved Application State", "/Library/SplashBoard", "/Library/Caches.noindex"]
        .iter()
        .any(|skip| path == *skip)
}

/// The name `name` takes in the new container's `dir`: a preferences file is named after the
/// bundle ID (`<id>.plist`), so it follows the app to its new one.
fn target_name(dir: &str, name: &str, from: &str, to: &str) -> String {
    if dir == "/Library/Preferences"
        && let Some(rest) = name.strip_prefix(from)
        && rest.starts_with('.')
    {
        return format!("{to}{rest}");
    }
    name.to_string()
}

/// What a copy did, for the reply.
#[derive(Default)]
struct Copied {
    files: u64,
    bytes: u64,
    folders: u64,
    /// Left behind on purpose: caches, links.
    skipped: Vec<String>,
    /// Files that didn't arrive whole.
    failed: Vec<String>,
}

/// Copies the data of `from` to `to`, both installed, through house_arrest over the tunnel.
/// Returns `{files, bytes, folders, skipped, failed, ms}`; a non-empty `failed` means the copy is
/// incomplete and the old copy must stay.
pub async fn migrate_container(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    from: &str,
    to: &str,
) -> Result<Value, Report> {
    if from == to {
        bail!("{from} is already the app to copy to");
    }
    let started = Instant::now();
    let mut source = HouseArrestClient::connect_rsd(adapter, handshake)
        .await
        .context("house_arrest connect failed")?
        .vend_container(from)
        .await
        .context(format!("{from}'s data isn't open to Apparition (is it installed, and development-signed?)"))?;
    let mut target = HouseArrestClient::connect_rsd(adapter, handshake)
        .await
        .context("house_arrest connect failed")?
        .vend_container(to)
        .await
        .context(format!("{to}'s data isn't open to Apparition (is it installed, and development-signed?)"))?;

    let mut done = Copied::default();
    // (folder in the old container, the same folder in the new one)
    let mut folders: Vec<(String, String)> = ROOTS.iter().map(|root| (root.to_string(), root.to_string())).collect();
    while let Some((from_dir, to_dir)) = folders.pop() {
        let names = match source.list_dir(from_dir.clone()).await {
            Ok(names) => names,
            // An app that never made its Library or Documents has nothing to move there.
            Err(_) if ROOTS.contains(&from_dir.as_str()) => continue,
            Err(error) => {
                done.failed.push(format!("{from_dir}: {error}"));
                continue;
            }
        };
        // mk_dir fails on a folder that exists, which is fine.
        let _ = target.mk_dir(to_dir.clone()).await;
        done.folders += 1;
        for name in names.into_iter().filter(|name| name != "." && name != "..") {
            let from_path = format!("{from_dir}/{name}");
            if skipped(&from_path) {
                done.skipped.push(from_path);
                continue;
            }
            let to_path = format!("{to_dir}/{}", target_name(&from_dir, &name, from, to));
            let info = match source.get_file_info(from_path.clone()).await {
                Ok(info) => info,
                Err(error) => {
                    done.failed.push(format!("{from_path}: {error}"));
                    continue;
                }
            };
            match info.st_ifmt.as_str() {
                "S_IFDIR" => folders.push((from_path, to_path)),
                "S_IFREG" => match copy_file(&mut source, &mut target, &from_path, &to_path, info.size as u64).await {
                    Ok(bytes) => {
                        done.files += 1;
                        done.bytes += bytes;
                    }
                    Err(error) => done.failed.push(format!("{from_path}: {error}")),
                },
                _ => done.skipped.push(from_path),
            }
            if done.failed.len() >= MAX_FAILURES {
                bail!("stopped after {} files failed, the last: {}", done.failed.len(), done.failed.last().map(String::as_str).unwrap_or(""));
            }
        }
    }

    Ok(json!({
        "files": done.files,
        "bytes": done.bytes,
        "folders": done.folders,
        "skipped": done.skipped.len(),
        "failed": done.failed,
        "ms": started.elapsed().as_millis() as u64,
    }))
}

/// One file across, `CHUNK` at a time, then checked to be as long as it was. Returns its size.
async fn copy_file(source: &mut AfcClient, target: &mut AfcClient, from: &str, to: &str, size: u64) -> Result<u64, Report> {
    let mut reader = source.open(from.to_string(), AfcFopenMode::RdOnly).await?;
    let mut writer = target.open(to.to_string(), AfcFopenMode::WrOnly).await?;
    let mut copied = 0u64;
    loop {
        let chunk = reader.read_n(CHUNK).await?;
        if chunk.is_empty() {
            break;
        }
        writer.write_entire(&chunk).await?;
        copied += chunk.len() as u64;
    }
    reader.close().await?;
    writer.close().await?;
    if copied != size {
        bail!("copied {copied} of {size} bytes");
    }
    Ok(copied)
}

/// Copies the data of `from` to `to`, two installed apps, over the open tunnel (handles from
/// idevice's `tunnel_create_rppairing`, borrowed). Returns `{ok, files, bytes, folders, skipped,
/// failed, ms}`; check that `failed` is empty before removing the old copy.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_migrate_container(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    from: *const std::ffi::c_char,
    to: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let from = unsafe { crate::text(from) }?;
        let to = unsafe { crate::text(to) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(migrate_container(adapter, handshake, &from, &to))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_stay_behind_and_data_goes() {
        assert!(skipped("/Library/Caches"));
        assert!(skipped("/Library/Saved Application State"));
        assert!(!skipped("/Library/Application Support"));
        assert!(!skipped("/Documents/Caches"));
        assert!(!skipped("/Library/Preferences"));
    }

    #[test]
    fn preferences_follow_the_bundle_id() {
        let (from, to) = ("com.utmapp.UTM.U8T3Z4P7JT", "com.utmapp.UTM");
        assert_eq!(target_name("/Library/Preferences", "com.utmapp.UTM.U8T3Z4P7JT.plist", from, to), "com.utmapp.UTM.plist");
        // Another app's preferences, or a name that only starts like the bundle ID, stay as they are.
        assert_eq!(target_name("/Library/Preferences", "com.apple.foo.plist", from, to), "com.apple.foo.plist");
        assert_eq!(target_name("/Library/Preferences", "com.utmapp.UTM.U8T3Z4P7JTx.plist", from, to), "com.utmapp.UTM.U8T3Z4P7JTx.plist");
        // Only Preferences is renamed.
        assert_eq!(target_name("/Documents", "com.utmapp.UTM.U8T3Z4P7JT.plist", from, to), "com.utmapp.UTM.U8T3Z4P7JT.plist");
    }
}
