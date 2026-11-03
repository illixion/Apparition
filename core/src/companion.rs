//! The companion: what a computer runs for Apparition on a device. The relay (`relay.rs`), and
//! an anisette server the device can sign in to Apple with:
//!
//! - built in (the `anisette` feature): Apple's libraries in this process, served on the LAN
//!   on port 6969;
//! - or the self-hosted Docker image (selfhost/anisette/compose.yaml), started with the user's
//!   Docker, which the companion offers once it answers.
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

/// The compose file for the self-hosted anisette server.
pub const COMPOSE: &str = include_str!("../../selfhost/anisette/compose.yaml");

/// Where the anisette server the companion offers comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AnisetteSource {
    Builtin,
    /// Another server on this machine's port 6969, normally the Docker one.
    Local,
}

pub struct Companion {
    pub relay: Relay,
    /// Devices paired to use the anisette server, and the pairings waiting for the person.
    pub devices: Arc<PairedDevices>,
    /// Whether the built-in anisette serves anyone on the network (the compatibility mode)
    /// rather than paired devices only.
    anisette_open: AtomicBool,
    pairing_port: u16,
    channel_port: u16,
    _pairing_task: Option<AbortOnDrop>,
    _channel_task: Option<AbortOnDrop>,
    channel_enabled: Arc<std::sync::atomic::AtomicBool>,
    paired_path: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(all(feature = "anisette", unix))]
    builtin: tokio::sync::Mutex<Option<crate::anisette::Server>>,
    source: Mutex<Option<AnisetteSource>>,
    data_dir: PathBuf,
    anisette_port: u16,
}

impl Companion {
    /// Starts the relay on `port`. `data_dir` holds the built-in anisette's libraries, and
    /// `anisette_port` is where an anisette server is served or looked for (normally
    /// [`ANISETTE_PORT`]).
    pub async fn start(port: u16, anisette_port: u16, advertise: bool, data_dir: &Path, log: Log) -> Result<Companion, Report> {
        let relay = Relay::start(port, advertise, log.clone()).await?;
        let devices = Arc::new(PairedDevices::load(data_dir));
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
        let channel_task = if channel_port == 0 {
            None
        } else {
            match crate::companion_channel::start(
                std::net::SocketAddr::from(([0, 0, 0, 0], channel_port)),
                std::net::SocketAddr::from(([127, 0, 0, 1], anisette_port)),
                devices.clone(), channel_enabled.clone(), paired_path.clone(),
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
            anisette_open: AtomicBool::new(false),
            pairing_port,
            channel_port: if channel_task.is_some() { channel_port } else { 0 },
            _pairing_task: pairing_task,
            _channel_task: channel_task,
            channel_enabled,
            paired_path,
            #[cfg(all(feature = "anisette", unix))]
            builtin: Default::default(),
            source: Mutex::new(None),
            data_dir: data_dir.to_owned(),
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

    pub fn anisette_open(&self) -> bool {
        self.anisette_open.load(Ordering::Relaxed)
    }

    /// Serves the built-in anisette to anyone on the network (`true`, the compatibility mode
    /// every v3 client understands) or to paired devices only. A running server restarts in the
    /// new mode.
    pub async fn set_anisette_open(&self, open: bool) -> Result<(), Report> {
        if self.anisette_open.swap(open, Ordering::Relaxed) == open {
            return Ok(());
        }
        if self.anisette() == Some(AnisetteSource::Builtin) {
            self.stop_builtin_anisette().await;
            self.share_builtin_anisette().await?;
        }
        Ok(())
    }

    /// Advertises pairing whenever a provider is available; only the built-in HTTP route needs
    /// the token path after the encrypted channel has authenticated the device.
    fn update_pairing(&self) {
        let can_pair = self.anisette().is_some() && self.pairing_port != 0;
        let paired_http_path = self.anisette() == Some(AnisetteSource::Builtin) && !self.anisette_open();
        self.devices.accepting.store(can_pair, Ordering::Relaxed);
        self.channel_enabled.store(self.anisette().is_some(), Ordering::Release);
        self.paired_path.store(paired_http_path, Ordering::Release);
        self.relay.set_pairing(can_pair.then_some(self.pairing_port));
        self.relay.set_channel((self.channel_port != 0 && self.anisette().is_some()).then_some(self.channel_port));
    }

    /// Serves the built-in anisette on the LAN and offers it.
    pub async fn share_builtin_anisette(&self) -> Result<(), Report> {
        #[cfg(all(feature = "anisette", unix))]
        {
            let mut builtin = self.builtin.lock().await;
            if builtin.is_none() {
                let port = self.anisette_port;
                let bind = std::net::SocketAddr::from(([0, 0, 0, 0], port));
                let access: crate::anisette::Access = if self.anisette_open() {
                    None
                } else {
                    let devices = self.devices.clone();
                    Some(Arc::new(move |token: &str| devices.accepts(token)))
                };
                let server = crate::anisette::start(&self.data_dir, bind, access).await.context(format!(
                    "Couldn't serve anisette on port {port}. If another program uses that port (the Docker server, say), stop it or offer that one instead."
                ))?;
                *builtin = Some(server);
            }
            *self.source.lock().unwrap() = Some(AnisetteSource::Builtin);
            self.relay.set_anisette(Some(self.anisette_port));
            self.update_pairing();
            Ok(())
        }
        #[cfg(not(all(feature = "anisette", unix)))]
        {
            let _ = &self.data_dir;
            bail!("This build has no built-in anisette; run the Docker server instead")
        }
    }

    /// Stops serving the built-in anisette.
    pub async fn stop_builtin_anisette(&self) {
        #[cfg(all(feature = "anisette", unix))]
        {
            self.builtin.lock().await.take();
        }
        if self.anisette() == Some(AnisetteSource::Builtin) {
            *self.source.lock().unwrap() = None;
            self.relay.set_anisette(None);
        }
        self.update_pairing();
    }

    /// When not serving the built-in one, offers a server on this machine's port 6969 if one
    /// answers (and stops offering it when it doesn't). Call every few seconds.
    pub async fn refresh(&self) {
        if self.anisette() == Some(AnisetteSource::Builtin) {
            return;
        }
        let answers = crate::relay::anisette_answers(self.anisette_port).await;
        let source = answers.then_some(AnisetteSource::Local);
        *self.source.lock().unwrap() = source;
        self.relay.set_anisette(answers.then_some(self.anisette_port));
        self.update_pairing();
    }
}

/// Runs `docker compose up -d` (or `down`) with the bundled compose file.
pub async fn docker_anisette(up: bool) -> Result<(), Report> {
    let file = std::env::temp_dir().join("apparition-anisette-compose.yaml");
    std::fs::write(&file, COMPOSE).context(format!("Can't write {}", file.display()))?;
    let mut command = tokio::process::Command::new("docker");
    command.arg("compose").arg("-f").arg(&file);
    if up {
        command.args(["up", "-d"]);
    } else {
        command.arg("down");
    }
    let output = command
        .output()
        .await
        .context("Can't run docker (Docker Desktop, OrbStack, Colima, or Docker Engine with the compose plugin)")?;
    if !output.status.success() {
        bail!("docker compose failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
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
    /// Told when a pairing starts waiting: the desktop app brings its window up, the command
    /// line asks in the terminal.
    notify: Mutex<Option<Arc<dyn Fn(u64, &pairing::Request) + Send + Sync>>>,
}

impl PairedDevices {
    /// The store alone, to list or unpair devices without running a companion.
    pub fn open(data_dir: &Path) -> PairedDevices {
        PairedDevices::load(data_dir)
    }

    fn load(data_dir: &Path) -> PairedDevices {
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

    #[cfg(all(feature = "anisette", unix))]
    fn accepts(&self, token: &str) -> bool {
        !token.is_empty() && self.file.lock().unwrap().devices.iter().any(|device| device.token == token)
    }

    pub(crate) fn channel_token(&self, id: &str) -> Option<String> {
        self.file.lock().unwrap().devices.iter().find(|device| device.id == id).map(|device| device.token.clone())
    }

    pub(crate) fn channel_allows_anisette(&self, id: &str) -> bool {
        self.file.lock().unwrap().devices.iter().find(|device| device.id == id)
            .is_some_and(|device| device.grants.iter().any(|grant| grant == "anisette"))
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
    fn add(&self, request: &pairing::Request) -> String {
        let token = pairing::new_token();
        {
            let mut file = self.file.lock().unwrap();
            file.devices.retain(|device| device.id != request.device_id);
            file.devices.push(PairedDevice {
                id: request.device_id.clone(),
                name: request.device_name.clone(),
                token: token.clone(),
                paired: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
                grants: vec!["anisette".into()],
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
            // One at a time: a second device asking meanwhile is declined, so nobody on the
            // network can pile up prompts.
            if !devices.accepting.load(Ordering::Relaxed) || !devices.waiting.lock().unwrap().is_empty() {
                return;
            }
            let host_id = devices.host_id();
            let host_name = crate::relay::machine_name();
            let addresses = crate::relay::primary_address().map(|ip| vec![ip.to_string()]).unwrap_or_default();
            let peer_text = peer.ip().to_string();
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
                vec!["anisette".into()],
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
