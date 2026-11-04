//! The Store as this computer runs it, for the desktop app and `apparition store serve` alike:
//! in the Store's folder (`apparition_core::api::store_dir()`), on config.json's `port` (8484
//! by default) on loopback, with a token only this user's programs read from `developer.json`,
//! and the devices this computer is paired with (`paired_devices`).
//!
//! One Store per folder: `start` refuses while another (the app's, or `store serve` in another
//! terminal) answers at the address its `developer.json` names.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use apparition_core::companion_channel::DeveloperRoute;
use serde_json::{Value, json};

pub const DEFAULT_PORT: u16 = 8484;

/// The Store's folder: `APPARITION_STORE_DIR` when set (a second Store, a test), else the app's.
pub fn store_dir() -> PathBuf {
    std::env::var_os("APPARITION_STORE_DIR").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(apparition_core::api::store_dir)
}

fn handoff_path(dir: &Path) -> PathBuf {
    dir.join("developer.json")
}

/// The Store's config, made on first start: an empty project list on the usual port.
pub fn ensure_config(dir: &Path) -> Result<(PathBuf, u16), String> {
    std::fs::create_dir_all(dir.join("data")).map_err(|e| format!("Couldn't create {}: {e}", dir.display()))?;
    let path = dir.join("config.json");
    if !path.is_file() {
        let config = json!({ "port": DEFAULT_PORT, "bundlePrefix": [], "repos": [] });
        write_private(&path, (serde_json::to_string_pretty(&config).unwrap() + "\n").as_bytes())?;
    }
    let port = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|c| c["port"].as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(DEFAULT_PORT);
    Ok((path, port))
}

/// Written beside, then renamed into place, readable only by this user.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let temporary = path.with_extension("new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // Windows: the profile folder's ACL already keeps other users out.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temporary).map_err(|e| format!("Couldn't write {}: {e}", temporary.display()))?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    drop(file);
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}

/// The Store running for this folder, if one answers: its route and the process that runs it.
pub fn running(dir: &Path) -> Option<(DeveloperRoute, Option<u32>)> {
    let value: Value = serde_json::from_slice(&std::fs::read(handoff_path(dir)).ok()?).ok()?;
    let address: SocketAddr = value["url"].as_str()?.strip_prefix("http://")?.parse().ok()?;
    let token = value["token"].as_str()?.to_string();
    // Left behind by a Store that didn't stop cleanly: nothing answers there.
    std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300)).ok()?;
    let pid = value["pid"].as_u64().and_then(|p| u32::try_from(p).ok());
    Some((DeveloperRoute { address, token }, pid))
}

/// A started Store; it stops when this is dropped.
pub struct Hosted {
    pub route: DeveloperRoute,
    dir: PathBuf,
    served: crate::server::Served,
}

impl Hosted {
    /// Stops serving and its builds, and takes `developer.json` away, so nothing here tries it again.
    pub fn stop(self) {}

    /// Runs until the server stops by itself (it doesn't, short of an error).
    pub async fn wait(&mut self) {
        self.served.wait().await;
    }

    /// Why it isn't on the port config.json asks for, when it isn't.
    pub fn moved(&self) -> Option<&str> {
        self.served.moved.as_deref()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Hosted {
    fn drop(&mut self) {
        self.served.stop();
        forget_handoff(&self.dir, &self.route);
    }
}

/// Removes `developer.json` only while it is still this Store's.
fn forget_handoff(dir: &Path, route: &DeveloperRoute) {
    let path = handoff_path(dir);
    let ours = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["token"].as_str() == Some(route.token.as_str()));
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

pub struct Options {
    /// The Store's folder: config.json and data/.
    pub dir: PathBuf,
    /// In place of config.json's.
    pub port: Option<u16>,
    /// How the Store introduces itself to the apps' debug servers.
    pub client_name: String,
}

impl Options {
    pub fn new(client_name: String) -> Options {
        Options { dir: store_dir(), port: None, client_name }
    }
}

/// Starts the Store with this computer's pairings, and writes `developer.json` for the
/// programs here (`apparition mcp`, `apparition store`).
pub async fn start(options: Options) -> Result<Hosted, String> {
    let Options { dir, port, client_name } = options;
    if let Some((route, pid)) = running(&dir) {
        let by = pid.map(|p| format!(" (process {p})")).unwrap_or_default();
        return Err(format!("A Store for {} already runs at http://{}{by}: the Apparition app's, or `apparition store serve` elsewhere.", dir.display(), route.address));
    }
    let (config, config_port) = ensure_config(&dir)?;
    let token = crate::auth::new_token();
    let mut settings = crate::server::Settings::new(config, dir.join("data"), dir.join("public"));
    settings.port = Some(port.unwrap_or(config_port));
    // The web UI compiled into this binary, not a folder.
    settings.public_dir = None;
    settings.host_token = Some(token.clone());
    settings.client_name = client_name;
    settings.devices = Some(std::sync::Arc::new(crate::paired_devices::CoreDevices::new()));
    let (address, served) = crate::server::serve_loopback(settings).await.map_err(|e| e.to_string())?;
    let route = DeveloperRoute { address, token };
    let body = json!({ "url": format!("http://{}", route.address), "token": route.token, "pid": std::process::id() });
    // On failure `served` is dropped here, which stops it.
    write_private(&handoff_path(&dir), body.to_string().as_bytes())?;
    Ok(Hosted { route, dir, served })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_store_per_folder_and_its_handoff_goes_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let first = start(Options { dir: dir.path().into(), port: Some(0), client_name: "test".into() }).await.unwrap();
        let config: Value = serde_json::from_slice(&std::fs::read(dir.path().join("config.json")).unwrap()).unwrap();
        assert_eq!(config["port"], DEFAULT_PORT, "a new folder gets the usual port");
        let (route, pid) = running(dir.path()).expect("developer.json names the running Store");
        assert_eq!((route.address, pid), (first.route.address, Some(std::process::id())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("developer.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the token is this user's alone");
        }
        let second = start(Options { dir: dir.path().into(), port: Some(0), client_name: "test".into() }).await;
        assert!(second.err().unwrap().contains("already runs"));
        first.stop();
        assert!(!dir.path().join("developer.json").exists());
    }
}
