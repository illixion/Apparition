//! The device tools beside JIT: process control, whole-device location simulation, the device
//! log, live Instruments monitors and OS-provided connection conditions.
//!
//! Each goes through the tunnel like everything else, from Apparition on the device (through
//! LocalDevVPN or the companion) or from a computer paired with it:
//! - the process list and signals are CoreDevice's app service, which JIT already uses;
//! - the log is os_trace_relay's RSD shim, the stream Console.app reads;
//! - the location is Instruments' LocationSimulation, through its service hub. That one is on
//!   the device only while its developer disk image is mounted (Xcode mounts it, and a restart
//!   unmounts it), and the location holds only while the connection that set it stays open: a
//!   location session runs until it's stopped, and the device goes back to its real location
//!   when it ends.
//!
//! Long-running sessions are registered by a caller-picked key, so a stop or location move
//! reaches the right one from another thread.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use idevice::{RsdService, rsd::RsdHandshake, tcp::handle::AdapterHandle};
use rootcause::prelude::*;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Notify, watch};

/// The service hub Instruments' channels (LocationSimulation among them) open on.
const SERVICE_HUB: &str = "com.apple.instruments.dtservicehub";

/// What a running process is, as far as its executable's path tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProcessKind {
    /// An app someone installed (under /var/containers/Bundle/Application).
    App,
    /// An app that comes with the system (/Applications, or a system app's container).
    SystemApp,
    /// An app extension (a widget, a keyboard, a share extension), of either kind of app.
    Extension,
    /// Everything else: daemons and agents.
    System,
}

/// One running process, with the app it belongs to when it's one.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    pub pid: u32,
    /// The executable's file name.
    pub name: String,
    /// Its path on the device, with `/private` dropped.
    pub path: String,
    pub kind: ProcessKind,
    /// The installed app (or the app holding the extension) it runs from, when known.
    pub bundle_id: Option<String>,
    /// That app's name as the Home Screen shows it.
    pub app_name: Option<String>,
}

/// An installed bundle, as installation_proxy's browse lists it.
struct Bundle {
    /// `bundle_dir` of its path: what its processes' executable paths start with.
    dir: String,
    bundle_id: String,
    name: String,
}

/// The device's running processes, apps first (by name), then the rest (by name).
pub async fn processes(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Vec<Process>, Report> {
    let tokens = process_tokens(adapter, handshake).await?;
    // Names are a nicety: a device that won't browse its apps still lists its processes.
    let bundles = installed_bundles(adapter, handshake).await.unwrap_or_else(|error| {
        tracing::warn!("couldn't browse the installed apps for process names: {}", crate::errors::describe(&error));
        Vec::new()
    });
    let mut list: Vec<Process> = tokens
        .iter()
        .filter_map(|token| {
            let url = token.executable_url.as_ref()?.relative.clone();
            Some(describe_process(token.pid, &url, &bundles))
        })
        .collect();
    sort_processes(&mut list);
    Ok(list)
}

/// A fresh appservice connection retries a transient closed device socket once.
/// A process-list read is idempotent; reusing the failed service is not safe.
pub(crate) async fn process_tokens(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake)
    -> Result<Vec<idevice::core_device::ProcessToken>, Report> {
    for attempt in 0..2 {
        let result: Result<_, Report> = async {
            let mut service = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake)
                .await.context("appservice connect failed")?;
            Ok(service.list_processes().await.context("Couldn't list the device's running processes")?)
        }.await;
        if result.is_ok() || attempt == 1 { return result; }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    unreachable!()
}

fn sort_processes(list: &mut [Process]) {
    let rank = |kind: ProcessKind| match kind {
        ProcessKind::App => 0,
        ProcessKind::SystemApp => 1,
        ProcessKind::Extension => 2,
        ProcessKind::System => 3,
    };
    list.sort_by(|a, b| {
        let label = |p: &Process| p.app_name.clone().unwrap_or_else(|| p.name.clone()).to_lowercase();
        rank(a.kind).cmp(&rank(b.kind)).then_with(|| label(a).cmp(&label(b))).then(a.pid.cmp(&b.pid))
    });
}

/// Every installed bundle (user and system apps) with where it lives and what it's called.
async fn installed_bundles(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake) -> Result<Vec<Bundle>, Report> {
    let mut installer = idevice::installation_proxy::InstallationProxyClient::connect_rsd(adapter, handshake)
        .await
        .context("installation_proxy connect failed")?;
    let options = plist::Value::Dictionary(plist::Dictionary::from_iter([
        ("ApplicationType".to_string(), plist::Value::String("Any".into())),
        (
            "ReturnAttributes".to_string(),
            plist::Value::Array(
                ["CFBundleIdentifier", "Path", "CFBundleDisplayName", "CFBundleName"].map(|key| plist::Value::String(key.into())).to_vec(),
            ),
        ),
    ]));
    let listed = installer.browse(Some(options)).await.context("Can't browse the installed apps")?;
    Ok(listed
        .iter()
        .filter_map(|entry| {
            let entry = entry.as_dictionary()?;
            let text = |key: &str| entry.get(key).and_then(plist::Value::as_string).map(str::to_string);
            let bundle_id = text("CFBundleIdentifier")?;
            Some(Bundle {
                dir: bundle_dir(&text("Path")?),
                name: text("CFBundleDisplayName").or_else(|| text("CFBundleName")).unwrap_or_else(|| bundle_id.clone()),
                bundle_id,
            })
        })
        .collect())
}

/// An installed bundle's path as it appears at the start of its processes' executable paths
/// (see `jit::bundle_dir`): `/private` dropped, a trailing `/` so `Foo.app` misses `Foo.appex`.
fn bundle_dir(path: &str) -> String {
    format!("{}/", path.trim_end_matches('/').trim_start_matches("/private"))
}

/// `pid`'s process from its executable URL (`file:///private/var/…/Foo.app/Foo`).
fn describe_process(pid: u32, url: &str, bundles: &[Bundle]) -> Process {
    let path = url.strip_prefix("file://").unwrap_or(url);
    let path = percent_decode(path);
    let path = path.trim_start_matches("/private").to_string();
    let name = path.rsplit('/').find(|part| !part.is_empty()).unwrap_or(&path).to_string();
    // The longest match: an extension's executable sits inside its app's directory too.
    let bundle = bundles.iter().filter(|b| path.starts_with(&b.dir)).max_by_key(|b| b.dir.len());
    let extension = path.contains(".appex/");
    let kind = if extension {
        ProcessKind::Extension
    } else if path.starts_with("/var/containers/Bundle/Application/") {
        ProcessKind::App
    } else if path.starts_with("/Applications/") || bundle.is_some() {
        ProcessKind::SystemApp
    } else {
        ProcessKind::System
    };
    Process {
        pid,
        name,
        path,
        kind,
        bundle_id: bundle.map(|b| b.bundle_id.clone()),
        app_name: bundle.map(|b| b.name.clone()),
    }
}

/// `%20` and the like, as CoreDevice escapes spaces in executable URLs. Anything that doesn't
/// decode is kept as it was.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escape = bytes.get(index + 1..index + 3).and_then(|hex| std::str::from_utf8(hex).ok());
        if bytes[index] == b'%'
            && let Some(byte) = escape.and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

/// Sends `signal` to `pid` (SIGTERM 15 asks a process to end, SIGKILL 9 ends it). The device
/// refuses what it doesn't let a developer end.
pub async fn signal(adapter: &mut AdapterHandle, handshake: &mut RsdHandshake, pid: u32, signal: u32) -> Result<(), Report> {
    if pid <= 1 {
        bail!("Process {pid} can't be ended");
    }
    let mut service = idevice::core_device::AppServiceClient::connect_rsd(adapter, handshake)
        .await
        .context("appservice connect failed")?;
    service.send_signal(pid, signal).await.context(format!("Couldn't end process {pid}"))?;
    Ok(())
}

// MARK: Sessions

/// A running session's stop, and for a location session, where to move it.
struct Session {
    stop: Arc<Notify>,
    moves: Option<watch::Sender<Coordinate>>,
}

static SESSIONS: Mutex<Option<HashMap<String, Session>>> = Mutex::new(None);

/// Holds a session's place in `SESSIONS` until it ends.
struct Registration(String);

impl Registration {
    fn new(key: &str, session: Session) -> Result<Self, Report> {
        let mut sessions = SESSIONS.lock().unwrap();
        let sessions = sessions.get_or_insert_with(HashMap::new);
        if sessions.contains_key(key) {
            bail!("A session named {key} is already running");
        }
        sessions.insert(key.to_string(), session);
        Ok(Registration(key.to_string()))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(sessions) = SESSIONS.lock().unwrap().as_mut() {
            sessions.remove(&self.0);
        }
    }
}

/// Ends the session `key` (clearing a location or condition, or stopping a stream). False when
/// none runs under that key.
pub fn stop(key: &str) -> bool {
    let sessions = SESSIONS.lock().unwrap();
    match sessions.as_ref().and_then(|sessions| sessions.get(key)) {
        Some(session) => {
            // notify_one keeps the permit when the session isn't waiting at this moment.
            session.stop.notify_one();
            true
        }
        None => false,
    }
}

/// Moves the running location session `key` to `to`. False when none runs under that key.
pub fn move_location(key: &str, to: Coordinate) -> Result<bool, Report> {
    to.check()?;
    let sessions = SESSIONS.lock().unwrap();
    Ok(match sessions.as_ref().and_then(|sessions| sessions.get(key)).and_then(|s| s.moves.as_ref()) {
        Some(moves) => moves.send(to).is_ok(),
        None => false,
    })
}

// MARK: Location

/// A place on Earth, in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
}

impl Coordinate {
    pub fn check(&self) -> Result<(), Report> {
        if !self.latitude.is_finite() || !(-90.0..=90.0).contains(&self.latitude) {
            bail!("A latitude is between -90 and 90 degrees, not {}", self.latitude);
        }
        if !self.longitude.is_finite() || !(-180.0..=180.0).contains(&self.longitude) {
            bail!("A longitude is between -180 and 180 degrees, not {}", self.longitude);
        }
        Ok(())
    }
}

/// How often a running location is set again, as idevice's own tool does: it keeps the
/// simulation in force, shows the connection still holds (a dropped one ends the session rather
/// than leaving it looking active), and costs one small message.
const LOCATION_REFRESH: Duration = Duration::from_secs(5);

/// Why the device has no LocationSimulation to offer.
fn no_service_hub() -> Report {
    report!(
        "This device isn't offering location simulation right now. It comes with the developer disk image, which a \
         device mounts when Xcode (or a tool like StikDebug) prepares it for development, and unmounts when it \
         restarts. Connect it to Xcode once, then try again."
    )
    .into()
}

/// Simulates `start` for the whole device until the session `key` is stopped (or its
/// connection drops), following `move_location` meanwhile; then puts the real location back.
/// `on_set` sees each coordinate once the device has taken it. Returns
/// `{ended: "stopped"|"lost", seconds, moves}`.
pub async fn simulate_location(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    key: &str,
    start: Coordinate,
    on_set: &mut (dyn FnMut(Coordinate) + Send),
) -> Result<Value, Report> {
    start.check()?;
    if !handshake.services.contains_key(SERVICE_HUB) {
        return Err(no_service_hub());
    }
    let stop = Arc::new(Notify::new());
    let (moves, mut moved) = watch::channel(start);
    let _registration = Registration::new(key, Session { stop: stop.clone(), moves: Some(moves) })?;

    let mut hub = idevice::dvt::remote_server::RemoteServerClient::connect_rsd(adapter, handshake)
        .await
        .map_err(|error| match error {
            idevice::IdeviceError::ServiceNotFound => no_service_hub(),
            error => report!("Couldn't reach Instruments' service hub: {error}").into(),
        })?;
    hub.read_message(0).await.context("The service hub didn't greet")?;
    let mut location = idevice::dvt::location_simulation::LocationSimulationClient::new(&mut hub)
        .await
        .context("The device refused location simulation")?;
    let started = Instant::now();
    let mut current = start;
    location.set(current.latitude, current.longitude).await.context("The device didn't take the location")?;
    on_set(current);
    let mut count = 0u32;

    let ended = loop {
        tokio::select! {
            biased;
            _ = stop.notified() => break "stopped",
            changed = moved.changed() => {
                if changed.is_err() {
                    break "stopped";
                }
                current = *moved.borrow_and_update();
                if location.set(current.latitude, current.longitude).await.is_err() {
                    break "lost";
                }
                count += 1;
                on_set(current);
            }
            _ = tokio::time::sleep(LOCATION_REFRESH) => {
                if location.set(current.latitude, current.longitude).await.is_err() {
                    break "lost";
                }
            }
        }
    };
    if ended == "stopped" {
        // Best effort: closing the connection puts the real location back anyway.
        let _ = tokio::time::timeout(Duration::from_secs(3), location.clear()).await;
    }
    Ok(json!({ "ended": ended, "seconds": started.elapsed().as_secs(), "moves": count }))
}

// MARK: Log

/// One line of the device's log, as `stream_log` hands it on.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    /// Milliseconds since 1970, UTC.
    pub time: i64,
    pub pid: u32,
    /// The process's executable name.
    pub process: String,
    /// The library or executable that logged it.
    pub sender: String,
    /// notice, info, debug, error or fault.
    pub level: &'static str,
    pub subsystem: Option<String>,
    pub category: Option<String>,
    /// As the device sends it: values a process logs as private arrive as `<private>`.
    pub message: String,
}

/// The least important lines a log stream passes on, as `log stream --level` chooses: `Notice`
/// ("default") passes notice, error and fault, `Info` adds info, `Debug` everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Notice,
    Error,
    Fault,
}

impl Level {
    fn of(level: idevice::os_trace_relay::LogLevel) -> Self {
        use idevice::os_trace_relay::LogLevel;
        match level {
            LogLevel::Debug => Level::Debug,
            LogLevel::Info => Level::Info,
            LogLevel::Notice => Level::Notice,
            LogLevel::Error => Level::Error,
            LogLevel::Fault => Level::Fault,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Notice => "notice",
            Level::Error => "error",
            Level::Fault => "fault",
        }
    }

    /// From `name`, or the C ABI's number (0 debug … 4 fault); "default" is notice.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "debug" => Level::Debug,
            "info" => Level::Info,
            "notice" | "default" => Level::Notice,
            "error" => Level::Error,
            "fault" => Level::Fault,
            _ => return None,
        })
    }

    fn from_number(number: u32) -> Self {
        match number {
            0 => Level::Debug,
            1 => Level::Info,
            2 => Level::Notice,
            3 => Level::Error,
            _ => Level::Fault,
        }
    }
}

fn last_component(path: &str) -> String {
    path.rsplit('/').find(|part| !part.is_empty()).unwrap_or(path).to_string()
}

fn entry(log: idevice::os_trace_relay::OsTraceLog) -> LogEntry {
    LogEntry {
        time: log.timestamp.and_utc().timestamp_millis(),
        pid: log.pid,
        process: last_component(&log.filename),
        sender: last_component(&log.image_name),
        level: Level::of(log.level).name(),
        subsystem: log.label.as_ref().map(|label| label.subsystem.clone()).filter(|s| !s.is_empty()),
        category: log.label.map(|label| label.category).filter(|c| !c.is_empty()),
        message: log.message.trim_end().to_string(),
    }
}

/// Lines held back before they're handed on, and for how long at most: a busy device logs
/// thousands of lines a second, and one call per line would be most of the work.
const BATCH_LINES: usize = 200;
const BATCH_WAIT: Duration = Duration::from_millis(250);

/// Streams the device's log, only `pid`'s when given and only lines at `minimum` or above, to
/// `on_batch` until the session `key` is stopped or the device closes the stream. A whole
/// device logs thousands of debug lines a second, which `minimum` drops here rather than at
/// the far end of the C ABI. Returns `{ended: "stopped"|"lost", lines}` (lines passed on).
pub async fn stream_log(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    key: &str,
    pid: Option<u32>,
    minimum: Level,
    on_batch: &mut (dyn FnMut(Vec<LogEntry>) + Send),
) -> Result<Value, Report> {
    let stop = Arc::new(Notify::new());
    let _registration = Registration::new(key, Session { stop: stop.clone(), moves: None })?;
    let relay = idevice::os_trace_relay::OsTraceRelayClient::connect_rsd(adapter, handshake)
        .await
        .context("The device's log isn't available")?;
    let mut receiver = relay.start_trace(pid).await.context("The device didn't start its log stream")?;

    let mut batch = Vec::new();
    let mut since = Instant::now();
    let mut lines = 0u64;
    let ended = loop {
        let wait = BATCH_WAIT.saturating_sub(since.elapsed());
        tokio::select! {
            biased;
            _ = stop.notified() => break "stopped",
            next = receiver.next() => match next {
                Ok(log) => {
                    if Level::of(log.level) >= minimum {
                        lines += 1;
                        batch.push(entry(log));
                    }
                }
                Err(error) => {
                    tracing::info!("log stream ended: {error}");
                    break "lost";
                }
            },
            _ = tokio::time::sleep(wait), if !batch.is_empty() => {}
        }
        if batch.len() >= BATCH_LINES || (!batch.is_empty() && since.elapsed() >= BATCH_WAIT) {
            on_batch(std::mem::take(&mut batch));
            since = Instant::now();
        } else if batch.is_empty() {
            since = Instant::now();
        }
    };
    if !batch.is_empty() {
        on_batch(batch);
    }
    Ok(json!({ "ended": ended, "lines": lines }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundles() -> Vec<Bundle> {
        vec![
            Bundle { dir: bundle_dir("/private/var/containers/Bundle/Application/AAA/UTM.app"), bundle_id: "com.utmapp.UTM".into(), name: "UTM".into() },
            Bundle { dir: bundle_dir("/Applications/MobileSafari.app"), bundle_id: "com.apple.mobilesafari".into(), name: "Safari".into() },
            Bundle { dir: bundle_dir("/private/var/containers/Bundle/Application/BBB/My Notes.app"), bundle_id: "com.example.notes".into(), name: "My Notes".into() },
        ]
    }

    #[test]
    fn processes_are_named_after_their_app() {
        let utm = describe_process(400, "file:///private/var/containers/Bundle/Application/AAA/UTM.app/UTM", &bundles());
        assert_eq!(utm.kind, ProcessKind::App);
        assert_eq!(utm.bundle_id.as_deref(), Some("com.utmapp.UTM"));
        assert_eq!(utm.app_name.as_deref(), Some("UTM"));
        assert_eq!(utm.path, "/var/containers/Bundle/Application/AAA/UTM.app/UTM");

        let safari = describe_process(300, "file:///Applications/MobileSafari.app/MobileSafari", &bundles());
        assert_eq!(safari.kind, ProcessKind::SystemApp);
        assert_eq!(safari.app_name.as_deref(), Some("Safari"));

        let widget = describe_process(401, "file:///private/var/containers/Bundle/Application/AAA/UTM.app/PlugIns/Widget.appex/Widget", &bundles());
        assert_eq!(widget.kind, ProcessKind::Extension);
        assert_eq!(widget.bundle_id.as_deref(), Some("com.utmapp.UTM"), "an extension belongs to its app");

        let daemon = describe_process(90, "file:///usr/libexec/backboardd", &bundles());
        assert_eq!(daemon.kind, ProcessKind::System);
        assert_eq!(daemon.name, "backboardd");
        assert_eq!(daemon.bundle_id, None);

        let spaced = describe_process(500, "file:///private/var/containers/Bundle/Application/BBB/My%20Notes.app/My%20Notes", &bundles());
        assert_eq!(spaced.name, "My Notes");
        assert_eq!(spaced.bundle_id.as_deref(), Some("com.example.notes"));
    }

    #[test]
    fn a_similar_name_is_not_its_app() {
        let other = describe_process(1, "file:///private/var/containers/Bundle/Application/AAA/UTM.app2/UTM", &bundles());
        assert_eq!(other.bundle_id, None);
    }

    #[test]
    fn apps_come_first() {
        let mut list = vec![
            describe_process(90, "file:///usr/libexec/backboardd", &bundles()),
            describe_process(300, "file:///Applications/MobileSafari.app/MobileSafari", &bundles()),
            describe_process(400, "file:///private/var/containers/Bundle/Application/AAA/UTM.app/UTM", &bundles()),
        ];
        sort_processes(&mut list);
        assert_eq!(list.iter().map(|p| p.pid).collect::<Vec<_>>(), [400, 300, 90]);
    }

    #[test]
    fn percent_escapes_decode_and_broken_ones_stay() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn levels_order_as_log_stream_does() {
        assert!(Level::Debug < Level::Info && Level::Info < Level::Notice && Level::Notice < Level::Error && Level::Error < Level::Fault);
        assert_eq!(Level::parse("default"), Some(Level::Notice));
        assert_eq!(Level::parse("verbose"), None);
        assert_eq!(Level::from_number(1), Level::Info);
        assert_eq!(Level::from_number(99), Level::Fault);
    }

    #[test]
    fn coordinates_are_checked() {
        assert!(Coordinate { latitude: 59.437, longitude: 24.7536 }.check().is_ok());
        assert!(Coordinate { latitude: -90.0, longitude: 180.0 }.check().is_ok());
        assert!(Coordinate { latitude: 91.0, longitude: 0.0 }.check().is_err());
        assert!(Coordinate { latitude: 0.0, longitude: -181.0 }.check().is_err());
        assert!(Coordinate { latitude: f64::NAN, longitude: 0.0 }.check().is_err());
    }

    #[test]
    fn a_session_key_is_taken_once_and_freed_when_it_ends() {
        let key = "test-session";
        let first = Registration::new(key, Session { stop: Arc::new(Notify::new()), moves: None }).unwrap();
        assert!(Registration::new(key, Session { stop: Arc::new(Notify::new()), moves: None }).is_err());
        assert!(stop(key));
        assert!(!move_location(key, Coordinate { latitude: 0.0, longitude: 0.0 }).unwrap(), "a log session doesn't move");
        drop(first);
        assert!(!stop(key));
    }

    #[test]
    fn a_location_session_moves() {
        let key = "test-location";
        let (moves, moved) = watch::channel(Coordinate { latitude: 0.0, longitude: 0.0 });
        let _session = Registration::new(key, Session { stop: Arc::new(Notify::new()), moves: Some(moves) }).unwrap();
        assert!(move_location(key, Coordinate { latitude: 1.0, longitude: 2.0 }).unwrap());
        assert_eq!(*moved.borrow(), Coordinate { latitude: 1.0, longitude: 2.0 });
        assert!(move_location(key, Coordinate { latitude: 100.0, longitude: 0.0 }).is_err());
    }
}

// MARK: Power-user tools

/// Converts the plist values emitted by Instruments into JSON so the C bridge can stream
/// samples without making assumptions about which fields a particular OS build returns.
fn plist_json(value: &plist::Value) -> Value {
    match value {
        plist::Value::Boolean(value) => json!(value),
        plist::Value::Integer(value) => value
            .as_signed()
            .map_or_else(|| json!(value.to_string()), |n| json!(n)),
        plist::Value::Real(value) => json!(value),
        plist::Value::String(value) => json!(value),
        plist::Value::Array(values) => Value::Array(values.iter().map(plist_json).collect()),
        plist::Value::Dictionary(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), plist_json(value)))
                .collect(),
        ),
        plist::Value::Data(_) => json!("<data>"),
        plist::Value::Date(value) => json!(value.to_xml_format()),
        _ => json!("<unsupported>"),
    }
}

/// Streams process CPU, footprint and system CPU samples until stopped or the tunnel drops.
/// The actual fields are kept as supplied by Instruments: their names vary across OS releases.
pub async fn stream_activity(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    key: &str,
    on_sample: &mut (dyn FnMut(Value) + Send),
) -> Result<Value, Report> {
    if !handshake.services.contains_key(SERVICE_HUB) {
        bail!(
            "This device doesn't expose Instruments monitoring right now. Connect it to Xcode to prepare it for development, then try again."
        );
    }
    let stop = Arc::new(Notify::new());
    let _registration = Registration::new(
        key,
        Session {
            stop: stop.clone(),
            moves: None,
        },
    )?;
    let mut hub = idevice::dvt::remote_server::RemoteServerClient::connect_rsd(adapter, handshake)
        .await
        .context("Couldn't reach Instruments' service hub")?;
    hub.read_message(0)
        .await
        .context("Instruments' service hub didn't greet")?;

    let mut info = idevice::dvt::device_info::DeviceInfoClient::new(&mut hub)
        .await
        .context("Couldn't query the device's monitoring fields")?;
    let available = info
        .sysmon_process_attributes()
        .await
        .context("The device didn't list its process monitoring fields")?;
    let attributes = available
        .into_iter()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.contains("cpu") || lower.contains("footprint") || lower.contains("resident")
        })
        .collect();
    drop(info);

    let mut tap = idevice::dvt::sysmontap::SysmontapClient::new(&mut hub)
        .await
        .context("This device doesn't offer the Instruments activity monitor")?;
    tap.set_config(&idevice::dvt::sysmontap::SysmontapConfig {
        interval_ms: 1000,
        process_attributes: attributes,
        system_attributes: Vec::new(),
    })
    .await
    .context("Couldn't configure activity sampling")?;
    tap.start()
        .await
        .context("Couldn't start activity sampling")?;
    on_sample(json!({ "type": "started" }));

    let samples: Result<&'static str, Report> = loop {
        tokio::select! {
            biased;
            _ = stop.notified() => break Ok("stopped"),
            sample = tap.next_sample() => match sample {
                Ok(sample) => {
                    let processes = sample.processes.as_ref()
                        .map(|dict| plist_json(&plist::Value::Dictionary(dict.clone())))
                        .unwrap_or(Value::Null);
                    let system_cpu = sample.system_cpu_usage.as_ref()
                        .map(|dict| plist_json(&plist::Value::Dictionary(dict.clone())))
                        .unwrap_or(Value::Null);
                    on_sample(json!({
                        "processes": processes,
                        "systemCPUUsage": system_cpu,
                    }));
                }
                Err(error) => break Err(report!("The activity monitor ended: {error}").into()),
            }
        }
    };
    let _ = tap.stop().await;
    let ended = samples?;
    Ok(json!({ "ended": ended }))
}

/// Available network and thermal profiles exposed by this OS build.
pub async fn condition_profiles(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
) -> Result<Value, Report> {
    if !handshake.services.contains_key(SERVICE_HUB) {
        bail!(
            "This device doesn't expose Instruments conditions right now. Connect it to Xcode to prepare it for development, then try again."
        );
    }
    let mut hub = idevice::dvt::remote_server::RemoteServerClient::connect_rsd(adapter, handshake)
        .await
        .context("Couldn't reach Instruments' service hub")?;
    hub.read_message(0)
        .await
        .context("Instruments' service hub didn't greet")?;
    let mut inducer = idevice::dvt::condition_inducer::ConditionInducerClient::new(&mut hub)
        .await
        .context("This device doesn't offer connection condition controls")?;
    let groups = inducer
        .available_conditions()
        .await
        .context("Couldn't read available conditions")?;
    Ok(json!({ "groups": groups.into_iter().map(|group| json!({
        "identifier": group.identifier,
        "profiles": group.profiles.into_iter().map(|profile| json!({
            "identifier": profile.identifier,
            "description": profile.description,
        })).collect::<Vec<_>>(),
    })).collect::<Vec<_>>() }))
}

/// Applies one condition while keeping its DVT connection alive. The periodic profile query
/// notices a dropped connection; either path disables the condition before returning.
pub async fn condition_session(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    key: &str,
    condition: &str,
    profile: &str,
    on_enabled: &mut (dyn FnMut() + Send),
) -> Result<Value, Report> {
    if !handshake.services.contains_key(SERVICE_HUB) {
        bail!(
            "This device doesn't expose Instruments conditions right now. Connect it to Xcode to prepare it for development, then try again."
        );
    }
    let stop = Arc::new(Notify::new());
    let _registration = Registration::new(
        key,
        Session {
            stop: stop.clone(),
            moves: None,
        },
    )?;
    let mut hub = idevice::dvt::remote_server::RemoteServerClient::connect_rsd(adapter, handshake)
        .await
        .context("Couldn't reach Instruments' service hub")?;
    hub.read_message(0)
        .await
        .context("Instruments' service hub didn't greet")?;
    let mut inducer = idevice::dvt::condition_inducer::ConditionInducerClient::new(&mut hub)
        .await
        .context("This device doesn't offer connection condition controls")?;
    inducer
        .enable_condition(condition, profile)
        .await
        .context("The device refused this condition profile")?;
    on_enabled();

    let ended = loop {
        tokio::select! {
            biased;
            _ = stop.notified() => break "stopped",
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                if let Err(error) = inducer.available_conditions().await {
                    tracing::info!("condition session ended after a device connection error: {error}");
                    break "lost";
                }
            }
        }
    };
    let cleared = inducer.disable_condition().await.is_ok();
    Ok(json!({ "ended": ended, "cleared": cleared }))
}

/// Streams per-process connections and cumulative traffic/latency counters.
pub async fn stream_network(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    key: &str,
    on_event: &mut (dyn FnMut(Value) + Send),
) -> Result<Value, Report> {
    if !handshake.services.contains_key(SERVICE_HUB) {
        bail!(
            "This device doesn't expose Instruments network monitoring right now. Connect it to Xcode to prepare it for development, then try again."
        );
    }
    let stop = Arc::new(Notify::new());
    let _registration = Registration::new(
        key,
        Session {
            stop: stop.clone(),
            moves: None,
        },
    )?;
    let mut hub = idevice::dvt::remote_server::RemoteServerClient::connect_rsd(adapter, handshake)
        .await
        .context("Couldn't reach Instruments' service hub")?;
    hub.read_message(0)
        .await
        .context("Instruments' service hub didn't greet")?;
    let mut monitor = idevice::dvt::network_monitor::NetworkMonitorClient::new(&mut hub)
        .await
        .context("This device doesn't offer the Instruments network monitor")?;
    monitor
        .start_monitoring()
        .await
        .context("Couldn't start network monitoring")?;
    on_event(json!({ "type": "started" }));

    let ended = loop {
        tokio::select! {
            biased;
            _ = stop.notified() => break "stopped",
            event = monitor.next_event() => match event {
                Ok(idevice::dvt::network_monitor::NetworkEvent::InterfaceDetection(event)) => on_event(json!({
                    "type": "interface", "index": event.interface_index, "name": event.name,
                })),
                Ok(idevice::dvt::network_monitor::NetworkEvent::ConnectionDetection(event)) => on_event(json!({
                    "type": "connection", "pid": event.pid, "serial": event.serial_number,
                    "interface": event.interface_index, "kind": event.kind,
                    "local": event.local_address.map(|address| json!({"address": address.addr, "port": address.port})),
                    "remote": event.remote_address.map(|address| json!({"address": address.addr, "port": address.port})),
                })),
                Ok(idevice::dvt::network_monitor::NetworkEvent::ConnectionUpdate(event)) => on_event(json!({
                    "type": "update", "serial": event.connection_serial,
                    "rxBytes": event.rx_bytes, "txBytes": event.tx_bytes,
                    "rxPackets": event.rx_packets, "txPackets": event.tx_packets,
                    "retransmits": event.tx_retx, "minRTT": event.min_rtt, "avgRTT": event.avg_rtt,
                    "time": event.time,
                })),
                Ok(idevice::dvt::network_monitor::NetworkEvent::Unknown(kind)) => on_event(json!({"type":"unknown", "kind":kind})),
                Err(error) => {
                    tracing::info!("network monitor ended: {error}");
                    break "lost";
                }
            }
        }
    };
    let _ = monitor.stop_monitoring().await;
    Ok(json!({ "ended": ended }))
}

/// Captures the device's sysdiagnose directly into Apparition's sandbox, streaming to disk.
/// The returned length is checked against CoreDevice's expected length before the export is shared.
pub async fn capture_sysdiagnose(
    adapter: &mut AdapterHandle,
    handshake: &mut RsdHandshake,
    destination: &std::path::Path,
) -> Result<Value, Report> {
    let mut service =
        idevice::core_device::DiagnostisServiceClient::connect_rsd(adapter, handshake)
            .await
            .context("This device doesn't offer CoreDevice sysdiagnose capture")?;
    let response = service
        .capture_sysdiagnose(false)
        .await
        .context("Couldn't start sysdiagnose capture")?;
    let expected = response.expected_length;
    let filename = response.preferred_filename;
    let mut stream = response.stream;
    let mut file = tokio::fs::File::create(destination)
        .await
        .context("Couldn't create the sysdiagnose export")?;
    let mut bytes = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Sysdiagnose transfer ended early")?;
        file.write_all(&chunk)
            .await
            .context("Couldn't save the sysdiagnose")?;
        bytes += chunk.len();
    }
    file.flush()
        .await
        .context("Couldn't finish saving the sysdiagnose")?;
    if bytes != expected {
        bail!("Sysdiagnose was incomplete: received {bytes} of {expected} bytes");
    }
    Ok(json!({ "filename": filename, "bytes": bytes }))
}

/// What the paired device currently advertises over its Remote Service Discovery tunnel.
pub fn service_readiness(handshake: &RsdHandshake) -> Value {
    let services = handshake
        .services
        .iter()
        .map(|(name, service)| {
            json!({
                "name": name,
                "port": service.port,
                "remoteXPC": service.uses_remote_xpc,
            })
        })
        .collect::<Vec<_>>();
    json!({ "services": services, "instruments": handshake.services.contains_key(SERVICE_HUB) })
}

// MARK: C ABI (Apparition on the device)

/// The device's running processes: `{processes: [{pid, name, path, kind, bundleId, appName}]}`,
/// apps first. `kind` is app, systemApp, extension or system.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_processes(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let list = crate::runtime().block_on(crate::stop::stoppable(processes(adapter, handshake)))?;
        Ok(json!({ "processes": list }))
    })())
}

/// Sends `signal` to `pid` (15 asks it to end, 9 ends it): `{}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_signal_process(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    pid: u32,
    signal_number: u32,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(crate::stop::stoppable(signal(adapter, handshake, pid, signal_number)))?;
        Ok(json!({}))
    })())
}

/// Simulates the location (`latitude`, `longitude`) for the whole device until
/// `apparition_tool_stop(key)` (or the connection drops), moving with `apparition_move_location`
/// meanwhile, then puts the real location back. `on_set(context, latitude, longitude)` is called
/// each time the device has taken a coordinate. Blocks for the whole session; the tunnel must
/// outlive it. `{ended: "stopped"|"lost", seconds, moves}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_simulate_location(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    key: *const std::ffi::c_char,
    latitude: f64,
    longitude: f64,
    on_set: Option<unsafe extern "C" fn(*mut std::ffi::c_void, f64, f64)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let key = unsafe { crate::text(key) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let context = context as usize;
        let mut on_set = move |at: Coordinate| {
            if let Some(on_set) = on_set {
                unsafe { on_set(context as *mut std::ffi::c_void, at.latitude, at.longitude) }
            }
        };
        crate::runtime().block_on(simulate_location(adapter, handshake, &key, Coordinate { latitude, longitude }, &mut on_set))
    })())
}

/// Moves the location session `key` to (`latitude`, `longitude`): `{moved}` (false: no location
/// session runs under that key).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_move_location(key: *const std::ffi::c_char, latitude: f64, longitude: f64) -> *mut std::ffi::c_char {
    crate::reply((|| {
        let key = unsafe { crate::text(key) }?;
        Ok(json!({ "moved": move_location(&key, Coordinate { latitude, longitude })? }))
    })())
}

/// Streams the device's log (only process `pid`'s when it isn't negative, and only lines at
/// `minimum_level` or above: 0 debug, 1 info, 2 notice, 3 error, 4 fault) until
/// `apparition_tool_stop(key)` or the device ends the stream. Lines arrive in batches:
/// `on_batch(context, json)` with a JSON array of `{time, pid, process, sender, level, subsystem,
/// category, message}` (`time` in milliseconds since 1970); `json` is only valid during the call.
/// Blocks for the whole session; the tunnel must outlive it. `{ended: "stopped"|"lost", lines}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_stream_log(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    key: *const std::ffi::c_char,
    pid: i64,
    minimum_level: u32,
    on_batch: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_char)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let key = unsafe { crate::text(key) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let pid = u32::try_from(pid).ok();
        let context = context as usize;
        let mut on_batch = move |entries: Vec<LogEntry>| {
            let (Some(on_batch), Ok(text)) = (on_batch, serde_json::to_string(&entries)) else { return };
            let Ok(text) = std::ffi::CString::new(text) else { return };
            unsafe { on_batch(context as *mut std::ffi::c_void, text.as_ptr()) }
        };
        crate::runtime().block_on(stream_log(adapter, handshake, &key, pid, Level::from_number(minimum_level), &mut on_batch))
    })())
}

/// Ends the location or log session `key`; its call then returns with `ended: "stopped"`.
/// `{stopped}` (false: none runs under that key).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_tool_stop(key: *const std::ffi::c_char) -> *mut std::ffi::c_char {
    crate::reply((|| {
        let key = unsafe { crate::text(key) }?;
        Ok(json!({ "stopped": stop(&key) }))
    })())
}


/// Streams Instruments process and system activity samples as JSON until stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_stream_activity(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    key: *const std::ffi::c_char,
    on_sample: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_char)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let key = unsafe { crate::text(key) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let context = context as usize;
        let mut callback = move |sample: Value| {
            let (Some(callback), Ok(text)) = (on_sample, serde_json::to_string(&sample)) else { return };
            let Ok(text) = std::ffi::CString::new(text) else { return };
            unsafe { callback(context as *mut std::ffi::c_void, text.as_ptr()) }
        };
        crate::runtime().block_on(crate::stop::stoppable(stream_activity(adapter, handshake, &key, &mut callback)))
    })())
}

/// Lists Instruments connection and thermal profiles available on this OS build.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_condition_profiles(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        crate::runtime().block_on(crate::stop::stoppable(condition_profiles(adapter, handshake)))
    })())
}

/// Applies an Instruments condition until the keyed session is stopped or the tunnel drops.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_condition_session(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    key: *const std::ffi::c_char,
    condition: *const std::ffi::c_char,
    profile: *const std::ffi::c_char,
    on_enabled: Option<unsafe extern "C" fn(*mut std::ffi::c_void)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let key = unsafe { crate::text(key) }?;
        let condition = unsafe { crate::text(condition) }?;
        let profile = unsafe { crate::text(profile) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let context = context as usize;
        let mut callback = move || {
            if let Some(callback) = on_enabled {
                unsafe { callback(context as *mut std::ffi::c_void) }
            }
        };
        crate::runtime().block_on(crate::stop::stoppable(condition_session(
            adapter, handshake, &key, &condition, &profile, &mut callback,
        )))
    })())
}

/// Streams Instruments network interface, connection and traffic events as JSON until stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_stream_network(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    key: *const std::ffi::c_char,
    on_event: Option<unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_char)>,
    context: *mut std::ffi::c_void,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let key = unsafe { crate::text(key) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let context = context as usize;
        let mut callback = move |event: Value| {
            let (Some(callback), Ok(text)) = (on_event, serde_json::to_string(&event)) else { return };
            let Ok(text) = std::ffi::CString::new(text) else { return };
            unsafe { callback(context as *mut std::ffi::c_void, text.as_ptr()) }
        };
        crate::runtime().block_on(crate::stop::stoppable(stream_network(adapter, handshake, &key, &mut callback)))
    })())
}

/// Captures a sysdiagnose to a caller-owned path, streaming the archive to disk.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_capture_sysdiagnose(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    destination: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let destination = unsafe { crate::text(destination) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let destination = std::path::Path::new(&destination);
        crate::runtime().block_on(crate::stop::stoppable(capture_sysdiagnose(adapter, handshake, destination)))
    })())
}

/// Lists services advertised by the paired device's RSD handshake.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_tool_service_readiness(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let handshake = unsafe { &(*handshake).0 };
        Ok(service_readiness(handshake))
    })())
}
