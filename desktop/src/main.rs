//! Apparition, the desktop app: a guided installer over the same core the command line uses
//! (`apparition_core::api` and `release`), and the companion Apparition on a device relies on
//! (the relay and an anisette server, `companion.rs`), kept running from the tray or menu bar.
//! Nothing here speaks a protocol of its own.
//!
//! Secrets: the Apple ID password and the .p12 password arrive from the page as command
//! arguments, are moved straight into the core (which drops them once used), and are never
//! logged, stored or echoed back. The pairing file stays in the shared device store and is
//! never sent to the page.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod companion;
mod flow;

use std::sync::Arc;

use tauri::Manager;

fn main() {
    apparition_core::api::init();
    let state = Arc::new(flow::State::new());
    let companion_state = Arc::new(companion::CompanionState::new(&apparition_core::api::DeviceStore::default()));
    let cleanup = state.clone();
    tauri::Builder::default()
        // Opening the app again shows the running one's window, which is also the way back to
        // it on a Linux desktop with no tray.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| companion::show_window(app)))
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec![companion::HIDDEN])))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .manage(companion_state.clone())
        .invoke_handler(tauri::generate_handler![
            flow::app_info,
            flow::check_update,
            flow::open_downloads,
            flow::open_help,
            flow::paired_devices,
            flow::discover,
            flow::forget_device,
            flow::pair_device,
            flow::usb_devices,
            flow::pair_usb_device,
            flow::answer_prompt,
            flow::cancel_prompt,
            flow::saved_settings,
            flow::builtin_anisette,
            flow::apple_id_sign_in,
            flow::sign_out,
            flow::pick_file,
            flow::import_certificate,
            flow::prepare_latest,
            flow::prepare_local,
            flow::install,
            flow::list_certificates,
            flow::revoke_certificate,
            flow::list_app_ids,
            flow::delete_app_id,
            companion::companion_status,
            companion::companion_set_relay,
            companion::companion_set_anisette,
            companion::companion_docker,
            companion::quit_app,
            companion::launch_at_login,
            companion::set_launch_at_login,
        ])
        .setup(move |app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title("Apparition");
            }
            companion::setup(app.handle(), companion_state)?;
            // Started at login: straight to the tray.
            if std::env::args().any(|arg| arg == companion::HIDDEN) {
                companion::hide_window(app.handle());
            }
            Ok(())
        })
        // Closing the window keeps the companion running; Quit is in the tray menu.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                companion::hide_window(window.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .expect("couldn't start Apparition")
        .run(move |app, event| match event {
            tauri::RunEvent::Exit => cleanup.clean_up(),
            // macOS: clicking the Dock icon (or opening the app again) brings the window back.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => companion::show_window(app),
            _ => {
                let _ = app;
            }
        });
}
