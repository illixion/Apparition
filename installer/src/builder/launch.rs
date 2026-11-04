//! Launching the build on the device after install (`--run`, `--log`, `--mcp`, `--jit`), through
//! Apparition's own pairing (`apparition launch`), never `devicectl`. A console or a JIT session
//! has to outlive the build, so it is a detached `apparition launch` whose output goes to
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

pub struct Detached<'a> {
    pub bundle_id: &'a str,
    pub device: &'a str,
    pub console: bool,
    pub jit: bool,
    /// Passed to the app's environment, as `APPARITION_CHILD_<KEY>` (never in argv).
    pub environment: &'a [(&'a str, &'a str)],
    pub log: &'a Path,
    pub pidfile: &'a Path,
    pub wait: Duration,
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

/// Starts `apparition launch` detached and waits for its "Launched application" line. Alive but
/// quiet is not a launch: the client sits connecting while the device's tunnel is unreachable.
/// Returns the streamer's pid.
pub fn detached(d: Detached) -> Result<u32, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = fs::OpenOptions::new().create(true).append(true).open(d.log).map_err(|e| e.to_string())?;
    let mut command = Command::new(exe);
    command.arg("launch").arg(d.bundle_id).arg("--device").arg(d.device);
    if d.console { command.arg("--console"); }
    if d.jit { command.arg("--jit"); }
    for (key, _) in std::env::vars().filter(|(k, _)| k.starts_with("APPARITION_CHILD_")) {
        command.env_remove(key);
    }
    for (key, value) in d.environment {
        command.env(format!("APPARITION_CHILD_{key}"), value);
    }
    command.stdin(Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session, so the build's terminal closing doesn't end the console.
        // SAFETY: setsid is async-signal-safe and touches nothing of this process.
        unsafe { command.pre_exec(|| { libc::setsid(); Ok(()) }); }
    }
    let mut child = command.spawn().map_err(|e| format!("couldn't start apparition launch: {e}"))?;
    let pid = child.id();
    fs::write(d.pidfile, format!("{pid}\n")).map_err(|e| e.to_string())?;
    let started = Instant::now();
    while started.elapsed() < d.wait {
        if read(d.log).to_lowercase().contains("launched application") {
            return Ok(pid);
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(d.pidfile);
    let output = tail(d.log, 15);
    Err(format!(
        "Failed to launch {} on {} through Apparition: {}.\n  The app is installed. A headset has to be worn (or the device unlocked) to launch; then build again with --run.{}",
        d.bundle_id, d.device,
        if output.trim().is_empty() { format!("no answer in {} s", d.wait.as_secs()) } else { "it stopped".to_string() },
        if output.trim().is_empty() { String::new() } else { format!("\n---- launch output ----\n{output}") }
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
    let Some(target) = targets.next() else { return false };
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&target, Duration::from_secs(3)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let host = if address.contains(':') { format!("[{address}]") } else { address.to_string() };
    if stream.write_all(format!("GET / HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n").as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.") && &head[9..12] == b"200"
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
}

/// The launch record the Store's `apps` tools route by: `<dir>/<bundle id>@<device>.json`.
pub fn write_session(s: Session) -> Result<PathBuf, String> {
    let mut record = json!({
        "bundleId": s.bundle_id, "appName": s.app_name, "device": s.device, "deviceId": s.device_id,
        "url": s.url, "pid": s.pid, "consoleLog": s.console_log, "projectRoot": s.project_root,
        "token": s.token, "startedAt": super::utc_now().offset,
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
            token: "t", key_id: Some("k"), inject_dir: None,
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
    fn a_launch_that_never_says_launched_fails_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let (log, pidfile) = (dir.path().join("console.log"), dir.path().join("console.pid"));
        // The test binary is current_exe here: given `launch`, it runs no test and exits.
        let error = detached(Detached {
            bundle_id: "com.example.demo", device: "Nowhere", console: true, jit: false, environment: &[("A", "1")],
            log: &log, pidfile: &pidfile, wait: Duration::from_secs(20),
        }).unwrap_err();
        assert!(error.contains("Failed to launch com.example.demo on Nowhere"), "{error}");
        assert!(!pidfile.exists());
    }
}
