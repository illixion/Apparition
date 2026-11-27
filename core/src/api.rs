//! The core as a Rust library, for the desktop installer (`installer/`). The app reaches the
//! same code through the C ABI; here nothing is global, and the caller owns its tokio runtime,
//! its pairing files and its Apple ID session.
//!
//! A desktop isn't the device it installs to, so it reaches the device's RemotePairing
//! service directly: no relay, no LocalDevVPN.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

pub use idevice::remote_pairing::RpPairingFile;
pub use isideload::auth::apple_account::{TwoFactorCallbackParams, TwoFactorCallbackResponse};
use idevice::{
    RsdService,
    afc::opcode::AfcFopenMode,
    house_arrest::HouseArrestClient,
    lockdown::LockdownClient,
    remote_pairing::{RemotePairingClient, RpPairingSocket},
};
use rootcause::prelude::*;
use serde_json::Value;

pub use crate::errors::describe;
pub use crate::jit::{Target as JitTarget, stop as stop_jit};
pub use crate::tools::stop as stop_tool;
pub use crate::launch::Launched;
pub use crate::{
    Session,
    account::{delete_app_id, list_app_ids, list_certificates, revoke_certificate},
    identity::{Platform, check_app_os, import_identity, import_profile, list as signing_inventory, sign_offline, sign_offline_keeping, sign_code, sign_mac_app},
    share::{SEALED_EXTENSION, export_apple_id_key, export_apple_id_key_sealed, import_any_signing_file, import_signing_file, sealed_from},
    AnisetteCheck, AnisetteLibraries, ANDROID_ANISETTE, BUILTIN_ANISETTE, android_anisette_available, anisette_libraries_unverified, import_anisette_libraries, reset_anisette_libraries, NO_ANISETTE, check_anisette, PAIRED_MAC_ANISETTE, VM_ANISETTE, default_anisette, REFUSED_ANDROID_ANISETTE, REFUSED_ANISETTE, SharedCertificate, builtin_anisette_available, is_shared_certificate, native_anisette,
    sign_in, sign_with_apple_id, sign_with_apple_id_as, capability_refusals,
    anisette_dir, failure_kind,
};
/// Pairing this computer with a Mac for its anisette (companion_client.rs).
#[cfg(feature = "desktop")]
pub use crate::companion_client as paired_mac;
#[cfg(feature = "desktop")]
pub use crate::firewall;
/// Serving the VM setup to a macOS VM in recoveryOS (vm_setup.rs).
#[cfg(all(feature = "desktop", target_os = "macos"))]
pub use crate::vm_setup;
/// The anisette VM on this Mac: setting it up, running it (vm.rs).
#[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
pub use crate::vm;
/// Handing the certificate key to a nearby Apparition, checked by a six-digit code (nearby.rs).
#[cfg(feature = "desktop")]
pub use crate::nearby::net as nearby;

/// Sets up what the core's crypto and isideload need. Call once, before anything else.
pub fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    // Before isideload's, which then leaves these in place (errors.rs).
    crate::errors::install_hooks();
    let _ = isideload::init();
}

/// An open RemotePairing tunnel with its RSD handshake: what every device service needs.
pub struct Tunnel {
    adapter: idevice::tcp::handle::AdapterHandle,
    handshake: idevice::rsd::RsdHandshake,
}

/// Pairs with a device whose Settings > General > Remote Devices screen is open: pair-setup
/// on its manual-pairing service at `device`, with the PIN it shows (`pin` asks for it).
/// Fills in `pairing`, which the caller keeps for every later connection.
pub async fn pair(
    device: SocketAddr,
    hostname: &str,
    pairing: &mut RpPairingFile,
    pin: impl Fn() -> String,
) -> Result<(), Report> {
    let control = crate::tunnel::dial(device, None).await.context("Can't reach the device's pairing service")?;
    let mut rpc = RemotePairingClient::new(RpPairingSocket::new(control), hostname);
    crate::tunnel::verify(&mut rpc, pairing, device, pin).await.context("Pairing failed")?;
    Ok(())
}

/// A device usbmuxd knows: plugged in over USB, or on the network once this computer is
/// trusted. iPhone and iPad have no PIN pairing screen, so they pair this way.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UsbDevice {
    pub udid: String,
    /// False for a device usbmuxd reaches over the network.
    pub usb: bool,
}

/// The devices usbmuxd lists (Apple's on macOS and Windows, usbmuxd on Linux).
pub async fn usb_devices() -> Result<Vec<UsbDevice>, Report> {
    let mut usbmuxd = idevice::usbmuxd::UsbmuxdConnection::default()
        .await
        .context("Can't reach usbmuxd. On Windows install Apple Devices (or iTunes); on Linux install and start usbmuxd.")?;
    let mut devices: Vec<UsbDevice> = Vec::new();
    for device in usbmuxd.get_devices().await? {
        let usb = matches!(device.connection_type, idevice::usbmuxd::Connection::Usb);
        match devices.iter_mut().find(|d| d.udid == device.udid) {
            Some(known) => known.usb |= usb,
            None => devices.push(UsbDevice { udid: device.udid, usb }),
        }
    }
    Ok(devices)
}

/// Whether a device with lockdown's `product_type` and `os_version` predates the RemotePairing
/// tunnel Apparition uses, so it's reached through usbmuxd and lockdown instead: see
/// [`crate::legacy`]. CoreDevice arrived with iOS 17, but the tunnel over TCP only with 17.4
/// (StikDebug, on the same idevice library, starts there too), so an iPhone or iPad on 17.0 to
/// 17.3 counts: TrollStore's iOS 17.0 among them. An Apple TV has no cable to fall back to and
/// counts only before 17. visionOS has had the tunnel from 1.0, and its versions only reached 26
/// later, so a headset never counts. Anything that doesn't read as a version counts as current.
pub fn is_legacy_os(product_type: &str, os_version: &str) -> bool {
    let mut parts = os_version.split('.').map(|part| part.parse::<u32>().ok());
    let Some(Some(major)) = parts.next() else { return false };
    let minor = parts.next().flatten().unwrap_or(0);
    match Platform::of_product(product_type) {
        Platform::VisionOS => false,
        Platform::TvOS => major < 17,
        Platform::IOS => (major, minor) < (17, 4),
    }
}

/// Whether Apparition itself is best effort on this device: it runs on iOS and tvOS 17.4 and
/// visionOS 2, but has only been tried on 26 and later, so the desktop app and the command line
/// say so and ask how it went. A legacy device ([`is_legacy_os`]) doesn't count: Apparition
/// can't run there at all, and it gets its own notice. Nor does a version that doesn't read as one.
pub fn is_untested_os(product_type: &str, os_version: &str) -> bool {
    let Some(Ok(major)) = os_version.split('.').next().map(str::parse::<u32>) else { return false };
    major < 26 && !is_legacy_os(product_type, os_version)
}

/// The phone and headset app's bug form, for a report on a version [`is_untested_os`] counts.
pub const DEVICE_REPORT_URL: &str = "https://github.com/illixion/Apparition/issues/new?template=bug_report.yml";

/// usbmuxd's provider for `udid`: the cable when it's plugged in, else the network (Wi-Fi sync).
pub(crate) async fn usb_provider(udid: &str) -> Result<idevice::provider::UsbmuxdProvider, Report> {
    let mut usbmuxd = idevice::usbmuxd::UsbmuxdConnection::default().await.context("Can't reach usbmuxd")?;
    let devices = usbmuxd.get_devices().await?;
    // Prefer the cable: lockdown services that set up pairing may refuse the network.
    let device = devices
        .iter()
        .filter(|d| d.udid == udid)
        .min_by_key(|d| !matches!(d.connection_type, idevice::usbmuxd::Connection::Usb))
        .ok_or_else(|| report!("{udid} isn't connected. Plug it in with a cable (or turn on its Wi-Fi sync in Finder or Apple Devices) and unlock it."))?;
    Ok(device.to_provider(idevice::usbmuxd::UsbmuxdAddr::default(), "apparition"))
}

/// The lockdown identity of `udid` over usbmuxd, and the provider that reached it. When there's
/// no usbmuxd pair record for it (Linux, where nothing makes one), this computer asks the device
/// to trust it first, and `waiting` hears once that it's waiting for the user to tap Trust.
pub async fn usb_identity(
    udid: &str,
    hostname: &str,
    waiting: &(dyn Fn(&str) + Send + Sync),
) -> Result<(idevice::provider::UsbmuxdProvider, DeviceInfo), Report> {
    use idevice::{IdeviceService, provider::IdeviceProvider};
    let provider = usb_provider(udid).await?;
    let pairing_file = match provider.get_pairing_file().await {
        Ok(file) => file,
        #[cfg(feature = "desktop")]
        Err(_) => {
            let mut usbmuxd = idevice::usbmuxd::UsbmuxdConnection::default().await.context("Can't reach usbmuxd")?;
            trust(&mut usbmuxd, &provider, udid, hostname, waiting).await?;
            provider_pairing_file(&provider).await?
        }
        #[cfg(not(feature = "desktop"))]
        Err(_) => {
            let _ = (hostname, waiting);
            provider_pairing_file(&provider).await?
        }
    };
    let info = {
        let mut lockdown = LockdownClient::connect(&provider)
            .await
            .context("Can't reach lockdown. Unlock the device and tap Trust if it asks.")?;
        lockdown.start_session(&pairing_file).await.context("This computer isn't trusted by the device yet. Unlock it, tap Trust, then try again.")?;
        let mut value = async |key: &str| -> Result<String, Report> {
            let value = lockdown.get_value(Some(key), None).await?;
            Ok(value.as_string().unwrap_or_default().to_string())
        };
        DeviceInfo {
            udid: value("UniqueDeviceID").await?,
            name: value("DeviceName").await?,
            product_type: value("ProductType").await?,
            os_version: value("ProductVersion").await?,
        }
    };
    Ok((provider, info))
}

/// Pairs with `udid` through usbmuxd and lockdown: remotepairingdeviced's lockdown service
/// creates the RemotePairing record (no PIN). It needs usbmuxd's lockdown pair record for the
/// device (see [`usb_identity`], which makes one when there's none). Fills in `pairing`, the same
/// record the PIN flow makes, so every later connection is over the network. Returns the
/// device's identity, read over the same lockdown connection.
pub async fn pair_usb(
    udid: &str,
    hostname: &str,
    pairing: &mut RpPairingFile,
    waiting: &(dyn Fn(&str) + Send + Sync),
) -> Result<DeviceInfo, Report> {
    use idevice::{IdeviceService, remote_pairing::RemotePairingLockdownService};
    let (provider, info) = usb_identity(udid, hostname, waiting).await?;
    if is_legacy_os(&info.product_type, &info.os_version) {
        bail!("{} runs iOS {}, which has no RemotePairing tunnel. Apparition installs to it over usbmuxd instead.", info.name, info.os_version);
    }
    let service = RemotePairingLockdownService::connect(&provider)
        .await
        .context("The device didn't start its pairing service (remotepairingdeviced)")?;
    let mut client = service.into_client(hostname)?;
    client.connect(pairing, async || "000000".to_string()).await.context("Pairing failed")?;
    Ok(info)
}

/// Makes usbmuxd's lockdown pair record for `udid`, as Finder or Apple Devices would: lockdown
/// pairing with a new host certificate, which the device shows as "Trust This Computer?". Waits
/// up to two minutes for the user to unlock it and tap Trust, then saves the record to usbmuxd
/// (SavePairRecord), where later runs, and the system, find it.
#[cfg(feature = "desktop")]
async fn trust(
    usbmuxd: &mut idevice::usbmuxd::UsbmuxdConnection,
    provider: &idevice::provider::UsbmuxdProvider,
    udid: &str,
    hostname: &str,
    waiting: &(dyn Fn(&str) + Send + Sync),
) -> Result<(), Report> {
    use idevice::{IdeviceError, IdeviceService};
    use std::time::{Duration, Instant};
    let buid = usbmuxd.get_buid().await.context("usbmuxd didn't give its system BUID")?;
    let host_id = random_uuid();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut name = None;
    let mut asked = false;
    loop {
        let mut lockdown = LockdownClient::connect(provider)
            .await
            .context("Can't reach lockdown. Keep the device plugged in and unlocked.")?;
        if name.is_none() {
            // Lockdown tells an untrusted computer the device's name.
            name = lockdown.get_value(Some("DeviceName"), None).await.ok().and_then(|v| v.as_string().map(str::to_string));
        }
        let device = name.as_deref().unwrap_or("the device");
        match lockdown.pair_once(host_id.as_str(), buid.as_str(), Some(hostname)).await {
            Ok(mut record) => {
                record.udid = Some(udid.to_string());
                let record = record.serialize().context("Can't write the pair record")?;
                if let Err(error) = usbmuxd.save_pair_record(udid, record).await {
                    // Linux's usbmuxd saves the record but may answer with something other than
                    // the result idevice expects: check that the record is there before failing.
                    let mut check = idevice::usbmuxd::UsbmuxdConnection::default().await.context("Can't reach usbmuxd")?;
                    if check.get_pair_record(udid).await.is_err() {
                        return Err(report!(error).context("usbmuxd didn't save the pair record").into());
                    }
                    tracing::debug!("usbmuxd saved the pair record despite answering {error}");
                }
                return Ok(());
            }
            Err(IdeviceError::PairingDialogResponsePending | IdeviceError::PasswordProtected) => {
                if !asked {
                    waiting(&format!("Unlock {device} and tap Trust, then enter its passcode."));
                    asked = true;
                }
            }
            Err(IdeviceError::UserDeniedPairing) => {
                bail!("{device} doesn't trust this computer (Don't Trust was tapped). Unplug it and plug it back in to be asked again, then pair again.");
            }
            Err(error) => Err(error).context(format!("{device} refused to pair with this computer"))?,
        }
        if Instant::now() >= deadline {
            bail!("{device} wasn't trusted in time. Unlock it, tap Trust and enter its passcode, then pair again.");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// A random (version 4) UUID in capitals, the form lockdown's HostID takes.
pub(crate) fn random_uuid() -> String {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

async fn provider_pairing_file(
    provider: &impl idevice::provider::IdeviceProvider,
) -> Result<idevice::pairing_file::PairingFile, Report> {
    Ok(provider.get_pairing_file().await.context("usbmuxd has no pair record for this device: trust this computer on it first")?)
}

/// Opens a tunnel to a paired device's RemotePairing service at `device` (port 49152 on
/// visionOS). `pin` is asked only if the device wants to pair again.
pub async fn connect(
    device: SocketAddr,
    hostname: &str,
    pairing: &mut RpPairingFile,
    pin: impl Fn() -> String,
) -> Result<Tunnel, Report> {
    let (adapter, handshake) = crate::tunnel::open(device, None, hostname, pairing, pin, true).await?;
    Ok(Tunnel { adapter, handshake })
}

/// Like [`connect`], but only pair-verifies: a pairing the device turns down fails with
/// [`PairingRejected`](crate::tunnel::PairingRejected) (see [`is_pairing_rejected`]) instead of
/// starting a pair-setup. For iPhone and iPad, which have no PIN screen to answer one.
pub async fn connect_verified(device: SocketAddr, hostname: &str, pairing: &mut RpPairingFile) -> Result<Tunnel, Report> {
    let (adapter, handshake) = crate::tunnel::open(device, None, hostname, pairing, String::new, false).await?;
    Ok(Tunnel { adapter, handshake })
}

pub use crate::tunnel::{PairingRejected, is_pairing_rejected};

/// An installed app, as [`Tunnel::installed_apps`] lists it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InstalledApp {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    pub build: Option<String>,
}

/// The device's identity, from lockdown over the tunnel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub product_type: String,
    pub os_version: String,
}

/// The display name of `bundle_id` in an installation_proxy lookup, if it's there.
pub(crate) fn installed_name(apps: &std::collections::HashMap<String, plist::Value>, bundle_id: &str) -> Option<String> {
    apps.get(bundle_id).map(|info| {
        let info = info.as_dictionary();
        let name = |key: &str| info.and_then(|i| i.get(key)).and_then(plist::Value::as_string);
        name("CFBundleDisplayName").or_else(|| name("CFBundleName")).unwrap_or(bundle_id).to_string()
    })
}

/// The bundle IDs in an installation_proxy lookup that are `id` or start with `id.`, sorted.
pub(crate) fn installed_ids(apps: &std::collections::HashMap<String, plist::Value>, id: &str) -> Vec<String> {
    let prefix = format!("{id}.");
    let mut found: Vec<String> = apps.keys().filter(|app| *app == id || app.starts_with(&prefix)).cloned().collect();
    found.sort();
    found
}

/// What a computer reaches a device through: the RemotePairing tunnel (iOS 17 and later,
/// visionOS, tvOS), or usbmuxd and lockdown before iOS 17.4 ([`crate::legacy`]). The
/// legacy one only does what lockdown can without CoreDevice: list, install and remove apps.
pub enum Connection {
    Tunnel(Tunnel),
    Legacy(crate::legacy::Legacy),
}

impl Connection {
    pub async fn device_info(&mut self) -> Result<DeviceInfo, Report> {
        match self {
            Connection::Tunnel(tunnel) => tunnel.device_info().await,
            Connection::Legacy(device) => device.device_info().await,
        }
    }

    pub async fn installed_app(&mut self, bundle_id: &str) -> Result<Option<String>, Report> {
        match self {
            Connection::Tunnel(tunnel) => tunnel.installed_app(bundle_id).await,
            Connection::Legacy(device) => device.installed_app(bundle_id).await,
        }
    }

    pub async fn installed_like(&mut self, id: &str) -> Result<Vec<String>, Report> {
        match self {
            Connection::Tunnel(tunnel) => tunnel.installed_like(id).await,
            Connection::Legacy(device) => device.installed_like(id).await,
        }
    }

    pub async fn uninstall(&mut self, bundle_id: &str) -> Result<(), Report> {
        match self {
            Connection::Tunnel(tunnel) => tunnel.uninstall(bundle_id).await,
            Connection::Legacy(device) => device.uninstall(bundle_id).await,
        }
    }

    /// Packs and installs a signed app. Over the tunnel a running copy is closed first; lockdown
    /// can't close apps, so before iOS 17.4 an update of an open app waits until it's closed.
    pub async fn install(&mut self, app_path: &Path, source_ipa: Option<&Path>, detach_after_ms: u64) -> Result<Value, Report> {
        match self {
            Connection::Tunnel(tunnel) => tunnel.install(app_path, source_ipa, detach_after_ms).await,
            Connection::Legacy(device) => device.install(app_path, source_ipa, detach_after_ms).await,
        }
    }

    /// The Apple Watches paired with this iPhone, through the tunnel; none before iOS 17.4,
    /// where Apparition doesn't reach the Watch.
    pub async fn paired_watches(&mut self) -> Result<Vec<crate::watch::PairedWatch>, Report> {
        match self {
            Connection::Tunnel(tunnel) => tunnel.paired_watches().await,
            Connection::Legacy(_) => Ok(Vec::new()),
        }
    }

    /// The tunnel, for what only it can do (JIT, Apple Watch installs, handing Apparition its
    /// pairing and signing); `None` before iOS 17.4.
    pub fn tunnel(&mut self) -> Option<&mut Tunnel> {
        match self {
            Connection::Tunnel(tunnel) => Some(tunnel),
            Connection::Legacy(_) => None,
        }
    }
}

impl Tunnel {
    pub async fn device_info(&mut self) -> Result<DeviceInfo, Report> {
        let mut lockdown = LockdownClient::connect_rsd(&mut self.adapter, &mut self.handshake)
            .await
            .context("lockdown connect failed")?;
        let mut value = async |key: &str| -> Result<String, Report> {
            let value = lockdown.get_value(Some(key), None).await?;
            Ok(value.as_string().unwrap_or_default().to_string())
        };
        Ok(DeviceInfo {
            udid: value("UniqueDeviceID").await?,
            name: value("DeviceName").await?,
            product_type: value("ProductType").await?,
            os_version: value("ProductVersion").await?,
        })
    }

    /// The Apple Watches paired with this iPhone; empty for any other device. Signing an app
    /// with a watch app for this phone registers them (it remembers what this said).
    pub async fn paired_watches(&mut self) -> Result<Vec<crate::watch::PairedWatch>, Report> {
        crate::watch::paired_and_remembered(&mut self.adapter, &mut self.handshake).await
    }

    /// The Watch `udid`, paired with this iPhone, through the phone's companion_proxy: its
    /// lockdown services, for installing its watch app directly. The trust record lives in
    /// `dir`; the first time, the Watch asks to trust the computer and `waiting` is told.
    pub async fn watch<'a>(
        &'a mut self,
        udid: &str,
        dir: &Path,
        waiting: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<crate::watch::Companion<'a>, Report> {
        crate::watch::Companion::connect(&mut self.adapter, &mut self.handshake, udid, dir, waiting).await
    }

    /// Removes an installed copy and its app container. Keychain and shared groups may remain.
    pub async fn uninstall(&mut self, bundle_id: &str) -> Result<(), Report> {
        let mut installer = idevice::installation_proxy::InstallationProxyClient::connect_rsd(&mut self.adapter, &mut self.handshake).await?;
        installer.uninstall(bundle_id.to_string(), None).await?;
        Ok(())
    }

    /// Copies an app's Documents and Library into `dir`. See `crate::container::pull`.
    pub async fn pull_container(&mut self, bundle_id: &str, dir: &Path) -> Result<Value, Report> {
        crate::container::pull(&mut self.adapter, &mut self.handshake, bundle_id, dir).await
    }

    /// Copies a folder `pull_container` made back into an app's container.
    pub async fn push_container(&mut self, bundle_id: &str, dir: &Path) -> Result<Value, Report> {
        crate::container::push(&mut self.adapter, &mut self.handshake, bundle_id, dir).await
    }

    /// How many folders and files an app's data holds.
    pub async fn count_container(&mut self, bundle_id: &str) -> Result<Value, Report> {
        crate::container::count(&mut self.adapter, &mut self.handshake, bundle_id).await
    }

    pub async fn migrate_data(&mut self, from: &str, to: &str) -> Result<Value, Report> {
        crate::migrate::migrate_container(&mut self.adapter, &mut self.handshake, from, to).await
    }

    /// The display name of `bundle_id` if it's installed, from installation_proxy.
    pub async fn installed_app(&mut self, bundle_id: &str) -> Result<Option<String>, Report> {
        let mut installer =
            idevice::installation_proxy::InstallationProxyClient::connect_rsd(&mut self.adapter, &mut self.handshake)
                .await
                .context("installation_proxy connect failed")?;
        let apps = installer.get_apps(Some("User"), Some(vec![bundle_id.to_string()])).await?;
        Ok(installed_name(&apps, bundle_id))
    }

    /// The user apps installed whose bundle ID is `id` or starts with `id.`, sorted.
    pub async fn installed_like(&mut self, id: &str) -> Result<Vec<String>, Report> {
        let mut installer =
            idevice::installation_proxy::InstallationProxyClient::connect_rsd(&mut self.adapter, &mut self.handshake)
                .await
                .context("installation_proxy connect failed")?;
        let apps = installer.get_apps(Some("User"), None).await?;
        Ok(installed_ids(&apps, id))
    }

    /// The installed apps (user apps, or every app with `all`) with the Info.plist fields a build
    /// tool compares: `CFBundleVersion` is where build-and-sign stamps the git commit. Through
    /// installation_proxy, which answers over this tunnel (CoreDevice's `listapps` doesn't).
    pub async fn installed_apps(&mut self, all: bool) -> Result<Vec<InstalledApp>, Report> {
        let mut installer =
            idevice::installation_proxy::InstallationProxyClient::connect_rsd(&mut self.adapter, &mut self.handshake)
                .await
                .context("installation_proxy connect failed")?;
        let apps = installer.get_apps(Some(if all { "Any" } else { "User" }), None).await?;
        let text = |info: &plist::Value, key: &str| info.as_dictionary().and_then(|d| d.get(key)).and_then(|v| v.as_string()).map(str::to_string);
        let mut found: Vec<InstalledApp> = apps
            .iter()
            .map(|(bundle_id, info)| InstalledApp {
                name: text(info, "CFBundleDisplayName").or_else(|| text(info, "CFBundleName")).unwrap_or_else(|| bundle_id.clone()),
                version: text(info, "CFBundleShortVersionString"),
                build: text(info, "CFBundleVersion"),
                bundle_id: bundle_id.clone(),
            })
            .collect();
        found.sort_by(|a, b| a.bundle_id.cmp(&b.bundle_id));
        Ok(found)
    }

    /// The pid of `bundle_id`'s running process, if it runs.
    pub async fn running_pid(&mut self, bundle_id: &str) -> Result<Option<u64>, Report> {
        crate::jit::running_pid(&mut self.adapter, &mut self.handshake, bundle_id).await
    }

    /// The device's running processes, apps first. See `crate::tools::processes`.
    pub async fn processes(&mut self) -> Result<Vec<crate::tools::Process>, Report> {
        crate::tools::processes(&mut self.adapter, &mut self.handshake).await
    }

    /// Sends `signal` to `pid` (15 asks it to end, 9 ends it).
    pub async fn signal_process(&mut self, pid: u32, signal: u32) -> Result<(), Report> {
        crate::tools::signal(&mut self.adapter, &mut self.handshake, pid, signal).await
    }

    /// Simulates `at` for the whole device until `crate::tools::stop(key)`. See
    /// `crate::tools::simulate_location`.
    pub async fn simulate_location(
        &mut self,
        key: &str,
        at: crate::tools::Coordinate,
        on_set: &mut (dyn FnMut(crate::tools::Coordinate) + Send),
    ) -> Result<Value, Report> {
        crate::tools::simulate_location(&mut self.adapter, &mut self.handshake, key, at, on_set).await
    }

    /// Streams the device's log (only `pid`'s when given) until `crate::tools::stop(key)`. See
    /// `crate::tools::stream_log`.
    pub async fn stream_log(
        &mut self,
        key: &str,
        pid: Option<u32>,
        minimum: crate::tools::Level,
        on_batch: &mut (dyn FnMut(Vec<crate::tools::LogEntry>) + Send),
    ) -> Result<Value, Report> {
        crate::tools::stream_log(&mut self.adapter, &mut self.handshake, key, pid, minimum, on_batch).await
    }

    /// Closes `bundle_id` if it's running (never the calling app); true when it was. See
    /// `crate::ipa::close_running`, which `install` runs before an update.
    pub async fn close_running(&mut self, bundle_id: &str) -> bool {
        crate::ipa::close_running(&mut self.adapter, &mut self.handshake, bundle_id).await
    }

    /// Launches or attaches to an app and serves its JIT requests until it no longer needs a
    /// debugger. See `crate::jit::serve`.
    pub async fn serve_jit(
        &mut self,
        target: JitTarget<'_>,
        wait_seconds: u32,
        on_event: &mut (dyn FnMut(&str) + Send),
    ) -> Result<Value, Report> {
        crate::jit::serve(&mut self.adapter, &mut self.handshake, target, wait_seconds, on_event).await
    }

    /// Packs and installs a signed app. See `apparition_install_ipa`.
    pub async fn install(&mut self, app_path: &Path, source_ipa: Option<&Path>, detach_after_ms: u64) -> Result<Value, Report> {
        crate::ipa::install(&mut self.adapter, &mut self.handshake, app_path, source_ipa, detach_after_ms).await
    }

    /// Launches an app (ending a running instance), optionally with its console and suspended
    /// for a debugger. See `crate::launch::launch`.
    pub async fn launch(
        &mut self,
        bundle_id: &str,
        arguments: &[&str],
        environment: &[(String, String)],
        console: bool,
        suspended: bool,
    ) -> Result<Launched, Report> {
        let environment = environment.iter().map(|(k, v)| (k.clone(), plist::Value::String(v.clone()))).collect();
        crate::launch::launch(&mut self.adapter, &mut self.handshake, bundle_id, arguments, environment, console, suspended).await
    }

    /// Installs an IPA already signed for this device, as it is. See `crate::ipa::install_signed_ipa`.
    pub async fn install_signed_ipa(&mut self, ipa: &Path, bundle_id: Option<&str>, detach_after_ms: u64) -> Result<Value, Report> {
        crate::ipa::install_signed_ipa(&mut self.adapter, &mut self.handshake, ipa, bundle_id, detach_after_ms).await
    }

    /// Writes files into an installed app's Documents folder through house_arrest, under
    /// `folder` (created if missing; "" for Documents itself). Works for apps that share files with Files
    /// (UIFileSharingEnabled), Apparition among them.
    ///
    /// tvOS doesn't let an app open its own Documents (only Caches), so on an Apple TV the
    /// files go to `Library/Caches/Apparition/Documents`, where Apparition keeps what it would
    /// keep in Documents elsewhere (AppFolders.documents). That needs the whole container,
    /// which house_arrest vends only for a development-signed app; Apparition is one.
    pub async fn put_documents(&mut self, bundle_id: &str, folder: &str, files: &[(String, Vec<u8>)]) -> Result<(), Report> {
        let tv = self.device_info().await.is_ok_and(|i| Platform::of_product(&i.product_type) == Platform::TvOS);
        let house_arrest = HouseArrestClient::connect_rsd(&mut self.adapter, &mut self.handshake)
            .await
            .context("house_arrest connect failed")?;
        let (mut afc, root) = if tv {
            let afc = house_arrest
                .vend_container(bundle_id)
                .await
                .context(format!("{bundle_id}'s container isn't open to this computer (is it installed, and development-signed?)"))?;
            (afc, "/Library/Caches/Apparition/Documents")
        } else {
            let afc = house_arrest
                .vend_documents(bundle_id)
                .await
                .context(format!("{bundle_id} doesn't share its Documents (is it installed?)"))?;
            (afc, "/Documents")
        };
        let folder = match folder.trim_matches('/') {
            "" => root.to_string(),
            name => format!("{root}/{name}"),
        };
        // mk_dir doesn't make parents, so each level in turn (failing on one that exists).
        let mut path = String::new();
        for part in folder.split('/').filter(|p| !p.is_empty()) {
            path = format!("{path}/{part}");
            let _ = afc.mk_dir(path.clone()).await;
        }
        for (name, data) in files {
            if name.contains('/') || name.starts_with('.') {
                bail!("bad file name {name}");
            }
            let mut file = afc.open(format!("{folder}/{name}"), AfcFopenMode::WrOnly).await?;
            file.write_entire(data).await?;
            file.close().await?;
        }
        Ok(())
    }
}

// MARK: Shared by the desktop front ends (the installer's command line and its app)

/// Apparition's bundle ID. An installed app whose ID starts with it gets the signing handoff.
pub const APPARITION_ID: &str = "com.illixion.apparition";
/// The RemotePairing port on visionOS; mDNS says when it differs.
pub const TUNNEL_PORT: u16 = 49152;

/// The name this computer pairs under, shown in the headset's Remote Devices list.
pub fn hostname() -> String {
    let machine = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .or_else(|| {
            std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty());
    // Kept short: a headset's Remote Devices list shows it and can only be cleared whole.
    match machine.as_deref().map(|m| m.strip_suffix(".local").unwrap_or(m)) {
        Some(machine) => format!("Apparition on {machine}"),
        None => "Apparition".to_string(),
    }
}

/// The bundle ID of an unpacked app.
pub fn bundle_id(app: &Path) -> Option<String> {
    let info: plist::Dictionary = plist::from_file(app.join("Info.plist")).ok()?;
    info.get("CFBundleIdentifier")?.as_string().map(str::to_string)
}

/// One of a free account's limits, which signing can run into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Slot {
    Certificates,
    AppIds,
}

impl Slot {
    /// Which limit a signing error is about (the same test as the app's DeveloperSlots).
    pub fn of_error(text: &str) -> Option<Slot> {
        let lower = text.to_lowercase();
        if lower.contains("7460") || lower.contains("maximum number of certificates") {
            return Some(Slot::Certificates);
        }
        (lower.contains("app id") && ["limit", "maximum", "more than", "9401", "not enough"].iter().any(|w| lower.contains(w)))
            .then_some(Slot::AppIds)
    }

    pub fn full_message(self) -> &'static str {
        match self {
            Slot::Certificates => "The account has no room for another development certificate (a free account has two).",
            Slot::AppIds => "The account can't make another App ID right now (a free account gets ten a week).",
        }
    }
}

/// Downloads the visionOS app of a verified release into `dir`, checked against the signed
/// manifest. `progress` gets (bytes received, total).
pub async fn download_apparition(
    verified: &crate::release::Verified,
    dir: &Path,
    progress: impl Fn(u64, u64),
) -> Result<std::path::PathBuf, Report> {
    download_apparition_for(verified, Platform::VisionOS, dir, progress).await
}

/// How release.json names the app built for `platform` ("visionos", "ios", "tvos").
pub fn release_platform(platform: Platform) -> &'static str {
    match platform {
        Platform::VisionOS => "visionos",
        Platform::IOS => "ios",
        Platform::TvOS => "tvos",
    }
}

/// [`download_apparition`] for the app built for `platform`.
pub async fn download_apparition_for(
    verified: &crate::release::Verified,
    platform: Platform,
    dir: &Path,
    progress: impl Fn(u64, u64),
) -> Result<std::path::PathBuf, Report> {
    let asset = verified
        .app(release_platform(platform))
        .ok_or_else(|| report!("Apparition {} has no {} build", verified.release.version, platform.os_name()))?
        .clone();
    crate::release::download(verified, &asset, dir, progress).await
}

/// What signed Apparition, for handing to it so it can refresh itself and its apps.
pub enum Handoff<'a> {
    /// The Apple ID's certificate key, from `<data_dir>/storage.json`: Apparition then signs
    /// with the same certificate, so the two use one of a free account's two.
    AppleId { data_dir: &'a Path, email: &'a str, team_id: Option<&'a str> },
    /// The .p12 files and profiles, as files; Apparition asks for each .p12's password once.
    Certificate { p12s: &'a [PathBuf], profiles: &'a [PathBuf] },
}

/// The signing material `build-and-sign` keeps on a Mac: `certs/*.p12` and
/// `profiles/*.mobileprovision` under `~/.config/build-signing` (`BUILD_SIGNING_DIR` overrides
/// it). It's what the Apparition Store build service signs with, so handing it to Apparition lets the
/// device refresh those apps on its own. Only the files are collected; the .p12 passwords stay
/// in `global.conf`, and the device asks for each one itself.
#[derive(Debug, Clone, Default)]
pub struct BuildSigning {
    pub dir: PathBuf,
    pub p12s: Vec<PathBuf>,
    pub profiles: Vec<PathBuf>,
}

impl BuildSigning {
    pub fn default_dir() -> PathBuf {
        match std::env::var_os("BUILD_SIGNING_DIR").filter(|d| !d.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config").join("build-signing"),
        }
    }

    /// Lists what's in `dir` (the default when `None`); errors when there's no certificate.
    pub fn discover(dir: Option<&Path>) -> Result<Self, Report> {
        let dir = dir.map(Path::to_path_buf).unwrap_or_else(Self::default_dir);
        let collect = |sub: &str, extension: &str| -> Vec<PathBuf> {
            let mut found: Vec<PathBuf> = std::fs::read_dir(dir.join(sub))
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && !path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))
                        && path.extension().is_some_and(|e| e.eq_ignore_ascii_case(extension))
                })
                .collect();
            found.sort();
            found
        };
        let signing = Self { p12s: collect("certs", "p12"), profiles: collect("profiles", "mobileprovision"), dir };
        if signing.p12s.is_empty() {
            bail!("no certificate (.p12) in {}/certs: is build-and-sign set up on this computer?", signing.dir.display());
        }
        Ok(signing)
    }

    pub fn handoff(&self) -> Handoff<'_> {
        Handoff::Certificate { p12s: &self.p12s, profiles: &self.profiles }
    }
}

impl Tunnel {
    /// Puts `handoff` in the Files inbox (Documents/Signing) of the installed `bundle_id`.
    pub async fn hand_off_signing(&mut self, bundle_id: &str, handoff: Handoff<'_>) -> Result<(), Report> {
        let file_name = |path: &Path| -> Result<String, Report> {
            Ok(path.file_name().ok_or_else(|| report!("bad file name"))?.to_string_lossy().to_string())
        };
        let files = match handoff {
            Handoff::AppleId { data_dir, email, team_id } => {
                let file = export_apple_id_key(data_dir, email, team_id, &hostname())?;
                vec![("apple-id.apparitionsigning".to_string(), file)]
            }
            Handoff::Certificate { p12s, profiles } => {
                let mut files = Vec::new();
                for p12 in p12s {
                    files.push((file_name(p12)?, std::fs::read(p12)?));
                }
                for profile in profiles {
                    files.push((file_name(profile)?, std::fs::read(profile)?));
                }
                files
            }
        };
        self.put_documents(bundle_id, "Signing", &files).await
    }
}

#[cfg(feature = "desktop")]
pub use desktop::*;

/// Paired devices and mDNS discovery, for the computer's side (the `desktop` feature).
#[cfg(feature = "desktop")]
mod desktop {
    use std::{
        net::{IpAddr, SocketAddr},
        path::PathBuf,
        time::Duration,
    };

    use rootcause::prelude::*;
    use serde::{Deserialize, Serialize};

    use super::{RpPairingFile, TUNNEL_PORT, Tunnel, hostname};

    /// What Apparition calls itself when it pairs (ProbeModel.hostname). An iPhone or iPad
    /// paired over USB gets a second record under this name, for Apparition on it.
    pub const APP_PAIRING_HOST: &str = "Apparition";

    pub const MANUAL_PAIRING_SERVICE: &str = "_remotepairing-manual-pairing._tcp.local.";
    pub const PAIRED_SERVICE: &str = "_remotepairing._tcp.local.";

    /// A device this computer has paired with.
    #[derive(Debug, Serialize, Deserialize, Clone)]
    pub struct PairedDevice {
        pub udid: String,
        pub name: String,
        pub address: IpAddr,
        pub port: u16,
        /// File name under pairings/.
        pub pairing: String,
        /// Lockdown's ProductType (RealityDevice14,1, iPad16,6...); missing from records made
        /// before iPhone and iPad could pair, which were all headsets.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub product_type: Option<String>,
        /// iOS 17.3 or earlier, reached through usbmuxd and lockdown ([`crate::legacy`]): it has no
        /// RemotePairing, so `address`, `port` and `pairing` mean nothing. Untested on a real
        /// device so far.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub legacy: bool,
        /// The device turned this computer's record down at pair-verify (it was erased, its
        /// trust was reset, or another pairing took this computer's place on it). Kept so
        /// `devices` and the app say to pair again, and cleared when a connection gets through
        /// or the device is paired anew.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        pub needs_repair: bool,
    }

    impl PairedDevice {
        /// The platform to sign and download for.
        pub fn platform(&self) -> super::Platform {
            self.product_type.as_deref().map(super::Platform::of_product).unwrap_or(super::Platform::VisionOS)
        }
    }

    /// Where pairings and the Apple ID's storage live: `devices.json`, `pairings/<udid>.plist`
    /// and `apple-id/`. The command line and the app share the default one.
    #[derive(Debug, Clone)]
    pub struct DeviceStore {
        pub dir: PathBuf,
    }

    impl Default for DeviceStore {
        fn default() -> Self {
            Self { dir: dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Apparition Installer") }
        }
    }

    /// The Apparition Store's folder (config, data, and `developer.json` while the app runs):
    /// `~/Library/Application Support/Apparition/Store` on a Mac, `%APPDATA%\Apparition\Store` on
    /// Windows, `~/.local/share/Apparition/Store` on Linux.
    pub fn store_dir() -> PathBuf {
        dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Apparition").join("Store")
    }

    impl DeviceStore {
        fn devices_path(&self) -> PathBuf {
            self.dir.join("devices.json")
        }

        /// The Apple ID's data directory (anisette state and the certificate key).
        pub fn apple_id_dir(&self) -> PathBuf {
            self.dir.join("apple-id")
        }

        pub fn devices(&self) -> Vec<PairedDevice> {
            std::fs::read(self.devices_path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
        }

        pub fn save(&self, devices: &[PairedDevice]) -> std::io::Result<()> {
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(self.devices_path(), serde_json::to_vec_pretty(devices)?)
        }

        pub fn device(&self, udid: &str) -> Option<PairedDevice> {
            self.devices().into_iter().find(|d| d.udid == udid)
        }

        /// Forgets a device and its pairing file. The headset keeps this computer in its Remote
        /// Devices list until it's removed there.
        pub fn forget(&self, udid: &str) -> std::io::Result<()> {
            let mut devices = self.devices();
            if let Some(device) = devices.iter().find(|d| d.udid == udid && !d.legacy) {
                let _ = std::fs::remove_file(self.dir.join("pairings").join(&device.pairing));
            }
            let _ = std::fs::remove_file(self.dir.join("pairings").join(format!("{udid}.app.plist")));
            devices.retain(|d| d.udid != udid);
            self.save(&devices)
        }

        /// Pairs with a device showing Settings > General > Remote Devices (an Apple TV:
        /// Remotes and Devices > Remote App and Devices), through its manual-pairing service at
        /// `address:pairing_port` (`pin` returns the PIN it shows), then asks the paired service
        /// who it is and remembers it. An Apple TV is asked a second PIN, for Apparition's record.
        pub async fn pair(
            &self,
            address: IpAddr,
            pairing_port: u16,
            pin: impl Fn() -> String,
        ) -> Result<(PairedDevice, super::DeviceInfo), Report> {
            let hostname = hostname();
            let mut pairing = RpPairingFile::generate(&hostname);
            super::pair(SocketAddr::new(address, pairing_port), &hostname, &mut pairing, &pin).await?;
            // The device now trusts this key in place of any earlier one of this computer's, so
            // the old file is already dead: keep the new one before anything else can fail
            // (the tunnel, the Apple TV's second PIN, a cancel). The device isn't known by its
            // UDID yet, so it goes where a device at this address keeps its record, and is
            // renamed below.
            std::fs::create_dir_all(self.dir.join("pairings"))?;
            let known = self.devices().into_iter().find(|d| !d.legacy && d.address == address);
            if let Some(known) = &known {
                std::fs::write(self.dir.join("pairings").join(&known.pairing), pairing.to_bytes())?;
                self.set_needs_repair(&known.udid, false);
            }
            let mut tunnel = super::connect(SocketAddr::new(address, TUNNEL_PORT), &hostname, &mut pairing, &pin).await?;
            let info = tunnel.device_info().await?;
            let file = format!("{}.plist", info.udid);
            std::fs::write(self.dir.join("pairings").join(&file), pairing.to_bytes())?;
            // An Apple TV's pairing screen closes when Apparition comes to the front, so it
            // can't pair itself as on visionOS: pair a record for it now (the TV shows a second
            // PIN) and hand it over at install, as for iPhone and iPad.
            if super::Platform::of_product(&info.product_type) == super::Platform::TvOS {
                let mut app_pairing = RpPairingFile::generate(APP_PAIRING_HOST);
                super::pair(SocketAddr::new(address, pairing_port), APP_PAIRING_HOST, &mut app_pairing, &pin)
                    .await
                    .context("Pairing Apparition's own record failed")?;
                std::fs::write(self.dir.join("pairings").join(format!("{}.app.plist", info.udid)), app_pairing.to_bytes())?;
            }
            let device = PairedDevice {
                udid: info.udid.clone(),
                name: info.name.clone(),
                address,
                port: TUNNEL_PORT,
                pairing: file,
                product_type: Some(info.product_type.clone()),
                legacy: false,
                needs_repair: false,
            };
            let mut devices = self.devices();
            devices.retain(|d| d.udid != info.udid);
            devices.push(device.clone());
            self.save(&devices)?;
            Ok((device, info))
        }

        /// Opens a tunnel to a paired device. `pin` is asked only if a headset or Apple TV wants
        /// to pair again; an iPhone or iPad has no PIN screen, so a pairing it turns down is an
        /// error saying to pair again over usbmuxd.
        pub async fn connect(&self, device: &PairedDevice, pin: impl Fn() -> String) -> Result<Tunnel, Report> {
            if device.legacy {
                bail!("{} runs iOS 17.3 or earlier, which has no RemotePairing tunnel Apparition can use: Apparition can only install and remove apps on it, over usbmuxd.", device.name);
            }
            let verified = self.connect_verified(device).await;
            match &verified {
                Err(error) if device.platform() != super::Platform::IOS && super::is_pairing_rejected(error) => {}
                _ => return verified,
            }
            // Only an explicitly rejected pairing can need a new PIN. Discovery tries each
            // alternate endpoint with the existing key, without pairing to a different device.
            let (mut pairing, address) = self.pairing_of(device)?;
            let before = pairing.to_bytes();
            let connected = super::connect(address, &hostname(), &mut pairing, pin).await;
            // A device that turned the record down is paired anew in place, which replaces the
            // key it trusts: keep the new record whether or not the rest of the connection
            // worked, or the next connection finds the file dead again.
            if pairing.to_bytes() != before {
                let _ = std::fs::write(self.dir.join("pairings").join(&device.pairing), pairing.to_bytes());
                self.set_needs_repair(&device.udid, false);
            } else if connected.is_ok() {
                self.set_needs_repair(&device.udid, false);
            }
            connected
        }

        /// Opens the developer services used by JIT and location simulation. After installing
        /// a DDI, reopen the tunnel so its service list includes the new services.
        pub async fn connect_for_developer(
            &self,
            device: &PairedDevice,
            pin: impl Fn() -> String,
            waiting: &(dyn Fn(&str) + Send + Sync),
            consent: crate::ddi::Consent<'_>,
        ) -> Result<Tunnel, Report> {
            let tunnel = self.connect_healing(device, &pin, waiting).await?;
            self.prepare_developer(device, tunnel, pin, waiting, consent).await
        }

        /// The second half of [`Self::connect_for_developer`], for callers that put their own time limit on
        /// connecting: mounts the Developer Disk Image on `tunnel` when none is, which can take a download
        /// and Apple's signing, and returns a tunnel that lists the developer services.
        pub async fn prepare_developer(
            &self,
            device: &PairedDevice,
            mut tunnel: Tunnel,
            pin: impl Fn() -> String,
            waiting: &(dyn Fn(&str) + Send + Sync),
            consent: crate::ddi::Consent<'_>,
        ) -> Result<Tunnel, Report> {
            if device.platform() != super::Platform::IOS
                || crate::ddi::mounted(&mut tunnel.adapter, &mut tunnel.handshake).await? {
                return Ok(tunnel);
            }
            waiting("Preparing the iOS Developer Disk Image");
            let bundle = crate::ddi::source(&self.dir.join("anisette"), consent).await?;
            crate::ddi::mount(&mut tunnel.adapter, &mut tunnel.handshake, &bundle).await?;
            drop(tunnel);
            self.connect_healing(device, pin, waiting).await
        }

        /// Where the iOS Developer Disk Image would come from for a hand-off or JIT here, and its size and
        /// Xcode build: `apparition ddi`.
        pub async fn developer_image(&self, consent: crate::ddi::Consent<'_>) -> Result<(String, usize, String), Report> {
            let (origin, bundle) = crate::ddi::resolve(&self.dir.join("anisette"), consent).await?;
            Ok((origin.describe().to_owned(), bundle.len(), crate::ddi::bundle_build(&bundle)))
        }

        /// Gives the on-device iOS app a checked copy of the Developer Disk Image (Xcode's, or a
        /// download `consent` allowed) so it can mount it later with no computer around.
        pub async fn hand_over_ddi(&self, tunnel: &mut Tunnel, bundle_id: &str, consent: crate::ddi::Consent<'_>) -> Result<(), Report> {
            let data = crate::ddi::source(&self.dir.join("anisette"), consent).await?;
            tunnel.put_documents(bundle_id, "", &[(crate::ddi::BUNDLE_NAME.to_owned(), data)]).await
        }

        /// [`DeviceStore::connect`], except that an iPhone or iPad which turned this computer's
        /// pairing down is paired again over usbmuxd first and then reached with the new one.
        /// That needs no PIN, only that it trusts this computer (it asks, through `waiting`; Wi-Fi
        /// is enough once it does), so a pairing that went stale heals itself instead of failing
        /// every install. A headset or Apple TV can't: re-pairing it takes a PIN typed from its
        /// pairing screen, which `connect` asks `pin` for.
        pub async fn connect_healing(
            &self,
            device: &PairedDevice,
            pin: impl Fn() -> String,
            waiting: &(dyn Fn(&str) + Send + Sync),
        ) -> Result<Tunnel, Report> {
            let first = match self.connect(device, &pin).await {
                Ok(tunnel) => return Ok(tunnel),
                Err(error) => error,
            };
            if device.legacy || device.platform() != super::Platform::IOS || !super::is_pairing_rejected(&first) {
                return Err(first);
            }
            waiting(&format!("{} no longer trusts this computer's pairing. Pairing again over usbmuxd...", device.name));
            let repaired = tokio::time::timeout(Duration::from_secs(90), self.pair_usb(&device.udid, waiting)).await;
            match repaired {
                Ok(Ok((fresh, _))) => {
                    waiting("Paired again.");
                    let mut tunnel = self.connect_verified(&fresh).await?;
                    // Pairing again replaced the key Apparition on the device was handed too, so
                    // its copy is dead as well: give it the new one now, or the device's own
                    // Apparition says it no longer trusts its pairing the next time it's used.
                    match tunnel.installed_like(super::APPARITION_ID).await {
                        Ok(ids) => {
                            for id in ids {
                                match self.hand_over_pairing(&mut tunnel, &id, &fresh.udid).await {
                                    Ok(true) => waiting(&format!("Gave {id} on {} its new pairing.", fresh.name)),
                                    Ok(false) => {}
                                    Err(error) => waiting(&format!(
                                        "Couldn't give {id} its new pairing ({}): run `apparition pairing hand-off`.",
                                        super::describe(&error)
                                    )),
                                }
                            }
                        }
                        Err(error) => waiting(&format!("Couldn't look for Apparition on {}: {}", fresh.name, super::describe(&error))),
                    }
                    Ok(tunnel)
                }
                Ok(Err(error)) => Err(report!(
                    "{} no longer trusts this computer's pairing, and pairing it again didn't work: {}\n\
                     Plug it in with a cable (or turn on its Wi-Fi sync in Finder or Apple Devices), unlock it, tap Trust if it asks, and run `apparition pair --usb`.",
                    device.name,
                    super::describe(&error).lines().next().unwrap_or("")
                ).into_dynamic()),
                Err(_) => Err(report!(
                    "{} no longer trusts this computer's pairing, and pairing it again timed out.\n\
                     Unlock it, tap Trust if it asks, and run `apparition pair --usb`.",
                    device.name
                ).into_dynamic()),
            }
        }

        /// Records that `udid`'s device turned this computer's pairing down (or that it didn't
        /// any more), when that differs from what's kept. Nothing is written otherwise.
        pub fn set_needs_repair(&self, udid: &str, needs: bool) {
            let mut devices = self.devices();
            if let Some(device) = devices.iter_mut().find(|d| d.udid == udid && d.needs_repair != needs) {
                device.needs_repair = needs;
                let _ = self.save(&devices);
            }
        }

        /// Opens what reaches `device`: its tunnel, or usbmuxd for a [`PairedDevice::legacy`] one.
        pub async fn open(&self, device: &PairedDevice, pin: impl Fn() -> String) -> Result<super::Connection, Report> {
            if device.legacy {
                return Ok(super::Connection::Legacy(crate::legacy::Legacy::connect(&device.udid).await?));
            }
            Ok(super::Connection::Tunnel(self.connect(device, pin).await?))
        }

        /// [`DeviceStore::open`] with [`DeviceStore::connect_healing`].
        pub async fn open_healing(
            &self,
            device: &PairedDevice,
            pin: impl Fn() -> String,
            waiting: &(dyn Fn(&str) + Send + Sync),
        ) -> Result<super::Connection, Report> {
            if device.legacy {
                return Ok(super::Connection::Legacy(crate::legacy::Legacy::connect(&device.udid).await?));
            }
            Ok(super::Connection::Tunnel(self.connect_healing(device, pin, waiting).await?))
        }

        fn pairing_of(&self, device: &PairedDevice) -> Result<(RpPairingFile, SocketAddr), Report> {
            let data = std::fs::read(self.dir.join("pairings").join(&device.pairing))
                .context(format!("The pairing with {} is missing; pair again", device.name))?;
            Ok((RpPairingFile::from_bytes(&data)?, SocketAddr::new(device.address, device.port)))
        }

        /// Opens a tunnel to a paired device without ever asking for a PIN: a pairing the device
        /// turns down is an error (see [`is_pairing_rejected`](super::is_pairing_rejected)), so a
        /// program with nobody at a terminal never waits on a pair-setup. If the saved endpoint
        /// fails, the device is looked up again under its name.
        pub async fn connect_verified(&self, device: &PairedDevice) -> Result<Tunnel, Report> {
            if device.legacy {
                bail!("{} runs iOS 17.3 or earlier, which has no RemotePairing tunnel Apparition can use: Apparition can only install and remove apps on it, over usbmuxd.", device.name);
            }
            let (mut pairing, address) = self.pairing_of(device)?;
            let connected = super::connect_verified(address, &hostname(), &mut pairing).await;
            // A device may move to another address, or an iPhone may change ports. The old
            // endpoint might even belong to another device now. Before saving an advertised
            // endpoint, check the existing pairing works and lockdown reports the expected UDID.
            if connected.is_err() {
                let found = browse_services(3, &[PAIRED_SERVICE]).await.unwrap_or_default();
                let recovered = tokio::time::timeout(Duration::from_secs(12), async {
                    for candidate in alternate_endpoints(&found, &device.name, address) {
                        let verified = tokio::time::timeout(Duration::from_secs(4), async {
                            let mut tunnel = super::connect_verified(candidate, &hostname(), &mut pairing).await.ok()?;
                            let info = tunnel.device_info().await.ok()?;
                            (info.udid.eq_ignore_ascii_case(&device.udid)).then_some(tunnel)
                        }).await;
                        if let Ok(Some(tunnel)) = verified {
                            return Some((candidate, tunnel));
                        }
                    }
                    None
                }).await;
                if let Ok(Some((candidate, tunnel))) = recovered {
                    let mut devices = self.devices();
                    if let Some(saved) = devices.iter_mut().find(|d| d.udid == device.udid
                        && d.pairing == device.pairing && SocketAddr::new(d.address, d.port) == address) {
                        (saved.address, saved.port) = (candidate.ip(), candidate.port());
                        saved.needs_repair = false;
                        let _ = self.save(&devices);
                    }
                    return Ok(tunnel);
                }
            }
            match &connected {
                Ok(_) => self.set_needs_repair(&device.udid, false),
                Err(error) if super::is_pairing_rejected(error) => self.set_needs_repair(&device.udid, true),
                Err(_) => {}
            }
            match connected {
                Err(error) if super::is_pairing_rejected(&error) => {
                    let how = if device.platform() == super::Platform::IOS {
                        "`apparition pair --usb` (over Wi-Fi works once it trusts this computer)"
                    } else {
                        "`apparition pair`"
                    };
                    Err(error
                        .context(format!("{} no longer trusts this computer's pairing. Pair again with {how}.", device.name))
                        .into_dynamic())
                }
                other => other,
            }
        }

        /// Pairs with an iPhone or iPad over usbmuxd (it has no PIN screen; if it doesn't trust
        /// this computer yet, it's asked to, and `waiting` hears what to do on it), then finds its own `_remotepairing._tcp` service over mDNS (its
        /// port isn't fixed as on visionOS) by trying each until one answers to the new record.
        /// Also makes and keeps a record for Apparition on the device, which
        /// [`DeviceStore::hand_over_pairing`] gives it at install.
        pub async fn pair_usb(
            &self,
            udid: &str,
            waiting: &(dyn Fn(&str) + Send + Sync),
        ) -> Result<(PairedDevice, super::DeviceInfo), Report> {
            let hostname = hostname();
            // Before iOS 17.4 there's no RemotePairing tunnel to pair for: usbmuxd's trust is the pairing.
            let (_, identity) = super::usb_identity(udid, &hostname, waiting).await?;
            if super::is_legacy_os(&identity.product_type, &identity.os_version) {
                let device = PairedDevice {
                    udid: identity.udid.clone(),
                    name: identity.name.clone(),
                    address: std::net::Ipv4Addr::UNSPECIFIED.into(),
                    port: 0,
                    pairing: String::new(),
                    product_type: Some(identity.product_type.clone()),
                    legacy: true,
                    needs_repair: false,
                };
                let mut devices = self.devices();
                devices.retain(|d| d.udid != identity.udid);
                devices.push(device.clone());
                self.save(&devices)?;
                return Ok((device, identity));
            }
            let mut pairing = RpPairingFile::generate(&hostname);
            let info = super::pair_usb(udid, &hostname, &mut pairing, waiting).await?;
            // The device now trusts this key in place of any earlier one of this computer's, so
            // the old file is already dead: keep the new one before anything else can fail
            // (the second pairing, finding the device on the network, a cancel).
            let pairings = self.dir.join("pairings");
            std::fs::create_dir_all(&pairings)?;
            let file = format!("{}.plist", info.udid);
            std::fs::write(pairings.join(&file), pairing.to_bytes())?;
            self.set_needs_repair(&info.udid, false);
            // Apparition on the device can't pair with its own device either, so it gets a
            // record of its own (under its name, so the device lists it separately).
            let mut app_pairing = RpPairingFile::generate(APP_PAIRING_HOST);
            super::pair_usb(udid, APP_PAIRING_HOST, &mut app_pairing, waiting).await?;
            std::fs::write(pairings.join(format!("{}.app.plist", info.udid)), app_pairing.to_bytes())?;

            let no_pin = || String::new();
            let mut reached = None;
            for found in browse(4).await?.into_iter().filter(|f| !f.ready_to_pair) {
                let Some(address) = found.address() else { continue };
                let attempt = tokio::time::timeout(
                    Duration::from_secs(5),
                    super::connect(SocketAddr::new(address, found.port), &hostname, &mut pairing, no_pin),
                );
                if let Ok(Ok(mut tunnel)) = attempt.await {
                    if tunnel.device_info().await.is_ok_and(|i| i.udid == info.udid) {
                        reached = Some((address, found.port));
                        break;
                    }
                }
            }
            let (address, port) = reached.ok_or_else(|| {
                report!("Paired, but {} didn't answer on the network. Keep it unlocked on the same Wi-Fi as this computer and pair again.", info.name)
            })?;
            let device = PairedDevice {
                udid: info.udid.clone(),
                name: info.name.clone(),
                address,
                port,
                pairing: file,
                product_type: Some(info.product_type.clone()),
                legacy: false,
                needs_repair: false,
            };
            let mut devices = self.devices();
            devices.retain(|d| d.udid != info.udid);
            devices.push(device.clone());
            self.save(&devices)?;
            Ok((device, info))
        }

        /// For a device paired with [`DeviceStore::pair_usb`] (or an Apple TV, which pairs a
        /// second record for Apparition): puts the record made for Apparition where it keeps its
        /// own (Documents/apparition_rp_pairing.plist), so it can reach its device. False when
        /// there's none (a headset, which pairs itself in Setup). Installing Apparition does this;
        /// `apparition pairing hand-off` does it for one already installed.
        pub async fn hand_over_pairing(&self, tunnel: &mut Tunnel, bundle_id: &str, udid: &str) -> Result<bool, Report> {
            if !self.has_app_pairing(udid) {
                return Ok(false);
            }
            let record = std::fs::read(self.app_pairing_path(udid))?;
            tunnel.put_documents(bundle_id, "", &[("apparition_rp_pairing.plist".to_string(), record)]).await?;
            Ok(true)
        }

        fn app_pairing_path(&self, udid: &str) -> PathBuf {
            self.dir.join("pairings").join(format!("{udid}.app.plist"))
        }

        /// Whether there's a record for Apparition on `udid` to hand over
        /// ([`DeviceStore::hand_over_pairing`]).
        pub fn has_app_pairing(&self, udid: &str) -> bool {
            self.app_pairing_path(udid).is_file()
        }

        /// Updates a paired device's address, when mDNS finds it somewhere else.
        pub fn set_address(&self, udid: &str, address: IpAddr) -> std::io::Result<()> {
            let mut devices = self.devices();
            for device in devices.iter_mut().filter(|d| d.udid == udid) {
                device.address = address;
            }
            self.save(&devices)
        }
    }

    /// A RemotePairing service seen on the network.
    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Found {
        pub name: String,
        /// Showing Settings > General > Remote Devices: the manual-pairing service.
        pub ready_to_pair: bool,
        pub addresses: Vec<IpAddr>,
        pub port: u16,
    }

    impl Found {
        /// Prefers an IPv4 address: link-local IPv6 ones need a scope this doesn't track. A
        /// link-local IPv4 one (169.254.*, a cable's or AWDL's) is a last resort: it's gone
        /// when the device is unplugged or moves. So is an iPhone's own Personal Hotspot
        /// address (172.20.10.0/28), which only answers while this computer uses the hotspot.
        pub fn address(&self) -> Option<IpAddr> {
            let hotspot = |v4: &std::net::Ipv4Addr| matches!(v4.octets(), [172, 20, 10, last] if last < 16);
            let lan = |a: &&IpAddr| matches!(a, IpAddr::V4(v4) if !v4.is_link_local() && !hotspot(v4));
            let routable = |a: &&IpAddr| matches!(a, IpAddr::V4(v4) if !v4.is_link_local());
            self.addresses.iter().find(lan)
                .or_else(|| self.addresses.iter().find(routable))
                .or_else(|| self.addresses.iter().find(|a| a.is_ipv4()))
                .or_else(|| self.addresses.first())
                .copied()
        }
    }

    fn alternate_endpoints(found: &[Found], name: &str, saved: SocketAddr) -> Vec<SocketAddr> {
        let mut candidates = Vec::new();
        for service in found.iter().filter(|f| !f.ready_to_pair && f.name == name) {
            for ip in &service.addresses {
                // A link-local IPv6 address needs an interface scope, which Found doesn't track.
                if matches!(ip, IpAddr::V6(ip) if ip.is_unicast_link_local()) {
                    continue;
                }
                let candidate = SocketAddr::new(*ip, service.port);
                if candidate != saved && !candidates.contains(&candidate) {
                    candidates.push(candidate);
                }
            }
        }
        candidates.sort_by_key(|address| match address.ip() {
            IpAddr::V4(ip) if !ip.is_link_local() => 0,
            IpAddr::V6(_) => 1,
            IpAddr::V4(_) => 2,
        });
        candidates
    }

    #[cfg(test)]
    #[test]
    fn rediscovery_prefers_lan_ipv4_and_keeps_a_changed_port() {
        let advertised = |name: &str, port, ready_to_pair| Found {
            name: name.to_owned(),
            ready_to_pair,
            addresses: ["fd00::1", "fe80::1", "169.254.0.2", "192.168.1.2", "192.168.1.2"]
                .map(|ip| ip.parse().unwrap()).to_vec(),
            port,
        };
        let found = [advertised("AVP", 2, false), advertised("Other", 3, false), advertised("AVP", 4, true)];
        let saved = "[fd00::1]:1".parse().unwrap();
        let expected = ["192.168.1.2:2", "[fd00::1]:2", "169.254.0.2:2"].map(|address| address.parse::<SocketAddr>().unwrap());
        assert_eq!(alternate_endpoints(&found, "AVP", saved), expected);
        assert_eq!(alternate_endpoints(&found, "AVP", expected[0]), expected[1..]);
    }

    /// Browses both RemotePairing services for `seconds`. On macOS a program outside an app
    /// bundle can't use multicast (Local Network privacy), so it finds nothing there.
    pub async fn browse(seconds: u64) -> Result<Vec<Found>, Report> {
        browse_services(seconds, &[MANUAL_PAIRING_SERVICE, PAIRED_SERVICE]).await
    }

    async fn browse_services(seconds: u64, services: &[&str]) -> Result<Vec<Found>, Report> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let mut found: Vec<Found> = Vec::new();
        for &service in services {
            let ready_to_pair = service == MANUAL_PAIRING_SERVICE;
            let events = daemon.browse(service)?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
            while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
                if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                    let name = info.get_property_val_str("name").unwrap_or(info.get_hostname()).trim_end_matches(".local.").to_string();
                    let addresses: Vec<IpAddr> = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                    // One device resolves once per interface: merge them.
                    match found.iter_mut().find(|f| f.ready_to_pair == ready_to_pair && f.name == name && f.port == info.get_port()) {
                        Some(existing) => {
                            for address in addresses {
                                if !existing.addresses.contains(&address) {
                                    existing.addresses.push(address);
                                }
                            }
                        }
                        None => found.push(Found { name, ready_to_pair, addresses, port: info.get_port() }),
                    }
                }
            }
            let _ = daemon.stop_browse(service);
        }
        let _ = daemon.shutdown();
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::{BuildSigning, Slot};

    #[cfg(feature = "desktop")]
    #[test]
    fn a_rejected_pairing_is_remembered_until_one_gets_through() {
        let dir = tempfile::tempdir().unwrap();
        let store = super::DeviceStore { dir: dir.path().to_path_buf() };
        // A devices.json from before the flag existed has no such field.
        std::fs::write(
            dir.path().join("devices.json"),
            br#"[{"udid":"U1","name":"ixPhone","address":"10.0.0.2","port":49152,"pairing":"U1.plist","product_type":"iPhone16,2"}]"#,
        )
        .unwrap();
        assert!(!store.device("U1").unwrap().needs_repair);

        store.set_needs_repair("U1", true);
        assert!(store.device("U1").unwrap().needs_repair);
        assert!(std::fs::read_to_string(dir.path().join("devices.json")).unwrap().contains("needs_repair"));

        store.set_needs_repair("U1", false);
        assert!(!store.device("U1").unwrap().needs_repair);
        // Back to the old shape, so an older Apparition reads the file the same.
        assert!(!std::fs::read_to_string(dir.path().join("devices.json")).unwrap().contains("needs_repair"));

        // An unknown device, or no change, writes nothing.
        let before = std::fs::metadata(dir.path().join("devices.json")).unwrap().modified().unwrap();
        store.set_needs_repair("nobody", true);
        store.set_needs_repair("U1", false);
        assert_eq!(std::fs::metadata(dir.path().join("devices.json")).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn build_signing_collects_p12s_and_profiles_only() {
        let dir = tempfile::tempdir().unwrap();
        for (sub, name) in [
            ("certs", "development.p12"),
            ("certs", "notes.txt"),
            ("certs", ".hidden.p12"),
            ("profiles", "b.mobileprovision"),
            ("profiles", "a.mobileprovision"),
            ("profiles", "Mac.provisionprofile"),
        ] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
            std::fs::write(dir.path().join(sub).join(name), b"x").unwrap();
        }
        let found = BuildSigning::discover(Some(dir.path())).unwrap();
        let names = |files: &[std::path::PathBuf]| files.iter().map(|f| f.file_name().unwrap().to_string_lossy().to_string()).collect::<Vec<_>>();
        assert_eq!(names(&found.p12s), ["development.p12"]);
        assert_eq!(names(&found.profiles), ["a.mobileprovision", "b.mobileprovision"]);
        let empty = tempfile::tempdir().unwrap();
        assert!(BuildSigning::discover(Some(empty.path())).is_err());
    }

    #[test]
    fn slot_errors() {
        assert_eq!(Slot::of_error("Apple error 7460: no more certificates"), Some(Slot::Certificates));
        assert_eq!(Slot::of_error("You already have the maximum number of certificates"), Some(Slot::Certificates));
        assert_eq!(Slot::of_error("Not enough available app IDs"), Some(Slot::AppIds));
        assert_eq!(Slot::of_error("Cannot create more than 10 App IDs"), Some(Slot::AppIds));
        assert_eq!(Slot::of_error("Device registration failed"), None);
    }

    #[test]
    fn legacy_os_is_before_ios_17_4() {
        assert!(super::is_legacy_os("iPhone10,4", "16.3.1"));
        assert!(super::is_legacy_os("iPad7,5", "12.5.7"));
        assert!(super::is_legacy_os("AppleTV6,2", "16.6"));
        assert!(super::is_legacy_os("iPhone11,8", "17.0"));
        assert!(super::is_legacy_os("iPhone11,8", "17"));
        assert!(super::is_legacy_os("iPhone11,8", "17.3.1"));
        assert!(!super::is_legacy_os("iPhone11,8", "17.4"));
        assert!(!super::is_legacy_os("iPhone11,8", "18.7.2"));
        assert!(!super::is_legacy_os("AppleTV6,2", "17.0"));
        assert!(!super::is_legacy_os("iPhone17,1", "26.1"));
        assert!(!super::is_legacy_os("RealityDevice14,1", "2.4"));
        assert!(!super::is_legacy_os("iPhone10,4", ""));
        assert!(!super::is_legacy_os("iPhone10,4", "beta"));
    }

    #[test]
    fn apparition_is_untested_below_26_where_it_runs() {
        assert!(super::is_untested_os("RealityDevice14,1", "2.4"));
        assert!(super::is_untested_os("iPhone11,8", "18.7.2"));
        assert!(super::is_untested_os("iPhone11,8", "17.4"));
        assert!(super::is_untested_os("AppleTV11,1", "18.2"));
        assert!(!super::is_untested_os("iPhone11,8", "17.3.1"), "legacy, with its own notice");
        assert!(!super::is_untested_os("iPhone10,4", "16.7"));
        assert!(!super::is_untested_os("RealityDevice14,1", "26.0"));
        assert!(!super::is_untested_os("iPhone17,1", "26.1"));
        assert!(!super::is_untested_os("AppleTV14,1", "27.0"));
        assert!(!super::is_untested_os("iPhone11,8", ""));
        assert!(!super::is_untested_os("iPhone11,8", "beta"));
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn paired_device_records_from_before_legacy_load_as_current() {
        let old = r#"[{"udid":"u","name":"n","address":"192.168.1.5","port":49152,"pairing":"u.plist"}]"#;
        let devices: Vec<super::PairedDevice> = serde_json::from_str(old).unwrap();
        assert!(!devices[0].legacy);
        let saved = serde_json::to_string(&devices).unwrap();
        assert!(!saved.contains("legacy"), "{saved}");
    }

    #[cfg(feature = "desktop")]
    #[test]
    fn found_address_prefers_the_lan() {
        let found = |addresses: &[&str]| super::Found {
            name: String::new(),
            ready_to_pair: false,
            addresses: addresses.iter().map(|a| a.parse().unwrap()).collect(),
            port: 0,
        }
        .address()
        .map(|a| a.to_string());
        let lan = found(&["fe80::1", "169.254.170.69", "172.20.10.1", "192.168.1.20"]);
        assert_eq!(lan.as_deref(), Some("192.168.1.20"));
        assert_eq!(found(&["169.254.170.69", "172.20.10.1"]).as_deref(), Some("172.20.10.1"));
        assert_eq!(found(&["172.20.10.20"]).as_deref(), Some("172.20.10.20"));
        assert_eq!(found(&["fe80::1", "169.254.170.69"]).as_deref(), Some("169.254.170.69"));
    }
}

/// Preview a certificate install's actual identifier and covered extensions before review.
pub fn offline_install_plan(data_dir: &Path, path: std::path::PathBuf, identity: &str, udid: &str, platform: Platform) -> Result<Value, Report> {
    crate::identity::offline_install_plan(data_dir, path, identity, udid, platform)
}
