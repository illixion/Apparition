//! The Apparition Store in this process, on a Mac: what an agent or another computer drives to
//! build, deploy and debug apps (MCP at `/mcp`, deploys at `/api/service/v1/deploy`), on a free
//! loopback port with a token of its own. Nothing outside this Mac reaches that port:
//!
//! - Other computers come through the companion's encrypted channel, once the person pairs them
//!   and allows developer access (`core/src/companion_channel.rs`, the `developer` grant). The
//!   companion adds this token to what they send.
//! - Agents on this Mac run `apparition mcp`, which reads the port and token from
//!   `developer.json` in the Store's folder (readable only by this user, gone when the app quits).
//!
//! The Node build service (`host.rs`) still runs builds beside it until those are ported
//! (docs/RUST_STORE.md); both read the same config and data.

use std::sync::OnceLock;

use apparition_core::companion_channel::DeveloperRoute;
use serde_json::json;
use tauri::Manager;

static ROUTE: OnceLock<DeveloperRoute> = OnceLock::new();

/// The running Store, once it is up.
pub fn route() -> Option<DeveloperRoute> {
    ROUTE.get().cloned()
}

fn handoff_path() -> std::path::PathBuf {
    crate::host::support_dir().join("developer.json")
}

/// Starts the Store and offers it to the companion.
pub fn setup(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match start().await {
            Ok(route) => {
                let companion = app.state::<std::sync::Arc<crate::companion::CompanionState>>().inner().clone();
                companion.offer_developer(route).await;
            }
            Err(error) => eprintln!("the Apparition Store didn't start: {error}"),
        }
    });
}

async fn start() -> Result<DeveloperRoute, String> {
    let (config, _) = crate::host::ensure_config()?;
    let dir = crate::host::support_dir();
    let token = crate::host::token()?;
    let mut settings = apparition_service::server::Settings::new(config, dir.join("data"), dir.join("public"));
    // The web UI compiled into the app, not a folder.
    settings.public_dir = None;
    settings.host_token = Some(token.clone());
    settings.client_name = format!("Apparition on {}", apparition_core::relay::machine_name());
    let (address, task) = apparition_service::server::serve_loopback(settings).await.map_err(|e| e.to_string())?;
    // Runs for the app's life.
    std::mem::forget(task);
    let route = DeveloperRoute { address, token };
    write_handoff(&route)?;
    let _ = ROUTE.set(route.clone());
    eprintln!("Apparition Store at http://{address} (MCP at /mcp)");
    Ok(route)
}

/// Where `apparition mcp` on this Mac finds the Store.
fn write_handoff(route: &DeveloperRoute) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = handoff_path();
    let temporary = path.with_extension("json.new");
    let body = json!({ "url": format!("http://{}", route.address), "token": route.token, "pid": std::process::id() });
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)
        .map_err(|e| format!("Couldn't write {}: {e}", temporary.display()))?;
    file.write_all(body.to_string().as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, &path).map_err(|e| e.to_string())
}

/// The app is quitting: agents here can't reach the Store any more.
pub fn shutdown() {
    let _ = std::fs::remove_file(handoff_path());
}
