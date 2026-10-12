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
    identity::{import_identity, import_profile, sign_offline},
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
    rpc.connect(pairing, async || pin()).await.context("Pairing failed")?;
    Ok(())
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
    /// `folder` (created if missing). Works for apps that share files with Files
    /// (UIFileSharingEnabled), Apparition among them.
    pub async fn put_documents(&mut self, bundle_id: &str, folder: &str, files: &[(String, Vec<u8>)]) -> Result<(), Report> {
        let house_arrest = HouseArrestClient::connect_rsd(&mut self.adapter, &mut self.handshake)
            .await
            .context("house_arrest connect failed")?;
        let mut afc = house_arrest
            .vend_documents(bundle_id)
            .await
            .context(format!("{bundle_id} doesn't share its Documents (is it installed?)"))?;
        let folder = format!("/Documents/{}", folder.trim_matches('/'));
        let _ = afc.mk_dir(folder.clone()).await;
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
