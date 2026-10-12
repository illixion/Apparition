//! The core as a Rust library, for the desktop installer (`installer/`). The app reaches the
//! same code through the C ABI; here nothing is global, and the caller owns its tokio runtime,
//! its pairing files and its Apple ID session.
//!
//! A desktop isn't the device it installs to, so it reaches the device's RemotePairing
//! service directly: no relay, no LocalDevVPN.

use std::{net::SocketAddr, path::Path};

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

pub use crate::{
    Session,
    account::{delete_app_id, list_app_ids, list_certificates, revoke_certificate},
    identity::{Platform, import_identity, import_profile, sign_offline},
    share::{export_apple_id_key, import_signing_file},
    sign_in, sign_with_apple_id,
};

/// Sets up what the core's crypto and isideload need. Call once, before anything else.
pub fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// Pairs with `udid` through usbmuxd and lockdown: remotepairingdeviced's lockdown service
/// creates the RemotePairing record (no PIN; the computer has to be trusted already, which
/// the device asks for when it's first plugged in). Fills in `pairing`, the same record the
/// PIN flow makes, so every later connection is over the network. Returns the device's
/// identity, read over the same lockdown connection.
pub async fn pair_usb(udid: &str, hostname: &str, pairing: &mut RpPairingFile) -> Result<DeviceInfo, Report> {
    use idevice::{IdeviceService, remote_pairing::RemotePairingLockdownService};
    let mut usbmuxd = idevice::usbmuxd::UsbmuxdConnection::default().await.context("Can't reach usbmuxd")?;
    let devices = usbmuxd.get_devices().await?;
    // Prefer the cable: lockdown services that set up pairing may refuse the network.
    let device = devices
        .iter()
        .filter(|d| d.udid == udid)
        .min_by_key(|d| !matches!(d.connection_type, idevice::usbmuxd::Connection::Usb))
        .ok_or_else(|| report!("{udid} isn't connected. Plug it in with a cable and unlock it."))?;
    let provider = device.to_provider(idevice::usbmuxd::UsbmuxdAddr::default(), "apparition");
    let info = {
        let mut lockdown = LockdownClient::connect(&provider)
            .await
            .context("Can't reach lockdown. Unlock the device and tap Trust if it asks.")?;
        let pairing_file = provider_pairing_file(&provider).await?;
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
    let service = RemotePairingLockdownService::connect(&provider)
        .await
        .context("The device didn't start its pairing service (remotepairingdeviced)")?;
    let mut client = service.into_client(hostname)?;
    client.connect(pairing, async || "000000".to_string()).await.context("Pairing failed")?;
    Ok(info)
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
    let (adapter, handshake) = crate::tunnel::open(device, None, hostname, pairing, pin).await?;
    Ok(Tunnel { adapter, handshake })
}

/// The device's identity, from lockdown over the tunnel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub udid: String,
    pub name: String,
    pub product_type: String,
    pub os_version: String,
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

    /// The display name of `bundle_id` if it's installed, from installation_proxy.
    pub async fn installed_app(&mut self, bundle_id: &str) -> Result<Option<String>, Report> {
        let mut installer =
            idevice::installation_proxy::InstallationProxyClient::connect_rsd(&mut self.adapter, &mut self.handshake)
                .await
                .context("installation_proxy connect failed")?;
        let apps = installer.get_apps(Some("User"), Some(vec![bundle_id.to_string()])).await?;
        Ok(apps.get(bundle_id).map(|info| {
            let info = info.as_dictionary();
            let name = |key: &str| info.and_then(|i| i.get(key)).and_then(plist::Value::as_string);
            name("CFBundleDisplayName").or_else(|| name("CFBundleName")).unwrap_or(bundle_id).to_string()
        }))
    }

    /// Packs and installs a signed app. See `apparition_install_ipa`.
    pub async fn install(&mut self, app_path: &Path, source_ipa: Option<&Path>, detach_after_ms: u64) -> Result<Value, Report> {
        crate::ipa::install(&mut self.adapter, &mut self.handshake, app_path, source_ipa, detach_after_ms).await
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
    match machine {
        Some(machine) => format!("Apparition Installer on {machine}"),
        None => "Apparition Installer".to_string(),
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
        (lower.contains("app id") && ["limit", "maximum", "more than", "9401"].iter().any(|w| lower.contains(w)))
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
        .asset("app", release_platform(platform))
        .ok_or_else(|| report!("Apparition {} has no {} build", verified.release.version, platform.os_name()))?
        .clone();
    crate::release::download(verified, &asset, dir, progress).await
}

/// What signed Apparition, for handing to it so it can refresh itself and its apps.
pub enum Handoff<'a> {
    /// The Apple ID's certificate key, from `<data_dir>/storage.json`: Apparition then signs
    /// with the same certificate, so the two use one of a free account's two.
    AppleId { data_dir: &'a Path, email: &'a str, team_id: &'a str },
    /// The .p12 and profiles, as files; Apparition asks for the .p12's password once.
    Certificate { p12: &'a Path, profiles: &'a [std::path::PathBuf] },
}

impl Tunnel {
    /// Puts `handoff` in the Files inbox (Documents/Signing) of the installed `bundle_id`.
    pub async fn hand_off_signing(&mut self, bundle_id: &str, handoff: Handoff<'_>) -> Result<(), Report> {
        let file_name = |path: &Path| -> Result<String, Report> {
            Ok(path.file_name().ok_or_else(|| report!("bad file name"))?.to_string_lossy().to_string())
        };
        let files = match handoff {
            Handoff::AppleId { data_dir, email, team_id } => {
                let file = export_apple_id_key(data_dir, email, Some(team_id), &hostname())?;
                vec![("apple-id.apparitionsigning".to_string(), file)]
            }
            Handoff::Certificate { p12, profiles } => {
                let mut files = vec![(file_name(p12)?, std::fs::read(p12)?)];
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
            if let Some(device) = devices.iter().find(|d| d.udid == udid) {
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
            let mut tunnel = super::connect(SocketAddr::new(address, TUNNEL_PORT), &hostname, &mut pairing, &pin).await?;
            let info = tunnel.device_info().await?;
            let file = format!("{}.plist", info.udid);
            std::fs::create_dir_all(self.dir.join("pairings"))?;
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
            };
            let mut devices = self.devices();
            devices.retain(|d| d.udid != info.udid);
            devices.push(device.clone());
            self.save(&devices)?;
            Ok((device, info))
        }

        /// Opens a tunnel to a paired device. `pin` is asked only if it wants to pair again.
        pub async fn connect(&self, device: &PairedDevice, pin: impl Fn() -> String) -> Result<Tunnel, Report> {
            let data = std::fs::read(self.dir.join("pairings").join(&device.pairing))
                .context(format!("The pairing with {} is missing; pair again", device.name))?;
            let mut pairing = RpPairingFile::from_bytes(&data)?;
            super::connect(SocketAddr::new(device.address, device.port), &hostname(), &mut pairing, pin).await
        }

        /// Pairs with an iPhone or iPad over usbmuxd (it has no PIN screen, and must trust this
        /// computer already), then finds its own `_remotepairing._tcp` service over mDNS (its
        /// port isn't fixed as on visionOS) by trying each until one answers to the new record.
        /// Also makes and keeps a record for Apparition on the device, which
        /// [`DeviceStore::hand_over_pairing`] gives it at install.
        pub async fn pair_usb(&self, udid: &str) -> Result<(PairedDevice, super::DeviceInfo), Report> {
            let hostname = hostname();
            let mut pairing = RpPairingFile::generate(&hostname);
            let info = super::pair_usb(udid, &hostname, &mut pairing).await?;
            // Apparition on the device can't pair with its own device either, so it gets a
            // record of its own (under its name, so the device lists it separately).
            let mut app_pairing = RpPairingFile::generate(APP_PAIRING_HOST);
            super::pair_usb(udid, APP_PAIRING_HOST, &mut app_pairing).await?;
            let pairings = self.dir.join("pairings");
            std::fs::create_dir_all(&pairings)?;
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
            let file = format!("{}.plist", info.udid);
            std::fs::write(pairings.join(&file), pairing.to_bytes())?;
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
            };
            let mut devices = self.devices();
            devices.retain(|d| d.udid != info.udid);
            devices.push(device.clone());
            self.save(&devices)?;
            Ok((device, info))
        }

        /// For a device paired with [`DeviceStore::pair_usb`]: puts the record made for
        /// Apparition where it keeps its own (Documents/apparition_rp_pairing.plist), so it
        /// can reach its device. False when there's none (a headset).
        pub async fn hand_over_pairing(&self, tunnel: &mut Tunnel, bundle_id: &str, udid: &str) -> Result<bool, Report> {
            let Ok(record) = std::fs::read(self.dir.join("pairings").join(format!("{udid}.app.plist"))) else {
                return Ok(false);
            };
            tunnel.put_documents(bundle_id, "", &[("apparition_rp_pairing.plist".to_string(), record)]).await?;
            Ok(true)
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
        /// when the device is unplugged or moves.
        pub fn address(&self) -> Option<IpAddr> {
            let lan = |a: &&IpAddr| matches!(a, IpAddr::V4(v4) if !v4.is_link_local());
            self.addresses.iter().find(lan).or_else(|| self.addresses.iter().find(|a| a.is_ipv4())).or_else(|| self.addresses.first()).copied()
        }
    }

    /// Browses both RemotePairing services for `seconds`. On macOS a program outside an app
    /// bundle can't use multicast (Local Network privacy), so it finds nothing there.
    pub async fn browse(seconds: u64) -> Result<Vec<Found>, Report> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let mut found: Vec<Found> = Vec::new();
        for service in [MANUAL_PAIRING_SERVICE, PAIRED_SERVICE] {
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
    use super::Slot;

    #[test]
    fn slot_errors() {
        assert_eq!(Slot::of_error("Apple error 7460: no more certificates"), Some(Slot::Certificates));
        assert_eq!(Slot::of_error("You already have the maximum number of certificates"), Some(Slot::Certificates));
        assert_eq!(Slot::of_error("Cannot create more than 10 App IDs"), Some(Slot::AppIds));
        assert_eq!(Slot::of_error("Device registration failed"), None);
    }
}
