//! Re-signing an app that is already on a device, from the exact signed IPA the Store kept for it
//! (`artifacts.rs`): the one thing the device itself can never give back. Ported from the Node
//! store's `lib/resign.js`.
//!
//! Signing the kept IPA with another profile and installing it over the old copy keeps the app's
//! data, as long as installd accepts the update. It refuses one thing: an upgrade whose
//! application-identifier differs from the installed app's, which is what switching to another
//! explicit App ID's profile changes (MismatchedApplicationIdentifierEntitlement). Then the only
//! way is to take the data off the device, remove the app, install the new signature and put the
//! data back, so that is what [`perform`] does, keeping the backup on disk throughout and putting
//! the previous signature back if the new one won't install.
//!
//! Everything that signs or touches a device goes through Apparition's own command line
//! (`apparition build --ipa`, `container pull|push|count`, `uninstall`), over its own pairing and
//! tunnel, never `devicectl`: that opens a CoreDevice tunnel, which wakes a sleeping Apple TV.
//! The module knows nothing of the job queue or the routes: the runner builds an [`Options`],
//! hands in an [`Exec`] (normally a [`CliExec`] that logs into the job) and a log callback.

use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// What the CLI prints of installd's refusal to update an app to another App ID: it prints only
/// the header of the error, which names the check that failed; the full sentence ("does not match
/// installed application's ...") stays in its own log.
const MISMATCH: [&str; 3] = [
    "MismatchedApplicationIdentifierEntitlement",
    "application-identifier entitlement string",
    "_validateApplicationIdentifierForNewBundleSigningInfo",
];
/// What lives in an app's data container and is worth moving. Caches and tmp are rebuilt by the app.
const DOMAINS: [&str; 2] = ["Documents", "Library"];
const SKIPPED: [&str; 2] = ["Library/Caches", "tmp"];

const DEVICE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SIGN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Whether a failed signing or install's output is installd's App ID refusal.
pub fn is_mismatch(output: &str) -> bool {
    MISMATCH.iter().any(|m| output.contains(m))
}

// ---------------------------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------------------------

/// One provisioning profile from the build-signing folder, as the picker and the planner need it.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    /// The file name without `.mobileprovision`.
    pub id: String,
    pub name: String,
    /// The team's display name, else its id.
    pub team: String,
    pub team_id: String,
    /// The application-identifier without the team prefix (`com.example.app`, or `*`).
    pub app_id: String,
    /// RFC 3339, to the second, UTC.
    pub expires: Option<String>,
    pub debuggable: bool,
    pub platforms: Vec<String>,
    /// The UDIDs it lists; empty for a distribution profile.
    pub devices: Vec<String>,
    pub file: PathBuf,
    pub expired: bool,
}

/// `$BUILD_SIGNING_DIR/profiles`, else `~/.config/build-signing/profiles`.
pub fn profile_dir() -> PathBuf {
    std::env::var_os("BUILD_SIGNING_DIR").map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".config/build-signing")).join("profiles")
}

/// `$APPARITION_RESIGN_BACKUPS`, else `~/.local/state/apparition/resign-backups`.
pub fn backup_root() -> PathBuf {
    std::env::var_os("APPARITION_RESIGN_BACKUPS").map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local/state/apparition/resign-backups"))
}

/// A profile id is a safe file name: what a request may name, never a path.
pub fn valid_profile_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && id.len() <= 128
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The plist inside a profile's CMS envelope: the XML document sits in the file as plain bytes.
pub fn profile_plist(bytes: &[u8]) -> Option<plist::Dictionary> {
    let start = bytes.windows(5).position(|w| w == b"<?xml")?;
    let end = start + bytes[start..].windows(8).position(|w| w == b"</plist>")? + 8;
    plist::Value::from_reader_xml(&bytes[start..end]).ok()?.into_dictionary()
}

fn strings(value: Option<&plist::Value>) -> Vec<String> {
    value.and_then(plist::Value::as_array).into_iter().flatten().filter_map(|v| v.as_string().map(String::from)).collect()
}

/// A profile's plist, decoded, as what the picker and the planner need. `file` and `expired` are
/// the caller's to fill in.
pub fn summarize(id: &str, plist: &plist::Dictionary) -> Profile {
    let text = |key: &str| plist.get(key).and_then(plist::Value::as_string).unwrap_or("").to_string();
    let entitlements = plist.get("Entitlements").and_then(plist::Value::as_dictionary);
    let team_id = strings(plist.get("TeamIdentifier")).into_iter().next().unwrap_or_default();
    let full = entitlements.and_then(|e| e.get("application-identifier")).and_then(plist::Value::as_string).unwrap_or("").to_string();
    let team_name = text("TeamName");
    let name = text("Name");
    Profile {
        id: id.to_string(),
        name: if name.is_empty() { id.to_string() } else { name },
        team: if team_name.is_empty() { team_id.clone() } else { team_name },
        app_id: match full.strip_prefix(&format!("{team_id}.")) {
            Some(rest) if !team_id.is_empty() => rest.to_string(),
            _ => full,
        },
        team_id,
        expires: plist.get("ExpirationDate").and_then(plist::Value::as_date).map(|d| d.to_xml_format()),
        debuggable: entitlements.and_then(|e| e.get("get-task-allow")).and_then(plist::Value::as_boolean) == Some(true),
        platforms: strings(plist.get("Platform")),
        devices: strings(plist.get("ProvisionedDevices")),
        file: PathBuf::new(),
        expired: false,
    }
}

/// Every `.mobileprovision` in `dir` that parses, by file name. `now` decides which have expired.
pub fn list_profiles_in(dir: &Path, now: SystemTime) -> Vec<Profile> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = read.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    let mut profiles = Vec::new();
    for name in names {
        let Some(id) = name.strip_suffix(".mobileprovision") else { continue };
        if !valid_profile_id(id) {
            continue;
        }
        let file = dir.join(&name);
        let Some(plist) = fs::read(&file).ok().and_then(|b| profile_plist(&b)) else { continue };
        let mut profile = summarize(id, &plist);
        profile.expired = plist.get("ExpirationDate").and_then(plist::Value::as_date).is_some_and(|d| SystemTime::from(d) < now);
        profile.file = file;
        profiles.push(profile);
    }
    profiles
}

/// Every profile in the build-signing folder.
pub fn list_profiles() -> Vec<Profile> {
    list_profiles_in(&profile_dir(), SystemTime::now())
}

pub fn find_profile_in(dir: &Path, id: &str, now: SystemTime) -> Option<Profile> {
    if !valid_profile_id(id) {
        return None;
    }
    list_profiles_in(dir, now).into_iter().find(|p| p.id == id)
}

/// The profile named `id` in the build-signing folder. Never reads a path the caller made up.
pub fn find_profile(id: &str) -> Option<Profile> {
    find_profile_in(&profile_dir(), id, SystemTime::now())
}

/// The public view (an element of `GET /api/resign/options`'s `profiles`): nothing about this
/// computer's paths, and the device list reduced to a count.
pub fn public_profile(profile: &Profile) -> Value {
    json!({
        "id": profile.id, "name": profile.name, "team": profile.team, "teamId": profile.team_id,
        "appId": profile.app_id, "expires": profile.expires, "debuggable": profile.debuggable,
        "platforms": profile.platforms, "deviceCount": profile.devices.len(), "expired": profile.expired,
    })
}

/// Why a profile can't sign for this device, or None. Pure, so the service can refuse early.
/// `device_udid` is None when the device isn't known (then the device list isn't checked).
pub fn profile_problem(profile: &Profile, device_udid: Option<&str>, platform: &str) -> Option<String> {
    if profile.expired {
        return Some(format!("{} has expired.", profile.name));
    }
    if let Some(udid) = device_udid.filter(|u| !u.is_empty())
        && !profile.devices.is_empty()
        && !profile.devices.iter().any(|d| d == udid)
    {
        return Some(format!("{} doesn't list this device.", profile.name));
    }
    let wanted: &[&str] = match platform.to_lowercase().as_str() {
        "visionos" => &["xros", "visionos"],
        "tvos" => &["tvos"],
        "ios" | "ipad" => &["ios"],
        _ => &[],
    };
    if !wanted.is_empty() && !profile.platforms.is_empty() && !profile.platforms.iter().any(|p| wanted.contains(&p.to_lowercase().as_str())) {
        return Some(format!("{} isn't a {platform} profile.", profile.name));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Running the command line
// ---------------------------------------------------------------------------------------------

/// What one command printed (stdout and stderr, in order) and how it ended.
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub code: i32,
    pub output: String,
}

/// One run of the Apparition command line. `cwd` and `env` (added to the process's own) matter
/// only to signing; the device commands leave them empty.
#[derive(Clone, Debug)]
pub struct Call {
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

/// Runs the `apparition` command line. [`CliExec`] is the real one, which streams what the command
/// prints into the job's log; tests use a fake that records the calls.
pub trait Exec: Send + Sync {
    fn run(&self, call: Call) -> impl Future<Output = Output> + Send;
}

/// Called with the child's pid when it starts and None when it has ended: what Cancel stops (the
/// pid leads its own process group, so `killpg` takes `apparition build` and what it started).
pub type PidHook = Arc<dyn Fn(Option<u32>) + Send + Sync>;
/// Where a command's lines go (the job log); also [`perform`]'s own lines.
pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// Spawns the Apparition CLI in its own process group, logging every line it prints as it comes.
pub struct CliExec {
    pub cli: PathBuf,
    pub log: Log,
    pub on_pid: Option<PidHook>,
}

/// SIGTERM to the process group `pid` leads (set up at spawn).
#[cfg(unix)]
pub fn stop(pid: u32) {
    // SAFETY: killpg only signals the group this module made for the child.
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
}

#[cfg(not(unix))]
pub fn stop(_pid: u32) {}

impl Exec for CliExec {
    async fn run(&self, call: Call) -> Output {
        use tokio::io::AsyncBufReadExt;
        let mut command = tokio::process::Command::new(&self.cli);
        command.args(&call.args).envs(call.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &call.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let output = format!("couldn't start {}: {e}\n", self.cli.display());
                (self.log)(&output);
                return Output { code: 127, output };
            }
        };
        let pid = child.id();
        if let (Some(hook), Some(pid)) = (&self.on_pid, pid) {
            hook(Some(pid));
        }
        let output = Arc::new(std::sync::Mutex::new(String::new()));
        let pump = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>| {
            let (log, output) = (self.log.clone(), output.clone());
            tokio::spawn(async move {
                let Some(stream) = stream else { return };
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = format!("{line}\n");
                    log(&line);
                    output.lock().unwrap().push_str(&line);
                }
            })
        };
        let a = pump(child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>));
        let b = pump(child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>));
        let status = match tokio::time::timeout(call.timeout, child.wait()).await {
            Ok(status) => status.ok(),
            Err(_) => {
                if let Some(pid) = pid {
                    stop(pid);
                }
                let _ = child.kill().await;
                let note = format!("timed out after {} s\n", call.timeout.as_secs());
                (self.log)(&note);
                output.lock().unwrap().push_str(&note);
                None
            }
        };
        // The process has exited; a helper it left behind may still hold the pipes open, so the
        // last lines get a moment, not forever.
        for pump in [a, b] {
            let _ = tokio::time::timeout(Duration::from_secs(2), pump).await;
        }
        if let Some(hook) = &self.on_pid {
            hook(None);
        }
        let output = output.lock().unwrap().clone();
        Output { code: status.and_then(|s| s.code()).unwrap_or(1), output }
    }
}

// ---------------------------------------------------------------------------------------------
// Re-signing
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DataMode {
    /// Move the app's data off the device and back when a reinstall is needed.
    #[default]
    Keep,
    /// Let a reinstall drop the data.
    Discard,
}

impl DataMode {
    /// `keep` or `discard` as the API spells them; anything else is None.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "keep" => Some(Self::Keep),
            "discard" => Some(Self::Discard),
            _ => None,
        }
    }
}

pub struct Options {
    /// The kept signed IPA. May be empty for a dry run, which signs nothing.
    pub ipa: PathBuf,
    pub bundle_id: String,
    /// The device's name or UDID, as `apparition --device` takes it.
    pub device: String,
    /// The `.mobileprovision` to sign with.
    pub profile_file: PathBuf,
    /// The project checkout `apparition build` runs in (its `build-signing.conf`); None for a dry run.
    pub cwd: Option<PathBuf>,
    /// Added to the environment of the signing run (`PLATFORM`, `APPARITION_BUILD_DIR`, ...).
    /// `BUILD_BUNDLE_ID` is set by [`perform`].
    pub env: Vec<(String, String)>,
    pub data_mode: DataMode,
    pub dry_run: bool,
    /// Where a data backup goes: a folder per re-sign under it.
    pub backups: PathBuf,
    /// The time the backup folder is named for.
    pub now: SystemTime,
}

impl Options {
    pub fn new(ipa: impl Into<PathBuf>, bundle_id: &str, device: &str, profile_file: impl Into<PathBuf>) -> Self {
        Options {
            ipa: ipa.into(), bundle_id: bundle_id.into(), device: device.into(), profile_file: profile_file.into(),
            cwd: None, env: Vec::new(), data_mode: DataMode::Keep, dry_run: false, backups: backup_root(), now: SystemTime::now(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Mode {
    /// Only checked that the app's data can be reached; nothing was changed.
    #[serde(rename = "dry-run")]
    DryRun,
    /// The device accepted the update; the data was never touched.
    #[serde(rename = "in-place")]
    InPlace,
    /// Removed and installed again, the data moved off and back.
    #[serde(rename = "reinstall")]
    Reinstall,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::DryRun => "dry-run",
            Mode::InPlace => "in-place",
            Mode::Reinstall => "reinstall",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub mode: Mode,
    /// Where the data backup is, after a reinstall that kept the data.
    pub backup: Option<PathBuf>,
    /// A dry run's answer: whether the app is installed with a readable (debuggable) container.
    pub data_readable: Option<bool>,
}

/// The app's data and the app itself on the device, through Apparition's tunnel.
struct DeviceOps<'a, E> {
    exec: &'a E,
    device: &'a str,
    bundle_id: &'a str,
}

impl<E: Exec> DeviceOps<'_, E> {
    async fn run(&self, args: &[&str]) -> Output {
        let mut full: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        full.extend(["--device".into(), self.device.into()]);
        self.exec.run(Call { args: full, cwd: None, env: Vec::new(), timeout: DEVICE_TIMEOUT }).await
    }

    /// Whether the container can be read, so the data can be backed up.
    async fn probe(&self) -> bool {
        self.run(&["container", "count", self.bundle_id, "--json"]).await.code == 0
    }

    async fn backup(&self, folder: &Path) -> Result<(), String> {
        let pulled = self.run(&["container", "pull", self.bundle_id, &folder.to_string_lossy(), "--json"]).await;
        if pulled.code != 0 {
            return Err("couldn't back up the app's data (is the app debuggable?); nothing was removed. Choose to discard data to continue anyway.".into());
        }
        Ok(())
    }

    async fn uninstall(&self) -> bool {
        self.run(&["uninstall", self.bundle_id]).await.code == 0
    }

    async fn restore(&self, folder: &Path) -> Result<(), String> {
        let pushed = self.run(&["container", "push", self.bundle_id, &folder.to_string_lossy(), "--json"]).await;
        if pushed.code != 0 {
            return Err(format!("couldn't put the data back; the backup is at {}", folder.display()));
        }
        Ok(())
    }

    /// Folders plus files the container holds now.
    async fn count(&self) -> u64 {
        let counted = self.run(&["container", "count", self.bundle_id, "--json"]).await;
        if counted.code != 0 {
            return 0;
        }
        let reply = counted.output.trim().lines().rev().find(|l| l.starts_with('{')).and_then(|l| serde_json::from_str::<Value>(l).ok());
        reply.map_or(0, |r| r["folders"].as_u64().unwrap_or(0) + r["files"].as_u64().unwrap_or(0))
    }
}

/// How many files and folders a backup holds under Documents and Library, minus the skipped ones:
/// what the device should hold again after the restore.
pub fn count_local(backup: &Path) -> u64 {
    fn walk(dir: &Path, relative: &str, count: &mut u64) {
        let Ok(read) = fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = read.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = format!("{relative}/{}", entry.file_name().to_string_lossy());
            if SKIPPED.contains(&name.as_str()) {
                continue;
            }
            *count += 1;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&entry.path(), &name, count);
            }
        }
    }
    let mut count = 0;
    for domain in DOMAINS {
        walk(&backup.join(domain), domain, &mut count);
    }
    count
}

/// `2026-11-02T00:00:00.000Z`, from the proleptic Gregorian calendar (no date crate needed).
fn iso_utc(time: SystemTime) -> String {
    let ms = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000)
}

/// Re-signs `opts.bundle_id` on `opts.device` with `opts.profile_file` from the kept IPA.
///
/// 1. A dry run only asks whether the app's data can be read, and changes nothing.
/// 2. Otherwise it first tries to update in place: signing and installing over the old copy keeps
///    the data, so if the device accepts it that is all.
/// 3. If the device refuses it for the App ID (installd's mismatch), the data goes off the device
///    first (unless `data_mode` is `Discard`), the app is removed only after that backup exists,
///    the new signature is installed and the data is pushed back and counted. If the new
///    signature won't install, the previous one (the project's own profile) is put back with its
///    data. The backup stays on disk throughout and is named in every error that could lose data.
///
/// Any other failure of the first attempt changes nothing and is an error.
pub async fn perform<E: Exec>(opts: &Options, exec: &E, log: &(dyn Fn(&str) + Send + Sync)) -> Result<Outcome, String> {
    let ops = DeviceOps { exec, device: &opts.device, bundle_id: &opts.bundle_id };
    let (bundle, device) = (opts.bundle_id.as_str(), opts.device.as_str());
    let sign = |with_profile: bool| {
        let mut args: Vec<String> = ["build", "--ipa"].map(String::from).into();
        args.extend([opts.ipa.to_string_lossy().into_owned(), "--device".into(), device.into(), "--no-mcp".into()]);
        if with_profile {
            args.extend(["--profile".into(), opts.profile_file.to_string_lossy().into_owned()]);
        }
        let mut env = opts.env.clone();
        env.retain(|(k, _)| k != "BUILD_BUNDLE_ID");
        env.push(("BUILD_BUNDLE_ID".into(), bundle.into()));
        exec.run(Call { args, cwd: opts.cwd.clone(), env, timeout: SIGN_TIMEOUT })
    };

    if opts.dry_run {
        let readable = ops.probe().await;
        log(&if readable {
            format!("==> Dry run: {bundle} on {device} is installed and its data container is readable, so it can be backed up. Nothing was changed.\n")
        } else {
            format!("==> Dry run: {bundle} isn't installed on {device}, or isn't debuggable, so its data can't be backed up first.\n")
        });
        return Ok(Outcome { mode: Mode::DryRun, backup: None, data_readable: Some(readable) });
    }
    if opts.ipa.as_os_str().is_empty() {
        return Err("no kept build to re-sign".into());
    }

    let profile_name = opts.profile_file.file_name().unwrap_or_default().to_string_lossy();
    log(&format!("==> Re-signing {bundle} on {device} with {profile_name}: trying an in-place update first\n"));
    let attempt = sign(true).await;
    if attempt.code == 0 {
        return Ok(Outcome { mode: Mode::InPlace, backup: None, data_readable: None });
    }
    if !is_mismatch(&attempt.output) {
        return Err("signing or installing failed (see the log above); the app was not changed".into());
    }

    log(&format!("\n==> The device refuses to update {bundle} to another App ID. Reinstalling instead; the data goes off the device first.\n"));
    let keep = opts.data_mode == DataMode::Keep;
    let backup = opts.backups.join(format!("{}-{bundle}", iso_utc(opts.now).replace([':', '.'], "-")));
    if keep {
        ops.backup(&backup).await?;
        log(&format!("==> Backed up {bundle}'s Documents and Library to {}\n", backup.display()));
    }

    let restore = || async {
        if !keep {
            return Ok(());
        }
        ops.restore(&backup).await?;
        let folder = backup.clone();
        let expected = tokio::task::spawn_blocking(move || count_local(&folder)).await.map_err(|e| e.to_string())?;
        let found = ops.count().await;
        let note = if found < expected { " (fewer than were backed up: check the backup)" } else { "" };
        log(&format!("==> Restored {found} of {expected} files{note}\n"));
        if found < expected {
            return Err(format!("only {found} of {expected} files came back; the backup is at {}", backup.display()));
        }
        Ok(())
    };

    if !ops.uninstall().await {
        let state = if keep { format!("the backup is at {}", backup.display()) } else { "nothing was backed up".into() };
        return Err(format!("couldn't remove {bundle} ({state})"));
    }

    if sign(true).await.code != 0 {
        log(&format!("\n==> The new signature won't install. Putting the previous one back{}.\n", if keep { " with its data" } else { "" }));
        let back = sign(false).await;
        if back.code == 0
            && let Err(e) = restore().await
        {
            log(&format!("==> {e}\n"));
        }
        return Err(format!("the new signature didn't install; {}", if back.code == 0 {
            "the previous one was put back".to_string()
        } else {
            format!("the previous one couldn't be put back either{}", if keep { format!("; the data is safe at {}", backup.display()) } else { String::new() })
        }));
    }
    restore().await?;
    Ok(Outcome { mode: Mode::Reinstall, backup: keep.then_some(backup), data_readable: None })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    const MISMATCH_TEXT: &str = "Upgrade's application-identifier entitlement string (T1.com.example.Other) does not match installed application's application-identifier string (T1.com.example.app); rejecting upgrade.";

    /// A `.mobileprovision` as the file holds it: binary CMS around a plain XML plist.
    fn profile_bytes(edit: impl FnOnce(&mut plist::Dictionary)) -> Vec<u8> {
        let mut entitlements = plist::Dictionary::new();
        entitlements.insert("application-identifier".into(), "T1.com.example.Other".into());
        entitlements.insert("get-task-allow".into(), true.into());
        let mut dict = plist::Dictionary::new();
        dict.insert("Name".into(), "my prof".into());
        dict.insert("TeamName".into(), "Team".into());
        dict.insert("TeamIdentifier".into(), plist::Value::Array(vec!["T1".into()]));
        dict.insert("ExpirationDate".into(), plist::Date::from_xml_format("2027-09-27T05:26:10Z").unwrap().into());
        dict.insert("Platform".into(), plist::Value::Array(["iOS", "xrOS", "visionOS"].map(Into::into).into()));
        dict.insert("ProvisionedDevices".into(), plist::Value::Array(vec!["UDID-1".into()]));
        dict.insert("DeveloperCertificates".into(), plist::Value::Array(vec![plist::Value::Data(vec![0, 159, 146, 150])]));
        dict.insert("Entitlements".into(), plist::Value::Dictionary(entitlements));
        edit(&mut dict);
        let mut xml = Vec::new();
        plist::Value::Dictionary(dict).to_writer_xml(&mut xml).unwrap();
        let mut file = vec![0x30, 0x82, 0xff, 0x00, 0x06, 0x09];
        file.extend(xml);
        file.extend([0x00, 0xff, 0x30, 0x82]);
        file
    }

    fn at(text: &str) -> SystemTime {
        SystemTime::from(plist::Date::from_xml_format(text).unwrap())
    }

    #[test]
    fn profiles_are_listed_from_the_folder_without_paths_or_device_lists_in_the_public_view() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: Vec<u8>| fs::write(dir.path().join(name), bytes).unwrap();
        write("a.mobileprovision", profile_bytes(|_| {}));
        write("old.mobileprovision", profile_bytes(|d| { d.insert("ExpirationDate".into(), plist::Date::from_xml_format("2020-01-01T00:00:00Z").unwrap().into()); }));
        write("notes.txt", b"x".to_vec());
        write(".hidden.mobileprovision", profile_bytes(|_| {}));
        write("bad name.mobileprovision", profile_bytes(|_| {}));
        write("garbage.mobileprovision", b"not a profile".to_vec());
        let now = at("2026-10-31T00:00:00Z");

        let profiles = list_profiles_in(dir.path(), now);
        assert_eq!(profiles.iter().map(|p| (p.id.as_str(), p.expired)).collect::<Vec<_>>(), [("a", false), ("old", true)]);
        assert_eq!(profiles[0].app_id, "com.example.Other");
        assert_eq!(profiles[0].team, "Team");
        assert_eq!(profiles[0].team_id, "T1");
        assert!(profiles[0].debuggable);
        assert_eq!(profiles[0].expires.as_deref(), Some("2027-09-27T05:26:10Z"));
        assert_eq!(profiles[0].file, dir.path().join("a.mobileprovision"));
        let shown = public_profile(&profiles[0]);
        assert!(shown.get("file").is_none());
        assert!(shown.get("devices").is_none());
        assert_eq!(shown["deviceCount"], 1);
        assert_eq!(shown["appId"], "com.example.Other");
        assert_eq!(shown["expired"], false);
        assert!(find_profile_in(dir.path(), "../a", now).is_none());
        assert_eq!(find_profile_in(dir.path(), "a", now).unwrap().id, "a");
        assert!(list_profiles_in(&dir.path().join("missing"), now).is_empty());
    }

    #[test]
    fn a_profile_that_expired_misses_the_device_or_is_for_another_platform_is_refused_early() {
        let plist = profile_plist(&profile_bytes(|_| {})).unwrap();
        let profile = summarize("p", &plist);
        assert_eq!(profile_problem(&profile, Some("UDID-1"), "visionOS"), None);
        assert!(profile_problem(&Profile { expired: true, ..profile.clone() }, Some("UDID-1"), "visionOS").unwrap().contains("expired"));
        assert!(profile_problem(&profile, Some("UDID-2"), "visionOS").unwrap().contains("doesn't list this device"));
        assert!(profile_problem(&Profile { platforms: vec!["tvOS".into()], ..profile.clone() }, Some("UDID-1"), "visionOS").unwrap().contains("isn't a visionOS profile"));
        assert_eq!(profile_problem(&Profile { devices: vec![], ..profile.clone() }, Some("UDID-2"), "iOS"), None, "a distribution profile lists no devices");
        assert_eq!(profile_problem(&profile, None, "iPad"), None, "an unknown device isn't checked against the list");
        assert_eq!(profile_problem(&profile, Some("UDID-2"), "macOS").unwrap(), "my prof doesn't list this device.");
    }

    #[test]
    fn a_wildcard_app_id_and_a_missing_team_name_are_summarised() {
        let plist = profile_plist(&profile_bytes(|d| {
            d.remove("TeamName");
            d.remove("Name");
            d.insert("Entitlements".into(), plist::Value::Dictionary(Default::default()));
        })).unwrap();
        let profile = summarize("wild", &plist);
        assert_eq!((profile.name.as_str(), profile.team.as_str(), profile.app_id.as_str(), profile.debuggable), ("wild", "T1", "", false));
        let star = profile_plist(&profile_bytes(|d| {
            let mut e = plist::Dictionary::new();
            e.insert("application-identifier".into(), "T1.*".into());
            d.insert("Entitlements".into(), plist::Value::Dictionary(e));
        })).unwrap();
        assert_eq!(summarize("w", &star).app_id, "*");
    }

    #[test]
    fn the_short_form_the_cli_prints_still_counts_as_the_app_id_refusal() {
        let printed = "ERROR: Failed to install the app on the device. (com.apple.dt.CoreDeviceError error 3002 (0xBBA))\n   FunctionName = -[MIInstallableBundle _validateApplicationIdentifierForNewBundleSigningInfo:error:]";
        assert!(is_mismatch(printed));
        assert!(is_mismatch(MISMATCH_TEXT));
        assert!(!is_mismatch("error: No signing certificate matching"));
        assert!(!is_mismatch("0xe8008012 could not install: profile does not include device"));
    }

    #[test]
    fn profile_ids_and_dates() {
        assert!(valid_profile_id("a") && valid_profile_id("Longwave_App.dev-1"));
        assert!(!valid_profile_id("") && !valid_profile_id(".a") && !valid_profile_id("../a") && !valid_profile_id("a b") && !valid_profile_id(&"a".repeat(129)));
        assert_eq!(iso_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(at("2026-11-02T00:00:00Z")), "2026-11-02T00:00:00.000Z");
        assert_eq!(iso_utc(at("2024-02-29T23:59:58Z") + Duration::from_millis(7)), "2024-02-29T23:59:58.007Z");
    }

    /// A fake device behind the command line: records every call, answers by rule, and plays the
    /// data copies out on disk like `container pull` does.
    struct Fake {
        calls: Mutex<Vec<Vec<String>>>,
        envs: Mutex<Vec<Vec<(String, String)>>>,
        /// Exit codes of the successive signings; the last one repeats.
        signs: Mutex<VecDeque<i32>>,
        pull: i32,
        push: i32,
        uninstall: i32,
        counted: (u64, u64),
    }

    impl Fake {
        fn new(signs: &[i32]) -> Self {
            Fake { calls: Default::default(), envs: Default::default(), signs: Mutex::new(signs.iter().copied().collect()), pull: 0, push: 0, uninstall: 0, counted: (1, 2) }
        }
        /// "sign" for a build, else the first two words ("container pull", "uninstall com.x...").
        fn verbs(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|c| if c[0] == "build" { "sign".to_string() } else { c[..2].join(" ") }).collect()
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Exec for Fake {
        async fn run(&self, call: Call) -> Output {
            let args = call.args;
            self.calls.lock().unwrap().push(args.clone());
            self.envs.lock().unwrap().push(call.env);
            let done = |code| Output { code, output: String::new() };
            match args[0].as_str() {
                "build" => {
                    let mut signs = self.signs.lock().unwrap();
                    let code = if signs.len() > 1 { signs.pop_front().unwrap() } else { signs.front().copied().unwrap_or(0) };
                    Output { code, output: if code == 0 { "Status: installed".into() } else { MISMATCH_TEXT.into() } }
                }
                "uninstall" => done(self.uninstall),
                "container" => match args[1].as_str() {
                    "pull" => {
                        if self.pull == 0 {
                            let folder = Path::new(&args[3]);
                            fs::create_dir_all(folder.join("Documents")).unwrap();
                            fs::write(folder.join("Documents/notes.txt"), "x").unwrap();
                            fs::create_dir_all(folder.join("Library/Application Support")).unwrap();
                            fs::write(folder.join("Library/Application Support/a.bin"), "x").unwrap();
                            // Never counted: rebuilt by the app.
                            fs::create_dir_all(folder.join("Library/Caches/deep")).unwrap();
                        }
                        done(self.pull)
                    }
                    "push" => done(self.push),
                    _ => Output { code: 0, output: format!("progress\n{}\n", json!({ "folders": self.counted.0, "files": self.counted.1, "bytes": 1, "failed": [] })) },
                },
                other => panic!("unexpected command {other}"),
            }
        }
    }

    fn options(backups: &Path) -> Options {
        let mut o = Options::new("/kept/a.ipa", "com.example.app", "AVP", "/p/other-dev.mobileprovision");
        o.cwd = Some("/repo".into());
        o.backups = backups.to_path_buf();
        o.now = at("2026-11-02T00:00:00Z");
        o
    }

    fn logger() -> (Arc<Mutex<String>>, impl Fn(&str) + Send + Sync) {
        let logs = Arc::new(Mutex::new(String::new()));
        let sink = logs.clone();
        (logs, move |s: &str| sink.lock().unwrap().push_str(s))
    }

    #[tokio::test]
    async fn an_update_the_device_accepts_is_done_in_place_touching_no_data() {
        let (backups, fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[0]), logger());
        let outcome = perform(&options(backups.path()), &fake, &log).await.unwrap();
        assert_eq!(outcome, Outcome { mode: Mode::InPlace, backup: None, data_readable: None });
        assert_eq!(fake.verbs(), ["sign"]);
        let call = &fake.calls()[0];
        assert_eq!(call.join(" "), "build --ipa /kept/a.ipa --device AVP --no-mcp --profile /p/other-dev.mobileprovision");
        assert!(fake.envs.lock().unwrap()[0].contains(&("BUILD_BUNDLE_ID".to_string(), "com.example.app".to_string())));
    }

    #[tokio::test]
    async fn a_changed_app_id_is_reinstalled_data_off_the_device_first_the_app_removed_only_after_data_put_back() {
        let (backups, fake, (logs, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1, 0]), logger());
        let outcome = perform(&options(backups.path()), &fake, &log).await.unwrap();
        assert_eq!(outcome.mode, Mode::Reinstall);
        let backup = outcome.backup.unwrap();
        assert_eq!(backup, backups.path().join("2026-11-02T00-00-00-000Z-com.example.app"));
        assert_eq!(fake.verbs(), ["sign", "container pull", "uninstall com.example.app", "sign", "container push", "container count"]);
        let calls = fake.calls();
        let pull = &calls[1];
        assert_eq!(pull[..3], ["container", "pull", "com.example.app"]);
        assert_eq!(pull[3], backup.to_string_lossy());
        assert_eq!(pull[4..], ["--json", "--device", "AVP"]);
        assert_eq!(calls[2], ["uninstall", "com.example.app", "--device", "AVP"]);
        assert!(calls[3].contains(&"--profile".to_string()));
        assert!(backup.join("Library/Application Support/a.bin").is_file(), "the backup stays on disk");
        assert!(logs.lock().unwrap().contains("Restored 3 of 3 files"));
    }

    #[tokio::test]
    async fn no_backup_no_uninstall_a_failed_backup_leaves_the_app_exactly_as_it_was() {
        let (backups, mut fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1]), logger());
        fake.pull = 1;
        let error = perform(&options(backups.path()), &fake, &log).await.unwrap_err();
        assert!(error.contains("couldn't back up") && error.contains("nothing was removed"), "{error}");
        assert_eq!(fake.verbs(), ["sign", "container pull"]);
    }

    #[tokio::test]
    async fn a_signing_failure_that_is_not_the_app_id_refusal_changes_nothing() {
        struct Failing(Mutex<usize>);
        impl Exec for Failing {
            async fn run(&self, _: Call) -> Output {
                *self.0.lock().unwrap() += 1;
                Output { code: 1, output: "No signing certificate".into() }
            }
        }
        let (backups, exec, (_, log)) = (tempfile::tempdir().unwrap(), Failing(Mutex::new(0)), logger());
        let error = perform(&options(backups.path()), &exec, &log).await.unwrap_err();
        assert!(error.contains("app was not changed"));
        assert_eq!(*exec.0.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn if_the_new_signature_will_not_install_the_previous_one_comes_back_with_its_data() {
        let (backups, fake, (logs, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1, 1, 0]), logger());
        let error = perform(&options(backups.path()), &fake, &log).await.unwrap_err();
        assert!(error.contains("previous one was put back"), "{error}");
        assert_eq!(fake.verbs(), ["sign", "container pull", "uninstall com.example.app", "sign", "sign", "container push", "container count"]);
        let calls = fake.calls();
        assert!(calls[3].contains(&"--profile".to_string()));
        assert!(!calls[4].contains(&"--profile".to_string()), "the recovery install uses the project's own profile");
        assert!(logs.lock().unwrap().contains("Putting the previous one back with its data"));
    }

    #[tokio::test]
    async fn when_even_the_recovery_fails_the_data_stays_on_disk_and_the_error_says_where() {
        let (backups, fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1, 1, 1]), logger());
        let error = perform(&options(backups.path()), &fake, &log).await.unwrap_err();
        assert!(error.contains("couldn't be put back either"), "{error}");
        assert!(error.contains(&backups.path().to_string_lossy().to_string()), "{error}");
        assert!(!fake.verbs().iter().any(|v| v == "container push"), "no data goes back onto a device with no app");
    }

    #[tokio::test]
    async fn too_few_restored_files_is_an_error_that_names_the_backup() {
        let (backups, mut fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1, 0]), logger());
        fake.counted = (1, 1);
        let error = perform(&options(backups.path()), &fake, &log).await.unwrap_err();
        assert!(error.starts_with("only 2 of 3 files came back; the backup is at "), "{error}");
        assert!(error.contains(&backups.path().to_string_lossy().to_string()));
    }

    #[tokio::test]
    async fn a_failed_push_names_the_backup_and_a_failed_uninstall_removes_nothing_more() {
        let (backups, mut fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1, 0]), logger());
        fake.push = 1;
        let error = perform(&options(backups.path()), &fake, &log).await.unwrap_err();
        assert!(error.starts_with("couldn't put the data back; the backup is at "), "{error}");
        let mut fake = Fake::new(&[1, 0]);
        fake.uninstall = 1;
        let error = perform(&options(backups.path()), &fake, &log).await.unwrap_err();
        assert!(error.starts_with("couldn't remove com.example.app (the backup is at "), "{error}");
        assert_eq!(fake.verbs(), ["sign", "container pull", "uninstall com.example.app"]);
    }

    #[tokio::test]
    async fn discarding_data_skips_the_backup_and_the_restore() {
        let (backups, fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[1, 0]), logger());
        let mut o = options(backups.path());
        o.data_mode = DataMode::Discard;
        let outcome = perform(&o, &fake, &log).await.unwrap();
        assert_eq!(outcome, Outcome { mode: Mode::Reinstall, backup: None, data_readable: None });
        assert_eq!(fake.verbs(), ["sign", "uninstall com.example.app", "sign"]);
        assert_eq!(fs::read_dir(backups.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_dry_run_only_asks_whether_the_data_can_be_reached() {
        let (backups, fake, (logs, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[0]), logger());
        let mut o = options(backups.path());
        o.dry_run = true;
        o.ipa = PathBuf::new();
        let outcome = perform(&o, &fake, &log).await.unwrap();
        assert_eq!(outcome, Outcome { mode: Mode::DryRun, backup: None, data_readable: Some(true) });
        assert_eq!(fake.verbs(), ["container count"]);
        assert!(logs.lock().unwrap().contains("Nothing was changed."));
        assert_eq!(serde_json::to_value(&outcome).unwrap(), json!({ "mode": "dry-run", "backup": null, "dataReadable": true }));
    }

    #[tokio::test]
    async fn a_real_run_without_a_kept_ipa_is_refused_before_touching_anything() {
        let (backups, fake, (_, log)) = (tempfile::tempdir().unwrap(), Fake::new(&[0]), logger());
        let mut o = options(backups.path());
        o.ipa = PathBuf::new();
        assert_eq!(perform(&o, &fake, &log).await.unwrap_err(), "no kept build to re-sign");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn the_local_count_skips_caches_and_counts_folders_too() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["Documents/a/b", "Library/Application Support", "Library/Caches/x", "Library/Preferences"] {
            fs::create_dir_all(dir.path().join(path)).unwrap();
        }
        fs::write(dir.path().join("Documents/a/b/f"), "x").unwrap();
        fs::write(dir.path().join("Library/Caches/x/g"), "x").unwrap();
        // Documents/a, a/b, a/b/f, Library/Application Support, Library/Preferences
        assert_eq!(count_local(dir.path()), 5);
        assert_eq!(count_local(&dir.path().join("none")), 0);
    }

    #[test]
    fn data_modes_parse_as_the_api_spells_them() {
        assert_eq!(DataMode::parse("keep"), Some(DataMode::Keep));
        assert_eq!(DataMode::parse("discard"), Some(DataMode::Discard));
        assert_eq!(DataMode::parse("Keep"), None);
    }

    /// The real command runner against a stand-in `apparition`: output streams to the log in
    /// order, the exit code and the environment arrive, the pid hook sees the child and its end,
    /// and a command that outlives its timeout is stopped.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_cli_runner_streams_reports_codes_and_stops_a_run_that_times_out() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("apparition");
        fs::write(&cli, "#!/bin/sh\ncase \"$1\" in\n  slow) sleep 30 ;;\n  fail) echo \"out $PWD $BUILD_BUNDLE_ID\"; echo \"err $*\" >&2; exit 3 ;;\n  *) echo \"ok $*\" ;;\nesac\n").unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
        let (logs, log) = logger();
        let pids = Arc::new(Mutex::new(Vec::new()));
        let hook = pids.clone();
        let exec = CliExec { cli: cli.clone(), log: Arc::new(log), on_pid: Some(Arc::new(move |p| hook.lock().unwrap().push(p))) };
        let call = |args: &[&str], timeout| Call { args: args.iter().map(|a| a.to_string()).collect(), cwd: Some(dir.path().to_path_buf()), env: vec![("BUILD_BUNDLE_ID".into(), "com.example.app".into())], timeout };

        let ok = exec.run(call(&["container", "count"], Duration::from_secs(20))).await;
        assert_eq!(ok, Output { code: 0, output: "ok container count\n".into() });
        let failed = exec.run(call(&["fail", "x"], Duration::from_secs(20))).await;
        assert_eq!(failed.code, 3);
        assert!(failed.output.contains("com.example.app") && failed.output.contains("err fail x"), "{}", failed.output);
        assert!(failed.output.contains(&dir.path().canonicalize().unwrap().to_string_lossy().to_string()), "runs in cwd: {}", failed.output);
        assert!(logs.lock().unwrap().contains("ok container count\n"));

        let started = std::time::Instant::now();
        let slow = exec.run(call(&["slow"], Duration::from_millis(300))).await;
        assert_ne!(slow.code, 0);
        assert!(slow.output.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(10));

        {
            let pids = pids.lock().unwrap();
            assert_eq!(pids.len(), 6);
            assert!(pids.chunks(2).all(|pair| pair[0].is_some() && pair[1].is_none()));
        }

        let missing = CliExec { cli: dir.path().join("nope"), log: Arc::new(|_: &str| {}), on_pid: None };
        let result = missing.run(call(&["x"], Duration::from_secs(1))).await;
        assert_eq!(result.code, 127);
        assert!(result.output.contains("couldn't start"));
    }
}
