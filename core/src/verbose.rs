//! The verbose record of a sign-in: what Apparition sends to Apple's servers and what they answer
//! (isideload's `wire_log`), in `apparition-verbose.log` beside the ordinary log.
//!
//! It is its own file, not part of `apparition-core.log`, so that nothing which reads or ships the
//! ordinary log (a report, a trace) carries it by accident, and so it can be cleared by itself.
//! It is off until switched on: the Settings switch on a device, the desktop app's, `--verbose` on
//! the command line, `APPARITION_VERBOSE=1` anywhere, or the debug server's `verboseLog`.
//! What it holds is structure, not secrets; see `isideload::auth::wire_log`.

use std::{
    ffi::c_char,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

use isideload::auth::wire_log;
use tracing_subscriber::fmt::MakeWriter;

/// The file's name, next to the ordinary log.
pub const FILE_NAME: &str = "apparition-verbose.log";

/// Past this size the file is moved to `<name>.1` (replacing the one before) before the next record.
const ROTATE_BYTES: u64 = 4 * 1024 * 1024;

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Where the record goes, once logging is set up.
pub fn path() -> Option<PathBuf> {
    PATH.get().cloned()
}

pub(crate) fn remember_path(path: PathBuf) {
    let _ = PATH.set(path);
}

pub fn is_on() -> bool {
    wire_log::enabled()
}

/// Switches the record on or off. Switching it on writes a line saying so, which also tells which
/// program and version the records after it are from.
pub fn set(on: bool) {
    let was = wire_log::enabled();
    wire_log::set_enabled(on);
    if on && !was {
        let program = std::env::current_exe().ok().and_then(|exe| exe.file_name().map(|name| name.to_string_lossy().into_owned())).unwrap_or_default();
        tracing::info!(
            target: wire_log::TARGET,
            "---- verbose record on: {program} {} ({} {}) ----",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
    }
}

/// `APPARITION_VERBOSE` set to anything but empty or `0`.
pub(crate) fn asked_for_by_environment() -> bool {
    switched_on_by(std::env::var_os("APPARITION_VERBOSE"))
}

fn switched_on_by(value: Option<std::ffi::OsString>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// The last `max_bytes` of the file, from the start of a line, and how big the whole file is.
/// Empty when nothing has been recorded.
pub fn tail(max_bytes: u64) -> (String, u64) {
    match path() {
        Some(path) => tail_of(&path, max_bytes),
        None => (String::new(), 0),
    }
}

fn tail_of(path: &Path, max_bytes: u64) -> (String, u64) {
    let Ok(mut file) = std::fs::File::open(path) else { return (String::new(), 0) };
    let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = size.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return (String::new(), size);
    }
    let mut bytes = Vec::new();
    let _ = file.take(max_bytes).read_to_end(&mut bytes);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0
        && let Some(line) = text.find('\n')
    {
        text.drain(..=line);
    }
    (text, size)
}

/// Empties the record: the file and the older one it rotated to.
pub fn clear() {
    if let Some(path) = path() {
        clear_at(&path);
    }
}

fn clear_at(path: &Path) {
    let _ = std::fs::remove_file(rotated(path));
    let _ = std::fs::remove_file(path);
}

fn rotated(path: &Path) -> PathBuf {
    let mut older = path.as_os_str().to_owned();
    older.push(".1");
    PathBuf::from(older)
}

/// Opens the file for one record, so switching the record on creates it and clearing it just
/// deletes it. A record is a few lines per request, so opening each time costs nothing.
pub(crate) struct Sink(pub PathBuf);

impl<'a> MakeWriter<'a> for Sink {
    type Writer = Box<dyn Write>;

    fn make_writer(&'a self) -> Self::Writer {
        if std::fs::metadata(&self.0).is_ok_and(|meta| meta.len() > ROTATE_BYTES) {
            let _ = std::fs::rename(&self.0, rotated(&self.0));
        }
        match std::fs::OpenOptions::new().create(true).append(true).open(&self.0) {
            Ok(file) => Box::new(file),
            Err(_) => Box::new(std::io::sink()),
        }
    }
}

/// Switches the verbose record on or off. Returns at once; the record goes to
/// `apparition-verbose.log` beside the log `apparition_init_logging` was given.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_set_verbose(enabled: bool) {
    set(enabled);
}

/// Where the record is written, or null before logging is set up. Free with `apparition_string_free`.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_verbose_path() -> *mut c_char {
    match path().and_then(|path| std::ffi::CString::new(path.to_string_lossy().into_owned()).ok()) {
        Some(path) => path.into_raw(),
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{filter::Targets, prelude::*};

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// One request through isideload's logging layer to a local server: the structure reaches the
    /// verbose file, the secrets and the ordinary log get nothing, and switched off it writes nothing.
    #[test]
    fn a_request_goes_to_the_verbose_file_as_structure_and_not_to_the_ordinary_log() {
        use std::io::{Read, Write};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let folder = std::env::temp_dir().join(format!("apparition-verbose-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let verbose = folder.join(FILE_NAME);
        let _ = std::fs::remove_file(&verbose);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 8192];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/x-xml-plist\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });

        let ordinary = Buffer::default();
        let main = {
            let ordinary = ordinary.clone();
            tracing_subscriber::fmt::layer()
                .with_writer(move || ordinary.clone())
                .with_ansi(false)
                .with_filter(Targets::new().with_default(LevelFilter::INFO).with_target(wire_log::TARGET, LevelFilter::OFF))
        };
        let record = tracing_subscriber::fmt::layer()
            .with_writer(Sink(verbose.clone()))
            .with_ansi(false)
            .with_filter(Targets::new().with_target(wire_log::TARGET, LevelFilter::INFO));
        let subscriber = tracing_subscriber::registry().with(main).with(record);

        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>o</key><string>init</string>
<key>u</key><string>someone@example.com</string>
<key>X-Apple-I-MD</key><string>the-one-time-password</string>
<key>X-Apple-I-Client-Time</key><string>2026-11-12T09:30:05Z</string>
</dict></plist>"#;
        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).with(wire_log::WireLog).build();
        let send = || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async {
                client
                    .post(format!("http://127.0.0.1:{port}/grandslam/GsService2/?teamId=SECRETTEAM"))
                    .header("Content-Type", "text/xml")
                    .header("X-Apple-GS-Token", "a-secret-token")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
            });
        };

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("an ordinary line");
            // Off: nothing is written, not even the file.
            wire_log::set_enabled(false);
            send();
            assert!(!verbose.exists(), "wrote while off");
            // On.
            wire_log::set_enabled(true);
            send();
            wire_log::set_enabled(false);
        });

        let written = std::fs::read_to_string(&verbose).unwrap();
        for kept in ["-> POST", "/grandslam/GsService2/", r#"o="init""#, "u=<string:19>", "X-Apple-I-MD=<string:21>", r#"X-Apple-I-Client-Time="2026-11-12T09:30:05Z""#, "<- 200 OK"] {
            assert!(written.contains(kept), "{kept} missing:\n{written}");
        }
        for secret in ["SECRETTEAM", "a-secret-token", "someone", "one-time-password"] {
            assert!(!written.contains(secret), "{secret} leaked:\n{written}");
        }
        let ordinary = String::from_utf8(ordinary.0.lock().unwrap().clone()).unwrap();
        assert!(ordinary.contains("an ordinary line") && !ordinary.contains("GsService2"), "{ordinary}");
        let _ = std::fs::remove_file(&verbose);
        let _ = std::fs::remove_dir(&folder);
    }

    #[test]
    fn the_tail_starts_on_a_whole_line_and_clearing_removes_both_files() {
        let folder = std::env::temp_dir().join(format!("apparition-verbose-tail-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join(FILE_NAME);
        std::fs::write(&file, "first line\nsecond line\nthird line\n").unwrap();
        std::fs::write(rotated(&file), "older\n").unwrap();
        assert_eq!(tail_of(&file, 18), ("third line\n".to_string(), 34));
        assert_eq!(tail_of(&file, 1000).0, "first line\nsecond line\nthird line\n");
        assert_eq!(tail_of(&folder.join("missing"), 10), (String::new(), 0));
        clear_at(&file);
        assert!(!file.exists() && !rotated(&file).exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_environment_switch_reads_empty_and_zero_as_off() {
        let value = |text: &str| Some(std::ffi::OsString::from(text));
        assert!(switched_on_by(value("1")) && switched_on_by(value("yes")));
        assert!(!switched_on_by(value("0")) && !switched_on_by(value("")) && !switched_on_by(None));
    }
}
