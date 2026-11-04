//! The devices this computer reaches through its own pairings, for the Store (`apparition_service::devices`):
//! the paired list, what each has installed, and installing an IPA that arrived from another
//! computer. All of it over the core's tunnel: no `devicectl`, so Xcode's DeviceHub never learns of
//! it and a sleeping Apple TV stays asleep.
//!
//! Signing is this computer's own, headless: a certificate and profiles imported once into the
//! `signing` folder of the device store (`apparition deploy-signing import`). With one that covers the
//! device, it signs the app and installs it. Without, the IPA goes as it is, which works only
//! for one already signed for that device; the error then says to import a certificate.

use apparition_core::api::{self, DeviceStore, Platform};
use crate::devices::{App, Device, Devices, Fut, Installed};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct CoreDevices {
    store: DeviceStore,
    /// One install at a time: each signs into the same folder and holds a tunnel.
    one_at_a_time: tokio::sync::Mutex<()>,
}

impl CoreDevices {
    pub fn new() -> CoreDevices {
        CoreDevices { store: DeviceStore::default(), one_at_a_time: tokio::sync::Mutex::new(()) }
    }

    fn device(&self, udid: &str) -> Result<api::PairedDevice, String> {
        self.store.device(udid).ok_or_else(|| format!("this computer is no longer paired with {udid}"))
    }

    pub fn signing_dir(&self) -> PathBuf {
        signing_dir(&self.store)
    }
}

/// Where this computer keeps the certificate and profiles it signs sent builds with.
pub fn signing_dir(store: &DeviceStore) -> PathBuf {
    store.dir.join("signing")
}

/// The first imported certificate that is not expired and has a profile for this app on this
/// device (`id`, `name`), by asking the core's own planner, which is what signing will use.
fn usable_identity(dir: &Path, ipa: &Path, udid: &str, platform: Platform) -> Option<(String, String)> {
    let inventory = api::signing_inventory(dir, Some(udid));
    pick(&inventory, |id| api::offline_install_plan(dir, ipa.to_path_buf(), id, udid, platform).is_ok())
}

fn pick(inventory: &Value, plan_exists: impl Fn(&str) -> bool) -> Option<(String, String)> {
    inventory["identities"].as_array()?.iter()
        .filter(|i| i["expired"].as_bool() != Some(true))
        .filter_map(|i| Some((i["id"].as_str()?.to_string(), i["name"].as_str().unwrap_or("certificate").to_string())))
        .find(|(id, _)| plan_exists(id))
}

/// The core's calls run on a thread of their own, driven with `block_on`: their futures aren't
/// provably `Send`, which a task on the shared runtime needs, and they are deep. The tunnel's
/// setup overflowed the 2 MiB stack of a tokio blocking thread in a debug build (the first run
/// against a Vision Pro, 2026-11-04), so the thread gets a stack sized like the main thread's.
async fn off_thread<T: Send + 'static>(job: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("store-device".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || { let _ = send.send(job()); })
        .map_err(|e| format!("couldn't start a thread for the device: {e}"))?;
    receive.await.map_err(|_| "the device call stopped without an answer".to_string())?
}

/// On `off_thread`'s own thread, which no runtime drives: a small one of its own.
fn block_on<F: std::future::Future<Output = Result<T, String>>, T>(future: F) -> Result<T, String> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
        .map_err(|e| format!("couldn't start a runtime for the device: {e}"))?
        .block_on(future)
}

impl Devices for CoreDevices {
    fn list(&self) -> Fut<'_, Vec<Device>> {
        Box::pin(async move {
            Ok(self.store.devices().into_iter().map(|d| Device {
                platform: d.platform().os_name().to_string(),
                model: d.product_type.clone().unwrap_or_default(),
                udid: d.udid, name: d.name,
            }).collect())
        })
    }

    fn apps(&self, udid: String) -> Fut<'_, Vec<App>> {
        Box::pin(async move {
            let device = self.device(&udid)?;
            let store = self.store.clone();
            off_thread(move || block_on(async {
                let mut tunnel = store.connect_verified(&device).await.map_err(|e| format!("{}: {e}", device.name))?;
                let apps = tunnel.installed_apps(false).await.map_err(|e| format!("{}: {e}", device.name))?;
                Ok(apps.into_iter().map(|a| App { bundle_id: a.bundle_id, name: a.name, version: a.version, build: a.build }).collect())
            })).await
        })
    }

    fn install(&self, udid: String, ipa: PathBuf) -> Fut<'_, Installed> {
        Box::pin(async move {
            let _turn = self.one_at_a_time.lock().await;
            let device = self.device(&udid)?;
            let (store, signing) = (self.store.clone(), self.signing_dir());
            off_thread(move || block_on(install_on(store, signing, device, ipa, true))).await
        })
    }

    fn install_signed(&self, udid: String, ipa: PathBuf) -> Fut<'_, Installed> {
        Box::pin(async move {
            let _turn = self.one_at_a_time.lock().await;
            let device = self.device(&udid)?;
            let (store, signing) = (self.store.clone(), self.signing_dir());
            off_thread(move || block_on(install_on(store, signing, device, ipa, false))).await
        })
    }
}

/// `resign`: sign it here first when an imported certificate covers the device; otherwise it
/// goes as it is.
async fn install_on(store: DeviceStore, signing: PathBuf, device: api::PairedDevice, ipa: PathBuf, resign: bool) -> Result<Installed, String> {
    let platform = device.platform();
    let bundle_id = apparition_core::install_plan::inspect(&ipa).map_err(|e| format!("the build isn't an app: {e}"))?.id;
    let choice = if resign { usable_identity(&signing, &ipa, &device.udid, platform) } else { None };
    let mut tunnel = store.connect_verified(&device).await.map_err(|e| format!("{} didn't answer: {e}", device.name))?;
    // An update of an app that is open waits in installd, so only an update stops waiting after 20 s.
    let existing = tunnel.installed_app(&bundle_id).await.map_err(|e| e.to_string())?.is_some();
    let detach = if existing { 20_000 } else { 0 };
    let started = std::time::Instant::now();
    let (reply, signed_with) = match choice {
        Some((id, name)) => {
            let signed = api::sign_offline(&signing, ipa.clone(), &id, None, &device.udid, true, None, platform)
                .map_err(|e| format!("signing with {name} failed: {e}"))?;
            let path = PathBuf::from(signed["signedPath"].as_str().ok_or("signing returned no app")?);
            let reply = tunnel.install(&path, Some(&ipa), detach).await.map_err(|e| e.to_string())?;
            (reply, Some(name))
        }
        None => {
            let reply = tunnel.install_signed_ipa(&ipa, Some(&bundle_id), detach).await.map_err(|e| format!(
                "{e}. No certificate imported on this computer covers {} for this app, so it was sent as it is; import one with `apparition deploy-signing import`", device.name))?;
            (reply, None)
        }
    };
    Ok(Installed {
        bundle_id, device: device.name,
        seconds: reply["elapsedMs"].as_u64().map(|ms| ms as f64 / 1000.0).unwrap_or_else(|| started.elapsed().as_secs_f64()),
        queued: reply["queued"].as_bool() == Some(true),
        signed_with,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_first_unexpired_certificate_with_a_plan_signs() {
        let inventory = json!({ "identities": [
            { "id": "old", "name": "Expired", "expired": true },
            { "id": "other", "name": "No profile", "expired": false },
            { "id": "mine", "name": "Apple Development: Someone", "expired": false },
        ] });
        let asked = std::cell::RefCell::new(Vec::new());
        let chosen = pick(&inventory, |id| { asked.borrow_mut().push(id.to_string()); id == "mine" });
        assert_eq!(chosen, Some(("mine".into(), "Apple Development: Someone".into())));
        assert_eq!(*asked.borrow(), ["other", "mine"], "an expired certificate is never tried");
        assert_eq!(pick(&inventory, |_| false), None);
        assert_eq!(pick(&json!({}), |_| true), None);
    }
}
