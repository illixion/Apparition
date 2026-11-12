//! Installs a signed app as one compressed IPA instead of its unpacked files.
//!
//! isideload uploads the signed `.app` file by file and uncompressed. Through a companion
//! relay every byte crosses Wi-Fi twice, so a refresh of UTM (819 MB unpacked, 146 MB as an
//! IPA) took 109 s over the LAN and ~160 s over Tailscale, against 38 s over LocalDevVPN.
//! Signing only rewrites the Mach-O binaries, the signatures, the profile and Info.plist, so
//! the IPA is rebuilt from the downloaded one: every file whose bytes didn't change is copied
//! across still compressed, and only the changed ones are deflated again (fast level).

use std::{
    collections::HashMap,
    ffi::c_char,
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use idevice::{
    RsdService,
    core_device::AppServiceClient,
    afc::{
        AfcClient,
        opcode::{AfcFopenMode, AfcOpcode},
    },
    installation_proxy::InstallationProxyClient,
};
use rootcause::prelude::*;
use serde_json::json;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

use crate::{reply, runtime, text};

const UPLOAD_CHUNK: usize = 1024 * 1024;

pub struct Packed {
    pub files: usize,
    pub reused: usize,
    pub bytes: u64,
}

/// Packs `app` (a signed `.app` directory) into `out` as `Payload/<name>.app/...`, reusing
/// unchanged entries of `source_ipa` without recompressing them.
pub fn pack(app: &Path, source_ipa: Option<&Path>, out: &Path) -> Result<Packed, Report> {
    let app_name = app.file_name().ok_or_else(|| report!("bad app path"))?.to_string_lossy().to_string();

    // The source's entries by their path inside the .app, with size and CRC, so a file can
    // be matched without reading the source's data.
    let mut source = match source_ipa {
        Some(path) => Some(ZipArchive::new(BufReader::new(File::open(path)?))?),
        None => None,
    };
    let mut index: HashMap<String, (usize, u64, u32)> = HashMap::new();
    if let Some(archive) = source.as_mut() {
        for i in 0..archive.len() {
            let entry = archive.by_index_raw(i)?;
            // A name that isn't valid in its encoding just isn't reused.
            let Ok(name) = entry.name().map(|n| n.into_owned()) else { continue };
            if entry.is_dir() {
                continue;
            }
            // Payload/<Anything>.app/<relative>
            let mut parts = name.splitn(3, '/');
            if let (Some("Payload"), Some(bundle), Some(relative)) = (parts.next(), parts.next(), parts.next()) {
                if bundle.ends_with(".app") {
                    index.insert(relative.to_string(), (i, entry.size(), entry.crc32()));
                }
            }
        }
    }

    let mut writer = ZipWriter::new(BufWriter::new(File::create(out)?));
    let mut packed = Packed { files: 0, reused: 0, bytes: 0 };
    let prefix = format!("Payload/{app_name}");
    writer.add_directory("Payload/", SimpleFileOptions::default())?;
    add_dir(&mut writer, app, &prefix, "", &index, source.as_mut(), &mut packed)?;
    writer.finish()?.flush()?;
    packed.bytes = std::fs::metadata(out)?.len();
    Ok(packed)
}

fn add_dir(
    writer: &mut ZipWriter<BufWriter<File>>,
    dir: &Path,
    prefix: &str,
    relative: &str,
    index: &HashMap<String, (usize, u64, u32)>,
    mut source: Option<&mut ZipArchive<BufReader<File>>>,
    packed: &mut Packed,
) -> Result<(), Report> {
    writer.add_directory(format!("{prefix}/{relative}"), SimpleFileOptions::default())?;
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let child = format!("{relative}{name}");
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            writer.add_symlink(format!("{prefix}/{child}"), target.to_string_lossy(), SimpleFileOptions::default())?;
        } else if meta.is_dir() {
            add_dir(writer, &path, prefix, &format!("{child}/"), index, source.as_deref_mut(), packed)?;
        } else {
            packed.files += 1;
            let data = std::fs::read(&path)?;
            let target = format!("{prefix}/{child}");
            if let (Some(&(i, size, crc)), Some(archive)) = (index.get(&child), source.as_deref_mut()) {
                if size == data.len() as u64 && crc == crc32fast::hash(&data) {
                    writer.raw_copy_file_rename(archive.by_index_raw(i)?, target)?;
                    packed.reused += 1;
                    continue;
                }
            }
            let mode = file_mode(&meta, &data, index.get(&child).and_then(|&(i, _, _)| {
                source.as_deref_mut()?.by_index_raw(i).ok()?.unix_mode()
            }));
            let options = SimpleFileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(1))
                .unix_permissions(mode)
                .large_file(data.len() as u64 >= u32::MAX as u64);
            writer.start_file(target, options)?;
            writer.write_all(&data)?;
        }
    }
    Ok(())
}

/// The file's Unix mode for the IPA: its own on Unix. Elsewhere (Windows) the source IPA's
/// entry's, else executable for a Mach-O and read-only otherwise.
fn file_mode(meta: &std::fs::Metadata, data: &[u8], source_mode: Option<u32>) -> u32 {
    #[cfg(unix)]
    {
        let _ = (data, source_mode);
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        let mach_o = matches!(data.get(..4), Some([0xcf, 0xfa, 0xed, 0xfe] | [0xca, 0xfe, 0xba, 0xbe]));
        source_mode.unwrap_or(if mach_o { 0o755 } else { 0o644 })
    }
}

/// Packs the signed app at `app_path` (reusing `source_ipa`'s unchanged entries when given),
/// uploads the IPA to PublicStaging over the tunnel and installs it.
/// Returns `{ok, elapsedMs, packMs, uploadMs, bytes, files, reused, queued, closed}`.
///
/// An update of an app that is running is held by installd until the app closes, so the app is
/// closed first, as Xcode does (`close_running`), and `closed` says whether it was. The calling
/// app itself is never closed. With `detach_after_ms` > 0, stops waiting that long after asking
/// for the install and returns `queued: true`. Installing over the calling app itself needs
/// this, as does an app that couldn't be closed: installd applies the update when it closes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_install_ipa(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    app_path: *const c_char,
    source_ipa: *const c_char,
    detach_after_ms: u64,
) -> *mut c_char {
    reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let app_path = PathBuf::from(unsafe { text(app_path) }?);
        let source = if source_ipa.is_null() { None } else { Some(PathBuf::from(unsafe { text(source_ipa) }?)) };
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        runtime().block_on(crate::stop::stoppable(install(adapter, handshake, &app_path, source.as_deref(), detach_after_ms)))
    })())
}

/// Uploads `out` to `staged` on the device without panicking if writing or closing fails.
async fn upload_staged(afc: &mut AfcClient, out: &Path, staged: &str, total_bytes: u64) -> Result<(), Report> {
    let _ = afc.mk_dir("PublicStaging").await;
    let mut file = afc.open(staged.to_string(), AfcFopenMode::WrOnly).await?;
    let mut uploaded = 0u64;
    let write_result = async {
        let mut input = BufReader::new(File::open(out)?);
        let mut buffer = vec![0u8; UPLOAD_CHUNK];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_entire(&buffer[..read])
                .await
                .context(format!("AFC upload of {staged} failed after {uploaded} of {total_bytes} bytes"))?;
            uploaded += read as u64;
        }
        Ok::<_, Report>(())
    }
    .await;
    let fd = file.as_raw_fd();
    // `idevice` asserts `!self.dropped` in Drop and only marks `dropped = true` after `FileClose`
    // succeeds, so any write/close error panics in debug builds instead of returning the failure.
    // Detach the handle first: on success `send_op(FileClose)` sends the identical wire close; on
    // error, dropping `afc` closes the session so iOS reclaims the descriptor automatically.
    std::mem::forget(file);
    if let Err(error) = write_result {
        tracing::warn!(staged, uploaded, total_bytes, "AFC upload failed: {error}");
        return Err(error);
    }
    afc.send_op(AfcOpcode::FileClose, fd.to_le_bytes().to_vec(), Vec::new())
        .await
        .context(format!("AFC close of {staged} failed after {uploaded} of {total_bytes} bytes"))
        .inspect_err(|error| tracing::warn!(staged, uploaded, total_bytes, "AFC FileClose failed: {error}"))?;
    Ok(())
}

/// See `apparition_install_ipa`.
pub async fn install(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    app_path: &Path,
    source: Option<&Path>,
    detach_after_ms: u64,
) -> Result<serde_json::Value, Report> {
    let started = Instant::now();
    let out = std::env::temp_dir().join(format!(
        "{}.ipa",
        app_path.file_stem().ok_or_else(|| report!("bad app path"))?.to_string_lossy()
    ));
    let packed = pack(app_path, source, &out).context("Packing the IPA failed")?;
    let pack_ms = started.elapsed().as_millis() as u64;
    tracing::info!("packed {} files ({} reused) into {} bytes in {pack_ms} ms", packed.files, packed.reused, packed.bytes);

    let bundle_id = crate::api::bundle_id(app_path);
    let result = upload_and_install(adapter, handshake, &out, bundle_id.as_deref(), detach_after_ms, started).await;
    let _ = std::fs::remove_file(&out);
    let (upload_ms, queued, closed) = result?;
    Ok(json!({
        "elapsedMs": started.elapsed().as_millis() as u64,
        "packMs": pack_ms,
        "uploadMs": upload_ms,
        "bytes": packed.bytes,
        "files": packed.files,
        "reused": packed.reused,
        "queued": queued,
        "closed": closed,
    }))
}

/// Installs an IPA that is already signed for the device, as it is: uploaded to PublicStaging
/// and handed to installd, with no unpacking, signing or repacking. For build tools that sign
/// themselves (`apparition push`). `bundle_id` is the app's, so a running copy is closed first.
/// Returns `{elapsedMs, uploadMs, bytes, queued, closed}`.
pub async fn install_signed_ipa(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    ipa: &Path,
    bundle_id: Option<&str>,
    detach_after_ms: u64,
) -> Result<serde_json::Value, Report> {
    let started = Instant::now();
    let bytes = std::fs::metadata(ipa)?.len();
    let (upload_ms, queued, closed) = upload_and_install(adapter, handshake, ipa, bundle_id, detach_after_ms, started).await?;
    Ok(json!({
        "elapsedMs": started.elapsed().as_millis() as u64,
        "uploadMs": upload_ms,
        "bytes": bytes,
        "queued": queued,
        "closed": closed,
    }))
}

/// Uploads `ipa` to PublicStaging and installs it; `(upload_ms, queued, closed)`.
async fn upload_and_install(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    out: &Path,
    bundle_id: Option<&str>,
    detach_after_ms: u64,
    started: Instant,
) -> Result<(u64, bool, bool), Report> {
    let base_ms = started.elapsed().as_millis() as u64;
    {
        let staged = format!("PublicStaging/{}", out.file_name().ok_or_else(|| report!("bad IPA path"))?.to_string_lossy());
        let mut afc = AfcClient::connect_rsd(adapter, handshake).await?;
        upload_staged(&mut afc, out, &staged, std::fs::metadata(out)?.len()).await?;
        let upload_ms = started.elapsed().as_millis() as u64 - base_ms;

        // Closed only now, once the upload is done, so the app stays usable while it's sent.
        let closed = match bundle_id {
            Some(bundle_id) => close_running(adapter, handshake, bundle_id).await,
            None => false,
        };

        let mut installer = InstallationProxyClient::connect_rsd(adapter, handshake).await?;
        if detach_after_ms == 0 {
            installer.install(staged, None).await.context("Install failed")?;
            return Ok::<_, Report>((upload_ms, false, closed));
        }
        match tokio::time::timeout(std::time::Duration::from_millis(detach_after_ms), installer.install(staged, None)).await {
            Ok(done) => {
                done.context("Install failed")?;
                Ok((upload_ms, false, closed))
            }
            Err(_) => {
                tracing::info!("install still waiting after {detach_after_ms} ms; leaving it queued");
                Ok((upload_ms, true, closed))
            }
        }
    }
}

/// How long a running app gets to exit on SIGTERM before it's killed, and then to be gone.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
const KILL_WAIT: Duration = Duration::from_secs(2);
/// The most closing may take in all, so a device that stops answering can't hold up the install.
const CLOSE_LIMIT: Duration = Duration::from_secs(15);

/// Closes `bundle_id` on the device if it's running, so installd can apply an update of it
/// straight away instead of holding it until the person closes the app: SIGTERM through
/// CoreDevice's appservice (what `devicectl device process terminate` sends), then SIGKILL if
/// it's still running after `CLOSE_GRACE`. The calling app is left alone: closing it would end
/// this install. True when an app was running and is now closed; anything that goes wrong only
/// leaves the update to wait, as before, so it's logged rather than returned.
pub async fn close_running(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
) -> bool {
    match tokio::time::timeout(CLOSE_LIMIT, try_close(adapter, handshake, bundle_id)).await.unwrap_or_else(|_| Err(report!("timed out").into())) {
        Ok(closed) => closed,
        Err(error) => {
            tracing::warn!("couldn't close {bundle_id} before updating it, so the update waits until it's closed: {}", crate::errors::describe(&error));
            false
        }
    }
}

async fn try_close(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut idevice::rsd::RsdHandshake,
    bundle_id: &str,
) -> Result<bool, Report> {
    use crate::jit::{AppProcess, app_process};
    let AppProcess::Running { pid, executable } = app_process(adapter, handshake, bundle_id).await? else {
        return Ok(false);
    };
    if is_this_app(&executable) {
        tracing::info!("{bundle_id} is this app; its update applies when it closes");
        return Ok(false);
    }
    tracing::info!("closing {bundle_id} (pid {pid}) so its update applies now");
    let mut apps = AppServiceClient::connect_rsd(adapter, handshake).await.context("appservice connect failed")?;
    apps.send_signal(pid, SIGTERM).await.context("Can't signal the running app")?;
    if gone(&mut apps, pid, CLOSE_GRACE).await? {
        return Ok(true);
    }
    tracing::info!("{bundle_id} still running after SIGTERM; killing it");
    apps.send_signal(pid, SIGKILL).await.context("Can't kill the running app")?;
    if gone(&mut apps, pid, KILL_WAIT).await? {
        return Ok(true);
    }
    bail!("pid {pid} is still running after SIGKILL")
}

const SIGTERM: u32 = 15;
const SIGKILL: u32 = 9;

/// Waits up to `limit` for `pid` to leave the device's process list.
async fn gone<R: idevice::ReadWrite>(apps: &mut AppServiceClient<R>, pid: u32, limit: Duration) -> Result<bool, Report> {
    let deadline = Instant::now() + limit;
    loop {
        let processes = apps.list_processes().await.context("Can't list the device's processes")?;
        if !processes.iter().any(|p| p.pid == pid) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Whether the process at `executable` (a CoreDevice executable URL) is this one. Matched on the
/// install's own directory (`<container UUID>/<name>.app`), which is unique per install, so an
/// app of the same name on another device never matches; the pid alone could.
fn is_this_app(executable: &str) -> bool {
    std::env::current_exe().is_ok_and(|this| is_install_of(executable, &this))
}

/// Whether `executable` is inside the same installed bundle as the binary at `this`.
fn is_install_of(executable: &str, this: &Path) -> bool {
    let Some(bundle) = this.parent().filter(|dir| dir.extension().is_some_and(|ext| ext == "app")) else {
        return false;
    };
    let Some(container) = bundle.parent().and_then(Path::file_name) else { return false };
    let suffix = format!("{}/{}/", container.to_string_lossy(), bundle.file_name().unwrap_or_default().to_string_lossy());
    executable.contains(&suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_own_install_only() {
        let this = Path::new("/private/var/containers/Bundle/Application/1A2B/Apparition.app/Apparition");
        let own = "file:///private/var/containers/Bundle/Application/1A2B/Apparition.app/Apparition";
        let reinstalled = "file:///private/var/containers/Bundle/Application/9F9F/Apparition.app/Apparition";
        let other = "file:///private/var/containers/Bundle/Application/1A2B/Other.app/Other";
        assert!(is_install_of(own, this));
        assert!(!is_install_of(reinstalled, this));
        assert!(!is_install_of(other, this));
        // A Mac app or a command line is never on the device it installs to.
        assert!(!is_install_of(own, Path::new("/Applications/Apparition.app/Contents/MacOS/Apparition")));
        assert!(!is_install_of(own, Path::new("/usr/local/bin/apparition")));
    }
}

#[cfg(test)]
mod upload_tests {
    use super::*;
    use idevice::afc::{
        MAGIC,
        errors::AfcError,
        packet::{AfcPacket, AfcPacketHeader},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_afc_op(stream: &mut tokio::io::DuplexStream) -> Result<(u64, u64, Vec<u8>), std::io::Error> {
        let mut hdr = [0u8; AfcPacketHeader::LEN as usize];
        stream.read_exact(&mut hdr).await?;
        let entire = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let hdr_payload_len = u64::from_le_bytes(hdr[16..24].try_into().unwrap());
        let num = u64::from_le_bytes(hdr[24..32].try_into().unwrap());
        let op = u64::from_le_bytes(hdr[32..40].try_into().unwrap());
        let rest_len = (entire - AfcPacketHeader::LEN) as usize;
        let mut rest = vec![0u8; rest_len];
        stream.read_exact(&mut rest).await?;
        let body_start = (hdr_payload_len - AfcPacketHeader::LEN) as usize;
        Ok((op, num, rest[body_start..].to_vec()))
    }

    async fn send_afc_reply(
        stream: &mut tokio::io::DuplexStream,
        num: u64,
        operation: AfcOpcode,
        header_payload: Vec<u8>,
    ) {
        let header_len = header_payload.len() as u64 + AfcPacketHeader::LEN;
        let pkt = AfcPacket {
            header: AfcPacketHeader {
                magic: MAGIC,
                entire_len: header_len,
                header_payload_len: header_len,
                packet_num: num,
                operation,
            },
            header_payload,
            payload: Vec::new(),
        };
        stream.write_all(&pkt.serialize()).await.unwrap();
    }

    fn temp_ipa_fixture(contents: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "apparition-ipa-upload-{}-{}.ipa",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[tokio::test]
    async fn upload_staged_closes_fd_on_success() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let payload = b"hello ipa";
        let path = temp_ipa_fixture(payload);
        let srv = tokio::spawn(async move {
            let mut ops = Vec::new();
            let mut written = Vec::new();
            while let Ok((op, num, data)) = read_afc_op(&mut server).await {
                ops.push(op);
                if op == AfcOpcode::FileOpen as u64 {
                    send_afc_reply(&mut server, num, AfcOpcode::FileOpenRes, 7u64.to_le_bytes().to_vec()).await;
                } else {
                    if op == AfcOpcode::Write as u64 {
                        written.extend_from_slice(&data);
                    }
                    send_afc_reply(&mut server, num, AfcOpcode::Status, (AfcError::Success as u64).to_le_bytes().to_vec()).await;
                    if op == AfcOpcode::FileClose as u64 {
                        break;
                    }
                }
            }
            (ops, written)
        });
        let mut afc = AfcClient::new(idevice::Idevice::new(Box::new(client), "test"));
        let res = upload_staged(&mut afc, &path, "PublicStaging/Test.ipa", payload.len() as u64).await;
        let _ = std::fs::remove_file(&path);
        assert!(res.is_ok(), "{res:?}");
        let (ops, written) = srv.await.unwrap();
        assert_eq!(written, payload);
        assert_eq!(ops, vec![
            AfcOpcode::MakeDir as u64,
            AfcOpcode::FileOpen as u64,
            AfcOpcode::Write as u64,
            AfcOpcode::FileClose as u64,
        ]);
    }

    #[tokio::test]
    async fn upload_staged_returns_write_error_without_drop_panic() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let path = temp_ipa_fixture(b"hello ipa");
        tokio::spawn(async move {
            while let Ok((op, num, _)) = read_afc_op(&mut server).await {
                if op == AfcOpcode::FileOpen as u64 {
                    send_afc_reply(&mut server, num, AfcOpcode::FileOpenRes, 7u64.to_le_bytes().to_vec()).await;
                } else if op == AfcOpcode::Write as u64 {
                    send_afc_reply(&mut server, num, AfcOpcode::Status, (AfcError::NoSpaceLeft as u64).to_le_bytes().to_vec()).await;
                    break;
                } else {
                    send_afc_reply(&mut server, num, AfcOpcode::Status, (AfcError::Success as u64).to_le_bytes().to_vec()).await;
                }
            }
        });
        let mut afc = AfcClient::new(idevice::Idevice::new(Box::new(client), "test"));
        let err = upload_staged(&mut afc, &path, "PublicStaging/Test.ipa", 9).await.unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(crate::errors::describe(&err).to_ascii_lowercase().contains("space"), "{err}");
    }

    #[tokio::test]
    async fn upload_staged_returns_close_error_without_drop_panic() {
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let path = temp_ipa_fixture(b"hello ipa");
        tokio::spawn(async move {
            while let Ok((op, num, _)) = read_afc_op(&mut server).await {
                if op == AfcOpcode::FileOpen as u64 {
                    send_afc_reply(&mut server, num, AfcOpcode::FileOpenRes, 7u64.to_le_bytes().to_vec()).await;
                } else if op == AfcOpcode::FileClose as u64 {
                    send_afc_reply(&mut server, num, AfcOpcode::Status, (AfcError::IoError as u64).to_le_bytes().to_vec()).await;
                    break;
                } else {
                    send_afc_reply(&mut server, num, AfcOpcode::Status, (AfcError::Success as u64).to_le_bytes().to_vec()).await;
                }
            }
        });
        let mut afc = AfcClient::new(idevice::Idevice::new(Box::new(client), "test"));
        assert!(upload_staged(&mut afc, &path, "PublicStaging/Test.ipa", 9).await.is_err());
        let _ = std::fs::remove_file(&path);
    }
}
