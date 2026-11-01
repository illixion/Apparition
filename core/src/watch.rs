//! Apple Watches paired with an iPhone, through the phone's companion_proxy.
//!
//! An iPhone app can carry a watchOS app in `Watch/`. iOS copies it to the paired Watch after
//! the phone installs it, and the Watch's installd checks the watch app's own profile against
//! the Watch's UDID. So signing such an app has to register the Watch on the team too, and the
//! phone is the only one that can say which Watch that is.

use std::{collections::BTreeMap, path::Path};

use idevice::{RsdService, companion_proxy::CompanionProxy, rsd::RsdHandshake, tcp::handle::AdapterHandle};
use rootcause::prelude::*;

/// A Watch paired with the phone, as its registry describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedWatch {
    pub udid: String,
    pub name: String,
    #[serde(default)]
    pub product_type: String,
    #[serde(default)]
    pub os_version: String,
}

/// The Watches paired with the phone at the other end of the tunnel. Empty on a device with no
/// companion_proxy (anything but an iPhone) or with no Watch paired.
pub async fn paired(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Vec<PairedWatch>, Report> {
    if !handshake.services.contains_key(CompanionProxy::rsd_service_name().as_ref()) {
        return Ok(Vec::new());
    }
    // companion_proxy answers one command per connection.
    let gizmos = CompanionProxy::connect_rsd(adapter, handshake)
        .await
        .context("companion_proxy connect failed")?
        .get_device_registry()
        .await
        .context("The iPhone didn't list its paired Watches")?;
    let mut watches = Vec::new();
    for gizmo in gizmos {
        let mut text = async |key: &str| -> Option<String> {
            let mut proxy = CompanionProxy::connect_rsd(adapter, handshake).await.ok()?;
            proxy.get_value(gizmo.clone(), key).await.ok()?.as_string().map(str::to_string)
        };
        // The registry is keyed by the Watch's UDID, the one the team registers.
        let udid = text("UniqueDeviceID").await.unwrap_or_else(|| gizmo.clone());
        let name = text("DeviceName").await.unwrap_or_else(|| "Apple Watch".to_string());
        let product_type = text("ProductType").await.unwrap_or_default();
        let os_version = text("ProductVersion").await.unwrap_or_default();
        watches.push(PairedWatch { udid, name, product_type, os_version });
    }
    Ok(watches)
}

/// `paired`, remembered under the phone's UDID (read from lockdown) for signing to find.
pub async fn paired_and_remembered(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Vec<PairedWatch>, Report> {
    let watches = paired(adapter, handshake).await?;
    let mut lockdown = idevice::lockdown::LockdownClient::connect_rsd(adapter, handshake).await.context("lockdown connect failed")?;
    let udid = lockdown.get_value(Some("UniqueDeviceID"), None).await?.as_string().unwrap_or_default().to_string();
    remember(&udid, &watches);
    Ok(watches)
}

/// The Apple Watches paired with the iPhone at the other end of the tunnel, `{watches: [...]}`;
/// empty on any other device. Call before signing an app with a watch app: signing registers
/// the Watches this last reported for the phone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_paired_watches(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let watches = crate::runtime().block_on(crate::stop::stoppable(paired_and_remembered(adapter, handshake)))?;
        Ok(serde_json::json!({ "watches": watches }))
    })())
}

/// Installs the signed watch app at `app_path` (`Watch/X.app` of a signed app) on the Watch
/// `watch_udid`, through the iPhone at the other end of the tunnel. The Watch's trust record is
/// kept under `record_dir`; the first time, the Watch asks to trust this device and `waiting`
/// is called (with `context`) when that prompt is up. Returns `{elapsedMs, bytes, files}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_install_watch_app(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    watch_udid: *const std::ffi::c_char,
    record_dir: *const std::ffi::c_char,
    app_path: *const std::ffi::c_char,
    waiting: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let udid = unsafe { crate::text(watch_udid) }?;
        let dir = std::path::PathBuf::from(unsafe { crate::text(record_dir) }?);
        let app = std::path::PathBuf::from(unsafe { crate::text(app_path) }?);
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let context = context as usize;
        let on_waiting = move |_: &str| {
            if let Some(waiting) = waiting {
                unsafe { waiting(context as *mut std::ffi::c_void) }
            }
        };
        crate::runtime().block_on(crate::stop::stoppable(async {
            let mut companion = Companion::connect(adapter, handshake, &udid, &dir, &on_waiting).await?;
            companion.install(&app, &|_| {}).await
        }))
    })())
}

/// Removes `bundle_id` from the Watch `watch_udid` through the iPhone at the other end of the
/// tunnel (see `apparition_install_watch_app` for the other arguments). Returns `{}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_uninstall_watch_app(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    watch_udid: *const std::ffi::c_char,
    record_dir: *const std::ffi::c_char,
    bundle_id: *const std::ffi::c_char,
    waiting: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let udid = unsafe { crate::text(watch_udid) }?;
        let dir = std::path::PathBuf::from(unsafe { crate::text(record_dir) }?);
        let bundle_id = unsafe { crate::text(bundle_id) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let context = context as usize;
        let on_waiting = move |_: &str| {
            if let Some(waiting) = waiting {
                unsafe { waiting(context as *mut std::ffi::c_void) }
            }
        };
        crate::runtime().block_on(crate::stop::stoppable(async {
            let mut companion = Companion::connect(adapter, handshake, &udid, &dir, &on_waiting).await?;
            companion.uninstall(&bundle_id).await?;
            Ok(serde_json::json!({}))
        }))
    })())
}

/// What each iPhone last said about its Watches, by the phone's UDID: kept in memory for this
/// process, and in `paired-watches.json` beside the Apple ID's data once signing used it, so a
/// refresh that signs without a tunnel open still registers the Watch.
static KNOWN: std::sync::Mutex<BTreeMap<String, Vec<PairedWatch>>> = std::sync::Mutex::new(BTreeMap::new());

const FILE: &str = "paired-watches.json";

/// Notes the Watches `phone` reported, for signing to use.
pub fn remember(phone: &str, watches: &[PairedWatch]) {
    KNOWN.lock().unwrap().insert(phone.to_owned(), watches.to_vec());
}

/// The Watches `phone` last reported, from this process or from `dir`; saves them to `dir`.
pub fn known(dir: &Path, phone: &str) -> Vec<PairedWatch> {
    let path = dir.join(FILE);
    let mut saved: BTreeMap<String, Vec<PairedWatch>> =
        std::fs::read(&path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
    let Some(fresh) = KNOWN.lock().unwrap().get(phone).cloned() else {
        return saved.remove(phone).unwrap_or_default();
    };
    if saved.get(phone) != Some(&fresh) {
        saved.insert(phone.to_owned(), fresh.clone());
        if let Ok(data) = serde_json::to_vec_pretty(&saved) {
            let _ = std::fs::write(&path, data);
        }
    }
    fresh
}

/// Lockdown's port on every Apple device, the Watch included.
const LOCKDOWN_PORT: u16 = 62078;

/// A paired Watch's lockdown services, reached through its iPhone: the phone's companion_proxy
/// forwards a port on the Watch to a port on the phone, which the phone's tunnel then reaches.
///
/// This is the route Xcode took before CoreDevice, and it still carries everything an install
/// needs: lockdown pairing (the Watch asks to trust the computer), then installation_proxy and
/// AFC. It needs no Bluetooth and no Wi-Fi on the Watch, unlike CoreDevice's own route, which
/// pairs over a Bluetooth LE control channel and tunnels over peer-to-peer Wi-Fi.
pub struct Companion<'a> {
    adapter: &'a mut AdapterHandle,
    handshake: &'a mut RsdHandshake,
    lockdown: idevice::lockdown::LockdownClient,
    pairing: idevice::pairing_file::PairingFile,
    /// Each forwarding lives as long as the companion_proxy connection that asked for it.
    proxies: Vec<CompanionProxy>,
}

/// Forwards `port` on the Watch through the phone and connects to it. The forwarding lasts as
/// long as the companion_proxy connection that asked for it, so that goes in `proxies`.
async fn forward(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, proxies: &mut Vec<CompanionProxy>, port: u16) -> Result<Box<dyn idevice::ReadWrite>, Report> {
    let mut proxy = CompanionProxy::connect_rsd(adapter, handshake).await.context("companion_proxy connect failed")?;
    let local = proxy.start_forwarding_service_port(port, None, None).await.context("The iPhone wouldn't forward a port to the Watch")?;
    let stream = adapter.connect(local).await.context("Connecting to the forwarded Watch port failed")?;
    proxies.push(proxy);
    Ok(Box::new(stream))
}

async fn open_lockdown(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, proxies: &mut Vec<CompanionProxy>) -> Result<idevice::lockdown::LockdownClient, Report> {
    let stream = forward(adapter, handshake, proxies, LOCKDOWN_PORT).await?;
    Ok(idevice::lockdown::LockdownClient::new(idevice::Idevice::new(stream, "apparition")))
}

impl<'a> Companion<'a> {
    /// Opens lockdown on the Watch `udid`, through the phone at the other end of the tunnel,
    /// and starts a session. The trust record lives in `dir`; the first time, the Watch shows
    /// a prompt and `waiting` is told to ask the user to tap Trust there.
    pub async fn connect(
        adapter: &'a mut AdapterHandle,
        handshake: &'a mut RsdHandshake,
        udid: &str,
        dir: &Path,
        waiting: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Companion<'a>, Report> {
        let record = dir.join("watches").join(format!("{udid}.lockdown.plist"));
        let saved = idevice::pairing_file::PairingFile::read_from_file(&record).ok();
        let mut proxies = Vec::new();
        let mut lockdown = open_lockdown(adapter, handshake, &mut proxies).await?;
        let device_udid = lockdown.get_value(Some("UniqueDeviceID"), None).await.ok().and_then(|v| v.as_string().map(str::to_owned));
        if device_udid.as_deref() != Some(udid) {
            bail!("The iPhone forwarded a different device ({}) instead of the Watch {udid}", device_udid.unwrap_or_default());
        }
        let pairing = match saved {
            Some(file) if lockdown.start_session(&file).await.is_ok() => file,
            _ => {
                // No record, or the Watch forgot it: pair again. Lockdown drops the connection
                // while its prompt is up, so each attempt reconnects.
                waiting("Tap Trust on the Watch");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
                let file = loop {
                    lockdown = open_lockdown(adapter, handshake, &mut proxies).await?;
                    match lockdown.pair(crate::api::random_uuid(), crate::api::random_uuid(), Some("Apparition")).await {
                        Ok(file) => break file,
                        Err(idevice::IdeviceError::UserDeniedPairing) => bail!("The Watch didn't trust this computer"),
                        Err(idevice::IdeviceError::PasswordProtected) => bail!("Unlock the Watch, then try again"),
                        Err(e) if std::time::Instant::now() < deadline => {
                            tracing::debug!("watch pairing attempt failed: {e}; retrying");
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                        Err(e) => return Err(e).context("Pairing with the Watch failed")?,
                    }
                };
                std::fs::create_dir_all(record.parent().unwrap())?;
                std::fs::write(&record, file.clone().serialize()?)?;
                lockdown = open_lockdown(adapter, handshake, &mut proxies).await?;
                lockdown.start_session(&file).await.context("The Watch accepted the pairing but refused a session")?;
                file
            }
        };
        Ok(Companion { adapter, handshake, lockdown, pairing, proxies })
    }

    /// A lockdown service on the Watch, by name, with TLS when lockdown says so.
    pub async fn service(&mut self, name: &str) -> Result<idevice::Idevice, Report> {
        let (port, ssl) = self.lockdown.start_service(name).await.context(format!("The Watch wouldn't start {name}"))?;
        let stream = forward(self.adapter, self.handshake, &mut self.proxies, port).await?;
        let mut idevice = idevice::Idevice::new(stream, "apparition");
        if ssl {
            idevice.start_session(&self.pairing, false).await.context("TLS to the Watch service failed")?;
        }
        Ok(idevice)
    }

    /// The Watch's name, model and version, from lockdown.
    pub async fn info(&mut self) -> Result<PairedWatch, Report> {
        let mut text = async |key: &str| -> String {
            self.lockdown.get_value(Some(key), None).await.ok().and_then(|v| v.as_string().map(str::to_owned)).unwrap_or_default()
        };
        Ok(PairedWatch { udid: text("UniqueDeviceID").await, name: text("DeviceName").await, product_type: text("ProductType").await, os_version: text("ProductVersion").await })
    }

    /// The display name of `bundle_id` if the Watch has it.
    pub async fn installed_app(&mut self, bundle_id: &str) -> Result<Option<String>, Report> {
        let mut installer = idevice::installation_proxy::InstallationProxyClient::new(self.service("com.apple.mobile.installation_proxy").await?);
        let apps = installer.get_apps(Some("User"), Some(vec![bundle_id.to_string()])).await?;
        Ok(apps.get(bundle_id).map(|info| {
            let info = info.as_dictionary();
            let name = |key: &str| info.and_then(|i| i.get(key)).and_then(plist::Value::as_string);
            name("CFBundleDisplayName").or_else(|| name("CFBundleName")).unwrap_or(bundle_id).to_string()
        }))
    }

    /// Removes `bundle_id` and its data from the Watch through its installation_proxy.
    pub async fn uninstall(&mut self, bundle_id: &str) -> Result<(), Report> {
        let mut installer = idevice::installation_proxy::InstallationProxyClient::new(self.service("com.apple.mobile.installation_proxy").await?);
        installer.uninstall(bundle_id.to_string(), None).await.context("The Watch refused to remove the app")?;
        Ok(())
    }

    /// Installs the signed Watch app at `app_path` on the Watch, streamed over
    /// `com.apple.streaming_zip_conduit`, the way Xcode did before CoreDevice: a Watch has no
    /// AFC to stage an IPA through. Returns `{elapsedMs, bytes, files}`.
    pub async fn install(&mut self, app_path: &Path, progress: &(dyn Fn(u64) + Send + Sync)) -> Result<serde_json::Value, Report> {
        let started = std::time::Instant::now();
        let name = app_path.file_name().ok_or_else(|| report!("bad app path"))?.to_string_lossy().to_string();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut conduit = self.service("com.apple.streaming_zip_conduit").await?.get_socket().ok_or_else(|| report!("no conduit socket"))?;
        let entries = zip_entries(app_path, &format!("Payload/{name}/"))?;
        let total: u64 = entries.iter().map(|e| e.size).sum();
        let files = entries.iter().filter(|e| !e.directory).count();
        // The conduit speaks lockdown-style plists: a big-endian length, then XML.
        let mut init = Vec::new();
        plist::to_writer_xml(&mut init, &zip_conduit::init_transfer(&format!("{name}.ipa")))?;
        conduit.write_all(&(init.len() as u32).to_be_bytes()).await?;
        conduit.write_all(&init).await.context("The Watch didn't accept the transfer")?;
        let mut stream = zip_conduit::Stream::new(entries.len() as u64, total);
        conduit.write_all(&stream.start()).await?;
        let mut sent = 0u64;
        for entry in &entries {
            if entry.directory {
                conduit.write_all(&stream.directory(&entry.name)).await?;
                continue;
            }
            let data = std::fs::read(&entry.path)?;
            conduit.write_all(&stream.file(&entry.name, &data)).await?;
            conduit.write_all(&data).await?;
            sent += data.len() as u64;
            progress(sent * 100 / total.max(1));
        }
        conduit.write_all(&stream.finish()).await?;
        conduit.flush().await?;
        loop {
            let mut length = [0u8; 4];
            conduit.read_exact(&mut length).await.context("The Watch stopped answering during the install")?;
            let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
            conduit.read_exact(&mut body).await.context("The Watch stopped answering during the install")?;
            let update: plist::Dictionary = plist::from_bytes(&body).context("Unreadable reply from the Watch")?;
            if zip_conduit::finished(&update)? {
                break;
            }
        }
        Ok(serde_json::json!({ "elapsedMs": started.elapsed().as_millis() as u64, "bytes": total, "files": files }))
    }
}

/// One entry of the app, as the conduit's zip stream names it (`Payload/X.app/...`).
struct ZipEntry {
    name: String,
    path: std::path::PathBuf,
    size: u64,
    directory: bool,
}

/// `Payload/`, the app's folder and everything in it, folders before their contents.
fn zip_entries(app: &Path, prefix: &str) -> Result<Vec<ZipEntry>, Report> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<ZipEntry>) -> Result<(), Report> {
        let mut names: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let path = entry.path();
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                out.push(ZipEntry { name: format!("{name}/"), path: path.clone(), size: 0, directory: true });
                walk(&path, &format!("{name}/"), out)?;
            } else if metadata.is_file() {
                out.push(ZipEntry { name, path, size: metadata.len(), directory: false });
            }
        }
        Ok(())
    }
    let mut out = vec![
        ZipEntry { name: "Payload/".into(), path: app.into(), size: 0, directory: true },
        ZipEntry { name: prefix.to_string(), path: app.into(), size: 0, directory: true },
    ];
    walk(app, prefix, &mut out)?;
    Ok(out)
}

/// The wire format of `com.apple.streaming_zip_conduit`, after go-ios's `zipconduit` (MIT,
/// Daniel Paulus): an `InitTransfer` plist, then a zip stream of stored (uncompressed) local
/// file headers with no data descriptors, led by a `META-INF/com.apple.ZipMetadata.plist`
/// that announces the record count and total size, and ended by a bare central directory
/// signature. The device then reports progress in plists until `Status` is `DataComplete`.
mod zip_conduit {
    use plist::{Dictionary, Value};
    use rootcause::prelude::*;

    const METADATA: &str = "META-INF/com.apple.ZipMetadata.plist";
    /// The UT (extended timestamp) and ux extra fields Xcode sends; only their size matters.
    const EXTRA: [u8; 32] = [
        0x55, 0x54, 0x0D, 0x00, 0x07, 0xF3, 0xA2, 0xEC, 0x60, 0xF6, 0xA2, 0xEC, 0x60, 0xF3, 0xA2, 0xEC, 0x60, 0x75, 0x78, 0x0B, 0x00, 0x01, 0x04,
        0xF5, 0x01, 0x00, 0x00, 0x04, 0x14, 0x00, 0x00, 0x00,
    ];

    pub fn init_transfer(ipa_name: &str) -> Dictionary {
        let mut options = Dictionary::new();
        options.insert("DisableDeltaTransfer".into(), Value::Integer(1.into()));
        options.insert("InstallDeltaTypeKey".into(), Value::String("InstallDeltaTypeSparseIPAFiles".into()));
        options.insert("IsUserInitiated".into(), Value::Integer(1.into()));
        options.insert("PackageType".into(), Value::String("Customer".into()));
        options.insert("PreferWifi".into(), Value::Integer(1.into()));
        let mut init = Dictionary::new();
        init.insert("InstallOptionsDictionary".into(), Value::Dictionary(options));
        init.insert("InstallTransferredDirectory".into(), Value::Integer(1.into()));
        init.insert("MediaSubdir".into(), Value::String(format!("PublicStaging/{ipa_name}")));
        init.insert("UserInitiatedTransfer".into(), Value::Integer(0.into()));
        init
    }

    /// Builds the stream's framing; the caller sends each file's data after its header.
    pub struct Stream {
        records: u64,
        total: u64,
    }

    impl Stream {
        pub fn new(records: u64, total: u64) -> Self {
            Stream { records, total }
        }

        /// The metadata folder and file that open the stream.
        pub fn start(&mut self) -> Vec<u8> {
            let mut metadata = Dictionary::new();
            // Xcode's values: 0o40755 directories, 0o644 files (as a signed 32-bit mode word).
            metadata.insert("StandardDirectoryPerms".into(), Value::Integer(16877.into()));
            metadata.insert("StandardFilePerms".into(), Value::Integer((-32348i64).into()));
            metadata.insert("RecordCount".into(), Value::Integer((2 + self.records).into()));
            metadata.insert("TotalUncompressedBytes".into(), Value::Integer(self.total.into()));
            metadata.insert("Version".into(), Value::Integer(2.into()));
            let mut data = Vec::new();
            plist::to_writer_xml(&mut data, &metadata).expect("plist serialises");
            let mut out = self.directory("META-INF/");
            out.extend(self.file(METADATA, &data));
            out.extend(data);
            out
        }

        pub fn directory(&self, name: &str) -> Vec<u8> {
            header(name, 0, 0)
        }

        pub fn file(&self, name: &str, data: &[u8]) -> Vec<u8> {
            header(name, data.len() as u32, crc32fast::hash(data))
        }

        pub fn finish(&self) -> Vec<u8> {
            vec![0x50, 0x4b, 0x01, 0x02]
        }
    }

    /// A local file header, stored, with a fixed timestamp and Xcode's extra field.
    fn header(name: &str, size: u32, crc: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(30 + name.len() + EXTRA.len());
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend(20u16.to_le_bytes()); // version needed
        out.extend(0u16.to_le_bytes()); // flags: no data descriptor
        out.extend(0u16.to_le_bytes()); // stored
        out.extend(0xBDEFu16.to_le_bytes());
        out.extend(0x52ECu16.to_le_bytes());
        out.extend(crc.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend(size.to_le_bytes());
        out.extend((name.len() as u16).to_le_bytes());
        out.extend((EXTRA.len() as u16).to_le_bytes());
        out.extend(name.as_bytes());
        out.extend(EXTRA);
        out
    }

    /// Whether a progress plist says the install finished; an error in it is returned.
    pub fn finished(update: &Dictionary) -> Result<bool, Report> {
        if let Some(status) = update.get("Status").and_then(Value::as_string) {
            return if status == "DataComplete" { Ok(true) } else { Err(report!("The Watch reported {status}")) };
        }
        let progress = update.get("InstallProgressDict").and_then(Value::as_dictionary).ok_or_else(|| report!("Unexpected reply from the Watch: {update:?}"))?;
        if let Some(error) = progress.get("Error").and_then(Value::as_string) {
            let description = progress.get("ErrorDescription").and_then(Value::as_string).unwrap_or("");
            bail!("The Watch refused the app: {error} {description}");
        }
        Ok(false)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn local_file_headers_are_stored_and_sized_for_the_name_and_extra() {
            let h = header("Payload/A.app/x", 5, 0xdeadbeef);
            assert_eq!(h.len(), 30 + "Payload/A.app/x".len() + 32);
            assert_eq!(&h[..4], &[0x50, 0x4b, 0x03, 0x04]);
            assert_eq!(u16::from_le_bytes([h[8], h[9]]), 0, "stored");
            assert_eq!(u32::from_le_bytes([h[14], h[15], h[16], h[17]]), 0xdeadbeef);
            assert_eq!(u32::from_le_bytes([h[18], h[19], h[20], h[21]]), 5);
            let mut stream = Stream::new(3, 10);
            let start = stream.start();
            assert!(start.windows(METADATA.len()).any(|w| w == METADATA.as_bytes()));
            let key: &[u8] = b"<key>RecordCount</key>";
            assert!(start.windows(key.len()).any(|w| w == key));
        }

        #[test]
        fn progress_plists_end_on_data_complete_or_an_error() {
            let mut done = Dictionary::new();
            done.insert("Status".into(), Value::String("DataComplete".into()));
            assert!(finished(&done).unwrap());
            let mut progress = Dictionary::new();
            let mut inner = Dictionary::new();
            inner.insert("PercentComplete".into(), Value::Integer(40.into()));
            progress.insert("InstallProgressDict".into(), Value::Dictionary(inner.clone()));
            assert!(!finished(&progress).unwrap());
            inner.insert("Error".into(), Value::String("ApplicationVerificationFailed".into()));
            progress.insert("InstallProgressDict".into(), Value::Dictionary(inner));
            assert!(finished(&progress).unwrap_err().to_string().contains("ApplicationVerificationFailed"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_watches_outlive_the_process_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(known(dir.path(), "phone-a").is_empty());
        let watch = PairedWatch { udid: "w".into(), name: "Watch".into(), product_type: "Watch7,12".into(), os_version: "27.0".into() };
        remember("phone-a", std::slice::from_ref(&watch));
        assert_eq!(known(dir.path(), "phone-a"), [watch.clone()]);
        KNOWN.lock().unwrap().remove("phone-a");
        assert_eq!(known(dir.path(), "phone-a"), [watch]);
        // A phone that reported none forgets the Watch it had.
        remember("phone-a", &[]);
        assert!(known(dir.path(), "phone-a").is_empty());
        KNOWN.lock().unwrap().remove("phone-a");
        assert!(known(dir.path(), "phone-a").is_empty());
    }

    #[test]
    fn renaming_the_app_renames_its_watch_app_and_the_ids_they_name_each_other_by() {
        use isideload::sideload::application::Application;
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("Test.app");
        let write = |relative: &str, extra: &str, id: &str| {
            let folder = app.join(relative);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("Info.plist"), format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>T</string>{extra}</dict></plist>"
            )).unwrap();
        };
        write("", "", "org.test");
        write("Watch/Test Watch.app", "<key>WKCompanionAppBundleIdentifier</key><string>org.test</string>", "org.test.watchkitapp");
        write(
            "Watch/Test Watch.app/PlugIns/Ext.appex",
            "<key>NSExtension</key><dict><key>NSExtensionAttributes</key><dict><key>WKAppBundleIdentifier</key><string>org.test.watchkitapp</string></dict></dict>",
            "org.test.watchkitapp.extension",
        );
        let mut application = Application::new(app.clone()).unwrap();
        assert_eq!(application.watch_bundles().len(), 2);
        application.update_bundle_id("org.test", "org.test.TEAM").unwrap();
        let watch = &application.bundle.watch_apps()[0];
        assert_eq!(watch.bundle_identifier(), Some("org.test.TEAM.watchkitapp"));
        assert_eq!(watch.app_info.get("WKCompanionAppBundleIdentifier").and_then(plist::Value::as_string), Some("org.test.TEAM"));
        let extension = &watch.app_extensions()[0];
        assert_eq!(extension.bundle_identifier(), Some("org.test.TEAM.watchkitapp.extension"));
        let attributes = extension.app_info.get("NSExtension").and_then(plist::Value::as_dictionary)
            .and_then(|e| e.get("NSExtensionAttributes")).and_then(plist::Value::as_dictionary).unwrap();
        assert_eq!(attributes.get("WKAppBundleIdentifier").and_then(plist::Value::as_string), Some("org.test.TEAM.watchkitapp"));
    }
}
