//! The companion: what a computer runs for Apparition on a device. The relay (`relay.rs`), and,
//! on a Mac that gives its own anisette, that anisette for the device to sign in to Apple with,
//! served on loopback and reached only through the encrypted channel by paired devices and
//! computers (companion_channel.rs). Any computer can instead offer a third-party anisette server
//! the person gave the address of, passed through the same way (`anisette::remote`).
//!
//! The command line (`apparition relay`) and the desktop app both run this.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::pairing;

#[allow(unused_imports)]
use crate::relay::{ANISETTE_PORT, Log, Relay};

/// Where the anisette server the companion offers comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AnisetteSource {
    /// This Mac's own (anisette::native).
    Builtin,
    /// This Mac's anisette VM (vm.rs), for a Mac on macOS 27: the VM's own, served the same way from here.
    Vm,
    /// A third-party anisette server at an address the person gave, passed through the same way, on any computer.
    Server,
    /// Apparition's own Android anisette (anisette::android), run in this process and served the same way: for a
    /// Mac that can't give its own, such as an 8 GB one that has no room for the anisette VM, or a Linux computer.
    Android,
}

pub struct Companion {
    pub relay: Relay,
    /// Devices paired to use the anisette server, and the pairings waiting for the person.
    pub devices: Arc<PairedDevices>,
    pairing_port: u16,
    channel_port: u16,
    _pairing_task: Option<AbortOnDrop>,
    _channel_task: Option<AbortOnDrop>,
    channel_enabled: Arc<std::sync::atomic::AtomicBool>,
    paired_path: Arc<std::sync::atomic::AtomicBool>,
    /// This Mac's Apparition Store, for computers granted `developer`.
    #[cfg(feature = "desktop")]
    developer: crate::companion_channel::DeveloperTarget,
    /// The server on loopback that the channel reaches, and the third-party address it passes through, if it does.
    served: tokio::sync::Mutex<Option<(crate::anisette::Server, Option<String>)>>,
    source: Mutex<Option<AnisetteSource>>,
    anisette_port: u16,
}

impl Companion {
    /// Starts the relay on `port`. `data_dir` holds the paired devices, and `anisette_port` is
    /// the loopback port this Mac's anisette is served on (normally [`ANISETTE_PORT`]).
    pub async fn start(port: u16, anisette_port: u16, advertise: bool, data_dir: &Path, log: Log) -> Result<Companion, Report> {
        let relay = Relay::start(port, advertise, log.clone()).await?;
        let devices = Arc::new(PairedDevices::load(data_dir));
        relay.set_id(&devices.host_id());
        // Pairing and the encrypted capability channel use relay +1 and +2. A single firewall
        // rule covering the companion executable can admit all three listeners.
        let pairing_port = if port == 0 { 0 } else { relay.port().wrapping_add(1) };
        let channel_port = if port == 0 { 0 } else { relay.port().wrapping_add(2) };
        let (pairing_port, pairing_task) = match TcpListener::bind(("0.0.0.0", pairing_port)).await {
            Ok(listener) => {
                let bound = listener.local_addr()?.port();
                let task = tokio::spawn(accept_pairings(listener, devices.clone(), log.clone()));
                (bound, Some(AbortOnDrop(task)))
            }
            Err(error) => {
                log(&format!("can't listen for device pairing on port {pairing_port}: {error}"));
                (0, None)
            }
        };
        let channel_enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paired_path = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(feature = "desktop")]
        let developer: crate::companion_channel::DeveloperTarget = Default::default();
        #[cfg(feature = "desktop")]
        let channel_task = if channel_port == 0 {
            None
        } else {
            match crate::companion_channel::start(
                std::net::SocketAddr::from(([0, 0, 0, 0], channel_port)),
                std::net::SocketAddr::from(([127, 0, 0, 1], anisette_port)),
                devices.clone(), channel_enabled.clone(), paired_path.clone(), developer.clone(),
            ).await {
                Ok((_, task)) => Some(AbortOnDrop(task)),
                Err(error) => {
                    log(&format!("can't listen for secure companion channels on port {channel_port}: {error}"));
                    None
                }
            }
        };
        #[cfg(not(feature = "desktop"))]
        let channel_task: Option<AbortOnDrop> = None;
        Ok(Companion {
            relay,
            devices,
            pairing_port,
            channel_port: if channel_task.is_some() { channel_port } else { 0 },
            _pairing_task: pairing_task,
            _channel_task: channel_task,
            channel_enabled,
            paired_path,
            #[cfg(feature = "desktop")]
            developer,
            served: Default::default(),
            source: Mutex::new(None),
            anisette_port,
        })
    }

    /// What the companion offers now.
    pub fn anisette(&self) -> Option<AnisetteSource> {
        *self.source.lock().unwrap()
    }

    pub fn anisette_port(&self) -> u16 {
        self.anisette_port
    }

    pub fn pairing_port(&self) -> u16 {
        self.pairing_port
    }

    pub fn channel_port(&self) -> u16 {
        self.channel_port
    }

    /// Whether paired computers granted `developer` reach this Mac's Apparition Store now.
    pub fn developer(&self) -> bool {
        #[cfg(feature = "desktop")]
        return self.developer.lock().unwrap().is_some();
        #[cfg(not(feature = "desktop"))]
        return false;
    }

    /// Offers this Mac's Apparition Store (on loopback, with the token it accepts from the app)
    /// to paired computers granted `developer`, or stops (`None`). Pairing is offered while it
    /// is, even on a Mac that doesn't offer its anisette.
    #[cfg(feature = "desktop")]
    pub fn set_developer(&self, route: Option<crate::companion_channel::DeveloperRoute>) {
        *self.developer.lock().unwrap() = route;
        self.update_pairing();
    }

    /// What a device can be granted now.
    fn offered(&self) -> Vec<String> {
        let mut offered = Vec::new();
        if self.anisette().is_some() {
            offered.extend(["anisette".to_string(), "appleRelay".to_string()]);
        }
        if self.developer() {
            offered.push("developer".to_string());
        }
        offered
    }

    /// Advertises pairing whenever something is offered; only the built-in HTTP route needs
    /// the token path after the encrypted channel has authenticated the device.
    fn update_pairing(&self) {
        let offered = self.offered();
        let can_pair = !offered.is_empty() && self.pairing_port != 0;
        let paired_http_path = self.anisette().is_some();
        self.devices.accepting.store(can_pair, Ordering::Relaxed);
        *self.devices.offered.lock().unwrap() = offered.clone();
        self.channel_enabled.store(self.anisette().is_some(), Ordering::Release);
        self.paired_path.store(paired_http_path, Ordering::Release);
        self.relay.set_developer(self.developer());
        self.relay.set_pairing(can_pair.then_some(self.pairing_port));
        self.relay.set_channel((self.channel_port != 0 && !offered.is_empty()).then_some(self.channel_port));
    }

    /// Serves the built-in anisette on loopback and offers it through the encrypted channel.
    pub async fn share_builtin_anisette(&self) -> Result<(), Report> {
        #[cfg(all(feature = "anisette", target_os = "macos"))]
        {
            self.share(AnisetteSource::Builtin, Offer::Native).await
        }
        #[cfg(not(all(feature = "anisette", target_os = "macos")))]
        {
            bail!("Only a Mac on macOS 26 or earlier, or a macOS VM prepared for it, offers its own anisette to devices. Pair your device with that Mac's Apparition instead, or offer a third-party anisette server.")
        }
    }

    /// Serves the anisette of this Mac's anisette VM in `bundle` (vm.rs) on loopback, and offers it through the
    /// encrypted channel to paired devices, as [`share_builtin_anisette`](Self::share_builtin_anisette) does this
    /// Mac's own. The VM resumes on the first request and suspends itself when idle.
    #[cfg(all(feature = "anisette", target_os = "macos"))]
    pub async fn share_vm_anisette(&self, bundle: PathBuf) -> Result<(), Report> {
        self.share(AnisetteSource::Vm, Offer::Vm(bundle)).await
    }

    /// Serves Apparition's own Android anisette (libraries fetched into `data_dir` on first use) and offers it
    /// through the encrypted channel to paired devices. Fails where it isn't available (a build without it, or switched off by
    /// [`crate::anisette::ANDROID_LIBRARIES`]).
    pub async fn share_android_anisette(&self, data_dir: PathBuf) -> Result<(), Report> {
        #[cfg(feature = "anisette")]
        {
            if !crate::android_anisette_available() {
                bail!("Apparition's own Android anisette is switched off in this build.");
            }
            self.share(AnisetteSource::Android, Offer::Android(data_dir)).await
        }
        #[cfg(not(feature = "anisette"))]
        {
            let _ = data_dir;
            bail!("This build has no Android anisette. Pair with a Mac, run an anisette server in Docker, or give a third-party server's address.")
        }
    }

    /// Passes the third-party anisette server at `address` (see [`crate::anisette::normalize_server_address`])
    /// through to paired devices on the encrypted channel, from any computer. The server isn't asked for anything
    /// until a device uses it; [`crate::check_anisette`] says whether it answers.
    pub async fn share_server_anisette(&self, address: &str) -> Result<(), Report> {
        let address = crate::anisette::normalize_server_address(address).map_err(|why| report!("{why}"))?;
        self.share(AnisetteSource::Server, Offer::Server(address)).await
    }

    async fn share(&self, source: AnisetteSource, offer: Offer) -> Result<(), Report> {
        let mut served = self.served.lock().await;
        let address = match &offer {
            Offer::Server(address) => Some(address.clone()),
            #[cfg(feature = "anisette")]
            _ => None,
        };
        // Serving already the same thing: nothing to restart.
        let unchanged = self.anisette() == Some(source) && served.as_ref().is_some_and(|(_, serving)| *serving == address);
        if !unchanged {
            // One port: a server for another source, or another address, makes way first.
            served.take();
            let port = self.anisette_port;
            let bind = std::net::SocketAddr::from(([127, 0, 0, 1], port));
            let devices = self.devices.clone();
            let access: crate::anisette::Access = Some(Arc::new(move |token: &str| devices.accepts(token)));
            let started = match offer {
                #[cfg(all(feature = "anisette", target_os = "macos"))]
                Offer::Native => crate::anisette::start(bind, access).await,
                #[cfg(all(feature = "anisette", target_os = "macos"))]
                Offer::Vm(bundle) => crate::anisette::start_vm(bind, access, bundle).await,
                #[cfg(feature = "anisette")]
                Offer::Android(data_dir) => crate::anisette::start_android(&data_dir, bind, access).await,
                Offer::Server(address) => crate::anisette::start_remote(bind, access, &address).await,
            };
            let started = started.context(format!("Couldn't serve anisette on port {port}. If another program uses that port, stop it."))?;
            *served = Some((started, address));
        }
        *self.source.lock().unwrap() = Some(source);
        self.relay.set_anisette(Some(self.anisette_port));
        self.update_pairing();
        Ok(())
    }

    /// Stops serving anisette, whichever it was.
    pub async fn stop_builtin_anisette(&self) {
        self.served.lock().await.take();
        if self.anisette().is_some() {
            *self.source.lock().unwrap() = None;
            self.relay.set_anisette(None);
        }
        self.update_pairing();
    }

}

/// What [`Companion::share`] serves.
enum Offer {
    #[cfg(all(feature = "anisette", target_os = "macos"))]
    Native,
    #[cfg(all(feature = "anisette", target_os = "macos"))]
    Vm(PathBuf),
    #[cfg(feature = "anisette")]
    Android(PathBuf),
    /// The third-party server at this (normalised) address.
    Server(String),
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A device paired to use this companion's anisette server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    pub id: String,
    pub name: String,
    /// Never shown or sent anywhere but in the sealed grant.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    token: String,
    /// Seconds since 1970.
    pub paired: u64,
    /// Capabilities granted by the person. Old files represented anisette-only pairings.
    #[serde(default = "legacy_grants")]
    pub grants: Vec<String>,
}

fn legacy_grants() -> Vec<String> { vec!["anisette".to_string()] }

#[derive(Debug, Default, Serialize, Deserialize)]
struct DevicesFile {
    #[serde(default)]
    host_id: String,
    #[serde(default)]
    devices: Vec<PairedDevice>,
}

/// A pairing waiting for the person to compare the codes.
struct Waiting {
    request: pairing::Request,
    answer: oneshot::Sender<bool>,
}

/// The devices paired with this companion (`paired-devices.json` beside the anisette data),
/// and the pairing waiting for an answer, if any.
pub struct PairedDevices {
    path: PathBuf,
    file: Mutex<DevicesFile>,
    waiting: Mutex<HashMap<u64, Waiting>>,
    next: AtomicU64,
    /// Pairing is offered (the built-in anisette serves paired devices only).
    accepting: AtomicBool,
    /// What a pairing can grant now (`Companion::offered`).
    offered: Mutex<Vec<String>>,
    /// Told when a pairing starts waiting: the desktop app brings its window up, the command
    /// line asks in the terminal.
    notify: Mutex<Option<Arc<dyn Fn(u64, &pairing::Request) + Send + Sync>>>,
}

impl PairedDevices {
    /// The store alone, to list or unpair devices without running a companion.
    pub fn open(data_dir: &Path) -> PairedDevices {
        PairedDevices::load(data_dir)
    }

    pub(crate) fn load(data_dir: &Path) -> PairedDevices {
        let path = data_dir.join("paired-devices.json");
        let mut file: DevicesFile = std::fs::read(&path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
        if file.host_id.is_empty() {
            file.host_id = pairing::new_token();
        }
        let devices = PairedDevices {
            path,
            file: Mutex::new(file),
            waiting: Default::default(),
            next: AtomicU64::new(1),
            accepting: AtomicBool::new(false),
            offered: Mutex::new(Vec::new()),
            notify: Mutex::new(None),
        };
        devices.save();
        devices
    }

    fn save(&self) {
        let file = self.file.lock().unwrap();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let Ok(data) = serde_json::to_vec_pretty(&*file) else { return };
        let temporary = self.path.with_extension("json.new");
        if std::fs::write(&temporary, data).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::rename(&temporary, &self.path);
        }
    }

    pub fn host_id(&self) -> String {
        self.file.lock().unwrap().host_id.clone()
    }

    fn accepts(&self, token: &str) -> bool {
        !token.is_empty() && self.file.lock().unwrap().devices.iter().any(|device| device.token == token)
    }

    pub(crate) fn channel_token(&self, id: &str) -> Option<String> {
        self.file.lock().unwrap().devices.iter().find(|device| device.id == id).map(|device| device.token.clone())
    }

    pub(crate) fn channel_allows_anisette(&self, id: &str) -> bool {
        self.channel_allows(id, "anisette")
    }

    pub(crate) fn channel_allows(&self, id: &str, capability: &str) -> bool {
        self.file.lock().unwrap().devices.iter().find(|device| device.id == id)
            .is_some_and(|device| device.grants.iter().any(|grant| grant == capability))
    }

    /// The paired devices, without their tokens.
    pub fn list(&self) -> Vec<PairedDevice> {
        self.file.lock().unwrap().devices.iter().map(|device| PairedDevice { token: String::new(), ..device.clone() }).collect()
    }

    /// Unpairs a device: its token stops working at once.
    pub fn forget(&self, id: &str) -> bool {
        let removed = {
            let mut file = self.file.lock().unwrap();
            let before = file.devices.len();
            file.devices.retain(|device| device.id != id);
            file.devices.len() != before
        };
        if removed {
            self.save();
        }
        removed
    }

    /// The pairings waiting for an answer, by id.
    pub fn waiting(&self) -> Vec<(u64, pairing::Request)> {
        self.waiting.lock().unwrap().iter().map(|(id, waiting)| (*id, waiting.request.clone())).collect()
    }

    /// The person's answer to a waiting pairing.
    pub fn decide(&self, id: u64, allow: bool) -> bool {
        match self.waiting.lock().unwrap().remove(&id) {
            Some(waiting) => waiting.answer.send(allow).is_ok(),
            None => false,
        }
    }

    pub fn on_pairing(&self, notify: Arc<dyn Fn(u64, &pairing::Request) + Send + Sync>) {
        *self.notify.lock().unwrap() = Some(notify);
    }

    /// Mints the device's token (replacing an earlier pairing of the same device) and saves it.
    pub(crate) fn add(&self, request: &pairing::Request) -> String {
        let token = pairing::new_token();
        {
            let mut file = self.file.lock().unwrap();
            file.devices.retain(|device| device.id != request.device_id);
            file.devices.push(PairedDevice {
                id: request.device_id.clone(),
                name: request.device_name.clone(),
                token: token.clone(),
                paired: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                grants: request.grants.clone(),
            });
        }
        self.save();
        token
    }
}

/// How long a pairing waits for the person.
const PAIRING_WAIT: Duration = Duration::from_secs(120);

async fn accept_pairings(listener: TcpListener, devices: Arc<PairedDevices>, log: Log) {
    loop {
        let Ok((mut stream, peer)) = listener.accept().await else { continue };
        let devices = devices.clone();
        let log = log.clone();
        tokio::spawn(async move {
            let _busy = crate::stay_awake::busy();
            // One at a time: a second device asking meanwhile is declined, so nobody on the
            // network can pile up prompts.
            if !devices.accepting.load(Ordering::Relaxed) || !devices.waiting.lock().unwrap().is_empty() {
                return;
            }
            let host_id = devices.host_id();
            let host_name = crate::relay::machine_name();
            // The LAN address, then the tailnet's, so a paired computer still reaches this Mac
            // from another network.
            let mut addresses = crate::relay::primary_address().map(|ip| vec![ip.to_string()]).unwrap_or_default();
            for ip in crate::relay::tailscale_addresses() {
                if !addresses.contains(&ip.to_string()) {
                    addresses.push(ip.to_string());
                }
            }
            let peer_text = peer.ip().to_string();
            let offered = devices.offered.lock().unwrap().clone();
            let decide = |request: pairing::Request| {
                let devices = devices.clone();
                async move {
                    let (answer, decision) = oneshot::channel();
                    let id = devices.next.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut waiting = devices.waiting.lock().unwrap();
                        if !waiting.is_empty() {
                            return None;
                        }
                        waiting.insert(id, Waiting { request: request.clone(), answer });
                    }
                    if let Some(notify) = devices.notify.lock().unwrap().clone() {
                        notify(id, &request);
                    }
                    let allowed = matches!(tokio::time::timeout(PAIRING_WAIT, decision).await, Ok(Ok(true)));
                    devices.waiting.lock().unwrap().remove(&id);
                    allowed.then(|| devices.add(&request))
                }
            };
            let exchange = pairing::serve(
                pairing::Namespace::APPARITION,
                &mut stream,
                peer_text.clone(),
                &host_id,
                &host_name,
                addresses,
                offered,
                decide,
            );
            match tokio::time::timeout(PAIRING_WAIT + Duration::from_secs(30), exchange).await {
                Ok(Ok(true)) => log(&format!("paired a device at {peer_text}")),
                Ok(Ok(false)) => log(&format!("declined pairing from {peer_text}")),
                Ok(Err(error)) => log(&format!("pairing from {peer_text} failed: {error}")),
                Err(_) => log(&format!("pairing from {peer_text} timed out")),
            }
        });
    }
}

#[cfg(all(test, feature = "anisette"))]
mod android_offer_tests {
    use super::*;

    /// The companion offers Apparition's own Android anisette to a paired device: its loopback server answers a
    /// device's token and refuses another. Fetches Apple Music's libraries: `APPARITION_NET=1`.
    #[tokio::test]
    async fn offers_the_android_anisette_to_a_paired_device_only() {
        if std::env::var_os("APPARITION_NET").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let file = serde_json::json!({
            "host_id": "host",
            "devices": [{ "id": "device", "name": "Test", "token": "good-token", "paired": 0, "grants": ["anisette"] }],
        });
        std::fs::write(dir.path().join("paired-devices.json"), file.to_string()).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let log: Log = Arc::new(|_: &str| {});
        let companion = Companion::start(0, port, false, dir.path(), log).await.unwrap();
        assert_eq!(companion.anisette(), None);
        companion.share_android_anisette(dir.path().to_path_buf()).await.unwrap();
        assert_eq!(companion.anisette(), Some(AnisetteSource::Android));
        let client = reqwest::Client::new();
        let ok: serde_json::Value = client.get(format!("http://127.0.0.1:{port}/t/good-token/v3/client_info")).send().await.unwrap().json().await.unwrap();
        assert_eq!(ok["apparition_anisette"], "android", "{ok}");
        let refused = client.get(format!("http://127.0.0.1:{port}/t/bad-token/v3/client_info")).send().await.unwrap();
        assert!(!refused.status().is_success(), "{}", refused.status());
        companion.stop_builtin_anisette().await;
        assert_eq!(companion.anisette(), None);
    }
}
