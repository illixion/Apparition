//! The anisette VM in the Companion tab: setting it up (core's vm.rs does the work, the same code as `apparition vm
//! setup`), what state it is in, checking it with Apple, removing it, and saving it when the app quits.
//!
//! Setup runs as a task of its own that reports `vm-progress` events (`{step, text, fraction}`) and ends with one
//! `vm-finished` (`{ok, error}`). It carries on from wherever an earlier run stopped, so stopping it, or the app
//! quitting, loses nothing but the step under way.

use serde_json::Value;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
use serde_json::json;
use tauri::AppHandle;

use crate::flow::Failure;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mac {
    use std::sync::{Arc, Mutex};

    use apparition_core::api::vm;
    use serde_json::{Value, json};
    use tauri::{AppHandle, Emitter, Manager};

    use crate::companion::CompanionState;
    use crate::flow::Failure;

    #[derive(Default)]
    struct Job {
        task: Option<tauri::async_runtime::JoinHandle<()>>,
        last: Option<vm::Progress>,
        error: Option<String>,
        /// The job puts newer programs into a VM that is set up, rather than setting one up.
        update: bool,
        /// ...and hands them to the running VM (signed, seconds), rather than from its recoveryOS.
        push: bool,
    }

    static JOB: Mutex<Job> = Mutex::new(Job { task: None, last: None, error: None, update: false, push: false });

    fn running() -> bool {
        JOB.lock().unwrap().task.as_ref().is_some_and(|task| !task.inner().is_finished())
    }

    pub async fn status() -> Value {
        let bundle = vm::default_bundle();
        let status = vm::status(&bundle).await;
        let job = JOB.lock().unwrap();
        let busy = job.task.as_ref().is_some_and(|task| !task.inner().is_finished());
        json!({
            "supported": status.unsupported.is_none(),
            "unsupported": status.unsupported,
            "exists": status.exists,
            "installed": status.installed,
            "ready": status.ready,
            "suspended": status.suspended,
            "outdated": status.outdated,
            "stale": status.stale,
            // The update runs while the VM keeps serving.
            "handingOver": busy && job.push,
            // Also one a terminal's `apparition vm update` runs.
            "updating": status.updating || (busy && job.update),
            "phase": status.live.as_ref().map(|live| if live.recovery { "recovery".to_string() } else { live.phase.clone() }),
            "serving": status.live.as_ref().is_some_and(|live| live.anisette.is_some()),
            "settingUp": busy,
            "progress": job.last,
            "error": job.error,
            "diskBytes": status.disk_bytes,
            "imageCached": status.image_cached,
            "image": status.image,
        })
    }

    pub fn setup(app: AppHandle, ipsw: Option<String>) -> Result<(), Failure> {
        let mut job = JOB.lock().unwrap();
        if job.task.as_ref().is_some_and(|task| !task.inner().is_finished()) {
            return Err("The anisette VM is being set up already.".into());
        }
        job.last = None;
        job.error = None;
        // vm::setup runs the update itself on a VM that is set up with older programs: from its recoveryOS when an
        // older app set it up, otherwise handed to it while it runs.
        let bundle = vm::default_bundle();
        job.push = !vm::outdated(&bundle) && vm::stale(&bundle);
        job.update = vm::outdated(&bundle) || job.push;
        let update = job.update;
        job.task = Some(tauri::async_runtime::spawn(async move {
            let options = vm::SetupOptions { bundle: vm::default_bundle(), ipsw: ipsw.map(Into::into), ..Default::default() };
            let emitter = app.clone();
            let say = move |progress: vm::Progress| {
                let _ = emitter.emit("vm-progress", &progress);
                JOB.lock().unwrap().last = Some(progress);
            };
            let result = vm::setup(&options, &say).await;
            let error = result.as_ref().err().map(|error| apparition_core::api::describe(error));
            JOB.lock().unwrap().error = error.clone();
            if result.is_ok() && !update {
                // Set up from the Companion tab: offer it to paired devices, unless something else is offered.
                app.state::<Arc<CompanionState>>().use_vm_if_off().await;
            }
            let _ = app.emit("vm-finished", json!({ "ok": error.is_none(), "error": error, "update": update }));
        }));
        Ok(())
    }

    /// At launch: a VM with other programs than this app's gets this app's, in the background, so a new app version
    /// never means making the VM again: handed to it signed while it runs (seconds), or, for a VM an older app set
    /// up, once from its own recoveryOS (a few minutes, no download). A failure shows in the Companion tab, whose
    /// button tries again.
    pub fn update_if_outdated(app: &AppHandle) {
        let bundle = vm::default_bundle();
        if (vm::outdated(&bundle) || vm::stale(&bundle)) && !running() {
            let _ = setup(app.clone(), None);
        }
    }

    pub fn cancel() {
        let mut job = JOB.lock().unwrap();
        if let Some(task) = job.task.take() {
            task.abort();
            job.error = Some("Stopped. Set it up again to carry on from where it was.".to_string());
        }
    }

    pub async fn check() -> Result<(), Failure> {
        let url = vm::serve(&vm::default_bundle()).await?;
        vm::check_with_apple(&url).await?;
        Ok(())
    }

    pub async fn remove(app: &AppHandle) -> Result<(), Failure> {
        cancel();
        JOB.lock().unwrap().error = None;
        app.state::<Arc<CompanionState>>().stop_vm().await;
        vm::remove(&vm::default_bundle()).await?;
        Ok(())
    }

    /// The app is quitting: save the VM, if it runs for the companion or for a setup of the app's. A VM that a
    /// terminal's `apparition vm` runs for something else is left alone.
    pub fn shutdown() {
        let setting_up = running();
        cancel();
        // A runtime of its own: by the app's Exit event, Tauri's no longer drives timers or sockets, and blocking on
        // it there waits forever (the save goes through, the wait for the runner to be gone never ends).
        let worker = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            runtime.block_on(async move {
                let bundle = vm::default_bundle();
                let Some(live) = vm::live(&bundle).await else { return };
                if (live.anisette.is_some() || setting_up) && !vm::updated_elsewhere(&bundle) {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), vm::quit(&bundle)).await;
                }
            });
        });
        let _ = worker.join();
    }
}

#[tauri::command]
pub async fn vm_status() -> Value {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return mac::status().await;
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    json!({ "supported": false, "unsupported": "The anisette VM needs a Mac with Apple silicon." })
}

/// Starts setting the VM up, from the restore image at `ipsw` if given.
#[tauri::command]
pub fn vm_setup(app: AppHandle, ipsw: Option<String>) -> Result<(), Failure> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return mac::setup(app, ipsw);
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = (app, ipsw);
        Err("The anisette VM needs a Mac with Apple silicon.".into())
    }
}

#[tauri::command]
pub fn vm_cancel() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    mac::cancel();
}

/// Asks Apple whether it accepts the VM's anisette, with no Apple ID.
#[tauri::command]
pub async fn vm_check() -> Result<(), Failure> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return mac::check().await;
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    Err("The anisette VM needs a Mac with Apple silicon.".into())
}

/// Stops the VM and moves it to the Trash.
#[tauri::command]
pub async fn vm_remove(app: AppHandle) -> Result<Value, Failure> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        mac::remove(&app).await?;
        return Ok(mac::status().await);
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = app;
        Err("The anisette VM needs a Mac with Apple silicon.".into())
    }
}

/// From the app's start: updates the programs of a VM an older app set up.
pub fn update_if_outdated(app: &AppHandle) {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    mac::update_if_outdated(app);
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    let _ = app;
}

/// From the app's exit: saves the VM it runs.
pub fn shutdown() {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    mac::shutdown();
}
