//! The Apparition Store in this process, on every desktop OS: what an agent or another computer
//! drives to deploy and debug apps (MCP at `/mcp`, deploys at `/api/service/v1/deploy`), on a free
//! loopback port with a token of its own. Nothing outside this computer reaches that port:
//!
//! - Other computers come through the companion's encrypted channel, once the person pairs them
//!   and allows developer access (`core/src/companion_channel.rs`, the `developer` grant). The
//!   companion adds this token to what they send.
//! - Agents here run `apparition mcp`, which reads the port and token from `developer.json` in
//!   the Store's folder (readable only by this user, gone when the app quits).
//!
//! On a Mac it also builds with Xcode (`apparition build`, queued by the Store). On Windows and
//! Linux there is no Xcode: builds come from xtool (or a Mac) as IPAs, through `apparition deploy`.

use std::sync::Mutex;

use apparition_core::companion_channel::DeveloperRoute;
use tauri::Manager;

static ROUTE: Mutex<Option<DeveloperRoute>> = Mutex::new(None);

/// The running Store, once it is up.
pub fn route() -> Option<DeveloperRoute> {
    ROUTE.lock().unwrap().clone()
}

/// Starts the Store and offers it to the companion.
pub fn setup(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match app.state::<crate::host::ServiceHost>().start().await {
            Ok(route) => {
                let companion = app.state::<std::sync::Arc<crate::companion::CompanionState>>().inner().clone();
                companion.offer_developer(Some(route)).await;
            }
            Err(error) => eprintln!("the Apparition Store didn't start: {error}"),
        }
    });
}

/// Starts the Store on config.json's port (another if that one is taken), with the devices this
/// computer is paired with; the second value stops it and takes `developer.json` away.
pub async fn start_store() -> Result<(DeveloperRoute, Option<String>, Box<dyn FnOnce() + Send>), String> {
    let name = format!("Apparition on {}", apparition_core::relay::machine_name());
    let hosted = apparition_service::hosted::start(apparition_service::hosted::Options::new(name)).await?;
    let route = hosted.route.clone();
    let moved = hosted.moved().map(String::from);
    *ROUTE.lock().unwrap() = Some(route.clone());
    eprintln!("Apparition Store at http://{} (MCP at /mcp)", route.address);
    Ok((route, moved, Box::new(move || hosted.stop())))
}

/// The Store was stopped: agents here and paired computers can't reach it any more.
pub fn store_stopped() {
    *ROUTE.lock().unwrap() = None;
}
