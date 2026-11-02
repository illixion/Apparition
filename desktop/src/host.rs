//! Runs the appstore build service on this Mac as a child of the app, which is then its GUI.
//!
//! The service ships inside the app (`Contents/Resources/service`, put there by
//! `scripts/bundle-service.sh`) and runs on the Mac's own Node.js. The app writes a fresh
//! token as the first line of the child's stdin and talks to it with that token, needing no
//! password (appstore's hosted mode, `APPSTORE_HOSTED=1`). When the app quits, the pipe closes
//! and the service ends with it. Its web pages stay off until the person turns them on with a
//! password (`host_web_ui`), which also lets other devices' Apparition connect.
//!
//! Running appstore directly (`node server.js` under launchd or systemd) stays the option for
//! a server; this is the option for a Mac someone works at.
//!
//! The service's config and data live in `~/Library/Application Support/Apparition/Service`.
//! `APPARITION_SERVICE_DIR` points at an appstore checkout instead of the bundled copy (for
//! working on appstore), `APPARITION_NODE` at a Node.js binary.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::service::BuildService;

const DEFAULT_PORT: u16 = 8484;

struct Running {
    child: Child,
    // Held, never written again: dropping it closes the pipe, which ends the service.
    _stdin: ChildStdin,
    port: u16,
    token: String,
    started: Instant,
}

#[derive(Default)]
pub struct ServiceHost {
    running: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
    /// One token for the app's whole run, so a restarted service keeps the session valid.
    token: std::sync::OnceLock<String>,
    /// Started and not stopped by the person: the watchdog (`setup`) brings it back if it exits.
    wanted: std::sync::atomic::AtomicBool,
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn support_dir() -> PathBuf {
    home().join("Library/Application Support/Apparition/Service")
}

fn log_path() -> PathBuf {
    support_dir().join("service.log")
}

/// The service's code: an appstore checkout named by `APPARITION_SERVICE_DIR` or by `checkout`
/// in the service folder's `host.json` (for someone working on appstore: an app opened from
/// Finder has no environment to set, and a restart of the service then runs their edits), else
/// the copy in this app's Resources.
fn service_dir() -> Option<PathBuf> {
    let checkout = std::env::var_os("APPARITION_SERVICE_DIR").map(PathBuf::from).or_else(|| {
        let host: Value = serde_json::from_slice(&std::fs::read(support_dir().join("host.json")).ok()?).ok()?;
        Some(PathBuf::from(host["checkout"].as_str()?))
    });
    if let Some(dir) = checkout {
        return dir.join("server.js").is_file().then_some(dir);
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.parent()?.join("Resources/service");
    dir.join("server.js").is_file().then_some(dir)
}

/// The Mac's Node.js. An app opened from Finder gets a bare PATH, so the usual install places
/// are looked at too.
fn node() -> Option<PathBuf> {
    if let Some(node) = std::env::var_os("APPARITION_NODE").map(PathBuf::from) {
        return node.is_file().then_some(node);
    }
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("node")).collect())
        .unwrap_or_default();
    candidates.extend(["/opt/homebrew/bin/node", "/usr/local/bin/node"].map(PathBuf::from));
    candidates.push(home().join(".volta/bin/node"));
    candidates.into_iter().find(|p| p.is_file())
}

/// The PATH the service and its build scripts get: the app's own, plus Homebrew's and the
/// system's, which a GUI app's environment lacks.
fn child_path() -> String {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !dirs.iter().any(|x| x == Path::new(d)) {
            dirs.push(PathBuf::from(d));
        }
    }
    std::env::join_paths(dirs).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

/// 32 random bytes as 43 characters of unpadded base64url, the token shape appstore accepts.
fn token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "Couldn't make a token for the build service.".to_string())?;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    Ok(out)
}

/// The service's config, made on first start: an empty project list on the usual port.
fn ensure_config() -> Result<(PathBuf, u16), String> {
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

fn log_tail() -> String {
    let text = std::fs::read_to_string(log_path()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(6)..].join("\n")
}

impl ServiceHost {
    /// Whether it is running, with where and why not; nothing secret.
    fn status(&self) -> Value {
        let mut running = self.running.lock().unwrap();
        if let Some(r) = running.as_mut()
            && let Ok(Some(exit)) = r.child.try_wait()
        {
            *self.last_error.lock().unwrap() = Some(format!("The build service stopped ({exit}). {}", log_tail()));
            *running = None;
        }
        json!({
            "available": cfg!(target_os = "macos"),
            "running": running.is_some(),
            "port": running.as_ref().map(|r| r.port),
            "uptimeSeconds": running.as_ref().map(|r| r.started.elapsed().as_secs()),
            "node": node().map(|p| p.display().to_string()),
            "bundled": service_dir().map(|p| p.display().to_string()),
            "folder": support_dir().display().to_string(),
            "log": log_path().display().to_string(),
            "error": self.last_error.lock().unwrap().clone(),
        })
    }

    fn start(&self) -> Result<(u16, String), String> {
        let mut running = self.running.lock().unwrap();
        if let Some(r) = running.as_mut() {
            if r.child.try_wait().ok().flatten().is_none() {
                return Ok((r.port, r.token.clone()));
            }
            *running = None;
        }
        if !cfg!(target_os = "macos") {
            return Err("The build service runs on a Mac with Xcode. Connect to one instead.".into());
        }
        let service = service_dir().ok_or("This copy of Apparition doesn't include the build service.")?;
        let node = node().ok_or("The build service needs Node.js 18 or later. Install it (brew install node, or nodejs.org), then try again.")?;
        let (config, port) = ensure_config()?;
        if std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).is_ok() {
            return Err(format!(
                "Something else already uses port {port}, perhaps appstore run on its own. Stop it, or change \"port\" in {}.",
                config.display()
            ));
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .map_err(|e| format!("Couldn't open the service log: {e}"))?;
        let mut command = Command::new(&node);
        command
            .arg(service.join("server.js"))
            .current_dir(&service)
            .env("APPSTORE_HOSTED", "1")
            .env("APPSTORE_CONFIG", &config)
            .env("APPSTORE_DATA", support_dir().join("data"))
            .env("PATH", child_path())
            .stdin(Stdio::piped())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log);
        // The command line this app carries, for installs and launches over Apparition's tunnel.
        if let Some(cli) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("apparition-cli"))).filter(|p| p.is_file()) {
            command.env("APPARITION_CLI", cli);
        }
        let mut child = command.spawn().map_err(|e| format!("Couldn't start the build service: {e}"))?;
        let token = match self.token.get() {
            Some(t) => t.clone(),
            None => self.token.get_or_init(|| token().unwrap_or_default()).clone(),
        };
        if token.len() != 43 {
            return Err("Couldn't make a token for the build service.".into());
        }
        // Agents and scripts restart the service with `kill $(cat <folder>/service.pid)`: the
        // watchdog starts it again within seconds.
        let _ = std::fs::write(support_dir().join("service.pid"), format!("{}\n", child.id()));
        let mut stdin = child.stdin.take().ok_or("The build service has no input pipe.")?;
        stdin
            .write_all(format!("{token}\n").as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("Couldn't hand the build service its token: {e}"))?;
        // Up when it answers; gone if it exits first (a bad config, a port taken meanwhile).
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(Some(exit)) = child.try_wait() {
                let message = format!("The build service didn't start ({exit}). {}", log_tail());
                *self.last_error.lock().unwrap() = Some(message.clone());
                return Err(message);
            }
            if std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200)).is_ok() {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                return Err(format!("The build service didn't answer within 20 s. {}", log_tail()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        *self.last_error.lock().unwrap() = None;
        *running = Some(Running { child, _stdin: stdin, port, token: token.clone(), started: Instant::now() });
        self.wanted.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok((port, token))
    }

    fn stop(&self) {
        self.wanted.store(false, std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_file(support_dir().join("service.pid"));
        if let Some(mut r) = self.running.lock().unwrap().take() {
            // Closing its stdin is how it is asked to stop; it is waited for, briefly.
            drop(r._stdin);
            let deadline = Instant::now() + Duration::from_secs(5);
            while r.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
            if r.child.try_wait().ok().flatten().is_none() {
                let _ = r.child.kill();
                let _ = r.child.wait();
            }
        }
    }
}

#[tauri::command]
pub fn host_status(host: tauri::State<'_, ServiceHost>) -> Value {
    host.status()
}

/// Starts the service if needed and makes it the one the Developer tab talks to.
#[tauri::command]
pub async fn host_connect(host: tauri::State<'_, ServiceHost>, service: tauri::State<'_, BuildService>) -> Result<Value, String> {
    let (port, token) = host.start()?;
    service.adopt_local(port, token).await
}

#[tauri::command]
pub fn host_stop(host: tauri::State<'_, ServiceHost>, service: tauri::State<'_, BuildService>) {
    service.forget_local();
    host.stop();
}

/// Turns the service's web pages on or off; a password, when given, replaces the old one.
#[tauri::command]
pub async fn host_web_ui(service: tauri::State<'_, BuildService>, enabled: bool, password: Option<String>) -> Result<Value, String> {
    service.host_call("host/web-ui", Some(json!({ "enabled": enabled, "password": password.filter(|p| !p.is_empty()) }))).await
}

/// Opens the service's Debug console (live log, tuning panels, profile, memory) in its own
/// window, signed in by a one-time link only this app can get. The window loads the service's
/// page over 127.0.0.1 and has no access to the app (no capability names it). `app` is a debug
/// key id to open straight to that app.
#[tauri::command]
pub async fn host_open_console(app: tauri::AppHandle, service: tauri::State<'_, BuildService>, key: Option<String>) -> Result<(), String> {
    use tauri::Manager;
    let link = service.host_call("host/web-link", Some(json!({}))).await?;
    let path = link["path"].as_str().filter(|p| p.starts_with("/host-login?code=")).ok_or("The build service didn't give a console link.")?;
    let target = match key.as_deref().filter(|k| !k.is_empty() && k.len() <= 64 && k.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))) {
        Some(k) => format!("/#debug/{k}"),
        None => "/#debug".to_string(),
    };
    let base = service.local_base().ok_or("Start the build service on this Mac first.")?;
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

/// At launch on a Mac that carries the service and has Node.js: start it in the background, so
/// apps' trace uploads, OTA links and the AltStore feed work while the app is open, before anyone
/// opens the Developer tab (which then just adopts it). A failure is kept for `host_status`.
pub fn setup(app: &tauri::AppHandle) {
    use tauri::Manager;
    if !cfg!(target_os = "macos") || service_dir().is_none() || node().is_none() {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let host = app.state::<ServiceHost>();
        let _ = host.start();
        // The watchdog: a service that exits while wanted (a crash, or killed to restart it after
        // an edit) is started again, backing off while it keeps failing.
        let mut wait = Duration::from_secs(2);
        loop {
            std::thread::sleep(wait);
            let down = host.status()["running"] == false;
            if !down || !host.wanted.load(std::sync::atomic::Ordering::SeqCst) {
                wait = Duration::from_secs(2);
                continue;
            }
            wait = if host.start().is_ok() { Duration::from_secs(2) } else { (wait * 2).min(Duration::from_secs(60)) };
        }
    });
}

/// The app is quitting: the service goes with it.
pub fn shutdown(host: &ServiceHost) {
    host.stop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_have_the_shape_appstore_accepts() {
        let a = token().unwrap();
        assert_eq!(a.len(), 43);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_ne!(a, token().unwrap());
    }

    /// The real thing: the appstore checkout beside this repo, as a child, under a throwaway HOME.
    /// Skipped without Node.js or the checkout. Changes HOME for the process, so it is the only
    /// test here that reads it.
    #[test]
    fn starts_the_sibling_appstore_and_reaches_it_by_token() {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../appstore");
        if node().is_none() || !checkout.join("server.js").is_file() || !cfg!(target_os = "macos") {
            eprintln!("skipped: no Node.js or no appstore checkout");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("APPARITION_SERVICE_DIR", &checkout);
        }
        std::fs::create_dir_all(support_dir()).unwrap();
        std::fs::write(support_dir().join("config.json"), json!({"port": port, "bundlePrefix": [], "repos": []}).to_string()).unwrap();
        let host = ServiceHost::default();
        let (got, token) = host.start().unwrap();
        assert_eq!(got, port);
        assert_eq!(host.status()["running"], true);
        let get = |path: &str, bearer: Option<&str>| {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            let auth = bearer.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
            write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n").unwrap();
            let mut reply = String::new();
            s.read_to_string(&mut reply).unwrap();
            reply
        };
        assert!(get("/api/service/v1/host", Some(&token)).starts_with("HTTP/1.1 200"));
        assert!(get("/api/service/v1/host", None).starts_with("HTTP/1.1 401"));
        assert!(get("/", None).contains("web UI is off"));
        host.stop();
        assert_eq!(host.status()["running"], false);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "the service ended with its pipe");
    }

    #[test]
    fn the_service_path_has_homebrew_and_the_system() {
        let path = child_path();
        for dir in ["/opt/homebrew/bin", "/usr/bin", "/bin"] {
            assert!(std::env::split_paths(&path).any(|p| p == Path::new(dir)), "{dir}");
        }
    }
}
