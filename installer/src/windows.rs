//! What the command line does for a copy of Apparition installed on Windows: the firewall's
//! rules for it (`firewall`), its folder on PATH (`link`), and the background service that is
//! the companion while nobody is signed in (`service`).
//!
//! Apparition's setup (`scripts/windows/setup.nsi`) runs these rather than doing the work in its
//! own script, so a copy unpacked from the zip can do exactly what setup does.
//!
//! # The service
//!
//! A PC set up for everyone gets a Windows service, `Apparition`, that starts with Windows: the
//! relay, and the anisette the desktop app's Settings was set to offer, with no one signed
//! in. It is the same companion as `apparition relay`, with three differences.
//!
//! - It runs as `NT AUTHORITY\LocalService` with no privileges beyond the one every program has
//!   (`SeChangeNotifyPrivilege`), not as the system: the companion takes connections from the
//!   network and runs the Android anisette libraries in its own process. Its own identity
//!   (`NT SERVICE\Apparition`) is given the companion's folder and nothing else of the person's:
//!   the paired devices and the anisette's state, not the pairings with their devices or the
//!   signing keys beside them.
//! - Nobody can answer it, so a device asking to pair is declined. It steps aside while an
//!   Apparition someone is using holds the ports' claim (`companion::Front`), so pairing happens
//!   there, and takes over again when that one ends.
//! - Its settings are the desktop app's, which keeps a copy in the companion's folder
//!   (`companion.json`) for it. Without one it relays and offers no anisette.

#[cfg(windows)]
use std::path::PathBuf;

use rootcause::prelude::*;

type Error = Report;

fn fail(message: impl Into<String>) -> Error {
    report!("{}", message.into())
}

#[cfg(not(windows))]
const ELSEWHERE: &str = "is for Windows: nothing here needs it";

/// `firewall allow|remove|status [<program>...]`: Windows Defender Firewall's rules for
/// Apparition's programs. Without programs, this one and the desktop app beside it.
pub fn firewall(args: &[String]) -> Result<(), Error> {
    #[cfg(windows)]
    {
        use apparition_core::api::firewall;
        let programs: Vec<PathBuf> = match args.get(1..).unwrap_or_default() {
            [] => installed::programs()?,
            named => named.iter().map(PathBuf::from).collect(),
        };
        match args.first().map(String::as_str) {
            Some("allow") => {
                firewall::allow_programs(&programs)?;
                println!("Windows Defender Firewall lets your devices reach Apparition.");
                Ok(())
            }
            Some("remove") => {
                firewall::remove_programs(&programs)?;
                println!("Apparition's rules are out of Windows Defender Firewall.");
                Ok(())
            }
            Some("status") => {
                for program in &programs {
                    let state = firewall::inbound_for(program).map(|state| state.name()).unwrap_or("unknown");
                    println!("{state}\t{}", program.display());
                }
                Ok(())
            }
            _ => Err(fail("usage: apparition firewall allow|remove|status [<program>...]")),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(fail(format!("firewall {ELSEWHERE}")))
    }
}

/// `link [--system] [--remove]` on Windows: this copy's folder on PATH, the person's or (as an
/// administrator) everyone's.
#[cfg(windows)]
pub fn link(args: &[String]) -> Result<(), Error> {
    let (mut system, mut remove) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--system" => system = true,
            "--remove" => remove = true,
            other => return Err(fail(format!("unexpected {other}; on Windows, link takes --system and --remove"))),
        }
    }
    let folder = installed::folder()?;
    let folder = folder.to_str().ok_or_else(|| fail("this copy's folder isn't a path PATH can hold"))?;
    let whose = if system { "everyone's" } else { "your" };
    match (installed::set_on_path(folder, system, !remove)?, remove) {
        (true, false) => println!("{folder} is on {whose} PATH: `apparition-cli` works in terminals opened from now on."),
        (false, false) => println!("{folder} is on {whose} PATH already."),
        (true, true) => println!("{folder} is off {whose} PATH."),
        (false, true) => println!("{folder} wasn't on {whose} PATH."),
    }
    Ok(())
}

/// `service install|uninstall|start|stop|status|run`.
pub fn service(args: &[String]) -> Result<(), Error> {
    #[cfg(windows)]
    {
        match args.first().map(String::as_str) {
            Some("install") => {
                let dir = match args.get(1..).unwrap_or_default() {
                    [] => default_dir(),
                    [flag, dir] if flag == "--data-dir" => PathBuf::from(dir),
                    _ => return Err(fail("usage: apparition service install [--data-dir <the companion's folder>]")),
                };
                installed::install_service(&dir)?;
                println!("The Apparition service is installed and starts with Windows. Its companion folder: {}", dir.display());
                Ok(())
            }
            Some("uninstall") => {
                installed::uninstall_service()?;
                println!("The Apparition service is removed.");
                Ok(())
            }
            Some("start") => installed::sc(&["start", installed::SERVICE]).map(|_| println!("Started.")),
            Some("stop") => installed::stop_service().map(|_| println!("Stopped.")),
            Some("status") => {
                match installed::service_dir() {
                    None => println!("The Apparition service isn't installed (Apparition's setup installs it when set up for everyone, or: apparition-cli service install, as an administrator)."),
                    Some(dir) => {
                        println!("The Apparition service is installed, and {}.", if installed::service_running() { "running" } else { "stopped" });
                        println!("Its companion folder: {}", dir.display());
                        if apparition_core::companion::Front::claimed(&dir) {
                            println!("An Apparition someone is using has the companion's ports now; the service takes them back when it ends.");
                        }
                        println!("Its log: {}", dir.join(run::LOG).display());
                    }
                }
                Ok(())
            }
            // Reached only when it couldn't be run as a service (main() tries that first).
            Some("run") => Err(fail("couldn't run as a service")),
            _ => Err(fail("usage: apparition service install [--data-dir <folder>] | uninstall | start | stop | status")),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(fail(format!("service {ELSEWHERE}. `apparition relay` runs under launchd or systemd as it is")))
    }
}

/// The companion's folder: where the desktop app and `apparition relay` keep the paired devices
/// and the anisette's state.
#[cfg(windows)]
fn default_dir() -> PathBuf {
    apparition_core::api::DeviceStore::default().dir.join("anisette")
}

/// `service run`, as Windows starts it: hands this process to the service manager and returns
/// when the service has stopped. `None` when this process wasn't started as a service (someone
/// typed it), so that the caller can say so.
#[cfg(windows)]
pub fn run_as_service() -> Option<std::process::ExitCode> {
    run::dispatch().then_some(std::process::ExitCode::SUCCESS)
}

#[cfg(windows)]
mod installed {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use super::{Error, fail};

    pub(super) const SERVICE: &str = "Apparition";
    /// The service's own identity, which Windows makes from its name.
    const IDENTITY: &str = r"NT SERVICE\Apparition";
    const KEY: &str = r"SYSTEM\CurrentControlSet\Services\Apparition";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// What `sc` exits with when no service has the name.
    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;

    pub(super) fn folder() -> Result<PathBuf, Error> {
        let program = std::env::current_exe().map_err(|error| fail(format!("couldn't tell where this program is: {error}")))?;
        program.parent().map(Path::to_owned).ok_or_else(|| fail("couldn't tell which folder this program is in"))
    }

    /// This program and the desktop app beside it, when there is one.
    pub(super) fn programs() -> Result<Vec<PathBuf>, Error> {
        let this = std::env::current_exe().map_err(|error| fail(format!("couldn't tell where this program is: {error}")))?;
        let app = folder()?.join("Apparition.exe");
        Ok(if app.is_file() { vec![app, this] } else { vec![this] })
    }

    /// Runs `sc.exe`, whose exit code is the Windows error. Its words are translated; the code
    /// isn't.
    fn sc_code(args: &[&str]) -> Result<i32, Error> {
        let output = Command::new("sc.exe").args(args).creation_flags(CREATE_NO_WINDOW).output().map_err(|error| fail(format!("couldn't run sc.exe: {error}")))?;
        Ok(output.status.code().unwrap_or(-1))
    }

    pub(super) fn sc(args: &[&str]) -> Result<(), Error> {
        match sc_code(args)? {
            0 => Ok(()),
            5 => Err(fail("Windows refused: run this as an administrator")),
            ERROR_SERVICE_DOES_NOT_EXIST => Err(fail("the Apparition service isn't installed")),
            code => Err(fail(format!("sc {} failed (Windows error {code})", args.first().copied().unwrap_or_default()))),
        }
    }

    fn parameters() -> Option<windows_registry::Key> {
        windows_registry::LOCAL_MACHINE.open(format!(r"{KEY}\Parameters")).ok()
    }

    /// The companion's folder the installed service uses, or `None` when it isn't installed.
    pub(super) fn service_dir() -> Option<PathBuf> {
        parameters()?.get_string("DataDir").ok().map(PathBuf::from)
    }

    /// Whether the service manager has the service running (or starting or stopping): its
    /// process is there. `sc query` says so only in words, so this asks for the process's number.
    pub(super) fn service_running() -> bool {
        let script = format!("exit [int]((Get-CimInstance Win32_Service -Filter \"Name='{SERVICE}'\").ProcessId -gt 0)");
        powershell(&script).is_ok_and(|output| output.status.code() == Some(1))
    }

    pub(super) fn stop_service() -> Result<(), Error> {
        match sc_code(&["stop", SERVICE])? {
            0 | ERROR_SERVICE_NOT_ACTIVE => {}
            5 => return Err(fail("Windows refused: run this as an administrator")),
            ERROR_SERVICE_DOES_NOT_EXIST => return Err(fail("the Apparition service isn't installed")),
            code => return Err(fail(format!("sc stop failed (Windows error {code})"))),
        }
        // Its file can be replaced only once its process has gone.
        for _ in 0..100 {
            if !service_running() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(fail("the Apparition service didn't stop"))
    }

    /// The service's identity as a number: Windows derives it from the service's name, so it is
    /// the same on every PC, and still names the entries once the service is gone.
    const IDENTITY_SID: &str = "S-1-5-80-1608185904-1558370571-3944578777-3032930172-4143467971";

    /// Takes the service's entry off `dir`, and with it what the folder's contents inherited.
    /// (`icacls /remove` leaves it there.)
    fn revoke(dir: &str) {
        let script = format!(
            "$acl = Get-Acl -LiteralPath '{0}'; $acl.PurgeAccessRules((New-Object System.Security.Principal.SecurityIdentifier('{IDENTITY_SID}'))); Set-Acl -LiteralPath '{0}' -AclObject $acl",
            dir.replace('\'', "''")
        );
        let _ = powershell(&script);
    }

    /// Runs `script` in Windows PowerShell. Without the module path of a PowerShell 7 this may
    /// have been started from (over SSH, say), which makes Windows PowerShell load that
    /// version's modules and fail at `Get-Acl`.
    fn powershell(script: &str) -> std::io::Result<std::process::Output> {
        Command::new("powershell.exe").env_remove("PSModulePath").args(["-NoProfile", "-NonInteractive", "-Command", script]).creation_flags(CREATE_NO_WINDOW).output()
    }

    fn icacls(args: &[&str]) -> bool {
        Command::new("icacls.exe").args(args).creation_flags(CREATE_NO_WINDOW).output().is_ok_and(|output| output.status.success())
    }

    /// Makes the service, or brings one that is there in line, and starts it.
    pub(super) fn install_service(dir: &Path) -> Result<(), Error> {
        let program = std::env::current_exe().map_err(|error| fail(format!("couldn't tell where this program is: {error}")))?;
        let program = program.to_str().ok_or_else(|| fail("this program's path isn't one a service can have"))?;
        let dir_text = dir.to_str().ok_or_else(|| fail("the companion's folder isn't a path a service can have"))?;
        let command = format!("\"{program}\" service run");
        let exists = sc_code(&["query", SERVICE])? != ERROR_SERVICE_DOES_NOT_EXIST;
        if exists {
            stop_service()?;
        }
        // `sc` takes each setting's name and its value as two arguments.
        sc(&[if exists { "config" } else { "create" }, SERVICE, "binPath=", &command, "start=", "auto", "obj=", r"NT AUTHORITY\LocalService", "DisplayName=", "Apparition Companion"])?;
        sc(&["description", SERVICE, "Lets Apparition on your devices reach this computer while nobody is signed in. With the Apparition app open, the app does this instead."])?;
        // Its own identity in its token, for the folder below; and no privilege but the one
        // every program has.
        sc(&["sidtype", SERVICE, "unrestricted"])?;
        sc(&["privs", SERVICE, "SeChangeNotifyPrivilege"])?;
        // Started again if it dies: after 5 seconds, then a minute.
        sc(&["failure", SERVICE, "reset=", "86400", "actions=", "restart/5000/restart/60000//"])?;
        std::fs::create_dir_all(dir).map_err(|error| fail(format!("can't make {dir_text}: {error}")))?;
        let key = windows_registry::LOCAL_MACHINE.create(format!(r"{KEY}\Parameters")).map_err(|error| fail(format!("can't keep the service's settings: {error}")))?;
        if let Some(old) = key.get_string("DataDir").ok().filter(|old| !old.eq_ignore_ascii_case(dir_text)) {
            revoke(&old);
        }
        key.set_string("DataDir", dir_text).map_err(|error| fail(format!("can't keep the service's settings: {error}")))?;
        // The folder, what is in it and what will be: read, write and delete, to the service only.
        // (Granted on the folder alone: what is in it inherits, and so loses it again when
        // the folder does.)
        if !icacls(&[dir_text, "/grant", &format!("{IDENTITY}:(OI)(CI)M"), "/C", "/Q"]) {
            return Err(fail(format!("couldn't give the service {dir_text}")));
        }
        // Access denied at this point isn't about who is asking: the service's account can't
        // read the program, which is the case inside someone's own folders.
        match sc_code(&["start", SERVICE])? {
            0 => Ok(()),
            5 => Err(fail(format!("the service is installed but Windows can't start it: its account (LocalService) can't read {program}. Put Apparition where every account can read it, as setup does (Program Files), and install the service from there"))),
            code => Err(fail(format!("the service is installed but didn't start (Windows error {code})"))),
        }
    }

    pub(super) fn uninstall_service() -> Result<(), Error> {
        let dir = service_dir();
        match stop_service() {
            Ok(()) => {}
            Err(_) if sc_code(&["query", SERVICE])? == ERROR_SERVICE_DOES_NOT_EXIST => return Ok(()),
            Err(error) => return Err(error),
        }
        sc(&["delete", SERVICE])?;
        if let Some(dir) = dir.as_deref().and_then(Path::to_str) {
            revoke(dir);
        }
        Ok(())
    }

    const USER_ENVIRONMENT: &str = "Environment";
    const SYSTEM_ENVIRONMENT: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

    /// PATH with `folder` added at its end, or taken out; `None` when it is as wanted already.
    /// Entries are compared as Windows does, whatever their case or a trailing backslash.
    pub(super) fn edited(path: &str, folder: &str, present: bool) -> Option<String> {
        let same = |entry: &str| entry.trim().trim_end_matches('\\').eq_ignore_ascii_case(folder.trim_end_matches('\\'));
        let has = path.split(';').any(same);
        match (has, present) {
            (true, true) | (false, false) => None,
            (false, true) => Some(match path.trim_end_matches(';') {
                "" => folder.to_owned(),
                rest => format!("{rest};{folder}"),
            }),
            (true, false) => Some(path.split(';').filter(|entry| !same(entry) && !entry.is_empty()).collect::<Vec<_>>().join(";")),
        }
    }

    /// Puts `folder` on PATH or takes it off, and says whether anything changed.
    pub(super) fn set_on_path(folder: &str, system: bool, present: bool) -> Result<bool, Error> {
        let denied = |error: &dyn std::fmt::Display| fail(if system { format!("can't change everyone's PATH ({error}): run this as an administrator") } else { format!("can't change your PATH: {error}") });
        let key = if system { windows_registry::LOCAL_MACHINE.create(SYSTEM_ENVIRONMENT) } else { windows_registry::CURRENT_USER.create(USER_ENVIRONMENT) }.map_err(|error| denied(&error))?;
        // Empty only when there is none: a PATH that can't be read is left alone, not replaced.
        let path = match key.get_string("Path") {
            Ok(path) => path,
            Err(_) if key.get_type("Path").is_err() => String::new(),
            Err(error) => return Err(fail(format!("can't read PATH as it is ({error}); nothing was changed"))),
        };
        let Some(path) = edited(&path, folder, present) else { return Ok(false) };
        // Kept as the kind that may hold %variables%, which PATH usually is.
        key.set_expand_string("Path", &path).map_err(|error| denied(&error))?;
        announce_environment();
        Ok(true)
    }

    /// Tells running programs (Explorer, so new terminals) that the environment changed.
    fn announce_environment() {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn SendMessageTimeoutW(window: isize, message: u32, w: usize, l: *const u16, flags: u32, timeout: u32, result: *mut usize) -> isize;
        }
        const HWND_BROADCAST: isize = 0xffff;
        const WM_SETTINGCHANGE: u32 = 0x001a;
        const SMTO_ABORTIFHUNG: u32 = 0x0002;
        let what: Vec<u16> = "Environment\0".encode_utf16().collect();
        let mut result = 0;
        unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, what.as_ptr(), SMTO_ABORTIFHUNG, 2000, &mut result) };
    }
}

/// The service as it runs.
#[cfg(windows)]
mod run {
    use std::ffi::c_void;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;

    use apparition_core::companion::{Companion, Front};
    use apparition_core::relay::{self, ANISETTE_PORT, RELAY_PORT};
    use tokio::sync::watch;

    pub(super) const LOG: &str = "service.log";

    #[repr(C)]
    struct TableEntry {
        name: *const u16,
        main: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
    }

    #[repr(C)]
    #[derive(Default)]
    struct Status {
        kind: u32,
        state: u32,
        accepts: u32,
        exit_code: u32,
        specific_exit_code: u32,
        checkpoint: u32,
        wait_hint: u32,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn StartServiceCtrlDispatcherW(table: *const TableEntry) -> i32;
        fn RegisterServiceCtrlHandlerExW(name: *const u16, handler: unsafe extern "system" fn(u32, u32, *mut c_void, *mut c_void) -> u32, context: *mut c_void) -> *mut c_void;
        fn SetServiceStatus(handle: *mut c_void, status: *const Status) -> i32;
    }

    const OWN_PROCESS: u32 = 0x10;
    const STOPPED: u32 = 1;
    const STOP_PENDING: u32 = 3;
    const RUNNING: u32 = 4;
    const ACCEPT_STOP_AND_SHUTDOWN: u32 = 0x1 | 0x4;
    const CONTROL_STOP: u32 = 1;
    const CONTROL_INTERROGATE: u32 = 4;
    const CONTROL_SHUTDOWN: u32 = 5;
    const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

    /// A service that owns its process may give any name here.
    const NAME: [u16; 11] = {
        let text = b"Apparition\0";
        let mut wide = [0u16; 11];
        let mut i = 0;
        while i < text.len() {
            wide[i] = text[i] as u16;
            i += 1;
        }
        wide
    };

    /// Set to `true` when Windows asks the service to stop.
    static STOP: OnceLock<watch::Sender<bool>> = OnceLock::new();
    /// The service manager's handle for reporting this service's state: an address, kept as a
    /// number so that the handler's thread may use it.
    static HANDLE: OnceLock<usize> = OnceLock::new();

    fn report(state: u32) {
        let Some(&handle) = HANDLE.get() else { return };
        let status = Status {
            kind: OWN_PROCESS,
            state,
            accepts: if state == RUNNING { ACCEPT_STOP_AND_SHUTDOWN } else { 0 },
            wait_hint: if state == STOP_PENDING { 5000 } else { 0 },
            ..Default::default()
        };
        unsafe { SetServiceStatus(handle as *mut c_void, &status) };
    }

    unsafe extern "system" fn handler(control: u32, _kind: u32, _data: *mut c_void, _context: *mut c_void) -> u32 {
        match control {
            CONTROL_STOP | CONTROL_SHUTDOWN => {
                report(STOP_PENDING);
                if let Some(stop) = STOP.get() {
                    let _ = stop.send(true);
                }
                0
            }
            CONTROL_INTERROGATE => 0,
            _ => ERROR_CALL_NOT_IMPLEMENTED,
        }
    }

    unsafe extern "system" fn service_main(_count: u32, _arguments: *mut *mut u16) {
        let handle = unsafe { RegisterServiceCtrlHandlerExW(NAME.as_ptr(), handler, std::ptr::null_mut()) };
        if handle.is_null() {
            return;
        }
        let _ = HANDLE.set(handle as usize);
        let (stop, stopped) = watch::channel(false);
        let _ = STOP.set(stop);
        report(RUNNING);
        // Its own thread, for the stack the Android anisette's libraries are given (see main()).
        let serving = std::thread::Builder::new().name("service".into()).stack_size(crate::STACK).spawn(move || {
            match tokio::runtime::Builder::new_multi_thread().thread_stack_size(crate::STACK).enable_all().build() {
                Ok(runtime) => runtime.block_on(serve(stopped)),
                Err(error) => note(&super::default_dir(), &format!("couldn't start: {error}")),
            }
        });
        if let Ok(serving) = serving {
            let _ = serving.join();
        }
        report(STOPPED);
    }

    /// Runs this process as the service until Windows stops it. `false` when Windows didn't
    /// start this process as a service.
    pub(super) fn dispatch() -> bool {
        let table = [TableEntry { name: NAME.as_ptr(), main: Some(service_main) }, TableEntry { name: std::ptr::null(), main: None }];
        unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) != 0 }
    }

    /// Year, month, day, hour, minute and second (UTC) of a time in seconds since 1970.
    pub(super) fn civil(seconds: u64) -> (u64, u64, u64, u64, u64, u64) {
        let days = seconds / 86_400;
        // Days to a date, counting from 1 March 0000 so that a leap day ends its year.
        let shifted = days + 719_468;
        let era = shifted / 146_097;
        let day_of_era = shifted % 146_097;
        let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_from_march = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
        let month = if month_from_march < 10 { month_from_march + 3 } else { month_from_march - 9 };
        let year = year_of_era + era * 400 + u64::from(month <= 2);
        (year, month, day, seconds % 86_400 / 3600, seconds % 3600 / 60, seconds % 60)
    }

    /// A line in the service's log, which is all it has to say things with.
    fn note(dir: &Path, line: &str) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or_default();
        let (year, month, day, hour, minute, second) = civil(now);
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join(LOG)) {
            let _ = writeln!(file, "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z {line}");
        }
    }

    /// A failure's report, which is a tree drawn over several lines, as one line of the log.
    pub(super) fn one_line(report: &str) -> String {
        let parts: Vec<&str> = report.lines().map(|line| line.trim_matches(|c: char| c.is_whitespace() || "●│├╰─".contains(c))).filter(|line| !line.is_empty()).collect();
        parts.join(": ")
    }

    /// What the service does, from the desktop app's copy of its companion settings.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Settings {
        relay: bool,
        advertise: bool,
        anisette: Anisette,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Anisette {
        Off,
        Android,
        Server(String),
    }

    impl Settings {
        /// As the desktop app reads its own (desktop/src/companion.rs): the relay on and
        /// announced, no anisette, unless the file says otherwise. The modes only a Mac has
        /// read as none.
        pub(super) fn parse(text: &[u8]) -> Settings {
            let value: serde_json::Value = serde_json::from_slice(text).unwrap_or_default();
            let anisette = match (value["anisette"].as_str(), value["anisetteServer"].as_str()) {
                (Some("android"), _) => Anisette::Android,
                (Some("server"), Some(address)) => Anisette::Server(address.to_owned()),
                _ => Anisette::Off,
            };
            Settings { relay: value["relay"].as_bool().unwrap_or(true), advertise: value["advertise"].as_bool().unwrap_or(true), anisette }
        }

        fn read(dir: &Path) -> Settings {
            Settings::parse(&std::fs::read(dir.join("companion.json")).unwrap_or_default())
        }
    }

    /// Waits a second, or less if the service is told to stop; says whether it was.
    async fn pause(stopped: &mut watch::Receiver<bool>, time: Duration) -> bool {
        let _ = tokio::time::timeout(time, stopped.wait_for(|stop| *stop)).await;
        *stopped.borrow()
    }

    async fn serve(mut stopped: watch::Receiver<bool>) {
        apparition_core::api::init();
        let dir: PathBuf = super::installed::service_dir().unwrap_or_else(super::default_dir);
        let _ = std::fs::create_dir_all(&dir);
        // The core's own lines (the relay's connections, the anisette's failures) go there too.
        apparition_core::init_logging(&dir.join(LOG), false);
        let log: relay::Log = {
            let dir = dir.clone();
            Arc::new(move |line: &str| note(&dir, line))
        };
        note(&dir, &format!("Apparition {} is the companion while nobody is signed in.", env!("CARGO_PKG_VERSION")));
        let (mut waiting, mut failure) = (false, None::<String>);
        while !*stopped.borrow() {
            let settings = Settings::read(&dir);
            if Front::claimed(&dir) || !settings.relay {
                if !waiting {
                    note(&dir, if settings.relay { "An Apparition someone is using has the ports; waiting for it to end." } else { "The relay is switched off in the app; waiting." });
                    waiting = true;
                }
                pause(&mut stopped, Duration::from_secs(1)).await;
                continue;
            }
            waiting = false;
            let companion = match Companion::start(RELAY_PORT, ANISETTE_PORT, settings.advertise, &dir, log.clone()).await {
                Ok(companion) => companion,
                Err(error) => {
                    // Said once per reason: the ports are often busy for a moment after the app quits.
                    let reason = one_line(&error.to_string());
                    if failure.as_ref() != Some(&reason) {
                        note(&dir, &format!("Can't start the relay yet: {reason}"));
                        failure = Some(reason);
                    }
                    pause(&mut stopped, Duration::from_secs(3)).await;
                    continue;
                }
            };
            failure = None;
            note(&dir, &format!("Relay listening on port {}.", companion.relay.port()));
            let devices = companion.devices.clone();
            let declined = dir.clone();
            companion.devices.on_pairing(Arc::new(move |id, request| {
                note(&declined, &format!("{} ({}) asked to pair; declined, as nobody can answer a service. Open the Apparition app and pair it there.", request.device_name, request.peer));
                devices.decide(id, false);
            }));
            let offered = match &settings.anisette {
                Anisette::Off => Ok("no anisette"),
                Anisette::Android => companion.share_android_anisette(dir.clone()).await.map(|_| "Apparition's own Android anisette"),
                Anisette::Server(address) => companion.share_server_anisette(address).await.map(|_| "the anisette server set in the app"),
            };
            match offered {
                Ok(what) => note(&dir, &format!("Offering {what} to paired devices and computers.")),
                Err(error) => note(&dir, &format!("Can't offer anisette: {}", one_line(&error.to_string()))),
            }
            // Until told to stop, an Apparition with a person in front of it wants the ports, or
            // the settings change.
            loop {
                if pause(&mut stopped, Duration::from_secs(1)).await {
                    break;
                }
                if Front::claimed(&dir) {
                    note(&dir, "An Apparition someone is using wants the ports; stepping aside.");
                    waiting = true;
                    break;
                }
                if Settings::read(&dir) != settings {
                    note(&dir, "The app's companion settings changed; starting again with them.");
                    break;
                }
            }
            companion.stop_builtin_anisette().await;
            drop(companion);
        }
        note(&dir, "Stopped.");
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn dates_come_out_of_seconds_since_1970() {
            assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
            assert_eq!(civil(951_782_400), (2000, 2, 29, 0, 0, 0));
            assert_eq!(civil(1_794_576_896), (2026, 11, 13, 13, 34, 56));
        }

        #[test]
        fn a_failure_is_one_line_of_the_log() {
            assert_eq!(one_line("\n ● Can't listen on port 49160\n │\n ● Only one usage (os error 10048)\n"), "Can't listen on port 49160: Only one usage (os error 10048)");
        }

        #[test]
        fn with_no_settings_the_service_relays_and_offers_no_anisette() {
            assert_eq!(Settings::parse(b""), Settings { relay: true, advertise: true, anisette: Anisette::Off });
        }

        #[test]
        fn the_apps_settings_are_read_and_a_macs_modes_are_none_here() {
            let settings = Settings::parse(br#"{"relay":true,"anisette":"android","advertise":false}"#);
            assert_eq!(settings, Settings { relay: true, advertise: false, anisette: Anisette::Android });
            let settings = Settings::parse(br#"{"relay":false,"anisette":"server","anisetteServer":"https://example.org"}"#);
            assert_eq!(settings, Settings { relay: false, advertise: true, anisette: Anisette::Server("https://example.org".into()) });
            assert_eq!(Settings::parse(br#"{"anisette":"server"}"#).anisette, Anisette::Off);
            assert_eq!(Settings::parse(br#"{"anisette":"builtin"}"#).anisette, Anisette::Off);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::installed::edited;

    const FOLDER: &str = r"C:\Program Files\Apparition";

    #[test]
    fn a_folder_goes_on_the_end_of_path_once() {
        assert_eq!(edited(r"C:\Windows;C:\Tools;", FOLDER, true).as_deref(), Some(r"C:\Windows;C:\Tools;C:\Program Files\Apparition"));
        assert_eq!(edited("", FOLDER, true).as_deref(), Some(FOLDER));
        assert_eq!(edited(r"C:\Windows;c:\program files\apparition\", FOLDER, true), None);
    }

    #[test]
    fn a_folder_comes_off_path_and_the_rest_stays() {
        assert_eq!(edited(r"C:\Windows;C:\Program Files\Apparition;%USERPROFILE%\bin", FOLDER, false).as_deref(), Some(r"C:\Windows;%USERPROFILE%\bin"));
        assert_eq!(edited(r"C:\Windows", FOLDER, false), None);
    }
}
