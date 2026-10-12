//! Apparition Installer, the desktop app: a guided front end over the same core the
//! command-line installer uses (`apparition_core::api` and `release`). Nothing here speaks a
//! protocol of its own.
//!
//! Secrets: the Apple ID password and the .p12 password arrive from the page as command
//! arguments, are moved straight into the core (which drops them once used), and are never
//! logged, stored or echoed back. The pairing file stays in the shared device store and is
//! never sent to the page.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod flow;

use std::sync::Arc;

use tauri::Manager;

fn main() {
    apparition_core::api::init();
    let state = Arc::new(flow::State::new());
    let cleanup = state.clone();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state)
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
        ])
        .setup(|app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title("Apparition Installer");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("couldn't start Apparition Installer")
        .run(move |_, event| {
            if let tauri::RunEvent::Exit = event {
                cleanup.clean_up();
            }
        });
}
