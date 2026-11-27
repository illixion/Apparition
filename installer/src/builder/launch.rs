//! Launching the build after install through Apparition's pairing or Xcode's tunnel. A console
//! or JIT session has to outlive the build, so the launcher is detached and its output goes to
//! `build/device-console.log` and whose pid is in `build/device-console.pid`, as with
//! `build-and-sign`. With `--mcp`, the app's debug server is found and a session record written
//! for the Store's `apps` tools.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::json;

#[derive(Clone, Copy)]
pub struct Detached<'a> {
    pub bundle_id: &'a str,
    pub device: &'a super::TargetDevice,
    pub console: bool,
    pub jit: bool,
    /// Passed to the app's environment through the launcher's child environment prefix.
    pub environment: &'a [(&'a str, &'a str)],
    pub log: &'a Path,
    pub pidfile: &'a Path,
}

#[derive(Clone, Copy)]
pub enum Route {
    Apparition,
    Devicectl,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::Apparition => "Apparition",
            Self::Devicectl => "Xcode's tunnel (devicectl)",
        }
    }
}

fn child_environment(command: &mut Command, prefix: &str, pairs: &[(&str, &str)]) {
    for (key, _) in std::env::vars_os().filter(|(key, _)| key.to_string_lossy().starts_with(prefix)) {
        command.env_remove(key);
    }
    for (key, value) in pairs {
        command.env(format!("{prefix}{key}"), value);
    }
}

fn devicectl(device: &super::TargetDevice, bundle_id: &str, console: bool, environment: &[(&str, &str)]) -> Command {
    let mut command = Command::new("/usr/bin/xcrun");
    command.args(["devicectl", "device", "process", "launch", "--device", &device.udid]);
    if console || device.paired.is_some() {
        command.arg("--terminate-existing");
    }
    if console { command.arg("--console"); }
    else { command.args(["--timeout", "30"]); }
    command.arg(bundle_id);
    child_environment(&mut command, "DEVICECTL_CHILD_", environment);
    command
}

pub fn plain_devicectl(device: &super::TargetDevice, bundle_id: &str, environment: &[(&str, &str)]) -> Result<(), String> {
    let output = devicectl(device, bundle_id, false, environment).output()
        .map_err(|e| format!("couldn't start xcrun devicectl: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stderr);
    let text = if text.trim().is_empty() { String::from_utf8_lossy(&output.stdout) } else { text };
    Err(format!("Xcode's tunnel couldn't launch {bundle_id} on {}:\n{}", device.name, text.trim()))
}

pub fn devicectl_has_build(udid: &str, bundle_id: &str, build: &str) -> bool {
    let output = Command::new("/usr/bin/xcrun")
        .args(["devicectl", "device", "info", "apps", "--device", udid, "--bundle-id", bundle_id,
            "--timeout", "8", "--json-output", "-"])
        .output();
    let Ok(output) = output else { return false };
    output.status.success() && devicectl_build_matches(&output.stdout, bundle_id, build)
}

fn devicectl_build_matches(json: &[u8], bundle_id: &str, build: &str) -> bool {
    let Ok(response) = serde_json::from_slice::<serde_json::Value>(json) else { return false };
    response["result"]["apps"].as_array().is_some_and(|apps| apps.iter().any(|app|
        app["bundleIdentifier"].as_str() == Some(bundle_id) && app["bundleVersion"].as_str() == Some(build)
    ))
}

fn read(path: &Path) -> String {
    fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

fn tail(path: &Path, lines: usize) -> String {
    let text = read(path);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Stops the console or JIT session a previous build left running, if its pid is still one.
pub fn stop_previous(pidfile: &Path) {
    let Some(pid) = fs::read_to_string(pidfile).ok().and_then(|p| p.trim().parse::<i32>().ok()).filter(|p| *p > 0) else {
        let _ = fs::remove_file(pidfile);
        return;
    };
    let command = Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    // Only a pid that is still a console streamer: pids are reused.
    if command.contains("devicectl") || command.contains("apparition") {
        #[cfg(unix)]
        // SAFETY: a plain SIGTERM to one pid, checked above to be the previous console streamer.
        unsafe { libc::kill(pid, libc::SIGTERM); }
    }
    let _ = fs::remove_file(pidfile);
}

/// Starts the launcher detached and waits for its "Launched application" line. The console may
/// stay open indefinitely, so devicectl's overall command timeout is only used for plain launches.
/// Returns the streamer's pid.
pub fn detached(d: Detached, route: Route) -> Result<u32, String> {
    let (mut command, wait) = match route {
        Route::Apparition => {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut command = Command::new(exe);
            command.args(["launch", d.bundle_id, "--device", &d.device.udid]);
            if d.console { command.arg("--console"); }
            if d.jit { command.arg("--jit"); }
            child_environment(&mut command, "APPARITION_CHILD_", d.environment);
            (command, Duration::from_secs(45))
        }
        Route::Devicectl => (devicectl(d.device, d.bundle_id, true, d.environment), Duration::from_secs(30)),
    };
    // Each route has its own output, so a failed attempt can't affect the next one's result.
    let log = fs::File::create(d.log).map_err(|e| e.to_string())?;
    command.stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session, so the build's terminal closing doesn't end the console.
        // SAFETY: setsid is async-signal-safe and touches nothing of this process.
        unsafe { command.pre_exec(|| { libc::setsid(); Ok(()) }); }
    }
    let mut child = command.spawn().map_err(|e| format!("couldn't start {}: {e}", route.name()))?;
    let pid = child.id();
    if let Err(error) = fs::write(d.pidfile, format!("{pid}\n")) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("couldn't record the console process: {error}"));
    }
    let started = Instant::now();
    let failure = loop {
        let status = child.try_wait().map_err(|e| format!("couldn't check the launcher's status: {e}"));
        if read(d.log).lines().any(|line| line.starts_with("Launched application with ")) {
            return Ok(pid);
        }
        match status {
            Ok(Some(status)) => break format!("the launcher exited ({status})"),
            Err(error) => break error,
            Ok(None) if started.elapsed() >= wait => break format!("no launch confirmation in {} s", wait.as_secs()),
            Ok(None) => std::thread::sleep(Duration::from_millis(250)),
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(d.pidfile);
    let output = tail(d.log, 15);
    Err(format!(
        "Failed to launch {} on {} through {}: {failure}{}",
        d.bundle_id, d.device.name, route.name(),
        if output.trim().is_empty() { String::new() } else { format!("\n---- launch output ----\n{output}") },
    ))
}

/// The port of "listening on port N" in the console log, waiting up to `wait` for it.
pub fn debug_server_port(log: &Path, wait: Duration) -> Option<u16> {
    let started = Instant::now();
    loop {
        let text = read(log);
        if let Some(at) = text.find("listening on port ") {
            let digits: String = text[at + "listening on port ".len()..].chars().take_while(char::is_ascii_digit).collect();
            if let Ok(port) = digits.parse() {
                return Some(port);
            }
        }
        if started.elapsed() >= wait {
            return None;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Whether `GET /` answers 200 at `address:port` within 3 s.
pub fn answers(address: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut targets) = (address, port).to_socket_addrs() else { return false };
    let host = if address.contains(':') { format!("[{address}]") } else { address.to_string() };
    let request = format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    let deadline = Instant::now() + Duration::from_secs(3);
    let timeout = || deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(1));
    targets.any(|target| {
        let connect_timeout = timeout();
        if connect_timeout.is_zero() { return false; }
        let Ok(mut stream) = std::net::TcpStream::connect_timeout(&target, connect_timeout) else { return false };
        let io_timeout = Some(timeout());
        let mut head = [0u8; 12];
        stream.set_write_timeout(io_timeout).is_ok()
            && stream.set_read_timeout(io_timeout).is_ok()
            && stream.write_all(request.as_bytes()).is_ok()
            && stream.read_exact(&mut head).is_ok()
            && head.starts_with(b"HTTP/1.") && &head[9..12] == b"200"
    })
}

/// Device display names aren't always DNS names. DebugTrace's signed Bonjour advertisement
/// gives us the server's actual hostname and identifies the build by its command token.
pub async fn debug_address(device_name: &str, port: u16, token: &str) -> Option<String> {
    use apparition_service::debug::{Browse, Mdns, advertisement_tag};
    let address = device_name.to_lowercase();
    if answers(&address, port) {
        return Some(address);
    }
    let browser = std::sync::Arc::new(Mdns::default());
    for service in browser.browse().await {
        if service.port != port {
            continue;
        }
        if let (Some(nonce), Some(tag)) = (service.txt.get("n"), service.txt.get("t"))
            && &advertisement_tag(token, nonce) == tag && answers(&service.host, port)
        {
            return Some(service.host);
        }
    }
    None
}

pub fn url(address: &str, port: u16) -> String {
    if address.contains(':') { format!("http://[{address}]:{port}") } else { format!("http://{address}:{port}") }
}

pub struct Session<'a> {
    pub dir: &'a Path,
    pub bundle_id: &'a str,
    pub app_name: &'a str,
    pub device: &'a str,
    pub device_id: &'a str,
    pub url: &'a str,
    pub pid: u32,
    pub console_log: &'a Path,
    pub project_root: &'a Path,
    pub token: &'a str,
    pub key_id: Option<&'a str>,
    pub inject_dir: Option<&'a Path>,
    /// How it was built and launched, so `apparition inject` can rebuild and relaunch it the same way.
    pub platform: &'a str,
    pub jit: bool,
}

/// The launch record the Store's `apps` tools route by: `<dir>/<bundle id>@<device>.json`.
pub fn write_session(s: Session) -> Result<PathBuf, String> {
    let mut record = json!({
        "bundleId": s.bundle_id, "appName": s.app_name, "device": s.device, "deviceId": s.device_id,
        "url": s.url, "pid": s.pid, "consoleLog": s.console_log, "projectRoot": s.project_root,
        "token": s.token, "startedAt": super::utc_now().offset, "platform": s.platform, "jit": s.jit,
    });
    if let Some(key) = s.key_id { record["keyId"] = key.into(); }
    if let Some(dir) = s.inject_dir { record["injectDir"] = json!(dir); }
    fs::create_dir_all(s.dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    for dir in [s.dir.parent(), Some(s.dir)].into_iter().flatten() {
        let _ = fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    let path = s.dir.join(format!("{}@{}.json", s.bundle_id, s.device));
    let mut file = super::private_file(&path)?;
    file.write_all(serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?.as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_device() -> crate::builder::TargetDevice {
        crate::builder::TargetDevice {
            name: "Nowhere".into(), udid: "test-device-id".into(),
            platform: apparition_core::api::Platform::VisionOS, paired: None,
        }
    }

    #[test]
    fn xcode_console_launches_the_requested_app_with_child_environment() {
        let command = devicectl(&test_device(), "com.example.demo", true, &[("DEBUGTRACE_SERVER", "1")]);
        assert_eq!(command.get_args().collect::<Vec<_>>(), [
            "devicectl", "device", "process", "launch", "--device", "test-device-id",
            "--terminate-existing", "--console", "com.example.demo",
        ]);
        assert!(command.get_envs().any(|(key, value)| key == "DEVICECTL_CHILD_DEBUGTRACE_SERVER"
            && value.is_some_and(|value| value == "1")));
    }

    #[test]
    fn xcode_reuses_only_the_matching_bundle_and_build() {
        let apps = br#"{"result":{"apps":[{"bundleIdentifier":"com.example.demo","bundleVersion":"123"}]}}"#;
        assert!(devicectl_build_matches(apps, "com.example.demo", "123"));
        assert!(!devicectl_build_matches(apps, "com.example.other", "123"));
        assert!(!devicectl_build_matches(apps, "com.example.demo", "122"));
    }

    #[test]
    fn the_port_comes_from_the_server_s_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("console.log");
        fs::write(&log, "boot\n[DebugTrace] debug server listening on port 52311 (bonjour)\n").unwrap();
        assert_eq!(debug_server_port(&log, Duration::ZERO), Some(52311));
        fs::write(&log, "nothing yet\n").unwrap();
        assert_eq!(debug_server_port(&log, Duration::ZERO), None);
    }

    #[test]
    fn a_server_that_answers_200_is_found() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for reply in ["HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n", "HTTP/1.1 401 Unauthorized\r\n\r\n"] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0u8; 256];
                let _ = stream.read(&mut buffer);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        assert!(answers("127.0.0.1", port));
        assert!(!answers("127.0.0.1", port));
        assert_eq!(url("fe80::1", 80), "http://[fe80::1]:80");
    }

    #[test]
    fn the_session_record_is_private_and_names_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("debugtrace/sessions");
        let path = write_session(Session {
            dir: &sessions, bundle_id: "com.example.demo", app_name: "Demo", device: "AVP", device_id: "00008112-X",
            url: "http://avp:5000", pid: 42, console_log: Path::new("/p/build/device-console.log"), project_root: Path::new("/p"),
            token: "t", key_id: Some("k"), inject_dir: None, platform: "visionOS", jit: false,
        }).unwrap();
        assert!(path.ends_with("com.example.demo@AVP.json"));
        let record: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(record["pid"], 42);
        assert_eq!(record["keyId"], "k");
        assert!(record.get("injectDir").is_none());
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions()) & 0o777, 0o600);
    }

    #[test]
    fn a_launch_that_never_says_launched_ignores_the_previous_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let (log, pidfile) = (dir.path().join("console.log"), dir.path().join("console.pid"));
        fs::write(&log, "Launched application with pid 123\n").unwrap();
        // The test binary is current_exe here: given `launch`, it runs no test and exits.
        let error = detached(Detached {
            bundle_id: "com.example.demo", device: &test_device(), console: true, jit: false,
            environment: &[("A", "1")], log: &log, pidfile: &pidfile,
        }, Route::Apparition).unwrap_err();
        assert!(error.contains("Failed to launch com.example.demo on Nowhere"), "{error}");
        assert!(!pidfile.exists());
    }
}
