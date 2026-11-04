//! The devices this Mac reaches itself, through the core's own pairings (no `devicectl`, no
//! Xcode tunnel): which are paired, what they have installed, and installing an IPA that
//! arrived from another computer (`deploy.rs`).
//!
//! The Store never talks to a device. The hosting app supplies a `Devices` (the desktop app
//! implements it over `apparition-core`), so this crate stays free of the core and the routes can
//! be tested with a fake. A Store with no `Devices` answers 501 to every device route.

use std::{future::Future, path::PathBuf, pin::Pin};

use serde::Serialize;

pub type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub udid: String,
    pub name: String,
    /// "iOS", "visionOS", "tvOS", "watchOS"; what the build manifests call `platform`.
    pub platform: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct App {
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
    pub build: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    pub bundle_id: String,
    pub device: String,
    pub seconds: f64,
    /// The device is holding an update until the open app is closed.
    pub queued: bool,
    /// What signed it: the certificate's name, or None when it went as it was.
    pub signed_with: Option<String>,
}

pub trait Devices: Send + Sync {
    /// The devices this Mac is paired with (the core's `DeviceStore`).
    fn list(&self) -> Fut<'_, Vec<Device>>;
    /// What `device` (a name or UDID from `list`) has installed, over this Mac's pairing.
    fn apps(&self, device: String) -> Fut<'_, Vec<App>>;
    /// Installs `ipa` on `device`. It signs the app here with the certificate and profiles
    /// imported on this Mac when one covers the device, and sends it as it is when none does and
    /// it is already signed for the device. The error says what to do next.
    fn install(&self, device: String, ipa: PathBuf) -> Fut<'_, Installed>;
}

/// The device a request names: by UDID, else by name ignoring case.
pub fn find<'a>(devices: &'a [Device], wanted: &str) -> Result<&'a Device, String> {
    let wanted = wanted.trim();
    if let Some(d) = devices.iter().find(|d| d.udid.eq_ignore_ascii_case(wanted)) {
        return Ok(d);
    }
    let matches: Vec<&Device> = devices.iter().filter(|d| d.name.to_lowercase() == wanted.to_lowercase()).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("this Mac is not paired with a device named '{wanted}'{}", if devices.is_empty() { String::new() } else {
            format!(" (paired: {})", devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", "))
        })),
        _ => Err(format!("more than one paired device is named '{wanted}'; use its UDID")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(udid: &str, name: &str) -> Device {
        Device { udid: udid.into(), name: name.into(), platform: "iOS".into(), model: String::new() }
    }

    #[test]
    fn a_device_is_found_by_udid_or_name() {
        let list = vec![device("AAAA-1", "ixPhone"), device("BBBB-2", "Vision Pro"), device("CCCC-3", "vision pro")];
        assert_eq!(find(&list, "aaaa-1").unwrap().name, "ixPhone");
        assert_eq!(find(&list, " IXPHONE ").unwrap().udid, "AAAA-1");
        assert!(find(&list, "vision pro").unwrap_err().contains("UDID"), "two share a name");
        assert_eq!(find(&list, "BBBB-2").unwrap().name, "Vision Pro");
        let missing = find(&list, "ghost").unwrap_err();
        assert!(missing.contains("ghost") && missing.contains("ixPhone"), "{missing}");
        assert!(find(&[], "ghost").unwrap_err().contains("not paired"));
    }
}
