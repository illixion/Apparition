//! The companion side of the app: the relay and the anisette server Apparition on a device
//! uses (`apparition_core::companion`), its settings, and the tray (menu-bar) icon that keeps
//! it running with the window closed.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apparition_core::api;
use apparition_core::companion::{self, AnisetteSource, Companion};
use apparition_core::relay::{self, ANISETTE_PORT, RELAY_PORT};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
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
    /// This computer's own, served on the LAN.
    Builtin,
    /// One already running here on port 6969 (the Docker one), whenever it answers.
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    relay: bool,
    anisette: AnisetteMode,
    auto_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        // The relay is what Apparition on a device needs to do anything, so it's on unless
        // turned off; serving anisette to the network is a choice the user makes.
        Self { relay: true, anisette: AnisetteMode::Off, auto_updates: false }
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
}

struct TrayItems {
    relay: MenuItem<Wry>,
    anisette: MenuItem<Wry>,
    update: MenuItem<Wry>,
}

type Shared<'a> = tauri::State<'a, Arc<CompanionState>>;

impl CompanionState {
    pub fn auto_updates(&self) -> bool {
        self.settings.lock().unwrap().auto_updates
    }

    fn set_auto_updates(&self, on: bool) -> Result<(), Failure> {
        let mut settings = self.settings.lock().unwrap();
        let mut next = settings.clone();
        next.auto_updates = on;
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
            match Companion::start(relay_port, anisette_port, true, &self.data_dir, log).await {
                Ok(companion) => {
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
        let result = match settings.anisette {
            AnisetteMode::Builtin => companion.share_builtin_anisette().await.map_err(|error| error.to_string()),
            AnisetteMode::Local | AnisetteMode::Off => {
                companion.stop_builtin_anisette().await;
                if settings.anisette == AnisetteMode::Local {
                    companion.refresh().await;
                }
                Ok(())
            }
        };
        *self.anisette_error.lock().unwrap() = result.err();
    }

    async fn status(&self) -> Value {
        let settings = self.settings.lock().unwrap().clone();
        let running = self.running.lock().await.clone();
        let offered = running.as_ref().and_then(|c| c.anisette());
        json!({
            "relay": {
                "on": settings.relay,
                "running": running.is_some(),
                "port": ports().0,
                "error": *self.relay_error.lock().unwrap(),
            },
            "anisette": {
                "mode": settings.anisette,
                "offered": offered.map(|source| match source { AnisetteSource::Builtin => "builtin", AnisetteSource::Local => "local" }),
                "port": ports().1,
                "builtinAvailable": api::builtin_anisette_available(),
                // This Mac's own headers, which Apple accepts, rather than the Android libraries'.
                "native": tokio::task::spawn_blocking(api::native_anisette).await.unwrap_or(false),
                "error": *self.anisette_error.lock().unwrap(),
            },
            "connections": running.as_ref().map(|c| c.relay.connections()).unwrap_or_default(),
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
            Some(AnisetteSource::Builtin) => "Anisette: offered from this computer".to_string(),
            Some(AnisetteSource::Local) => format!("Anisette: offering the server on port {}", ports().1),
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

/// Starts what the settings say, the tray icon, and a loop that keeps a Docker server's offer
/// and the tray current.
pub fn setup(app: &AppHandle, state: Arc<CompanionState>) -> tauri::Result<()> {
    let relay = MenuItem::with_id(app, "relay", "Relay", false, None::<&str>)?;
    let anisette = MenuItem::with_id(app, "anisette", "Anisette", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open Apparition", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Apparition", true, None::<&str>)?;
    let update = MenuItem::with_id(app, "update", "Check for Updates...", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&relay, &anisette, &PredefinedMenuItem::separator(app)?, &open, &update, &quit])?;
    *state.tray.lock().unwrap() = Some(TrayItems { relay, anisette, update });
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
            "update" => {
                show_window(app);
                crate::updater::request_check(app);
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
                Some(companion) if settings.anisette == AnisetteMode::Local => companion.refresh().await,
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
        let text = version.map(|v| format!("Apparition {v} available - Open to Update")).unwrap_or_else(|| "Check for Updates...".into());
        let _ = items.update.set_text(text);
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

/// Shows the window (and, on macOS, the Dock icon).
pub fn show_window(app: &AppHandle) {
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
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
}

// MARK: Commands

#[tauri::command]
pub fn auto_updates(state: Shared<'_>) -> bool {
    state.auto_updates()
}

#[tauri::command]
pub fn set_auto_updates(app: AppHandle, state: Shared<'_>, on: bool) -> Result<bool, Failure> {
    state.set_auto_updates(on)?;
    let _ = app.emit("auto-updates-changed", on);
    if on {
        crate::updater::request_automatic_check(&app);
    }
    Ok(on)
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
pub async fn companion_set_anisette(state: Shared<'_>, mode: AnisetteMode) -> Result<Value, Failure> {
    if mode == AnisetteMode::Builtin && !api::builtin_anisette_available() {
        return Err("This computer can't make its own anisette data; use the Docker server instead.".into());
    }
    state.settings.lock().unwrap().anisette = mode;
    state.save();
    state.apply().await;
    state.update_tray().await;
    Ok(state.status().await)
}

/// Starts or stops the self-hosted anisette server with the user's Docker.
#[tauri::command]
pub async fn companion_docker(state: Shared<'_>, up: bool) -> Result<Value, Failure> {
    companion::docker_anisette(up).await?;
    if let Some(companion) = state.running.lock().await.clone() {
        companion.refresh().await;
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
        assert!(!old.relay);
        assert_eq!(old.anisette, AnisetteMode::Local);
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
