//! The anisette VM: a macOS 26 virtual machine on this Mac whose only job is the Mac's own anisette, for a Mac on
//! macOS 27, which keeps its own to itself (anisette/native.rs). The app sets it up with one button and runs it behind
//! the companion; `apparition vm` does the same from a terminal.
//!
//! The VM itself is run by `apparition-vm` (selfhost/macos-vm/main.swift, Virtualization.framework), one process per
//! VM bundle, controlled over a loopback socket whose port and token it writes into the bundle (`control.json`). This
//! module is the rest:
//!
//! - **The guest**: macOS 26.6.2 (25G83), pinned ([`RESTORE_IMAGE`]). A copy already on this Mac is used (the cache,
//!   ~/Downloads, anything Spotlight finds by its size), otherwise it is downloaded from Apple, resumably, and checked
//!   against the SHA-256 Apple publishes with it either way.
//! - **Setup** ([`setup`]), restart-safe at every step: create and install; recoveryOS once to write the first boot
//!   (an account with a secure token, automatic login, no Setup Assistant, the vsock daemons, the trim); that first
//!   boot; recoveryOS again to turn SIP off (csrutil needs that account) and install `apparition` with its helper;
//!   then the boot that brings anisette up. The host drives both recoveryOS visits itself, by reading the VM's window
//!   (the runner's Vision OCR) and typing into it; what it types is a short `curl` line to this process, which serves
//!   the stage under a one-off token (vm_setup.rs). No administrator password is asked for.
//! - **Running** ([`serve`]): the runner's `serve` mode puts the guest's anisette on host loopback (port 6970) and
//!   suspends the VM after a few idle minutes; the next request resumes it in about a second. Setup ends by saving
//!   the VM, and every way the runner stops saves it again (quit, logout, shutdown, sleep), so the one-minute cold
//!   boot happens once, during setup. Only a crash or a power cut costs another.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

// MARK: The pinned guest

/// A macOS restore image, pinned by its size and SHA-256.
#[derive(Debug, Clone, Copy)]
pub struct RestoreImage {
    pub version: &'static str,
    pub build: &'static str,
    pub size: u64,
    pub sha256: &'static str,
    pub url: &'static str,
}

impl RestoreImage {
    /// The name the cache keeps it under (Apple's own file name).
    pub fn file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}

/// The macOS this whole flow is verified with: install, the recoveryOS automation, the trim, the RAM floor, Apple
/// accepting the anisette (the runner's `verifiedBuilds`). Apple's VM restore catalog lists only the newest macOS, so
/// this is Apple's CDN address for the build, pinned to the size and SHA-256 Apple publishes with it
/// (`x-amz-meta-digest-sha256`; ipsw.me lists the same).
pub const RESTORE_IMAGE: RestoreImage = RestoreImage {
    version: "26.6.2",
    build: "25G83",
    size: 19_772_231_540,
    sha256: "885503b7f4b06609e9a512f2befd40f59730640a3f1233e3892d60affdd51c95",
    url: "https://updates.cdn-apple.com/2026SummerFCS/fullrestores/140-75212/A2A24B94-1FC1-45A3-93F7-C51B02AF1F4D/UniversalMac_26.6.2_25G83_Restore.ipsw",
};

/// Where the app keeps its VM.
pub fn default_bundle() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Apparition").join("anisette.vm")
}

/// Where the restore image is kept (and found again).
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("Apparition")
}

/// What setup and its steps say, for a progress line: `step` is a stable name, `text` is for people.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub step: &'static str,
    pub text: String,
    /// 0 to 1, when the step can tell.
    pub fraction: Option<f64>,
}

pub type Say<'a> = &'a (dyn Fn(Progress) + Send + Sync);

fn tell(say: Say, step: &'static str, text: impl Into<String>, fraction: Option<f64>) {
    say(Progress { step, text: text.into(), fraction });
}

// MARK: The programs it takes

/// The programs a VM needs: the runner, and what goes into the guest.
#[derive(Debug, Clone)]
pub struct Tools {
    /// `apparition-vm` (selfhost/macos-vm/main.swift).
    pub runner: PathBuf,
    /// The guest's vsock status reporter and its anisette bridge (guest-status.c, guest-bridge.c).
    pub guest_status: PathBuf,
    pub guest_bridge: PathBuf,
    /// The `apparition` command line the guest runs (`anisette serve`), and its entitled helper.
    pub program: PathBuf,
    pub helper: PathBuf,
}

impl Tools {
    /// Next to this program, the way Apparition.app holds them (Contents/MacOS: apparition-vm,
    /// apparition-vm-guest-status, apparition-vm-guest-bridge, apparition-cli, apparition-anisette-helper). A build
    /// from a checkout takes the runner's three from `APPARITION_VM_TOOLS` or the checkout's build/macos-vm
    /// (selfhost/macos-vm/build.sh), and the guest's program from `APPARITION_VM_GUEST`, a folder holding an
    /// `apparition` and its signed helper (an installed Apparition.app's Contents/MacOS does).
    pub fn find() -> Result<Tools, Report> {
        let exe = std::env::current_exe().context("finding this program")?;
        let here = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let guest = std::env::var_os("APPARITION_VM_GUEST").map(PathBuf::from).unwrap_or_else(|| here.clone());
        let program = ["apparition-cli", "apparition"].iter().map(|name| guest.join(name)).find(|path| path.is_file())
            .or_else(|| exe.file_name().is_some_and(|name| name == "apparition").then(|| exe.clone()))
            .ok_or_else(|| report!("No apparition command line next to this program for the VM to run"))?;
        Ok(Tools {
            runner: Self::runner()?,
            guest_status: piece("apparition-vm-guest-status", "guest-status")?,
            guest_bridge: piece("apparition-vm-guest-bridge", "guest-bridge")?,
            helper: guest.join("apparition-anisette-helper"),
            program,
        })
    }

    /// Only the runner, which is all a VM that is set up already needs.
    pub fn runner() -> Result<PathBuf, Report> {
        piece("apparition-vm", "vm")
    }

    fn verify_helper(&self) -> Result<(), Report> {
        let output = std::process::Command::new("/usr/bin/codesign")
            .args(["-d", "--entitlements", ":-"])
            .arg(&self.helper)
            .output()
            .context("running codesign")?;
        let entitlements = plist::Value::from_reader(std::io::Cursor::new(&output.stdout)).ok();
        let has = |key: &str| {
            entitlements.as_ref().and_then(|value| value.as_dictionary()).and_then(|d| d.get(key)).and_then(|v| v.as_boolean()) == Some(true)
        };
        if !output.status.success()
            || !["com.apple.authkit.client.private", "com.apple.authkit.client.internal", "com.apple.private.alloy.anisette"]
                .iter()
                .all(|key| has(key))
        {
            bail!("{} isn't the build's signed anisette helper (it lacks AuthKit's entitlements); use the one in Apparition.app", self.helper.display());
        }
        Ok(())
    }
}

/// One of the runner's three programs: next to this one (`bundled`, in Apparition.app), else a checkout's build
/// (`checkout`, in `APPARITION_VM_TOOLS` or build/macos-vm).
fn piece(bundled: &str, checkout: &str) -> Result<PathBuf, Report> {
    if std::env::consts::ARCH != "aarch64" {
        bail!("The anisette VM needs a Mac with Apple silicon: macOS runs in a virtual machine only there.");
    }
    let exe = std::env::current_exe().context("finding this program")?;
    let inside = exe.parent().map(|here| here.join(bundled)).filter(|path| path.is_file());
    let built = std::env::var_os("APPARITION_VM_TOOLS")
        .map(PathBuf::from)
        .or_else(|| option_env!("CARGO_MANIFEST_DIR").map(|core| Path::new(core).join("../build/macos-vm")))
        .map(|dir| dir.join(checkout))
        .filter(|path| path.is_file());
    inside.or(built).ok_or_else(|| report!("{bundled} isn't next to this program (it comes with Apparition.app; in a checkout, run selfhost/macos-vm/build.sh)"))
}

// MARK: Talking to the runner

fn saved_state(bundle: &Path) -> PathBuf {
    bundle.join("suspended.vzvmsave")
}

/// A running runner, as it describes itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Live {
    /// starting, running, suspending, suspended or resuming.
    pub phase: String,
    pub running: bool,
    pub recovery: bool,
    pub network: bool,
    /// `serve` mode: the anisette front door, `http://127.0.0.1:6970`.
    pub anisette: Option<String>,
}

/// One control request. Replies are a JSON line; `ok: false` is an error, with the runner's reason.
async fn control(bundle: &Path, operation: &str, extra: &[(&str, &str)], timeout: Duration) -> Result<Value, Report> {
    let config: Value = serde_json::from_slice(&std::fs::read(bundle.join("control.json")).context("the VM isn't running")?)
        .context("reading control.json")?;
    let port = config["port"].as_u64().ok_or_else(|| report!("control.json has no port"))? as u16;
    let mut request = json!({ "token": config["token"], "operation": operation });
    for (key, value) in extra {
        request[*key] = Value::String(value.to_string());
    }
    let mut stream = tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(("127.0.0.1", port)))
        .await
        .map_err(|_| report!("the VM's runner didn't answer"))?
        .context("the VM isn't running")?;
    let exchange = async {
        stream.write_all(format!("{request}\n").as_bytes()).await?;
        let mut reply = Vec::new();
        let mut chunk = [0u8; 16384];
        while !reply.contains(&b'\n') && reply.len() < 4 << 20 {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            reply.extend_from_slice(&chunk[..n]);
        }
        Ok::<_, std::io::Error>(reply)
    };
    let reply = tokio::time::timeout(timeout, exchange).await.map_err(|_| report!("the VM's runner took too long to answer {operation}"))??;
    if reply.is_empty() {
        bail!("the VM's runner closed the connection");
    }
    let reply: Value = serde_json::from_slice(&reply).context("the runner's reply isn't JSON")?;
    if reply["ok"] != Value::Bool(true) {
        bail!("{}", reply["error"].as_str().unwrap_or("the VM's runner refused it"));
    }
    Ok(reply)
}

/// The runner of `bundle`, if one runs.
pub async fn live(bundle: &Path) -> Option<Live> {
    let reply = control(bundle, "status", &[], Duration::from_secs(3)).await.ok()?;
    serde_json::from_value(reply).ok()
}

/// What the guest reports over vsock (report-status.sh), from its current boot only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GuestReport {
    pub provisioned: bool,
    pub token: bool,
    pub owner: bool,
    pub sip_disabled: bool,
    pub amfi_active: bool,
    pub ssh: bool,
    pub relay: bool,
    pub anisette: bool,
    pub setup_assistant: bool,
    #[serde(rename = "consoleUID")]
    pub console_uid: i64,
    pub boot: String,
}

pub fn report(bundle: &Path) -> Option<GuestReport> {
    let path = bundle.join("guest-status/report.json");
    if std::fs::metadata(&path).ok()?.len() > 8192 {
        return None;
    }
    let value: GuestReport = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let boot = std::fs::read_to_string(bundle.join("guest-status/boot-id")).ok()?;
    (value.boot.trim() == boot.trim()).then_some(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A normal boot, its network attached or not (Setup Assistant must never see a network).
    Run { network: bool },
    /// recoveryOS.
    Recovery,
    /// The appliance: anisette on host loopback, suspended when idle.
    Serve,
}

/// Starts the runner for `bundle` in its own session (it outlives this process: the app or a terminal closing must
/// not take the VM down unsaved), with its output appended to the bundle's logs. Returns once it answers.
async fn start(bundle: &Path, runner: &Path, mode: Mode) -> Result<Live, Report> {
    use std::os::unix::process::CommandExt;
    if let Some(live) = live(bundle).await {
        bail!("The VM is already running ({}); stop it first", if live.recovery { "recoveryOS" } else { live.phase.as_str() });
    }
    let logs = bundle.join("logs");
    std::fs::create_dir_all(&logs).context("making the VM's logs folder")?;
    let name = match mode {
        Mode::Recovery => "recovery.log",
        Mode::Serve => "serve.log",
        Mode::Run { .. } => "boot.log",
    };
    // The runner never prints control tokens, typed text or credentials.
    let log = std::fs::OpenOptions::new().create(true).append(true).open(logs.join(name)).context("opening the VM's log")?;
    let mut command = std::process::Command::new(runner);
    command.arg(match mode {
        Mode::Recovery => "setup",
        Mode::Serve => "serve",
        Mode::Run { .. } => "run",
    });
    command.arg(bundle);
    if let Mode::Run { network } = mode {
        command.arg(if network { "--network=on" } else { "--network=off" });
    }
    // An accessory app whose window stays behind the others: it never takes the keyboard from whoever is at the Mac.
    command.arg("--background");
    command.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().context(format!("starting {}", runner.display()))?;
    // Reaped here, so a runner that exits while this process lives leaves no zombie behind.
    let (exited, mut gone) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = exited.send(child.wait());
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(status) = gone.try_recv() {
            // Another process may have started the same VM a moment earlier (the runner locks its bundle).
            if let Some(live) = live(bundle).await {
                return Ok(live);
            }
            bail!("The VM's runner stopped ({}); its log: {}\n{}", status.map(|s| s.to_string()).unwrap_or_default(), logs.join(name).display(), tail(&logs.join(name), 8));
        }
        if let Some(live) = live(bundle).await {
            return Ok(live);
        }
        if Instant::now() > deadline {
            bail!("The VM's runner didn't start in 90 seconds; its log: {}", logs.join(name).display());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// The last lines of a log, for an error.
fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

async fn wait_gone(bundle: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if live(bundle).await.is_none() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

async fn wait_for<T, F, Fut>(timeout: Duration, what: &str, mut check: F) -> Result<T, Report>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>, Report>>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = check().await? {
            return Ok(value);
        }
        if Instant::now() > deadline {
            bail!("Timed out waiting for {what}. Run setup again to carry on from here.");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Saves the VM and stops its runner: the next start resumes it in a second. Nothing to do when none runs.
pub async fn quit(bundle: &Path) -> Result<(), Report> {
    if live(bundle).await.is_none() {
        return Ok(());
    }
    // The runner answers once saved and exits right after; a reply lost to that exit is fine too.
    let _ = control(bundle, "quit", &[], Duration::from_secs(120)).await;
    if !wait_gone(bundle, Duration::from_secs(30)).await {
        bail!("The VM's runner didn't stop");
    }
    Ok(())
}

/// A clean shutdown of the guest, also of a suspended one: it's resumed first, since booting or changing the disk
/// over a saved state would throw that state away.
pub async fn shutdown(bundle: &Path) -> Result<(), Report> {
    if live(bundle).await.is_none() && saved_state(bundle).exists() {
        start(bundle, &Tools::runner()?, Mode::Run { network: false }).await?;
    }
    if live(bundle).await.is_some() {
        control(bundle, "shutdown", &[], Duration::from_secs(180)).await?;
        if !wait_gone(bundle, Duration::from_secs(180)).await {
            bail!("The VM didn't shut down in three minutes");
        }
    }
    Ok(())
}

/// Stops the VM at once, without saving it (a suspended one keeps its saved state).
pub async fn force_stop(bundle: &Path) -> Result<(), Report> {
    if live(bundle).await.is_some() {
        let _ = control(bundle, "force-stop", &[], Duration::from_secs(10)).await;
        if !wait_gone(bundle, Duration::from_secs(30)).await {
            bail!("The VM's runner didn't stop");
        }
    }
    Ok(())
}

/// The anisette the VM gives, on host loopback: starts the runner when none runs (resuming the saved VM), and
/// returns at once; a request to the front door waits for the guest.
pub async fn serve(bundle: &Path) -> Result<String, Report> {
    static STARTING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = STARTING.lock().await;
    if !bundle.join("ready").exists() {
        bail!("The anisette VM isn't set up yet");
    }
    let live = match live(bundle).await {
        Some(live) => live,
        None => start(bundle, &Tools::runner()?, Mode::Serve).await?,
    };
    live.anisette.ok_or_else(|| report!("The anisette VM is busy ({}); try again when it's done", if live.recovery { "in recoveryOS" } else { "running for setup" }))
}

/// The runner's own operations, for looking into a setup (`apparition vm screen`, `key`, `type`, ...): reading and
/// typing into the VM's window, suspending, resuming, its network. Not its lifecycle, which has its own functions.
pub async fn operate(bundle: &Path, operation: &str, extra: &[(&str, &str)]) -> Result<Value, Report> {
    const ALLOWED: &[&str] = &["screen", "key", "type", "click", "click-text", "suspend", "resume", "network-on", "network-off", "clock"];
    if !ALLOWED.contains(&operation) {
        bail!("{operation} isn't one of the VM's operations");
    }
    control(bundle, operation, extra, Duration::from_secs(180)).await
}

// MARK: Status

/// What there is of the VM, for the app and `apparition vm status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub bundle: PathBuf,
    /// Why this Mac can't run it, if it can't.
    pub unsupported: Option<String>,
    pub exists: bool,
    pub installed: bool,
    /// Setup finished: anisette came up and Apple accepted it.
    pub ready: bool,
    /// Saved to disk, not running: the next request resumes it.
    pub suspended: bool,
    pub live: Option<Live>,
    pub guest: Option<GuestReport>,
    /// The pinned restore image is in the cache, checked.
    pub image_cached: bool,
    pub image: String,
    /// Disk the bundle takes (its disk image is sparse).
    pub disk_bytes: u64,
}

pub async fn status(bundle: &Path) -> Status {
    let unsupported = Tools::runner().err().map(|error| error.format_current_context().to_string());
    let cached = cache_dir().join(RESTORE_IMAGE.file_name());
    Status {
        bundle: bundle.to_path_buf(),
        unsupported,
        exists: bundle.exists(),
        installed: bundle.join("installed").exists(),
        ready: bundle.join("ready").exists(),
        suspended: saved_state(bundle).exists(),
        live: live(bundle).await,
        guest: report(bundle),
        image_cached: stamped(&cached, &RESTORE_IMAGE),
        image: format!("macOS {} ({})", RESTORE_IMAGE.version, RESTORE_IMAGE.build),
        disk_bytes: allocated(bundle),
    }
}

/// Bytes allocated on disk under `dir` (sparse files count what they hold).
fn allocated(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.blocks() * 512).sum())
        .unwrap_or(0)
}

/// Stops the VM and moves its bundle to the Trash.
pub async fn remove(bundle: &Path) -> Result<(), Report> {
    force_stop(bundle).await?;
    if !bundle.exists() {
        return Ok(());
    }
    let status = std::process::Command::new("/usr/bin/trash").arg(bundle).status();
    if !status.is_ok_and(|s| s.success()) {
        std::fs::remove_dir_all(bundle).context(format!("removing {}", bundle.display()))?;
    }
    Ok(())
}

// MARK: The restore image

/// Whether `file` is the one `image` names, by the stamp its last check left: hashing 20 GB takes a minute, so the
/// stamp records that this very file (its size and time) was checked.
fn stamped(file: &Path, image: &RestoreImage) -> bool {
    let Ok(meta) = std::fs::metadata(file) else { return false };
    let stamp = std::fs::read_to_string(stamp_path(file)).unwrap_or_default();
    stamp.split_whitespace().collect::<Vec<_>>() == [image.sha256.to_string(), meta.len().to_string(), mtime(&meta).to_string()]
}

fn stamp_path(file: &Path) -> PathBuf {
    file.with_file_name(format!("{}.sha256", file.file_name().unwrap_or_default().to_string_lossy()))
}

fn mtime(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime()
}

/// SHA-256 of a file, telling `say` along the way.
async fn hash(file: &Path, size: u64, say: Say<'_>) -> Result<String, Report> {
    use sha2::Digest;
    let path = file.to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let work = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        use std::io::Read;
        let mut input = std::fs::File::open(&path)?;
        let mut digest = sha2::Sha256::new();
        let mut buffer = vec![0u8; 8 << 20];
        let mut done = 0u64;
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            digest.update(&buffer[..n]);
            done += n as u64;
            if done % (256 << 20) < n as u64 {
                let _ = tx.send(done);
            }
        }
        Ok(digest.finalize().iter().map(|b| format!("{b:02x}")).collect())
    });
    while let Some(done) = rx.recv().await {
        tell(say, "image", "Checking the macOS restore image", Some(done as f64 / size.max(1) as f64));
    }
    Ok(work.await.context("the hashing task failed")??)
}

/// Hashes `file` against `image`; stamps it when it matches.
async fn check(file: &Path, image: &RestoreImage, say: Say<'_>) -> Result<bool, Report> {
    if std::fs::metadata(file).map(|m| m.len()).unwrap_or(0) != image.size {
        return Ok(false);
    }
    if hash(file, image.size, say).await? != image.sha256 {
        return Ok(false);
    }
    let meta = std::fs::metadata(file)?;
    std::fs::write(stamp_path(file), format!("{} {} {}\n", image.sha256, meta.len(), mtime(&meta)))?;
    Ok(true)
}

/// Copies of the image this Mac may already have: in ~/Downloads, or anywhere Spotlight finds a file of its exact
/// size. Matched by size here; the hash decides.
fn local_copies(image: &RestoreImage) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if let Some(downloads) = dirs::download_dir() {
        if let Ok(entries) = std::fs::read_dir(downloads) {
            found.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ipsw")));
        }
    }
    if let Ok(output) = std::process::Command::new("/usr/bin/mdfind").arg(format!("kMDItemFSSize == {}", image.size)).output() {
        found.extend(String::from_utf8_lossy(&output.stdout).lines().map(PathBuf::from));
    }
    found.retain(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == image.size));
    found.dedup();
    found
}

/// The pinned restore image, checked: the cache's copy; else `given` or a copy already on this Mac, cloned into the
/// cache (instant on the same APFS volume) and checked; else downloaded from Apple into the cache, resuming a
/// partial download, and checked. Returns the cached file.
pub async fn restore_image(given: Option<&Path>, say: Say<'_>) -> Result<PathBuf, Report> {
    ensure_image(&RESTORE_IMAGE, &cache_dir(), given, true, say).await
}

/// `fetch`: also look for copies elsewhere on this Mac, and download it when there is none (tests turn it off).
async fn ensure_image(image: &RestoreImage, cache: &Path, given: Option<&Path>, fetch: bool, say: Say<'_>) -> Result<PathBuf, Report> {
    std::fs::create_dir_all(cache).context(format!("making {}", cache.display()))?;
    let cached = cache.join(image.file_name());
    if stamped(&cached, image) {
        return Ok(cached);
    }
    if cached.exists() {
        if check(&cached, image, say).await? {
            return Ok(cached);
        }
        let bad = cached.with_extension("ipsw.bad");
        std::fs::rename(&cached, &bad)?;
        tell(say, "image", format!("The cached restore image didn't match Apple's SHA-256; moved it to {}", bad.display()), None);
    }
    let partial = cache.join(format!("{}.partial", image.file_name()));
    let mut candidates: Vec<PathBuf> = given.map(Path::to_path_buf).into_iter().collect();
    if fetch {
        candidates.extend(local_copies(image));
    }
    for candidate in candidates {
        if candidate == cached || candidate == partial {
            continue;
        }
        if std::fs::metadata(&candidate).map(|m| m.len()).unwrap_or(0) != image.size {
            if given == Some(candidate.as_path()) {
                bail!("{} isn't macOS {} ({}): it is a different size", candidate.display(), image.version, image.build);
            }
            continue;
        }
        tell(say, "image", format!("Using the restore image at {}", candidate.display()), None);
        let _ = std::fs::remove_file(&partial);
        // A clone on the same APFS volume shares the blocks: no copy, no space.
        let cloned = std::process::Command::new("/bin/cp").arg("-c").arg(&candidate).arg(&partial).status().is_ok_and(|s| s.success());
        if !cloned {
            tell(say, "image", "Copying the restore image into the cache", None);
            let (from, to) = (candidate.clone(), partial.clone());
            tokio::task::spawn_blocking(move || std::fs::copy(from, to)).await.context("the copy failed")??;
        }
        if check(&partial, image, say).await? {
            std::fs::rename(&partial, &cached)?;
            let meta = std::fs::metadata(&cached)?;
            std::fs::write(stamp_path(&cached), format!("{} {} {}\n", image.sha256, meta.len(), mtime(&meta)))?;
            return Ok(cached);
        }
        let _ = std::fs::remove_file(&partial);
        if given == Some(candidate.as_path()) {
            bail!("{} doesn't match the SHA-256 Apple publishes for macOS {} ({})", candidate.display(), image.version, image.build);
        }
    }
    if !fetch {
        bail!("macOS {} ({}) isn't on this Mac", image.version, image.build);
    }
    download(image, &partial, say).await?;
    if !check(&partial, image, say).await? {
        let bad = cache.join(format!("{}.bad", image.file_name()));
        std::fs::rename(&partial, &bad)?;
        bail!("The download doesn't match the SHA-256 Apple publishes for it; it was moved to {}. Delete it and try again.", bad.display());
    }
    std::fs::rename(&partial, &cached)?;
    let meta = std::fs::metadata(&cached)?;
    std::fs::write(stamp_path(&cached), format!("{} {} {}\n", image.sha256, meta.len(), mtime(&meta)))?;
    Ok(cached)
}

/// Downloads `image` into `partial`, resuming what is there, retrying a dropped connection.
async fn download(image: &RestoreImage, partial: &Path, say: Say<'_>) -> Result<(), Report> {
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(30)).read_timeout(Duration::from_secs(60)).build()?;
    let mut failures = 0;
    loop {
        let have = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
        if have == image.size {
            return Ok(());
        }
        if have > image.size {
            std::fs::remove_file(partial)?;
            continue;
        }
        tell(say, "image", format!("Downloading macOS {} from Apple ({:.1} GB)", image.version, image.size as f64 / 1e9), Some(have as f64 / image.size as f64));
        let attempt = async {
            let mut response = client.get(image.url).header("Range", format!("bytes={have}-")).send().await?.error_for_status()?;
            // A server that ignores the range sends it all again: start over.
            let append = response.status() == reqwest::StatusCode::PARTIAL_CONTENT;
            let mut file = tokio::fs::OpenOptions::new().create(true).write(true).append(append).truncate(!append).open(partial).await?;
            let mut done = if append { have } else { 0 };
            let mut shown = Instant::now();
            while let Some(chunk) = response.chunk().await? {
                file.write_all(&chunk).await?;
                done += chunk.len() as u64;
                if shown.elapsed() > Duration::from_millis(500) {
                    shown = Instant::now();
                    tell(say, "image", format!("Downloading macOS {} from Apple ({:.1} of {:.1} GB)", image.version, done as f64 / 1e9, image.size as f64 / 1e9), Some(done as f64 / image.size as f64));
                }
            }
            file.flush().await?;
            Ok::<_, Report>(())
        };
        match attempt.await {
            Ok(()) => failures = 0,
            Err(error) => {
                failures += 1;
                if failures > 10 {
                    return Err(error.context("Downloading the macOS restore image failed; run setup again to resume it").into_dynamic());
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

// MARK: Setup

/// What [`setup`] takes.
#[derive(Debug, Clone, Default)]
pub struct SetupOptions {
    pub bundle: PathBuf,
    /// A restore image to use; otherwise one on this Mac, or Apple's.
    pub ipsw: Option<PathBuf>,
    /// Development only: authorize this SSH key in the guest and keep Remote Login on.
    pub public_key: Option<PathBuf>,
    /// Keep the cache's copy of the restore image once the VM is set up (for building VMs again and again).
    pub keep_image: bool,
}

/// Sets the VM up, or carries on from wherever an earlier run stopped, and ends with it saved: the next start
/// resumes it. Returns the anisette front door.
pub async fn setup(options: &SetupOptions, say: Say<'_>) -> Result<String, Report> {
    let bundle = options.bundle.as_path();
    let tools = Tools::find()?;
    tools.verify_helper()?;
    if bundle.join("ready").exists() {
        tell(say, "ready", "The anisette VM is set up", Some(1.0));
        return serve(bundle).await;
    }
    if !bundle.join("installed").exists() {
        let ipsw = restore_image(options.ipsw.as_deref(), say).await?;
        if !bundle.exists() {
            tell(say, "create", "Creating the VM", None);
            run_runner(&tools.runner, &["create".as_ref(), ipsw.as_os_str(), bundle.as_os_str()], |_| {}).await?;
            // 27 GB that rebuilds itself: not something for Time Machine to copy every hour.
            let _ = std::process::Command::new("/usr/bin/tmutil").arg("addexclusion").arg(bundle).output();
        }
        if !bundle.join("restore-build").exists() {
            bail!("{} wasn't made by Apparition; refusing to install over it", bundle.display());
        }
        tell(say, "install", "Installing macOS in the VM", Some(0.0));
        run_runner(&tools.runner, &["install".as_ref(), ipsw.as_os_str(), bundle.as_os_str()], |line| {
            if let Some(percent) = line.strip_prefix("install ").and_then(|p| p.trim_end_matches('%').parse::<f64>().ok()) {
                tell(say, "install", "Installing macOS in the VM", Some(percent / 100.0));
            }
        })
        .await?;
    }
    if !bundle.join("restore-build").exists() {
        bail!("{} wasn't made by Apparition", bundle.display());
    }
    let password = credentials(bundle)?;
    // A recoveryOS left running by an interrupted setup: its stage is simply done again.
    if live(bundle).await.is_some_and(|l| l.recovery) {
        force_stop(bundle).await?;
    }
    if !bundle.join("prepared").exists() {
        shutdown(bundle).await?;
        tell(say, "provision", "Writing the VM's first boot from its recoveryOS", None);
        recover(bundle, &tools, "provision", &password, options, say).await?;
        std::fs::write(bundle.join("prepared"), "recovery\n")?;
    }
    if report(bundle).is_none() && live(bundle).await.is_none() {
        start(bundle, &tools.runner, Mode::Run { network: false }).await?;
    }
    tell(say, "first-boot", "First boot: making the VM's account", None);
    let owner = wait_for(Duration::from_secs(300), "the VM's account", || async move {
        Ok(report(bundle).filter(|r| r.token && r.owner && r.provisioned))
    })
    .await?;
    if !owner.sip_disabled {
        tell(say, "recovery", "Turning SIP off and installing Apparition in the VM, from its recoveryOS", None);
        recover(bundle, &tools, "setup", &password, options, say).await?;
    }
    if live(bundle).await.is_none() {
        start(bundle, &tools.runner, Mode::Run { network: false }).await?;
    }
    // The first boot after recoveryOS may set the AMFI boot argument and restart once: wait for this boot's own
    // report. Setup Assistant must be gone before the network is attached.
    tell(say, "boot", "Starting macOS in the VM", None);
    wait_for(Duration::from_secs(300), "the VM's desktop session", || async move {
        if live(bundle).await.is_none() {
            bail!("The VM stopped while starting; its log: {}", bundle.join("logs/boot.log").display());
        }
        Ok(report(bundle).filter(|r| r.amfi_active && r.console_uid == 501 && !r.setup_assistant))
    })
    .await?;
    control(bundle, "network-on", &[], Duration::from_secs(5)).await?;
    tell(say, "network", "Waiting for the VM's anisette", None);
    wait_for(Duration::from_secs(300), "the VM's anisette agent", || async move { Ok(report(bundle).filter(|r| r.anisette).map(|_| ())) }).await?;
    // Saved now and resumed from here on: the cold boot happened once, here.
    tell(say, "save", "Saving the VM", None);
    quit(bundle).await?;
    if !saved_state(bundle).exists() {
        bail!("The VM didn't save; its log: {}", bundle.join("logs/boot.log").display());
    }
    std::fs::write(bundle.join("ready"), "")?;
    let url = serve(bundle).await;
    let url = match url {
        Ok(url) => url,
        Err(error) => {
            let _ = std::fs::remove_file(bundle.join("ready"));
            return Err(error);
        }
    };
    tell(say, "check", "Checking the VM's anisette with Apple", None);
    if let Err(error) = check_with_apple(&url).await {
        let _ = std::fs::remove_file(bundle.join("ready"));
        return Err(error);
    }
    if !options.keep_image && forget_image(&RESTORE_IMAGE, &cache_dir()) {
        tell(say, "ready", "Removed the macOS restore image from the cache: the VM never needs it again", None);
    }
    tell(say, "ready", "The anisette VM is set up", Some(1.0));
    Ok(url)
}

/// Removes the cache's copy of `image` (with its stamp and any partial download). A VM that is set up never reads it
/// again: its recoveryOS is on its own disk, and resuming or rebooting needs nothing else. Building a VM again later
/// takes the download (or the person's own copy) again. A copy the person had elsewhere is theirs and stays; the
/// cache's clone of it shares its blocks, so removing that frees nothing today and keeps the cache from holding
/// 20 GB once they delete theirs. True when there was a copy.
fn forget_image(image: &RestoreImage, cache: &Path) -> bool {
    let file = cache.join(image.file_name());
    let had = file.exists();
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_file(stamp_path(&file));
    let _ = std::fs::remove_file(cache.join(format!("{}.partial", image.file_name())));
    had
}

/// Runs the runner to completion (create, install), passing each output line to `line`.
async fn run_runner(runner: &Path, args: &[&std::ffi::OsStr], mut line: impl FnMut(&str)) -> Result<(), Report> {
    let mut child = tokio::process::Command::new(runner)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context(format!("starting {}", runner.display()))?;
    let mut out = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut err = child.stderr.take().expect("piped");
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = err.read_to_string(&mut text).await;
        text
    });
    let mut last = Vec::new();
    while let Some(text) = out.next_line().await? {
        line(&text);
        last.push(text);
        if last.len() > 8 {
            last.remove(0);
        }
    }
    let status = child.wait().await?;
    let errors = errors.await.unwrap_or_default();
    if !status.success() {
        bail!("{} failed: {}", args[0].to_string_lossy(), if errors.trim().is_empty() { last.join("\n") } else { errors.trim().to_string() });
    }
    Ok(())
}

/// The VM account's password, made once and kept in the bundle (mode 600): recoveryOS's csrutil asks for it, which
/// the host types.
fn credentials(bundle: &Path) -> Result<String, Report> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = bundle.join("credentials.txt");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return text
            .lines()
            .find_map(|line| line.strip_prefix("Password: "))
            .map(str::to_string)
            .ok_or_else(|| report!("No password in {}", path.display()));
    }
    let mut bytes = [0u8; 12];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).map_err(|_| report!("no random numbers"))?;
    let password: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).context("writing the VM's credentials")?;
    std::io::Write::write_all(&mut file, format!("VM account: apparition\nPassword: {password}\n").as_bytes())?;
    Ok(password)
}

/// 64 bits: the token only has to outlast one recoveryOS visit, on the VM's own network, and it is typed.
fn token() -> Result<String, Report> {
    let mut bytes = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).map_err(|_| report!("no random numbers"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// One visit to the VM's recoveryOS: `stage` is setup.sh's (provision before the first boot; setup to turn SIP off
/// and install Apparition), served by this process under a fresh token and typed into recoveryOS's Terminal by
/// reading the VM's window. Returns once the guest confirmed the stage and shut down.
async fn recover(bundle: &Path, tools: &Tools, stage: &str, password: &str, options: &SetupOptions, say: Say<'_>) -> Result<(), Report> {
    shutdown(bundle).await?;
    let token = token()?;
    let appliance = crate::vm_setup::Appliance {
        token: token.clone(),
        password: password.to_string(),
        guest_status: std::fs::read(&tools.guest_status).context(format!("reading {}", tools.guest_status.display()))?,
        guest_bridge: std::fs::read(&tools.guest_bridge).context(format!("reading {}", tools.guest_bridge.display()))?,
        authorized_keys: match &options.public_key {
            Some(path) => Some(std::fs::read(path).context(format!("reading {}", path.display()))?),
            None => None,
        },
    };
    let session = crate::vm_setup::serve_appliance(tools.program.clone(), tools.helper.clone(), appliance).await?;
    start(bundle, &tools.runner, Mode::Recovery).await?;
    let driven = drive_recovery(bundle, stage, &token, session.port, &session.progress, password, say).await;
    if let Err(error) = driven {
        let _ = force_stop(bundle).await;
        return Err(error);
    }
    if !wait_gone(bundle, Duration::from_secs(180)).await {
        let _ = force_stop(bundle).await;
    }
    Ok(())
}

/// Reads recoveryOS's screen and answers it, until the stage confirms. Only this VM's window is read and typed into
/// (the runner's own window, through its control socket). Never prints the password or what the screen shows.
async fn drive_recovery(
    bundle: &Path,
    stage: &str,
    token: &str,
    port: u16,
    progress: &tokio::sync::watch::Receiver<crate::vm_setup::Progress>,
    password: &str,
    say: Say<'_>,
) -> Result<(), Report> {
    let step = if stage == "provision" { "provision" } else { "recovery" };
    let (mut sent, mut consented, mut password_sent, mut terminal_attempts) = (false, false, false, 0);
    let deadline = Instant::now() + Duration::from_secs(15 * 60);
    while Instant::now() < deadline {
        if progress.borrow().done {
            tell(say, step, "recoveryOS finished; it shuts down", None);
            return Ok(());
        }
        if live(bundle).await.is_none() {
            bail!("recoveryOS stopped before it finished; its log: {}", bundle.join("logs/recovery.log").display());
        }
        let screen = match control(bundle, "screen", &[], Duration::from_secs(60)).await {
            Ok(reply) => reply["text"].as_str().unwrap_or_default().to_lowercase(),
            Err(error) => bail!("Couldn't read the VM's window ({}). If macOS asked to allow screen recording for Apparition, allow it and run setup again.", error.format_current_context()),
        };
        let has = |words: &[&str]| words.iter().all(|word| screen.contains(word));
        if has(&["language", "english"]) && !sent {
            control(bundle, "key", &[("key", "return")], Duration::from_secs(5)).await?;
        } else if has(&["options", "restart", "shut down"]) && !sent {
            // The startup picker: Options is the last icon, so Right twice always lands on it.
            for _ in 0..2 {
                control(bundle, "key", &[("key", "right")], Duration::from_secs(5)).await?;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            control(bundle, "key", &[("key", "return")], Duration::from_secs(5)).await?;
            tell(say, step, "Starting recoveryOS", None);
            tokio::time::sleep(Duration::from_secs(10)).await;
        } else if has(&["restore from time machine", "disk utility"]) && !sent {
            // Utilities > Terminal: the shortcut first, then the menu by its text if the shortcut didn't take.
            if terminal_attempts < 3 {
                control(bundle, "key", &[("key", "cmd+shift+t")], Duration::from_secs(5)).await?;
            } else {
                let _ = control(bundle, "click-text", &[("text", "Utilities"), ("region", "menubar")], Duration::from_secs(60)).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = control(bundle, "click-text", &[("text", "Terminal")], Duration::from_secs(60)).await;
            }
            terminal_attempts += 1;
            tokio::time::sleep(Duration::from_secs(3)).await;
        } else if has(&["password for user"]) && consented && !password_sent {
            type_slowly(bundle, password).await?;
            control(bundle, "key", &[("key", "return")], Duration::from_secs(5)).await?;
            password_sent = true;
        } else if has(&["booting unsigned"]) && !consented {
            control(bundle, "type", &[("text", "y\n")], Duration::from_secs(60)).await?;
            consented = true;
        } else if has(&["bash"]) && !sent {
            let Some(host) = gateway(bundle) else {
                // recoveryOS has no address yet: its network comes up a moment after the desktop.
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let line = format!("curl -fsS {host}:{port}/{token}/{stage}.sh -o /tmp/s.sh && bash /tmp/s.sh\n");
            type_slowly(bundle, &line).await?;
            sent = true;
            tell(say, step, if stage == "provision" { "recoveryOS is writing the first boot" } else { "recoveryOS is turning SIP off and installing Apparition" }, None);
        } else if has(&["no admin users authorized"]) || has(&["failed to authenticate"]) || has(&["authentication failure"]) {
            bail!("recoveryOS's csrutil didn't accept the VM's account; {} has its screen. Run setup again to retry.", bundle.join("screen.png").display());
        } else if has(&["error:"]) || has(&["incorrect password"]) || has(&["unknown user"]) {
            bail!("recoveryOS showed an error; the VM's window has it, and {} has a screenshot. Run setup again to retry.", bundle.join("screen.png").display());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    bail!("recoveryOS didn't finish in 15 minutes; {} has its last screen. Run setup again to retry.", bundle.join("screen.png").display())
}

/// Types `text` into the VM in short pieces: the guest takes key events more slowly than the runner can send them,
/// and a long burst is more than its keyboard queue needs to hold at once.
async fn type_slowly(bundle: &Path, text: &str) -> Result<(), Report> {
    let characters: Vec<char> = text.chars().collect();
    for piece in characters.chunks(24) {
        let piece: String = piece.iter().collect();
        control(bundle, "type", &[("text", &piece)], Duration::from_secs(60)).await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Ok(())
}

/// This Mac's address on the VM's network (the NAT gateway): the host interface whose network holds the address
/// the VM's DHCP lease gave it.
fn gateway(bundle: &Path) -> Option<std::net::Ipv4Addr> {
    let mac = std::fs::read_to_string(bundle.join("mac-address")).ok()?;
    let leases = std::fs::read_to_string("/var/db/dhcpd_leases").ok()?;
    let ip = lease(&leases, &mac)?;
    let interfaces = if_addrs::get_if_addrs().ok()?;
    interfaces.into_iter().find_map(|interface| match interface.addr {
        if_addrs::IfAddr::V4(v4) if same_network(v4.ip, v4.netmask, ip) && v4.ip != ip => Some(v4.ip),
        _ => None,
    })
}

fn same_network(host: std::net::Ipv4Addr, mask: std::net::Ipv4Addr, other: std::net::Ipv4Addr) -> bool {
    let (host, mask, other) = (u32::from(host), u32::from(mask), u32::from(other));
    mask != 0 && host & mask == other & mask
}

/// The address bootpd's lease file gives `mac`. Its hardware addresses drop leading zeros (`1,a:b:...`).
fn lease(leases: &str, mac: &str) -> Option<std::net::Ipv4Addr> {
    let normal = |text: &str| -> Option<Vec<u8>> { text.trim().split(':').map(|part| u8::from_str_radix(part, 16).ok()).collect() };
    let wanted = normal(mac)?;
    leases.split('}').find_map(|block| {
        let value = |key: &str| block.lines().find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('=').map(str::to_string));
        let hardware = value("hw_address")?;
        let hardware = hardware.split_once(',').map_or(hardware.as_str(), |(_, rest)| rest);
        (normal(hardware)? == wanted).then(|| value("ip_address")?.parse().ok()).flatten()
    })
}

pub async fn check_with_apple(url: &str) -> Result<(), Report> {
    let dir = cache_dir().join("vm-check");
    std::fs::create_dir_all(&dir)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let mut last = None;
    // The first request may meet a guest that is still starting AuthKit.
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        let result = crate::sign_in(
            &format!("vm-check-{stamp}-{attempt}@example.invalid"),
            "not-a-real-password".to_string(),
            url,
            &dir,
            |_| isideload::auth::apple_account::TwoFactorCallbackResponse::Abort,
        )
        .await;
        match result {
            Ok(_) => bail!("Apple signed a reserved test account in, which it never should"),
            Err(error) if crate::auth_code(&error) == Some(-20970) => return Ok(()),
            Err(error) if crate::auth_code(&error) == Some(-3000076) => {
                bail!("Apple refused the VM's anisette (-3000076): it stopped accepting it from this VM")
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last.expect("tried").context("Apple didn't answer the anisette check").into_dynamic())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY: RestoreImage = RestoreImage {
        version: "0",
        build: "0A0",
        size: 11,
        // SHA-256 of "hello world"
        sha256: "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        url: "https://example.invalid/path/Test_Restore.ipsw",
    };

    fn quiet(_: Progress) {}

    #[tokio::test]
    async fn a_local_copy_is_cloned_into_the_cache_checked_and_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("macos.ipsw");
        std::fs::write(&elsewhere, b"hello world").unwrap();
        let cache = dir.path().join("cache");
        let file = ensure_image(&TINY, &cache, Some(&elsewhere), false, &quiet).await.unwrap();
        assert_eq!(file, cache.join("Test_Restore.ipsw"));
        assert!(stamped(&file, &TINY));
        assert!(elsewhere.exists(), "the original is left where it was");
        // Stamped: found again without hashing, even with the original gone.
        std::fs::remove_file(&elsewhere).unwrap();
        assert_eq!(ensure_image(&TINY, &cache, None, false, &quiet).await.unwrap(), file);
    }

    #[tokio::test]
    async fn a_copy_that_does_not_match_is_refused_and_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let wrong = dir.path().join("wrong.ipsw");
        std::fs::write(&wrong, b"hello WORLD").unwrap();
        let cache = dir.path().join("cache");
        let error = ensure_image(&TINY, &cache, Some(&wrong), false, &quiet).await.unwrap_err();
        assert!(format!("{error}").contains("doesn't match"), "{error}");
        assert!(!cache.join("Test_Restore.ipsw").exists());
        assert!(!cache.join("Test_Restore.ipsw.partial").exists());
        let short = dir.path().join("short.ipsw");
        std::fs::write(&short, b"hello").unwrap();
        assert!(format!("{}", ensure_image(&TINY, &cache, Some(&short), false, &quiet).await.unwrap_err()).contains("different size"));
    }

    #[tokio::test]
    async fn a_changed_cached_file_is_hashed_again_and_a_bad_one_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let cached = cache.join("Test_Restore.ipsw");
        std::fs::write(&cached, b"hello world").unwrap();
        assert!(!stamped(&cached, &TINY), "no stamp yet");
        assert_eq!(ensure_image(&TINY, &cache, None, false, &quiet).await.unwrap(), cached);
        assert!(stamped(&cached, &TINY));
        std::fs::write(&cached, b"jello world").unwrap();
        // Same size, but written again: the stamp's time no longer matches, so it's hashed, fails, and moves aside.
        let stamp = std::fs::read_to_string(stamp_path(&cached)).unwrap();
        std::fs::write(stamp_path(&cached), stamp.replace(&TINY.sha256[..4], "0000")).unwrap();
        assert!(format!("{}", ensure_image(&TINY, &cache, None, false, &quiet).await.unwrap_err()).contains("isn't on this Mac"));
        assert!(cache.join("Test_Restore.ipsw.bad").exists());
    }

    #[tokio::test]
    async fn a_set_up_vm_forgets_the_caches_copy_and_leaves_the_persons_own() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = dir.path().join("Downloads/macos.ipsw");
        std::fs::create_dir_all(theirs.parent().unwrap()).unwrap();
        std::fs::write(&theirs, b"hello world").unwrap();
        let cache = dir.path().join("cache");
        let cached = ensure_image(&TINY, &cache, Some(&theirs), false, &quiet).await.unwrap();
        std::fs::write(cache.join("other-file"), b"not ours to remove").unwrap();
        assert!(forget_image(&TINY, &cache));
        assert!(!cached.exists() && !stamp_path(&cached).exists());
        assert!(theirs.exists(), "the person's own copy stays");
        assert!(cache.join("other-file").exists());
        assert!(!forget_image(&TINY, &cache), "nothing left the second time");
    }

    #[test]
    fn the_pinned_build_is_the_one_the_runner_verified() {
        let runner = include_str!("../../selfhost/macos-vm/main.swift");
        let line = runner.lines().find(|l| l.starts_with("let verifiedBuilds")).unwrap();
        let first = line.split('"').nth(1).unwrap();
        assert_eq!(first, RESTORE_IMAGE.build);
        assert!(RESTORE_IMAGE.url.ends_with(&format!("_{}_{}_Restore.ipsw", RESTORE_IMAGE.version, RESTORE_IMAGE.build)));
    }

    #[test]
    fn a_lease_is_found_by_its_hardware_address_without_leading_zeros() {
        let leases = "{\n\tname=other\n\tip_address=192.168.64.2\n\thw_address=1,de:ad:be:ef:0:1\n}\n{\n\tname=Mac\n\tip_address=192.168.64.7\n\thw_address=1,a2:b:c:d:e:f\n\tlease=0x1\n}\n";
        assert_eq!(lease(leases, "a2:0b:0c:0d:0e:0f\n"), Some("192.168.64.7".parse().unwrap()));
        assert_eq!(lease(leases, "a2:0b:0c:0d:0e:00"), None);
        assert!(same_network("192.168.64.1".parse().unwrap(), "255.255.255.0".parse().unwrap(), "192.168.64.7".parse().unwrap()));
        assert!(!same_network("10.0.0.1".parse().unwrap(), "255.0.0.0".parse().unwrap(), "192.168.64.7".parse().unwrap()));
    }

    #[test]
    fn a_report_counts_only_for_the_current_boot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("guest-status")).unwrap();
        std::fs::write(dir.path().join("guest-status/boot-id"), "B").unwrap();
        std::fs::write(dir.path().join("guest-status/report.json"), r#"{"provisioned":true,"token":true,"owner":true,"sipDisabled":false,"amfiActive":false,"ssh":false,"relay":false,"anisette":true,"setupAssistant":false,"consoleUID":501,"boot":"A"}"#).unwrap();
        assert!(report(dir.path()).is_none(), "an earlier boot's report");
        std::fs::write(dir.path().join("guest-status/boot-id"), "A").unwrap();
        let report = report(dir.path()).unwrap();
        assert!(report.anisette && report.owner && report.console_uid == 501);
    }

    #[test]
    fn the_password_is_made_once_and_kept_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let first = credentials(dir.path()).unwrap();
        assert_eq!(first.len(), 24);
        assert_eq!(credentials(dir.path()).unwrap(), first);
        assert_eq!(std::fs::metadata(dir.path().join("credentials.txt")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
