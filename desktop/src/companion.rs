//! The companion side of the app: the relay and the anisette server Apparition on a device
//! uses (`apparition_core::companion`), its settings, and the tray (menu-bar) icon that keeps
//! it running with the window closed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apparition_core::api;
use apparition_core::companion::{AnisetteSource, Companion};
use apparition_core::relay::{self, ANISETTE_PORT, RELAY_PORT};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, Wry};
use tauri_plugin_autostart::ManagerExt;

use crate::flow::Failure;

/// The command-line argument the login item starts the app with: straight to the tray.
pub const HIDDEN: &str = "--hidden";

/// The relay's and the anisette server's ports. A debug build can move them
/// (APPARITION_COMPANION_PORTS=relay,anisette), to run beside another companion.
fn ports() -> (u16, u16) {
    #[cfg(debug_assertions)]
    if let Some((relay, anisette)) = std::env::var("APPARITION_COMPANION_PORTS").ok().and_then(|value| {
        let (relay, anisette) = value.split_once(',')?;
        Some((relay.trim().parse().ok()?, anisette.trim().parse().ok()?))
    }) {
        return (relay, anisette);
    }
    (RELAY_PORT, ANISETTE_PORT)
}

/// What anisette server the companion offers devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum AnisetteMode {
    /// None.
    #[default]
    Off,
    /// This Mac's own, to paired devices and computers over the encrypted channel.
    Builtin,
    /// This Mac's anisette VM (the Companion tab sets it up), the same way: for a Mac on macOS 27.
    Vm,
    /// Apparition's own Android anisette, run in this app and passed on the same way: for a Mac with no room for
    /// the VM (8 GB), or on macOS 27 without one, and for a Windows or Linux computer.
    Android,
    /// A third-party anisette server at the address in `Settings::anisette_server`, passed through the same way.
    /// The one mode every computer has, so a Windows or Linux PC (or a Mac with no anisette of its own) can offer it.
    Server,
    /// The Docker server this app used to offer: read as `Off`, since only a Mac's own anisette
    /// is offered now.
    #[serde(other)]
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    relay: bool,
    anisette: AnisetteMode,
    /// The third-party anisette server's address (`AnisetteMode::Server`), kept while another mode is chosen so
    /// switching back needs no retyping.
    anisette_server: Option<String>,
    auto_updates: bool,
    /// Announce this computer over mDNS, so Find Companion lists it. Off, devices type its
    /// address; ones that already have it keep connecting as before.
    advertise: bool,
    /// Record what a sign-in sends to Apple and what comes back, as structure and sizes, in
    /// `apparition-verbose.log` (apparition_core::verbose). For troubleshooting; off unless chosen.
    verbose: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // The relay is what Apparition on a device needs to do anything, so it's on unless
        // turned off; serving anisette to the network is a choice the user makes.
        Self { relay: true, anisette: AnisetteMode::Off, anisette_server: None, auto_updates: false, advertise: true, verbose: false }
    }
}

pub struct CompanionState {
    path: PathBuf,
    data_dir: PathBuf,
    settings: Mutex<Settings>,
    running: tokio::sync::Mutex<Option<Arc<Companion>>>,
    /// The relay's, or the anisette's, last failure, for the page.
    relay_error: Mutex<Option<String>>,
    anisette_error: Mutex<Option<String>>,
    tray: Mutex<Option<TrayItems>>,
    /// For bringing the window up when a device asks to pair.
    app: Mutex<Option<AppHandle>>,
}

struct TrayItems {
    relay: MenuItem<Wry>,
    anisette: MenuItem<Wry>,
    /// "No Updates", greyed out, until an update is available to open.
    update: MenuItem<Wry>,
    auto_updates: CheckMenuItem<Wry>,
}

type Shared<'a> = tauri::State<'a, Arc<CompanionState>>;

impl CompanionState {
    pub fn auto_updates(&self) -> bool {
        self.settings.lock().unwrap().auto_updates
    }

    fn set_auto_updates(&self, on: bool) -> Result<(), Failure> {
        self.change(|settings| settings.auto_updates = on)
    }

    /// Whether the verbose record of sign-ins is on, as last chosen.
    pub fn verbose(&self) -> bool {
        self.settings.lock().unwrap().verbose
    }

    fn set_verbose(&self, on: bool) -> Result<(), Failure> {
        self.change(|settings| settings.verbose = on)
    }

    /// Saves `edit` applied to the settings, and keeps them only if the save worked.
    fn change(&self, edit: impl FnOnce(&mut Settings)) -> Result<(), Failure> {
        let mut settings = self.settings.lock().unwrap();
        let mut next = settings.clone();
        edit(&mut next);
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| Failure::new(error.to_string()))?;
        }
        let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())
            .map_err(|error| Failure::new(error.to_string()))?;
        serde_json::to_writer_pretty(&mut file, &next).map_err(|error| Failure::new(error.to_string()))?;
        file.persist(&self.path).map_err(|error| Failure::new(error.to_string()))?;
        *settings = next;
        Ok(())
    }

    pub fn new(store: &api::DeviceStore) -> Self {
        let path = store.dir.join("companion.json");
        let settings = std::fs::read(&path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
        Self {
            path,
            data_dir: store.dir.join("anisette"),
            settings: Mutex::new(settings),
            running: Default::default(),
            relay_error: Default::default(),
            anisette_error: Default::default(),
            tray: Default::default(),
            app: Default::default(),
        }
    }

    fn save(&self) {
        let settings = self.settings.lock().unwrap();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.path, serde_json::to_vec_pretty(&*settings).unwrap_or_default());
    }

    /// Brings what runs in line with the settings: starts or stops the relay, and the
    /// anisette server it offers.
    async fn apply(&self) {
        let settings = self.settings.lock().unwrap().clone();
        let mut running = self.running.lock().await;
        if !settings.relay {
            if let Some(companion) = running.take() {
                companion.stop_builtin_anisette().await;
            }
            *self.relay_error.lock().unwrap() = None;
            *self.anisette_error.lock().unwrap() = None;
            return;
        }
        if running.is_none() {
            let log: relay::Log = Arc::new(|line: &str| eprintln!("{line}"));
            let (relay_port, anisette_port) = ports();
            match Companion::start(relay_port, anisette_port, settings.advertise, &self.data_dir, log).await {
                Ok(companion) => {
                    if let Some(app) = self.app.lock().unwrap().clone() {
                        companion.devices.on_pairing(Arc::new(move |id, request| {
                            show_window(&app);
                            let _ = app.emit("companion-pairing", json!({ "id": id, "request": request }));
                        }));
                    }
                    // The Store may already be up: paired computers can reach it at once.
                    if let Some(route) = crate::developer::route() {
                        companion.set_developer(Some(route));
                    }
                    *running = Some(Arc::new(companion));
                    *self.relay_error.lock().unwrap() = None;
                }
                Err(error) => {
                    *self.relay_error.lock().unwrap() = Some(relay_failure(&error.to_string()));
                    return;
                }
            }
        }
        let companion = running.as_ref().expect("running").clone();
        drop(running);
        companion.relay.set_advertise(settings.advertise);
        let result = match settings.anisette {
            AnisetteMode::Builtin => companion.share_builtin_anisette().await.map_err(|error| error.to_string()),
            #[cfg(target_os = "macos")]
            AnisetteMode::Vm => companion.share_vm_anisette(api::vm::default_bundle()).await.map_err(|error| error.to_string()),
            #[cfg(not(target_os = "macos"))]
            AnisetteMode::Vm => Err("The anisette VM runs only on a Mac.".to_string()),
            AnisetteMode::Android => companion.share_android_anisette(self.data_dir.clone()).await.map_err(|error| error.to_string()),
            AnisetteMode::Server => match settings.anisette_server.as_deref() {
                Some(address) => companion.share_server_anisette(address).await.map_err(|error| error.to_string()),
                None => Err("Give the anisette server's address.".to_string()),
            },
            AnisetteMode::Off | AnisetteMode::Retired => {
                companion.stop_builtin_anisette().await;
                Ok(())
            }
        };
        *self.anisette_error.lock().unwrap() = result.err();
    }

    /// The anisette VM was just set up here: offer its anisette, unless the companion offers something already.
    pub async fn use_vm_if_off(&self) {
        {
            let mut settings = self.settings.lock().unwrap();
            if !matches!(settings.anisette, AnisetteMode::Off | AnisetteMode::Retired) {
                return;
            }
            settings.anisette = AnisetteMode::Vm;
        }
        self.save();
        self.apply().await;
        self.update_tray().await;
    }

    /// The anisette VM is going away: stop offering it.
    pub async fn stop_vm(&self) {
        let was_vm = {
            let mut settings = self.settings.lock().unwrap();
            let was = settings.anisette == AnisetteMode::Vm;
            if was {
                settings.anisette = AnisetteMode::Off;
            }
            was
        };
        if was_vm {
            self.save();
            self.apply().await;
            self.update_tray().await;
        }
    }

    /// The in-process Store is up: offer it to computers paired for developer access.
    /// The Store paired computers reach (None once it stopped): its port and token change with
    /// every start.
    pub async fn offer_developer(&self, route: Option<apparition_core::companion_channel::DeveloperRoute>) {
        if let Some(companion) = self.running.lock().await.as_ref() {
            companion.set_developer(route);
        }
    }

    async fn status(&self) -> Value {
        let settings = self.settings.lock().unwrap().clone();
        let running = self.running.lock().await.clone();
        let offered = running.as_ref().and_then(|c| c.anisette());
        json!({
            // What Windows Defender Firewall does with connections coming in for this app ("allowed", "blocked",
            // "unasked"), or null where there is nothing to manage (see core's firewall.rs).
            "firewall": tokio::task::spawn_blocking(apparition_core::firewall::inbound).await.ok().flatten().map(|inbound| inbound.name()),
            "relay": {
                "on": settings.relay,
                "running": running.is_some(),
                "port": ports().0,
                "error": *self.relay_error.lock().unwrap(),
            },
            "anisette": {
                "mode": match settings.anisette { AnisetteMode::Retired => AnisetteMode::Off, mode => mode },
                "offered": offered.map(|source| match source { AnisetteSource::Builtin => "builtin", AnisetteSource::Vm => "vm", AnisetteSource::Android => "android", AnisetteSource::Server => "server" }),
                // The third-party server's address, whichever mode is chosen.
                "server": settings.anisette_server,
                "port": ports().1,
                "builtinAvailable": api::builtin_anisette_available(),
                // Apparition's own Android anisette can be offered here (it is switched on in this build).
                "androidAvailable": api::android_anisette_available(),
                // This Mac gives its own anisette (macOS 26 or earlier, or a VM set up for it).
                "native": tokio::task::spawn_blocking(api::native_anisette).await.unwrap_or(false),
                "error": *self.anisette_error.lock().unwrap(),
            },
            "connections": running.as_ref().map(|c| c.relay.connections()).unwrap_or_default(),
            "advertise": {
                "on": settings.advertise,
                "running": running.as_ref().is_some_and(|c| c.relay.advertising()),
                // What to type on a device when this computer isn't announced.
                "address": relay::primary_address().map(|ip| ip.to_string()),
            },
            "pairing": {
                // Anisette for paired devices only.
                "required": matches!(settings.anisette, AnisetteMode::Builtin | AnisetteMode::Vm | AnisetteMode::Android | AnisetteMode::Server),
                "port": running.as_ref().map(|c| c.pairing_port()),
                "channelPort": running.as_ref().map(|c| c.channel_port()),
                "devices": running.as_ref().map(|c| c.devices.list()).unwrap_or_default(),
                "waiting": running.as_ref().map(|c| c.devices.waiting().into_iter()
                    .map(|(id, request)| json!({ "id": id, "request": request })).collect::<Vec<_>>()).unwrap_or_default(),
            },
            // What Apparition's Find companion lists (the mDNS name).
            "computerName": relay::machine_name(),
        })
    }

    /// The tray menu's two status lines.
    async fn update_tray(&self) {
        let running = self.running.lock().await.clone();
        let relay = match (&running, &*self.relay_error.lock().unwrap()) {
            (Some(companion), _) => {
                let open = companion.relay.connections().iter().filter(|c| c.outcome.is_none()).count();
                match open {
                    0 => format!("Relay on (port {})", companion.relay.port()),
                    1 => "Relay on, 1 connection".to_string(),
                    n => format!("Relay on, {n} connections"),
                }
            }
            (None, Some(_)) => "Relay failed to start".to_string(),
            (None, None) => "Relay off".to_string(),
        };
        let anisette = match running.as_ref().and_then(|c| c.anisette()) {
            Some(AnisetteSource::Builtin) => "Anisette: offered from this Mac".to_string(),
            Some(AnisetteSource::Vm) => "Anisette: offered from this Mac's VM".to_string(),
            Some(AnisetteSource::Android) => "Anisette: offered from Apparition's own".to_string(),
            Some(AnisetteSource::Server) => "Anisette: offered from a third-party server".to_string(),
            None => "Anisette: not offered".to_string(),
        };
        if let Some(tray) = &*self.tray.lock().unwrap() {
            let _ = tray.relay.set_text(relay);
            let _ = tray.anisette.set_text(anisette);
        }
    }
}

/// What to tell the user when the relay can't listen.
fn relay_failure(error: &str) -> String {
    if error.contains("in use") || error.contains("Address already") {
        let port = ports().0;
        format!("Port {port} is taken: another companion (Apparition Companion, or `apparition relay`) is probably running. Quit it, and the relay starts here within a few seconds.")
    } else {
        error.to_string()
    }
}

/// Starts what the settings say, the tray icon, and a loop that keeps the relay and the tray
/// current.
pub fn setup(app: &AppHandle, state: Arc<CompanionState>) -> tauri::Result<()> {
    *state.app.lock().unwrap() = Some(app.clone());
    let relay = MenuItem::with_id(app, "relay", "Relay", false, None::<&str>)?;
    let anisette = MenuItem::with_id(app, "anisette", "Anisette", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Apparition", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Apparition", true, None::<&str>)?;
    let update = MenuItem::with_id(app, "update", "No Updates", false, None::<&str>)?;
    let auto_updates = CheckMenuItem::with_id(
        app,
        "auto-updates",
        "Check Automatically",
        true,
        state.auto_updates(),
        None::<&str>,
    )?;
    let separator = || PredefinedMenuItem::separator(app);
    // The updates section: its state, then the setting.
    let menu = Menu::with_items(
        app,
        &[&relay, &anisette, &separator()?, &update, &auto_updates, &separator()?, &open, &quit],
    )?;
    *state.tray.lock().unwrap() = Some(TrayItems { relay, anisette, update, auto_updates });
    // A template image on macOS, which draws it in the menu bar's colour; the colour icon
    // elsewhere, where a black ghost would vanish on a dark panel or taskbar.
    #[cfg(target_os = "macos")]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    #[cfg(not(target_os = "macos"))]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Apparition")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_window(app),
            "update" => show_window(app),
            "auto-updates" => {
                let state = app.state::<Arc<CompanionState>>();
                // The menu toggled its check mark already; that is the requested state.
                let on = state
                    .tray
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|items| items.auto_updates.is_checked().ok());
                if let Some(on) = on {
                    let _ = change_auto_updates(app, &state, on);
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    tauri::async_runtime::spawn(async move {
        state.apply().await;
        loop {
            state.update_tray().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            let settings = state.settings.lock().unwrap().clone();
            let running = state.running.lock().await.clone();
            match running {
                // Say the port was taken: try again until it's free.
                None if settings.relay => state.apply().await,
                _ => {}
            }
        }
    });
    Ok(())
}

/// Keep the announcement visible even while the companion's window is closed.
pub fn update_available(app: &AppHandle, version: Option<&str>) {
    let state = app.state::<Arc<CompanionState>>();
    if let Some(items) = &*state.tray.lock().unwrap() {
        let _ = items
            .update
            .set_text(version.map_or("No Updates".into(), |v| format!("Update to {v}...")));
        let _ = items.update.set_enabled(version.is_some());
    }
    if let Some(tray) = app.tray_by_id("main") {
        let tooltip = version.map(|v| format!("Apparition {v} available")).unwrap_or_else(|| "Apparition".into());
        let _ = tray.set_tooltip(Some(tooltip));
        #[cfg(target_os = "macos")]
        let bytes = include_bytes!("../icons/tray.png").as_slice();
        #[cfg(not(target_os = "macos"))]
        let bytes = include_bytes!("../icons/32x32.png").as_slice();
        if let Ok(icon) = tauri::image::Image::from_bytes(bytes) {
            let mut pixels = icon.rgba().to_vec();
            if version.is_some() {
                let (width, height) = (icon.width() as i32, icon.height() as i32);
                let radius = (width / 6).max(2);
                let (cx, cy) = (width - radius - 1, radius);
                for y in 0..height {
                    for x in 0..width {
                        if (x - cx).pow(2) + (y - cy).pow(2) <= radius.pow(2) {
                            let offset = ((y * width + x) * 4) as usize;
                            pixels[offset..offset + 4].copy_from_slice(if cfg!(target_os = "macos") { &[0, 0, 0, 255] } else { &[235, 65, 80, 255] });
                        }
                    }
                }
            }
            let _ = tray.set_icon(Some(tauri::image::Image::new_owned(pixels, icon.width(), icon.height())));
        }
    }
}

/// Whether the window was sent to the tray by [`hide_window`] (closing it), as opposed to the system
/// hiding the whole app (Cmd+H on macOS) or minimising it. macOS asks the app to "reopen" whenever it is
/// activated with no visible window, and that includes a hidden app showing a tooltip for its menu-bar
/// icon: reopening then pulled the window back after Cmd+H.
static IN_TRAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What a macOS reopen request (the Dock icon, or opening the app again) should do: bring the window back
/// only when the app was closed to the tray. A hidden or minimised app is brought back by the system when
/// the person asks, and anything else that reopens it is not asking for the window.
pub fn reopen(app: &AppHandle, has_visible_windows: bool) {
    if !has_visible_windows && IN_TRAY.load(std::sync::atomic::Ordering::Relaxed) {
        show_window(app);
    }
}

/// Shows the window (and, on macOS, the Dock icon).
pub fn show_window(app: &AppHandle) {
    IN_TRAY.store(false, std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Hides the window; the companion keeps running from the tray (and, on macOS, leaves the Dock).
pub fn hide_window(app: &AppHandle) {
    IN_TRAY.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

// MARK: Commands

/// The verbose record of sign-ins, for the Companion tab: whether it is on, where it is and how big.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerboseLog {
    on: bool,
    path: Option<String>,
    bytes: u64,
}

fn verbose_log_state() -> VerboseLog {
    let path = apparition_core::verbose::path();
    let bytes = path.as_ref().and_then(|path| std::fs::metadata(path).ok()).map_or(0, |meta| meta.len());
    VerboseLog { on: apparition_core::verbose::is_on(), path: path.map(|path| path.display().to_string()), bytes }
}

#[tauri::command]
pub fn verbose_log() -> VerboseLog {
    verbose_log_state()
}

/// Switches the record on or off and remembers the choice for the next launch.
#[tauri::command]
pub fn set_verbose_log(state: Shared<'_>, on: bool) -> Result<VerboseLog, Failure> {
    state.set_verbose(on)?;
    apparition_core::verbose::set(on);
    Ok(verbose_log_state())
}

#[tauri::command]
pub fn clear_verbose_log() -> VerboseLog {
    apparition_core::verbose::clear();
    verbose_log_state()
}

/// Shows the record in the file manager, or the folder it will be in once there is one.
#[tauri::command]
pub fn reveal_verbose_log(app: AppHandle) -> Result<(), Failure> {
    use tauri_plugin_opener::OpenerExt;
    let path = apparition_core::verbose::path().ok_or_else(|| Failure::new("Logging isn't set up."))?;
    let result = if path.exists() {
        app.opener().reveal_item_in_dir(&path)
    } else {
        app.opener().open_path(path.parent().unwrap_or(&path).display().to_string(), None::<&str>)
    };
    result.map_err(|error| Failure::new(error.to_string()))
}

#[tauri::command]
pub fn auto_updates(state: Shared<'_>) -> bool {
    state.auto_updates()
}

#[tauri::command]
pub fn set_auto_updates(app: AppHandle, state: Shared<'_>, on: bool) -> Result<bool, Failure> {
    change_auto_updates(&app, &state, on)?;
    Ok(on)
}

/// From the page or the tray: saves the choice, then brings the other one in line.
fn change_auto_updates(app: &AppHandle, state: &CompanionState, on: bool) -> Result<(), Failure> {
    let result = state.set_auto_updates(on);
    let saved = state.auto_updates();
    if let Some(items) = &*state.tray.lock().unwrap() {
        let _ = items.auto_updates.set_checked(saved);
    }
    let _ = app.emit("auto-updates-changed", saved);
    result?;
    if on {
        crate::updater::request_automatic_check(app);
    }
    Ok(())
}

#[tauri::command]
pub async fn companion_status(state: Shared<'_>) -> Result<Value, Failure> {
    Ok(state.status().await)
}

#[tauri::command]
pub async fn companion_set_relay(state: Shared<'_>, on: bool) -> Result<Value, Failure> {
    state.settings.lock().unwrap().relay = on;
    state.save();
    state.apply().await;
    state.update_tray().await;
    Ok(state.status().await)
}

#[tauri::command]
pub async fn companion_set_anisette(state: Shared<'_>, mode: AnisetteMode, server: Option<String>) -> Result<Value, Failure> {
    // The third-party server's address: checked and kept in its normal form. Only its syntax is checked here;
    // `check_anisette_server` asks whether it answers.
    if mode == AnisetteMode::Server {
        let address = server.or_else(|| state.settings.lock().unwrap().anisette_server.clone()).unwrap_or_default();
        let address = apparition_core::anisette::normalize_server_address(&address).map_err(Failure::new)?;
        state.settings.lock().unwrap().anisette_server = Some(address);
    }
    if mode == AnisetteMode::Builtin && !tokio::task::spawn_blocking(api::native_anisette).await.unwrap_or(false) {
        return Err("Only a Mac on macOS 26 or earlier, or a macOS VM set up for it, offers its own anisette. Pair your devices with that Mac's Apparition instead.".into());
    }
    if mode == AnisetteMode::Android && !api::android_anisette_available() {
        return Err("Apparition's own Android anisette is switched off in this build. Offer a Mac's, a Docker server's or a third-party server's instead.".into());
    }
    #[cfg(target_os = "macos")]
    if mode == AnisetteMode::Vm && !api::vm::default_bundle().join("ready").exists() {
        return Err("Set up the anisette VM first (below).".into());
    }
    state.settings.lock().unwrap().anisette = mode;
    state.save();
    state.apply().await;
    state.update_tray().await;
    Ok(state.status().await)
}

/// Lets devices' connections through Windows Defender Firewall for this app: adds the rule, which Windows asks an
/// administrator to approve (core's firewall.rs).
#[tauri::command]
pub async fn companion_allow_firewall(state: Shared<'_>) -> Result<Value, Failure> {
    tokio::task::spawn_blocking(apparition_core::firewall::allow).await.map_err(|error| Failure::new(error.to_string()))??;
    Ok(state.status().await)
}

#[tauri::command]
pub async fn companion_set_advertise(state: Shared<'_>, on: bool) -> Result<Value, Failure> {
    state.settings.lock().unwrap().advertise = on;
    state.save();
    state.apply().await;
    Ok(state.status().await)
}

/// The person's answer to a device asking to pair.
#[tauri::command]
pub async fn companion_pairing_decide(state: Shared<'_>, id: u64, allow: bool) -> Result<Value, Failure> {
    if let Some(companion) = state.running.lock().await.clone() {
        companion.devices.decide(id, allow);
    }
    Ok(state.status().await)
}

/// Unpairs a device: its anisette access stops at once.
#[tauri::command]
pub async fn companion_forget_device(state: Shared<'_>, id: String) -> Result<Value, Failure> {
    if let Some(companion) = state.running.lock().await.clone() {
        companion.devices.forget(&id);
    }
    Ok(state.status().await)
}

#[tauri::command]
pub fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub fn launch_at_login(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
pub fn set_launch_at_login(app: AppHandle, on: bool) -> Result<bool, Failure> {
    let manager = app.autolaunch();
    let result = if on { manager.enable() } else { manager.disable() };
    result.map_err(|error| Failure::new(format!("Couldn't change the login item: {error}")))?;
    Ok(manager.is_enabled().unwrap_or(on))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_consent_is_opt_in_for_new_and_existing_settings() {
        assert!(!Settings::default().auto_updates);
        let old: Settings = serde_json::from_str(r#"{"relay":false,"anisette":"local"}"#).unwrap();
        assert!(!old.auto_updates);
        // Settings from before these existed: announced, and anisette for paired devices only.
        assert!(old.advertise);
        assert!(!old.relay);
        // The Docker server it offered is retired: nothing is offered in its place.
        assert_eq!(old.anisette, AnisetteMode::Retired);
    }

    #[test]
    fn the_android_anisette_is_a_mode_of_its_own() {
        let settings: Settings = serde_json::from_str(r#"{"anisette":"android"}"#).unwrap();
        assert_eq!(settings.anisette, AnisetteMode::Android);
        assert_eq!(serde_json::to_value(&settings).unwrap()["anisette"], "android");
    }

    #[test]
    fn a_third_party_server_is_a_mode_of_its_own_and_its_address_is_kept() {
        let settings: Settings = serde_json::from_str(r#"{"anisette":"server","anisetteServer":"https://ani.example.com"}"#).unwrap();
        assert_eq!(settings.anisette, AnisetteMode::Server);
        assert_eq!(settings.anisette_server.as_deref(), Some("https://ani.example.com"));
        // Settings from before it existed have none, and keep every other default.
        let old: Settings = serde_json::from_str(r#"{"anisette":"builtin"}"#).unwrap();
        assert_eq!(old.anisette, AnisetteMode::Builtin);
        assert!(old.anisette_server.is_none());
        let written = serde_json::to_value(&settings).unwrap();
        assert_eq!(written["anisette"], "server");
    }

    #[test]
    fn the_verbose_record_is_off_until_chosen_and_the_choice_is_kept() {
        assert!(!Settings::default().verbose);
        let old: Settings = serde_json::from_str(r#"{"relay":true,"autoUpdates":true}"#).unwrap();
        assert!(!old.verbose);
        let dir = tempfile::tempdir().unwrap();
        let store = api::DeviceStore { dir: dir.path().to_path_buf() };
        let state = CompanionState::new(&store);
        state.set_auto_updates(true).unwrap();
        state.set_verbose(true).unwrap();
        let reloaded = CompanionState::new(&store);
        // Saving one setting keeps the other.
        assert!(reloaded.verbose() && reloaded.auto_updates());
        state.set_verbose(false).unwrap();
        assert!(!CompanionState::new(&store).verbose() && CompanionState::new(&store).auto_updates());
    }

    #[test]
    fn update_consent_persists_without_changing_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let store = api::DeviceStore { dir: dir.path().to_path_buf() };
        let state = CompanionState::new(&store);
        for on in [true, false] {
            state.set_auto_updates(on).unwrap();
            let reloaded = CompanionState::new(&store);
            assert_eq!(reloaded.auto_updates(), on);
            assert!(reloaded.settings.lock().unwrap().relay);
        }
        std::fs::remove_file(&state.path).unwrap();
        std::fs::create_dir(&state.path).unwrap();
        assert!(state.set_auto_updates(true).is_err());
        assert!(!state.auto_updates());
    }
}
