//! Devices on iOS 16 and earlier, reached through usbmuxd and lockdown.
//!
//! Everything else here talks to a device through its RemotePairing tunnel and RSD, which
//! arrived with iOS 17. Before that the same services (AFC, installation_proxy) are lockdown
//! services, so a device that trusts this computer (usbmuxd's pair record, from Finder, Apple
//! Devices or `apparition pair --usb`) needs no pairing of its own. Over the cable, or Wi-Fi
//! sync once the device allows it. Installing and listing work this way; JIT does not (iOS 16's
//! debugserver needs a developer disk image mounted, and its own protocol).
//!
//! Best effort: this path has been written against the protocol, not yet run on an iOS 16
//! device.

use std::path::Path;

use idevice::{
    IdeviceService, afc::AfcClient, installation_proxy::InstallationProxyClient, lockdown::LockdownClient, provider::{IdeviceProvider, UsbmuxdProvider},
};
use rootcause::prelude::*;
use serde_json::Value;

use crate::api::{DeviceInfo, installed_ids, installed_name, usb_provider};

/// A device reached over usbmuxd. Each call opens its own lockdown connection.
pub struct Legacy {
    provider: UsbmuxdProvider,
}

impl Legacy {
    pub async fn connect(udid: &str) -> Result<Legacy, Report> {
        Ok(Legacy { provider: usb_provider(udid).await? })
    }

    pub async fn device_info(&mut self) -> Result<DeviceInfo, Report> {
        let pairing_file = self.provider.get_pairing_file().await.context("usbmuxd has no pair record for this device: trust this computer on it first")?;
        let mut lockdown = LockdownClient::connect(&self.provider)
            .await
            .context("Can't reach lockdown. Keep the device plugged in and unlocked.")?;
        lockdown.start_session(&pairing_file).await.context("This computer isn't trusted by the device. Unlock it and tap Trust, then try again.")?;
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

    async fn installer(&self) -> Result<InstallationProxyClient, Report> {
        Ok(InstallationProxyClient::connect(&self.provider).await.context("installation_proxy connect failed")?)
    }

    /// Removes an installed copy and its app container.
    pub async fn uninstall(&mut self, bundle_id: &str) -> Result<(), Report> {
        self.installer().await?.uninstall(bundle_id.to_string(), None).await?;
        Ok(())
    }

    /// The display name of `bundle_id` if it's installed.
    pub async fn installed_app(&mut self, bundle_id: &str) -> Result<Option<String>, Report> {
        let apps = self.installer().await?.get_apps(Some("User"), Some(vec![bundle_id.to_string()])).await?;
        Ok(installed_name(&apps, bundle_id))
    }

    /// The user apps installed whose bundle ID is `id` or starts with `id.`, sorted.
    pub async fn installed_like(&mut self, id: &str) -> Result<Vec<String>, Report> {
        let apps = self.installer().await?.get_apps(Some("User"), None).await?;
        Ok(installed_ids(&apps, id))
    }

    /// Packs and installs a signed app, as [`crate::api::Tunnel::install`] does over the tunnel.
    pub async fn install(&mut self, app_path: &Path, source_ipa: Option<&Path>, detach_after_ms: u64) -> Result<Value, Report> {
        let staged = crate::ipa::Staged::pack(app_path, source_ipa)?;
        let afc = AfcClient::connect(&self.provider).await.context("AFC connect failed")?;
        // What ideviceinstaller sends for a development-signed package.
        let mut options = plist::Dictionary::new();
        options.insert("PackageType".into(), "Developer".into());
        staged.send(afc, async || self.installer().await, detach_after_ms, Some(plist::Value::Dictionary(options))).await
    }
}
