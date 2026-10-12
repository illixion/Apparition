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
    time::Instant,
};

use idevice::{
    RsdService,
    afc::{AfcClient, opcode::AfcFopenMode},
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
            let name = entry.name().to_string();
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
/// Returns `{ok, elapsedMs, packMs, uploadMs, bytes, files, reused, queued}`.
///
/// With `detach_after_ms` > 0, stops waiting that long after asking for the install and
/// returns `queued: true`. Installing over the calling app itself needs this: installd holds
/// the update until the app closes, as it does for App Store updates, and applies it then.
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
        runtime().block_on(install(adapter, handshake, &app_path, source.as_deref(), detach_after_ms))
    })())
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

    let result = async {
        let staged = format!("PublicStaging/{}", out.file_name().unwrap().to_string_lossy());
        let mut afc = AfcClient::connect_rsd(adapter, handshake).await?;
        let _ = afc.mk_dir("PublicStaging").await;
        let mut file = afc.open(staged.clone(), AfcFopenMode::WrOnly).await?;
        let mut input = BufReader::new(File::open(&out)?);
        let mut buffer = vec![0u8; UPLOAD_CHUNK];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_entire(&buffer[..read]).await?;
        }
        file.close().await?;
        let upload_ms = started.elapsed().as_millis() as u64 - pack_ms;

        let mut installer = InstallationProxyClient::connect_rsd(adapter, handshake).await?;
        if detach_after_ms == 0 {
            installer.install(staged, None).await.context("Install failed")?;
            return Ok::<_, Report>((upload_ms, false));
        }
        match tokio::time::timeout(std::time::Duration::from_millis(detach_after_ms), installer.install(staged, None)).await {
            Ok(done) => {
                done.context("Install failed")?;
                Ok((upload_ms, false))
            }
            Err(_) => {
                tracing::info!("install still waiting after {detach_after_ms} ms; leaving it queued");
                Ok((upload_ms, true))
            }
        }
    }
    .await;
    let _ = std::fs::remove_file(&out);
    let (upload_ms, queued) = result?;
    Ok(json!({
        "elapsedMs": started.elapsed().as_millis() as u64,
        "packMs": pack_ms,
        "uploadMs": upload_ms,
        "bytes": packed.bytes,
        "files": packed.files,
        "reused": packed.reused,
        "queued": queued,
    }))
}
