//! The Apparition Store inside this app, which is then its GUI.
//!
//! The Store (`apparition-service`) runs in this process on the loopback port of config.json's
//! `port` (8484 by default: what apps upload debug traces to, and what `tailscale serve` points
//! at), with a token only this app and the agents on this computer (`developer.json`) hold. Its
//! web pages stay off until the person turns them on with a password (`host_web_ui`), which also
//! lets other devices' Apparition connect. Apps' signed trace uploads, OTA links, the AltStore
//! feed and Mac downloads work either way while the app is open.
//!
//! The Store's config and data live in `~/Library/Application Support/Apparition/Store` (the
//! per-OS equivalent elsewhere): `config.json`, `data/` (traces, logs, state, kept clones).

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use apparition_core::companion_channel::DeveloperRoute;
use serde_json::{Value, json};

use crate::service::BuildService;

const DEFAULT_PORT: u16 = 8484;

struct Running {
    stop: Box<dyn FnOnce() + Send>,
    route: DeveloperRoute,
    started: Instant,
}

#[derive(Default)]
pub struct ServiceHost {
    running: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
    /// One start at a time: the Developer tab and the launch both ask for it.
    start_turn: tokio::sync::Mutex<()>,
}

pub(crate) fn support_dir() -> PathBuf {
    apparition_core::api::store_dir()
}

/// 32 random bytes as 43 characters of unpadded base64url, the token shape the Store accepts.
pub(crate) fn token() -> Result<String, String> {
    Ok(apparition_service::auth::new_token())
}

/// The Store's config, made on first start: an empty project list on the usual port.
pub(crate) fn ensure_config() -> Result<(PathBuf, u16), String> {
    let dir = support_dir();
    std::fs::create_dir_all(dir.join("data")).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let path = dir.join("config.json");
    if !path.is_file() {
        let config = json!({ "port": DEFAULT_PORT, "bundlePrefix": [], "repos": [] });
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap() + "\n")
            .map_err(|e| format!("Couldn't write {}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    let port = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|c| c["port"].as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(DEFAULT_PORT);
    Ok((path, port))
}

impl ServiceHost {
    /// Whether it is running, with where and why not; nothing secret.
    fn status(&self) -> Value {
        let running = self.running.lock().unwrap();
        json!({
            "available": cfg!(target_os = "macos"),
            "running": running.is_some(),
            "bundled": true,
            "port": running.as_ref().map(|r| r.route.address.port()),
            "uptimeSeconds": running.as_ref().map(|r| r.started.elapsed().as_secs()),
            "folder": support_dir().display().to_string(),
            "error": self.last_error.lock().unwrap().clone(),
        })
    }

    /// Starts the Store if it isn't running, and says where it is.
    pub async fn start(&self) -> Result<DeveloperRoute, String> {
        let _turn = self.start_turn.lock().await;
        if let Some(running) = self.running.lock().unwrap().as_ref() {
            return Ok(running.route.clone());
        }
        match crate::developer::start_store().await {
            Ok((route, stop)) => {
                *self.last_error.lock().unwrap() = None;
                *self.running.lock().unwrap() = Some(Running { stop, route: route.clone(), started: Instant::now() });
                Ok(route)
            }
            Err(error) => {
                *self.last_error.lock().unwrap() = Some(error.clone());
                Err(error)
            }
        }
    }

    fn stop(&self) {
        if let Some(running) = self.running.lock().unwrap().take() {
            (running.stop)();
            crate::developer::store_stopped();
        }
    }
}

#[tauri::command]
pub fn host_status(host: tauri::State<'_, ServiceHost>) -> Value {
    host.status()
}

/// Starts the Store if needed and makes it the one the Developer tab talks to.
#[tauri::command]
pub async fn host_connect(host: tauri::State<'_, ServiceHost>, service: tauri::State<'_, BuildService>) -> Result<Value, String> {
    if !cfg!(target_os = "macos") {
        return Err("Builds need a Mac with Xcode. Connect to one instead.".into());
    }
    let route = host.start().await?;
    service.adopt_local(route.address.port(), route.token).await
}

#[tauri::command]
pub fn host_stop(host: tauri::State<'_, ServiceHost>, service: tauri::State<'_, BuildService>) {
    service.forget_local();
    host.stop();
}

/// Turns the Store's web pages on or off; a password, when given, replaces the old one.
#[tauri::command]
pub async fn host_web_ui(service: tauri::State<'_, BuildService>, enabled: bool, password: Option<String>) -> Result<Value, String> {
    service.host_call("host/web-ui", Some(json!({ "enabled": enabled, "password": password.filter(|p| !p.is_empty()) }))).await
}

/// Opens the Store's Debug console (live log, tuning panels, profile, memory) in its own
/// window, signed in by a one-time link only this app can get. The window loads the Store's
/// page over 127.0.0.1 and has no access to the app (no capability names it). `app` is a debug
/// key id to open straight to that app.
#[tauri::command]
pub async fn host_open_console(app: tauri::AppHandle, service: tauri::State<'_, BuildService>, key: Option<String>) -> Result<(), String> {
    use tauri::Manager;
    let link = service.host_call("host/web-link", Some(json!({}))).await?;
    let path = link["path"].as_str().filter(|p| p.starts_with("/host-login?code=")).ok_or("The Store didn't give a console link.")?;
    let target = match key.as_deref().filter(|k| !k.is_empty() && k.len() <= 64 && k.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))) {
        Some(k) => format!("/#debug/{k}"),
        None => "/#debug".to_string(),
    };
    let base = service.local_base().ok_or("Start the Store on this Mac first.")?;
    let next: String = target.bytes().map(|b| if b == b'#' { "%23".to_string() } else if b == b'/' { "/".to_string() } else { (b as char).to_string() }).collect();
    let url = base.join(&format!("{}&next={next}", path.trim_start_matches('/'))).map_err(|_| "Bad console link.")?;
    if let Some(window) = app.get_webview_window("console") {
        window.navigate(url).map_err(|e| e.to_string())?;
        let _ = window.unminimize();
        let _ = window.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(&app, "console", tauri::WebviewUrl::External(url))
        .title("Apparition Debug Console")
        .inner_size(1100.0, 780.0)
        .min_inner_size(640.0, 480.0)
        .build()
        .map_err(|e| format!("Couldn't open the console: {e}"))?;
    Ok(())
}

#[tauri::command]
pub async fn host_settings(service: tauri::State<'_, BuildService>) -> Result<Value, String> {
    service.host_call("host", None).await
}

/// The app is quitting: the Store goes with it.
pub fn shutdown(host: &ServiceHost) {
    host.stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_shape_the_store_accepts() {
        let a = token().unwrap();
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_ne!(a, token().unwrap());
    }
}
