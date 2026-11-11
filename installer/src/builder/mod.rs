//! `apparition build`: `build-and-sign` in Rust. It builds the repo it runs in with `xcodebuild`,
//! gives a DebugTrace app its trace key and debug-server token, signs it with the core's signer
//! (the conf's .p12 and profiles, never the login keychain), keeps it in the build store with its
//! dSYMs and installs and launches it through Apparition's own pairing, never `devicectl`. Same
//! confs, flags, `build/` layout and report as the script, so the Store and agents can call
//! either. No Node, Python or Homebrew bash: the confs are read by the system's `/bin/bash`.
//!
//! `--sign-dylib F...` signs loose Mach-O files with the conf's certificate, which is how `inject`
//! signs a live patch (`DEBUGTRACE_BAS="apparition build"`).
//!
//! A Mac app (`PLATFORM=macOS`) is built, signed with its target's .entitlements and kept as
//! `build/<Name>-macOS.app`, as `build-and-sign` does; it is never installed or launched.
//!
//! Not here yet (`build-and-sign` still does them): `--distribution`, and the conf keys
//! `EXTRA_ENTITLEMENTS_PLIST` and `ASSOCIATED_DOMAINS`, which no repo sets.

mod bundle;
mod conf;
mod keep;
mod launch;
mod provenance;

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use apparition_core::api::{self, Platform};
use conf::Conf;

const USAGE: &str = "\
apparition build: build, sign, keep and install the app of the repo you're in (build-and-sign's flags)

  apparition build [--debug] [--no-deploy] [--unsigned] [--sign-only] [--ipa FILE] [--profile FILE]
                   [--platform P] [--device NAME] [--set KEY=VALUE]... [--run] [--log] [--log-all]
                   [--mcp | --no-mcp] [--jit] [--inject] [--inject-backend dynamic|apparition]
  apparition build --sign-dylib FILE...     sign Mach-O files with the conf's certificate (a live patch)
  apparition build --plan [--platform P]    print what the conf resolves to, as JSON (for a Store)

Reads ~/.config/build-signing/global.conf and the repo's (scripts/)build-signing.conf. Signs with
the conf's .p12 and profile, keeps the build (and its dSYMs) in the build store, installs it over
Apparition's own pairing with the device and, with --run, launches it the same way: --log streams
its console to build/device-console.log, --mcp makes the launch a debug session for the `apps`
tools (MCP_DEFAULT=on makes every --run one), --jit serves the app's JIT requests.
";

pub(crate) fn home() -> PathBuf {
    std::env::home_dir().unwrap_or_default()
}

pub(crate) fn tilde(path: PathBuf) -> String {
    match path.strip_prefix(home()) {
        Ok(rest) if !home().as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

pub(crate) fn random_bytes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; count];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes).expect("the system's random source");
    bytes
}

pub(crate) fn random_hex(count: usize) -> String {
    random_bytes(count).iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// A file's SHA-256 (hex) and size.
pub(crate) fn file_digest(path: &Path) -> Result<(String, u64), String> {
    let mut file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer).map_err(|e| e.to_string())?;
        if count == 0 { break }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    Ok((hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect(), bytes))
}

/// A new file only this user can read.
pub(crate) fn private_file(path: &Path) -> Result<fs::File, String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) struct Now {
    /// 2026-11-04T12:00:00Z
    pub seconds: String,
    /// 2026-11-04T12:00:00.000Z, as JavaScript's toISOString.
    pub millis: String,
    /// 2026-11-04T12:00:00+00:00, as Python's isoformat.
    pub offset: String,
}

pub(crate) fn utc_now() -> Now {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let (days, rest) = ((since.as_secs() / 86_400) as i64, since.as_secs() % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let base = format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}", rest / 3600, rest % 3600 / 60, rest % 60);
    Now { seconds: format!("{base}Z"), millis: format!("{base}.{:03}Z", since.subsec_millis()), offset: format!("{base}+00:00") }
}

/// A uuid4 as 32 hex digits, as `build-and-sign` names builds.
fn new_build_id() -> String {
    let mut bytes = random_bytes(16);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lines that are always noise in Xcode's output, never the error.
fn is_noise(line: &str) -> bool {
    if ["DTDKRemoteDeviceConnection", "notification_proxy", "MobileDeviceErrorDomain", "dtdevicekit.stacktrace"].iter().any(|n| line.contains(n)) {
        return true;
    }
    let trimmed = line.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 { return false }
    let rest = trimmed[digits..].trim_start();
    rest.len() < trimmed.len() - digits
        && (rest.starts_with("DT") && rest[2..].starts_with(|c: char| c.is_ascii_alphabetic())
            || ["libdispatch", "DVTFoundation", "IDEDevice", "Xcode3Core", "IDEFoundation"].iter().any(|p| rest.starts_with(p)))
}

/// The error lines of a failed build's log, as `build-and-sign` prints them.
fn log_excerpt(log: &Path) -> Option<String> {
    let text = String::from_utf8_lossy(&fs::read(log).ok()?).into_owned();
    let lines: Vec<&str> = text.lines().filter(|l| {
        let lower = l.to_lowercase();
        (lower.contains("error:") || lower.contains("build failed") || lower.contains("failed with exit code")) && !is_noise(l)
    }).collect();
    (!lines.is_empty()).then(|| lines[lines.len().saturating_sub(25)..].join("\n"))
}

#[derive(Default, Debug)]
struct Options {
    config: &'static str,
    sign_only: bool,
    no_deploy: bool,
    unsigned: bool,
    run: bool,
    log: bool,
    log_all: bool,
    jit: bool,
    mcp: Option<bool>,
    inject: bool,
    inject_backend: Option<String>,
    platform: Option<String>,
    device: Option<String>,
    settings: Vec<String>,
    ipa: Option<PathBuf>,
    profile: Option<PathBuf>,
    /// `--sign-dylib F...`: sign these Mach-O files and stop.
    sign_dylibs: Vec<PathBuf>,
    /// `--plan`: print what the conf resolves to for this platform, as JSON, and stop.
    plan: bool,
}

fn parse(args: &[String]) -> Result<Option<Options>, String> {
    let mut o = Options { config: "Release", ..Options::default() };
    let mut args = args.iter();
    let value = |args: &mut std::slice::Iter<String>, flag: &str| args.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--debug" => o.config = "Debug",
            "--sign-only" => o.sign_only = true,
            "--no-deploy" => o.no_deploy = true,
            "--unsigned" => { o.unsigned = true; o.no_deploy = true; }
            "--run" => o.run = true,
            "--log" => { o.run = true; o.log = true; }
            "--log-all" => { o.run = true; o.log = true; o.log_all = true; }
            "--jit" => { o.run = true; o.jit = true; }
            "--mcp" => o.mcp = Some(true),
            "--no-mcp" => o.mcp = Some(false),
            "--inject" => o.inject = true,
            "--plan" => o.plan = true,
            "--inject-backend" => { o.inject_backend = Some(value(&mut args, arg)?); o.inject = true; }
            "--platform" => o.platform = Some(value(&mut args, arg)?),
            "--device" => o.device = Some(value(&mut args, arg)?),
            "--set" => o.settings.push(value(&mut args, arg)?),
            "--ipa" => { o.ipa = Some(PathBuf::from(value(&mut args, arg)?)); o.sign_only = true; }
            "--profile" => o.profile = Some(PathBuf::from(value(&mut args, arg)?)),
            "--distribution" => return Err("--distribution isn't supported by `apparition build`: it signs development builds for your devices".into()),
            // The last option: it takes the files.
            "--sign-dylib" => {
                o.sign_dylibs = args.by_ref().map(PathBuf::from).collect();
                if o.sign_dylibs.is_empty() { return Err("--sign-dylib needs the files to sign".into()) }
            }
            other => return Err(format!("Unknown option: {other}")),
        }
    }
    Ok(Some(o))
}

/// The SDK a platform's build is stamped with (`DTPlatformName`), or None for one this doesn't know.
fn expected_sdk(platform: &str) -> Option<&'static str> {
    match platform.to_lowercase().as_str() {
        "ios" => Some("iphoneos"),
        "visionos" | "xros" => Some("xros"),
        "tvos" => Some("appletvos"),
        "watchos" => Some("watchos"),
        "macos" | "osx" => Some("macosx"),
        _ => None,
    }
}

fn core_platform(platform: &str) -> Option<Platform> {
    match platform.to_lowercase().as_str() {
        "ios" => Some(Platform::IOS),
        "visionos" | "xros" => Some(Platform::VisionOS),
        "tvos" => Some(Platform::TvOS),
        _ => None,
    }
}

/// The checkout's build lock (`build/.build-and-sign.lock`, shared with `build-and-sign`): two
/// builds in one checkout share `build/`, so the second waits. Released on drop.
struct BuildLock(PathBuf);

fn holder_alive(pid: i32) -> bool {
    #[cfg(unix)]
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 { return false }
    Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output()
        .map(|o| { let c = String::from_utf8_lossy(&o.stdout); c.contains("build-and-sign") || c.contains("apparition build") })
        .unwrap_or(false)
}

impl BuildLock {
    fn acquire(build_dir: &Path, root: &Path) -> Result<BuildLock, String> {
        let dir = build_dir.join(".build-and-sign.lock");
        let mut waited = 0u32;
        loop {
            if fs::create_dir(&dir).is_ok() {
                fs::write(dir.join("pid"), format!("{}\n", std::process::id())).map_err(|e| e.to_string())?;
                if waited > 0 { eprintln!("==> Lock acquired after {waited}s") }
                return Ok(BuildLock(dir));
            }
            let holder = fs::read_to_string(dir.join("pid")).ok().and_then(|p| p.trim().parse::<i32>().ok());
            let stale = match holder {
                Some(pid) => !holder_alive(pid),
                None => fs::metadata(&dir).and_then(|m| m.modified()).ok()
                    .and_then(|t| t.elapsed().ok()).is_some_and(|age| age > Duration::from_secs(60)),
            };
            if stale {
                let aside = build_dir.join(format!(".build-and-sign.lock.stale.{}", std::process::id()));
                if fs::rename(&dir, &aside).is_ok() { let _ = fs::remove_dir_all(&aside); }
                continue;
            }
            let who = holder.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
            if waited == 0 {
                eprintln!("==> Another build (pid {who}) is building in {}; waiting for it (they share build/)", root.display());
            }
            waited += 1;
            if waited > 2400 {
                return Err(format!("gave up waiting for build pid {who} in {}. If it is stuck, stop that pid; a dead holder's lock is cleared automatically.", root.display()));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The run's scratch folder, removed when the run ends.
struct WorkDir(PathBuf);

impl WorkDir {
    fn new() -> Result<WorkDir, String> {
        let dir = crate::scratch::tagged_folder("build", &random_hex(8));
        #[allow(unused_mut)]
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir).map_err(|e| format!("couldn't make a scratch folder: {e}"))?;
        Ok(WorkDir(dir))
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_logged(command: &mut Command, log: &fs::File, what: &str) -> Result<(), String> {
    let status = command.stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log.try_clone().map_err(|e| e.to_string())?)
        .status().map_err(|e| format!("couldn't run {what}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{what} failed ({status})")) }
}

/// A copy of `source` at `destination`, cloned on APFS.
fn clone_tree(source: &Path, destination: &Path) -> Result<(), String> {
    let ok = Command::new("/bin/cp").arg("-cR").arg(source).arg(destination).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if ok { return Ok(()) }
    let ok = Command::new("/bin/cp").arg("-R").arg(source).arg(destination).status().is_ok_and(|s| s.success());
    if ok { Ok(()) } else { Err(format!("couldn't copy {}", source.display())) }
}

/// `zip -qr <ipa> Payload` in `dir`, replacing `ipa`.
fn zip_payload(dir: &Path, ipa: &Path, log: &fs::File) -> Result<(), String> {
    let _ = fs::remove_file(ipa);
    run_logged(Command::new("/usr/bin/zip").arg("-qr").arg(ipa).arg("Payload").current_dir(dir), log, "zip")
}

/// The first IPA or zip of a repo that ships a prebuilt app (no Xcode project): in the root,
/// `ipa-build/` or `iOS/`, never `build/` (where signed outputs from past runs are).
fn find_static_ipa(root: &Path, build_dir: &Path) -> Option<PathBuf> {
    fn walk(dir: &Path, skip: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.starts_with(skip) { continue }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                walk(&path, skip, out);
            } else if kind.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ipa") || e.eq_ignore_ascii_case("zip")) {
                out.push(path);
            }
        }
    }
    for dir in [root.to_path_buf(), root.join("ipa-build"), root.join("iOS")] {
        if !dir.is_dir() { continue }
        let mut found = Vec::new();
        walk(&dir, build_dir, &mut found);
        found.sort();
        if let Some(first) = found.into_iter().next() {
            return Some(first);
        }
    }
    None
}

/// Older and ad-hoc DerivedData trees left directly in `build/`, besides the stable caches.
fn prune_legacy_derived_data(build_dir: &Path) {
    let Ok(entries) = fs::read_dir(build_dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type().is_ok_and(|k| k.is_dir()) || ["DerivedData", "DerivedData-inject", "DerivedData-inject-apparition"].contains(&name.as_str()) {
            continue;
        }
        if path.join("Info.plist").is_file() && path.join("Build/Products").is_dir() && path.join("Build/Intermediates.noindex").is_dir() {
            println!("==> Removing legacy DerivedData cache: {}", path.display());
            let _ = fs::remove_dir_all(&path);
        }
    }
}

/// What `xcodebuild -showBuildSettings -json` says about `target`'s product.
struct Product {
    app: Option<PathBuf>,
    /// `CODE_SIGN_ENTITLEMENTS`, as written (relative to `project_dir` unless absolute).
    entitlements: Option<String>,
    project_dir: Option<PathBuf>,
}

fn locate_product(xcodebuild: &mut Command, target: &str) -> Option<Product> {
    let output = xcodebuild.stdin(Stdio::null()).stderr(Stdio::null()).output().ok().filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let entries: serde_json::Value = serde_json::from_str(&text[text.find('[')?..]).ok()?;
    let entries = entries.as_array()?;
    let chosen = entries.iter().find(|e| e["target"] == target).or(entries.first())?;
    let settings = &chosen["buildSettings"];
    let app = settings["BUILT_PRODUCTS_DIR"].as_str()
        .zip(settings["FULL_PRODUCT_NAME"].as_str().or(settings["WRAPPER_NAME"].as_str()))
        .filter(|(_, name)| name.ends_with(".app"))
        .map(|(dir, name)| Path::new(dir).join(name));
    Some(Product {
        app,
        entitlements: settings["CODE_SIGN_ENTITLEMENTS"].as_str().filter(|e| !e.is_empty()).map(String::from),
        project_dir: settings["PROJECT_DIR"].as_str().filter(|d| !d.is_empty()).map(PathBuf::from),
    })
}

fn first_app(dir: &Path) -> Option<PathBuf> {
    let mut apps: Vec<PathBuf> = fs::read_dir(dir).ok()?.filter_map(Result::ok).map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|e| e == "app")).collect();
    apps.sort();
    apps.into_iter().next()
}

/// The plist inside a profile's CMS envelope.
fn profile_plist(profile: &[u8]) -> Option<plist::Dictionary> {
    let text = String::from_utf8_lossy(profile);
    let (start, end) = (text.find("<?xml")?, text.find("</plist>")?);
    plist::Value::from_reader_xml(text.get(start..end + "</plist>".len())?.as_bytes()).ok()?.into_dictionary()
}

/// The UDIDs a profile lists (`ProvisionedDevices`).
fn profile_devices(profile: &[u8]) -> Vec<String> {
    profile_plist(profile).and_then(|d| d.get("ProvisionedDevices")?.as_array().cloned()).unwrap_or_default()
        .iter().filter_map(|v| v.as_string().map(String::from)).collect()
}

/// The profile's keychain groups as it grants them (a wildcard profile's `TEAM.*`), as the
/// entitlements to keep: what `build-and-sign` signs with, so the app keeps its Keychain items.
fn profile_keychain_groups(profile: &[u8]) -> Option<plist::Dictionary> {
    let groups = profile_plist(profile)?.get("Entitlements")?.as_dictionary()?.get("keychain-access-groups")?.clone();
    let mut kept = plist::Dictionary::new();
    kept.insert("keychain-access-groups".into(), groups);
    Some(kept)
}

/// What the device should sign an `--unsigned` build with, from the conf's profile: its UUID, and
/// its App ID and team so a renewed copy (new UUID) still matches. Without it the device can only
/// guess, and an unsigned app has no signature to say it needs an explicit App ID's entitlements.
fn signing_hint(profile: &[u8]) -> Option<serde_json::Value> {
    let plist = profile_plist(profile)?;
    let team = plist.get("TeamIdentifier")?.as_array()?.first()?.as_string()?.to_string();
    let entitlements = plist.get("Entitlements")?.as_dictionary()?;
    let identifier = entitlements.get("application-identifier")?.as_string()?;
    let pattern = identifier.strip_prefix(&format!("{team}.")).unwrap_or(identifier);
    let groups: Vec<String> = entitlements.get("keychain-access-groups").and_then(plist::Value::as_array)
        .map(|g| g.iter().filter_map(|v| v.as_string().map(String::from)).collect()).unwrap_or_default();
    Some(serde_json::json!({
        "uuid": plist.get("UUID")?.as_string()?, "name": plist.get("Name").and_then(plist::Value::as_string),
        "appIdPattern": pattern, "team": team, "keychainGroups": groups,
    }))
}

/// Every bundle and library signed inside `app`: frameworks, dylibs, extensions, watch apps, itself.
fn signed_items(app: &Path) -> usize {
    fn count(dir: &Path) -> usize {
        let Ok(entries) = fs::read_dir(dir) else { return 0 };
        entries.filter_map(Result::ok).map(|e| {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            let own = usize::from([".framework", ".dylib", ".appex", ".app"].iter().any(|s| name.ends_with(s)));
            own + if e.file_type().is_ok_and(|k| k.is_dir()) { count(&path) } else { 0 }
        }).sum()
    }
    count(app) + 1
}

/// `--sign-dylib`: the files signed in place with the development certificate and the chain of
/// its profile. Nothing about the identity is printed. No build lock: `inject` signs while a build
/// may be running, and this writes nothing in `build/`.
fn sign_dylibs(conf: &Conf, profile: Option<&Path>, files: &[PathBuf]) -> Result<(), String> {
    let p12_path = conf.get("DEV_P12_PATH").ok_or_else(|| format!("Required variable DEV_P12_PATH is not set in {}", conf.label()))?;
    let profile = profile.map(Path::to_path_buf).or_else(|| conf.get("DEV_PROFILE_PATH").map(PathBuf::from))
        .ok_or_else(|| format!("Required variable DEV_PROFILE_PATH is not set in {}", conf.label()))?;
    for file in files {
        if !file.is_file() { return Err(format!("not a file: {}", file.display())) }
    }
    let work = WorkDir::new()?;
    let signing = work.0.join("signing");
    let p12 = fs::read(p12_path).map_err(|e| format!("couldn't read the .p12: {e}"))?;
    let password = conf.get("DEV_P12_PASSWORD").or(conf.get("P12_PASSWORD")).unwrap_or_default().to_string();
    let identity = api::import_identity(&signing, &p12, password).map_err(|e| format!("No code-signing identity in {p12_path} (is the password correct?): {}", api::describe(&e)))?;
    let identity_id = identity["identity"]["id"].as_str().ok_or("the imported certificate has no id")?.to_string();
    let imported = api::import_profile(&signing, fs::read(&profile).map_err(|e| e.to_string())?, None).map_err(|e| api::describe(&e))?;
    let uuid = imported["profile"]["uuid"].as_str().ok_or("the profile has no UUID")?.to_string();
    api::sign_code(&signing, &identity_id, &uuid, files).map_err(|e| api::describe(&e))?;
    for file in files {
        println!("signed {}", file.display());
    }
    Ok(())
}

pub async fn run(args: &[String]) -> Result<(), crate::Error> {
    let options = match parse(args) {
        Ok(Some(options)) => options,
        Ok(None) => { print!("{USAGE}"); return Ok(()) }
        Err(e) => return Err(crate::fail(e)),
    };
    if !cfg!(target_os = "macos") {
        return Err(crate::fail("apparition build runs Xcode, so it runs on a Mac; elsewhere build with xtool and send the IPA with `apparition deploy`"));
    }
    let mut build = Build::default();
    let result = build.go(options).await;
    if let Err(message) = &result {
        eprintln!();
        eprintln!("✗ {message}");
        if build.show_log {
            match log_excerpt(&build.log_file) {
                Some(excerpt) => eprintln!("---- error ----\n{excerpt}"),
                None => eprintln!("(no specific error line matched — see full log)"),
            }
        }
        if build.log_started {
            eprintln!("Full log: {}", build.log_file.display());
        }
    }
    result.map_err(|_| crate::fail("the build failed"))
}

#[derive(Default)]
struct Build {
    log_file: PathBuf,
    /// Whether the failure came from a step that logged (the error excerpt is worth printing).
    show_log: bool,
    /// Whether this run wrote the log (a failure points at it).
    log_started: bool,
}

/// What the report says about the launch.
enum Launched {
    None,
    Run,
    /// Installed, but the plain launch didn't happen (a headset not worn): exit `LAUNCH_FAILED`.
    Failed(String),
    Jit,
    Log { debug_api: Option<String>, note: Option<String> },
}

impl Build {
    async fn go(&mut self, o: Options) -> Result<(), String> {
        let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
        let root = Command::new("git").args(["rev-parse", "--show-toplevel"]).stderr(Stdio::null()).output().ok()
            .filter(|out| out.status.success()).map(|out| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
            .unwrap_or(cwd);
        let build_dir = root.join("build");
        fs::create_dir_all(&build_dir).map_err(|e| e.to_string())?;
        self.log_file = build_dir.join("last-build.log");

        let caller_bundle_id = std::env::var("BUILD_BUNDLE_ID").ok().filter(|v| !v.is_empty());
        let context = [("PROJECT_ROOT", root.display().to_string()), ("BUILD_DIR", build_dir.display().to_string())];
        let mut conf = Conf::load(&root, &conf::default_global(), &context)?;
        if !o.sign_dylibs.is_empty() {
            return sign_dylibs(&conf, o.profile.as_deref(), &o.sign_dylibs);
        }
        let platform = o.platform.clone().or_else(|| conf.get("PLATFORM").map(String::from)).unwrap_or_else(|| "visionOS".into());
        conf.set("PLATFORM", platform.clone());
        if let Some(device) = &o.device { conf.set("DEVICE_NAME", device.clone()); }
        let mut target = conf.get("TARGET_NAME").or(conf.get("SCHEME_NAME")).unwrap_or_default().to_string();
        let profile_path = o.profile.clone().or_else(|| conf.get("DEV_PROFILE_PATH").map(PathBuf::from));
        if let Some(forced) = &o.profile {
            if !forced.is_file() { return Err(format!("--profile {} not found", forced.display())) }
        }
        let (mut sign_only, mut no_deploy, mut input_ipa) = (o.sign_only, o.no_deploy, o.ipa.clone());
        let sdk = expected_sdk(&platform);
        let mac = sdk == Some("macosx");
        if mac {
            if o.run || o.mcp == Some(true) {
                return Err("macOS builds produce a .app for manual copying; --run/--log/--mcp are device-only options.".into());
            }
            if sign_only || input_ipa.is_some() {
                return Err("macOS mode builds a fresh .app; --sign-only/--ipa are for mobile IPA files.".into());
            }
            if o.unsigned {
                return Err("--unsigned is only supported for a fresh mobile build kept for device-side signing".into());
            }
            no_deploy = true;
        }
        if conf.get("EXTRA_ENTITLEMENTS_PLIST").is_some() || conf.get("ASSOCIATED_DOMAINS").is_some() {
            return Err("this repo's conf sets EXTRA_ENTITLEMENTS_PLIST or ASSOCIATED_DOMAINS, which apparition build doesn't apply yet; use build-and-sign".into());
        }

        // A launch is --run's; --mcp (or MCP_DEFAULT=on) makes it a debug session.
        let mut log_app = o.log;
        let mut mcp_session = false;
        if o.run && o.mcp != Some(false) && (o.mcp == Some(true) || conf.get("MCP_DEFAULT") == Some("on")) {
            log_app = true;
            mcp_session = true;
        } else if o.mcp == Some(true) && !o.run && !no_deploy {
            eprintln!("NOTE: --mcp without --run installs only; add --run to launch as a debug session");
        }

        let backend = o.inject_backend.clone().or_else(|| conf.get("INJECT_BACKEND").map(String::from)).unwrap_or_else(|| "dynamic".into());
        let derived_data = match (o.inject, backend.as_str()) {
            (false, _) => build_dir.join("DerivedData"),
            (true, "dynamic") => build_dir.join("DerivedData-inject"),
            (true, "apparition") => build_dir.join("DerivedData-inject-apparition"),
            _ => return Err("INJECT_BACKEND must be dynamic or apparition".into()),
        };
        if !sign_only && !mac && conf.get("PROJECT_PATH").is_none() {
            if let Some(found) = find_static_ipa(&root, &build_dir) {
                if target.is_empty() { target = found.file_stem().unwrap_or_default().to_string_lossy().into_owned(); }
                input_ipa = Some(found);
                sign_only = true;
            }
        }
        if sign_only && target.is_empty() {
            if let Some(ipa) = &input_ipa { target = ipa.file_stem().unwrap_or_default().to_string_lossy().into_owned(); }
        }
        let device_name = conf.get("DEVICE_NAME").map(String::from);
        if sign_only && !no_deploy && device_name.is_none() {
            no_deploy = true;
        }
        if o.unsigned && (sign_only || o.run) {
            return Err("--unsigned is only supported for a fresh mobile build kept for device-side signing".into());
        }
        let mut required: Vec<(&str, bool)> = Vec::new();
        if !o.unsigned {
            required.push(("DEV_P12_PATH", conf.get("DEV_P12_PATH").is_some()));
            if !mac { required.push(("DEV_PROFILE_PATH", profile_path.is_some())) }
        }
        if sign_only {
            required.push(("TARGET_NAME", !target.is_empty()));
        } else {
            for name in ["PROJECT_PATH", "SCHEME_NAME"] { required.push((name, conf.get(name).is_some())); }
        }
        if !no_deploy || o.run {
            required.push(("DEVICE_NAME", device_name.is_some()));
        }
        if let Some((name, _)) = required.iter().find(|(_, set)| !set) {
            return Err(format!("Required variable {name} is not set in {}", conf.label()));
        }
        let appex_profile = conf.get("DEV_APPEX_PROFILE_PATH").map(PathBuf::from);
        let watch_profile = conf.get("DEV_WATCH_PROFILE_PATH").map(PathBuf::from);
        if !o.unsigned {
            let profiles = if mac { Vec::new() } else { vec![profile_path.clone(), appex_profile.clone(), watch_profile.clone()] };
            for file in std::iter::once(conf.get("DEV_P12_PATH").map(PathBuf::from)).chain(profiles).flatten() {
                if !file.is_file() { return Err(format!("File not found: {}", file.display())) }
            }
        }
        if o.plan {
            // What a Store needs to build one platform row once and install it on several devices:
            // rows that resolve alike are one build (the `iPad` alias is the iOS build).
            let profile = profile_path.as_ref().and_then(|p| fs::read(p).ok());
            let mut settings = conf.extra_build_settings.clone();
            settings.extend(o.settings.iter().cloned());
            let plan = serde_json::json!({
                "platform": platform, "sdk": sdk, "device": device_name, "project": conf.get("PROJECT_PATH"), "scheme": conf.get("SCHEME_NAME"),
                "target": target, "bundleId": conf.get("BUILD_BUNDLE_ID"), "settings": settings,
                "profile": profile_path, "certificate": conf.get("DEV_P12_PATH"),
                "profileDevices": profile.as_deref().map(profile_devices).unwrap_or_default(),
            });
            println!("{plan}");
            return Ok(());
        }
        // The device, from this computer's pairings: no network yet, and an unpaired name fails
        // before a long build rather than after.
        let device = match (&device_name, !no_deploy || o.run) {
            (Some(name), true) => Some(crate::find_device(Some(name)).map_err(|e| format!("{}; pair it with `apparition pair` (iPhone or iPad: `pair --usb`)", api::describe(&e)))?),
            _ => None,
        };

        let _lock = BuildLock::acquire(&build_dir, &root)?;
        let log = fs::File::create(&self.log_file).map_err(|e| e.to_string())?;
        self.log_started = true;
        let work = WorkDir::new()?;
        let build_id = new_build_id();
        let env = conf.environment.clone();
        let (mut before, mut after) = (None, None);
        let mut app_path: Option<PathBuf> = None;
        let mut inject_dir: Option<PathBuf> = None;
        let unsigned_ipa = build_dir.join(format!("{target}.ipa"));
        // A Mac app's copy that is signed and kept (`build/<product>-macOS.app`), and its entitlements.
        let mut mac_app: Option<PathBuf> = None;
        let mut mac_entitlements = plist::Dictionary::new();

        // Step 1: build.
        if !sign_only {
            prune_legacy_derived_data(&build_dir);
            let project = conf.get("PROJECT_PATH").unwrap_or_default().to_string();
            let flag = if project.ends_with(".xcworkspace") { "-workspace" } else if project.ends_with(".xcodeproj") { "-project" } else {
                return Err("PROJECT_PATH must end in .xcodeproj or .xcworkspace".into());
            };
            let resolved = [
                ("PROJECT_ROOT", root.display().to_string()), ("BUILD_DIR", build_dir.display().to_string()),
                ("PLATFORM", platform.clone()), ("DEVICE_NAME", device_name.clone().unwrap_or_default()),
                ("CONFIG", o.config.to_string()), ("INJECT", o.inject.to_string()),
            ];
            if conf.get("PRE_BUILD_HOOK").is_some() {
                writeln!(&log, "==> Running pre-build hook: {}", conf.get("PRE_BUILD_HOOK").unwrap_or_default()).ok();
                self.show_log = true;
                conf.run_hook(&resolved, &log)?;
                self.show_log = false;
            }
            let mut xcb: Vec<String> = vec![
                flag.into(), project, "-scheme".into(), conf.get("SCHEME_NAME").unwrap_or_default().into(),
                "-configuration".into(), o.config.into(), "-destination".into(), format!("generic/platform={platform}"),
                "-derivedDataPath".into(), derived_data.display().to_string(),
                "CODE_SIGN_IDENTITY=-".into(), "CODE_SIGNING_REQUIRED=NO".into(), "CODE_SIGNING_ALLOWED=NO".into(),
                "DEVELOPMENT_TEAM=".into(), "PROVISIONING_PROFILE_SPECIFIER=".into(),
            ];
            xcb.extend(conf.extra_build_settings.iter().cloned());
            xcb.extend(o.settings.iter().cloned());
            if o.inject {
                if conf.get("DEBUGTRACE_INJECT") == Some("off") {
                    return Err("this repo's build-signing.conf sets DEBUGTRACE_INJECT=off, so --inject is refused".into());
                }
                let flags: &[&str] = if backend == "apparition" {
                    &["ENABLE_TESTABILITY=YES", "DEAD_CODE_STRIPPING=NO", "OTHER_SWIFT_FLAGS=$(inherited) -Xfrontend -enable-private-imports -save-optimization-record"]
                } else if conf.get("INJECT_SCOPE") == Some("app") {
                    &["DEAD_CODE_STRIPPING=NO", "DTINJECT_TESTABILITY_app=YES", "ENABLE_TESTABILITY=$(DTINJECT_TESTABILITY_$(WRAPPER_EXTENSION))",
                      "DTINJECT_FLAGS_app=-Xfrontend -enable-implicit-dynamic -Xfrontend -enable-private-imports",
                      "OTHER_SWIFT_FLAGS=$(inherited) $(DTINJECT_FLAGS_$(WRAPPER_EXTENSION))"]
                } else {
                    &["ENABLE_TESTABILITY=YES", "DEAD_CODE_STRIPPING=NO", "OTHER_SWIFT_FLAGS=$(inherited) -Xfrontend -enable-implicit-dynamic -Xfrontend -enable-private-imports"]
                };
                xcb.extend(flags.iter().map(|f| f.to_string()));
            }
            let arguments: Vec<u8> = xcb.iter().flat_map(|a| a.bytes().chain([0])).collect();
            before = Some(provenance::snapshot(&root, Some(&arguments), Some(&env))?);
            let xcodebuild = |extra: &[&str]| {
                let mut command = Command::new("xcodebuild");
                command.env_clear().envs(env.iter().map(|(k, v)| (k, v))).current_dir(&root).args(extra).args(&xcb);
                command
            };
            println!("==> Building for {platform} ({}){}...", o.config, if o.inject { " for live patching" } else { "" });
            let _ = std::io::stdout().flush();
            self.show_log = true;
            // Not -quiet for --inject: the compiler's command lines in the log are what
            // `inject` replays to compile a patch as the app was compiled.
            let mut command = if o.inject { xcodebuild(&[]) } else { xcodebuild(&["-quiet"]) };
            run_logged(command.arg("build"), &log, "xcodebuild")?;
            self.show_log = false;
            println!("  Build succeeded.");

            let settings = locate_product(&mut xcodebuild(&["-showBuildSettings", "-json"]), &target);
            let mut product = settings.as_ref().and_then(|s| s.app.clone());
            if let Some(path) = product.as_ref().filter(|p| !p.is_dir()) {
                eprintln!("WARNING: xcodebuild reported a product that is not on disk: {}", path.display());
                product = None;
            }
            let products = derived_data.join("Build/Products").join(if mac { o.config.to_string() } else { format!("{}-{}", o.config, sdk.unwrap_or("*")) });
            if mac {
                if let Some(file) = settings.as_ref().and_then(|s| s.entitlements.clone()) {
                    let file = if Path::new(&file).is_absolute() { PathBuf::from(file) }
                        else { settings.as_ref().and_then(|s| s.project_dir.clone()).unwrap_or_else(|| root.clone()).join(file) };
                    if !file.is_file() { return Err(format!("Xcode entitlements file not found: {}", file.display())) }
                    mac_entitlements = plist::Value::from_file(&file).ok().and_then(plist::Value::into_dictionary)
                        .ok_or_else(|| format!("{} isn't a readable entitlements file", file.display()))?;
                }
            }
            let product = product
                .or_else(|| Some(products.join(format!("{target}.app"))).filter(|p| p.is_dir()))
                .or_else(|| first_app(&products).inspect(|p| println!("WARNING: {target}.app not found; using {} (product name != TARGET_NAME?)", p.file_name().unwrap_or_default().to_string_lossy())))
                .ok_or_else(|| format!("No .app bundle found in {}", products.display()))?;
            let tool = conf.get("DEBUGTRACE_INJECT_TOOL").map(PathBuf::from)
                .unwrap_or_else(|| home().join("Projects/DebugTrace/Tools/debugtrace-inject/debugtrace-inject"));
            if o.inject {
                if !tool.is_file() {
                    return Err(format!("--inject needs {} (DebugTrace); set DEBUGTRACE_INJECT_TOOL if it lives elsewhere", tool.display()));
                }
                let dir = build_dir.join("inject");
                run_logged(Command::new(&tool).arg("capture").arg("--log").arg(&self.log_file).arg("--out").arg(&dir).arg("--root").arg(&root)
                    .arg("--app-module").arg(&target).arg("--products").arg(product.parent().unwrap_or(&product))
                    .arg("--scope").arg(conf.get("INJECT_SCOPE").unwrap_or("all")).arg("--backend").arg(&backend)
                    .arg("--optimization-records").arg(derived_data.join("Build/Intermediates.noindex")), &log, "recording the build for live patching")?;
                inject_dir = Some(dir);
            }
            after = Some(provenance::snapshot(&root, Some(&arguments), Some(&env))?);
            // An earlier build-and-sign embedded the patch runtime in DerivedData's product itself.
            let _ = fs::remove_file(product.join("Frameworks/DebugTraceInject.dylib"));
            let _ = fs::remove_file(product.join("Contents/Frameworks/DebugTraceInject.dylib"));
            let packaged = if mac {
                // Named after the product, not TARGET_NAME (OneirosDesktop builds Oneiros.app).
                let copy = build_dir.join(format!("{}-macOS.app", product.file_stem().unwrap_or_default().to_string_lossy()));
                let _ = fs::remove_dir_all(&copy);
                clone_tree(&product, &copy)?;
                mac_app = Some(copy.clone());
                copy
            } else {
                let payload = build_dir.join("Payload");
                let _ = fs::remove_dir_all(&payload);
                fs::create_dir_all(&payload).map_err(|e| e.to_string())?;
                clone_tree(&product, &payload)?;
                payload.join(product.file_name().unwrap_or_default())
            };
            if let Some(dir) = &inject_dir {
                let frameworks = if mac { packaged.join("Contents/Frameworks") } else { packaged.join("Frameworks") };
                fs::create_dir_all(&frameworks).map_err(|e| e.to_string())?;
                run_logged(Command::new(&tool).arg("runtime").arg("--dir").arg(dir).arg("--out").arg(frameworks.join("DebugTraceInject.dylib")),
                    &log, "building the live-patching runtime")?;
            }
            if !mac {
                zip_payload(&build_dir, &unsigned_ipa, &log)?;
            }
            app_path = Some(product);
        }

        // Step 4: the app to sign, in the scratch folder.
        let unpack = work.0.join("unpack");
        fs::create_dir_all(&unpack).map_err(|e| e.to_string())?;
        if sign_only {
            let ipa = input_ipa.clone().unwrap_or_else(|| unsigned_ipa.clone());
            if !ipa.is_file() { return Err(format!("IPA not found at {}", ipa.display())) }
            run_logged(Command::new("/usr/bin/unzip").arg("-qo").arg(&ipa).arg("-d").arg(&unpack), &log, "unzip")?;
        } else if !mac {
            clone_tree(&build_dir.join("Payload"), &unpack)?;
        }
        // A Mac app is signed where it is kept; a mobile one in the scratch folder, then zipped.
        let app = match &mac_app { Some(app) => app.clone(), None => first_app(&unpack.join("Payload")).ok_or("No .app found in IPA")? };
        let info_path = bundle::info_path(&app);
        let info = bundle::read_info(&info_path)?;
        let original_bundle_id = bundle::string(&info, "CFBundleIdentifier").unwrap_or_default();

        // Step 4.1: never sign a bundle built for another platform.
        if let Some(expected) = sdk {
            let mut built = bundle::string(&info, "DTPlatformName");
            let ipad = info.get("UIDeviceFamily").and_then(plist::Value::as_array)
                .is_some_and(|f| f.iter().any(|v| v.as_unsigned_integer() == Some(2)));
            if input_ipa.is_some() && built.as_deref() == Some("iphoneos") && expected == "xros" && ipad {
                println!("==> iPad build on a visionOS device: it will run in iPad compatibility mode.");
                built = Some(expected.into());
            }
            if let Some(built) = built.filter(|b| b != expected) {
                return Err(format!(
                    "this bundle was built for '{built}', but the requested platform is '{platform}' (SDK '{expected}').\n       Bundle: {}\n       {}",
                    app.file_name().unwrap_or_default().to_string_lossy(),
                    if sign_only { "This run re-signs an existing IPA; that IPA is from another platform's build.".to_string() }
                    else { format!("Stale DerivedData product: remove {}/Build/Products and rebuild.", derived_data.display()) }
                ));
            }
        }

        // Step 4.5: bundle IDs, per bundle (a command-line PRODUCT_BUNDLE_IDENTIFIER would hit
        // every target). An --ipa keeps its own unless the caller set one.
        let bundle_id = if input_ipa.is_some() && caller_bundle_id.is_none() { original_bundle_id.clone() }
            else { conf.get("BUILD_BUNDLE_ID").map(String::from).unwrap_or_else(|| original_bundle_id.clone()) };
        let (has_debugtrace, has_server) = bundle::links_debugtrace(&app, &info);
        bundle::set_string(&info_path, "CFBundleIdentifier", &bundle_id)?;
        let appex = bundle::extension(&app);
        let mut extension_id = None;
        if let Some(appex) = &appex {
            let plist = appex.join("Info.plist");
            if input_ipa.is_some() && caller_bundle_id.is_none() && conf.get("APPEX_BUNDLE_ID").is_none() {
                extension_id = bundle::string(&bundle::read_info(&plist)?, "CFBundleIdentifier");
            } else {
                let id = conf.get("APPEX_BUNDLE_ID").map(String::from).unwrap_or_else(|| format!("{bundle_id}.broadcast"));
                bundle::set_string(&plist, "CFBundleIdentifier", &id)?;
                extension_id = Some(id);
            }
        }
        let watch = bundle::watch_app(&app);
        let watch_id = watch.as_ref().and_then(|w| bundle::read_info(&w.join("Info.plist")).ok()).and_then(|i| bundle::string(&i, "CFBundleIdentifier"));

        // Step 4.6: the source commit as the build number, for any inventory to show.
        let mut git_stamp = String::new();
        if let Some(before) = &before {
            git_stamp = provenance::stamp(before, &root)?;
            if !git_stamp.is_empty() {
                bundle::set_string(&info_path, "CFBundleVersion", &git_stamp)?;
                for other in [&appex, &watch].into_iter().flatten() {
                    bundle::set_string(&other.join("Info.plist"), "CFBundleVersion", &git_stamp)?;
                }
            }
        }

        // Step 4.7: a DebugTrace app's own trace key and debug-server token.
        let state = conf.get("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/state"));
        let mut credential = None;
        if has_debugtrace {
            let ledger = conf.get("DEBUGTRACE_KEY_DIR").map(PathBuf::from).unwrap_or_else(|| state.join("debugtrace/keys"));
            let branch = Command::new("git").arg("-C").arg(&root).args(["rev-parse", "--abbrev-ref", "HEAD"]).stderr(Stdio::null()).output()
                .ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            let upload = conf.get("DEBUGTRACE_UPLOAD_URL");
            let entry = bundle::LedgerEntry { bundle_id: &bundle_id, platform: &platform, configuration: o.config, project: &root, branch: &branch, device: device_name.as_deref().unwrap_or("") };
            let made = bundle::embed_credential(&app, &build_id, has_server && conf.get("DEBUGTRACE_SERVER") != Some("off"), upload, &ledger, entry)?;
            println!("==> Trace key {}{}", made.key_id, upload.map(|u| format!(" (uploads to {u})")).unwrap_or_default());
            if made.command_token.is_some() {
                bundle::add_debug_server_keys(&app)?;
                println!("==> Debug server on every launch (attach with the apps MCP tools or the Apparition Store page)");
            }
            credential = Some(made);
        }

        // Steps 5 and 6: sign with the conf's certificate and profiles, through the core.
        let mut signed_count = 0;
        if o.unsigned {
            println!("==> Keeping unsigned IPA for device-side signing");
        } else if mac {
            println!("==> Signing...");
            let signing = work.0.join("signing");
            let p12 = fs::read(conf.get("DEV_P12_PATH").unwrap_or_default()).map_err(|e| format!("couldn't read the .p12: {e}"))?;
            let password = conf.get("DEV_P12_PASSWORD").or(conf.get("P12_PASSWORD")).unwrap_or_default().to_string();
            let identity = api::import_identity(&signing, &p12, password).map_err(|e| format!("No code-signing identity in {} (is the password correct?): {}", conf.get("DEV_P12_PATH").unwrap_or_default(), api::describe(&e)))?;
            let identity_id = identity["identity"]["id"].as_str().ok_or("the imported certificate has no id")?.to_string();
            api::sign_mac_app(&signing, &identity_id, &app, mac_entitlements.clone()).map_err(|e| format!("Signing failed: {}", api::describe(&e)))?;
            signed_count = signed_items(&app);
        } else {
            println!("==> Signing...");
            let signing = work.0.join("signing");
            let p12 = fs::read(conf.get("DEV_P12_PATH").unwrap_or_default()).map_err(|e| format!("couldn't read the .p12: {e}"))?;
            let password = conf.get("DEV_P12_PASSWORD").or(conf.get("P12_PASSWORD")).unwrap_or_default().to_string();
            let identity = api::import_identity(&signing, &p12, password).map_err(|e| format!("No code-signing identity in {} (is the password correct?): {}", conf.get("DEV_P12_PATH").unwrap_or_default(), api::describe(&e)))?;
            let identity_id = identity["identity"]["id"].as_str().ok_or("the imported certificate has no id")?.to_string();
            let main_profile = fs::read(profile_path.as_ref().unwrap()).map_err(|e| e.to_string())?;
            let imported = api::import_profile(&signing, main_profile.clone(), None).map_err(|e| api::describe(&e))?;
            let profile_uuid = imported["profile"]["uuid"].as_str().ok_or("the profile has no UUID")?.to_string();
            for extra in [&appex_profile, &watch_profile].into_iter().flatten() {
                api::import_profile(&signing, fs::read(extra).map_err(|e| e.to_string())?, None).map_err(|e| api::describe(&e))?;
            }
            let (udid, core) = match &device {
                Some(d) => (d.udid.clone(), d.platform()),
                None => (
                    profile_devices(&main_profile).into_iter().next().ok_or("the profile lists no devices to sign for")?,
                    core_platform(&platform).ok_or_else(|| format!("apparition build can't sign for {platform}"))?,
                ),
            };
            let reply = api::sign_offline_keeping(&signing, app.clone(), &identity_id, Some(&profile_uuid), &udid, false, None, profile_keychain_groups(&main_profile), core)
                .map_err(|e| format!("Signing failed: {}", api::describe(&e)))?;
            writeln!(&log, "==> Signed with profile {} ({})", reply["profileName"].as_str().unwrap_or("?"), reply["profile"].as_str().unwrap_or("?")).ok();
            signed_count = signed_items(&app);
        }

        // Step 7: repack (a Mac app stays the signed .app).
        let signed_ipa = build_dir.join(format!("{target}-{}.ipa", if o.unsigned { "unsigned" } else { "signed" }));
        let artifact = match &mac_app {
            Some(app) => app.clone(),
            None => { zip_payload(&unpack, &signed_ipa, &log)?; signed_ipa.clone() }
        };

        // Step 8.5: keep the dSYMs and the build before anything is installed.
        let mut symbols = None;
        if has_debugtrace && !sign_only {
            if let Some(product) = &app_path {
                let archive = conf.get("DEBUGTRACE_DSYM_DIR").map(PathBuf::from).unwrap_or_else(|| state.join("debugtrace/dsyms"));
                let build_number = bundle::read_info(&info_path).ok().and_then(|i| bundle::string(&i, "CFBundleVersion")).unwrap_or_default();
                match keep::archive_dsyms(keep::Symbols {
                    archive: &archive, products: product.parent().unwrap_or(product), app: product, build_id: &build_id,
                    key_id: credential.as_ref().map(|c| c.key_id.as_str()).unwrap_or(""), bundle_id: &bundle_id,
                    device: device_name.as_deref().unwrap_or("local"), build: &build_number, platform: &platform, configuration: o.config,
                }) {
                    Ok(path) => symbols = path,
                    Err(e) => eprintln!("WARNING: could not keep the dSYMs of this build: {e}"),
                }
            }
        }
        let store = conf.get("APPARITION_BUILD_DIR").map(PathBuf::from).unwrap_or_else(|| state.join("apparition/builds"));
        let manifest = keep::publish(keep::Publish {
            store: &store, app: &app, artifact: &artifact, project: &root, before, after, build_id: &build_id,
            key_id: credential.as_ref().map(|c| c.key_id.as_str()), platform: &platform, configuration: o.config,
            original_bundle_id: &original_bundle_id, symbols: symbols.as_deref(),
            signing: if o.unsigned && !mac { profile_path.as_ref().and_then(|p| fs::read(p).ok()).as_deref().and_then(signing_hint) } else { None },
        }).map_err(|e| format!("Build manifest: {e}"))?;
        // Said now, not only in the report: the Store ties the kept build to its job by this line.
        println!("  {:<12} {build_id}", "Build ID:");
        let _ = std::io::stdout().flush();

        // Step 9: install, over Apparition's own pairing.
        let mut installed = false;
        if !no_deploy {
            let device = device.as_ref().ok_or("no device to install to")?;
            println!("==> Installing to {}...", device.name);
            let _ = std::io::stdout().flush();
            let mut tunnel = crate::store().connect_verified(device).await.map_err(|e| format!("{} didn't answer: {}", device.name, api::describe(&e)))?;
            let existing = tunnel.installed_app(&bundle_id).await.map_err(|e| api::describe(&e))?.is_some();
            let reply = tunnel.install_signed_ipa(&signed_ipa, Some(&bundle_id), if existing { 20_000 } else { 0 }).await
                .map_err(|e| format!("Install through Apparition failed:\n    {}", api::describe(&e)))?;
            if reply["queued"].as_bool() == Some(true) {
                println!("    {} holds the update until the app is closed", device.name);
            }
            installed = true;
        }

        // Step 10: launch.
        let app_log = build_dir.join("device-console.log");
        let pidfile = build_dir.join("device-console.pid");
        let mut launched = Launched::None;
        if o.run {
            let device = device.as_ref().ok_or("no device to launch on")?;
            launch::stop_previous(&pidfile);
            // DEVICECTL_CHILD_<KEY> from the caller reaches the app too, as with build-and-sign.
            let mut environment: Vec<(String, String)> = std::env::vars()
                .filter_map(|(k, v)| k.strip_prefix("DEVICECTL_CHILD_").filter(|k| !k.is_empty()).map(|k| (k.to_string(), v)))
                .filter(|(k, _)| !["DEBUGTRACE_TOKEN", "DEBUGTRACE_STDERR", "OS_ACTIVITY_DT_MODE", "DEBUGTRACE_SERVER"].contains(&k.as_str()))
                .collect();
            if log_app {
                println!("==> Launching {bundle_id} (console → {})...", app_log.display());
                fs::write(&app_log, "").map_err(|e| e.to_string())?;
                let _ = fs::remove_file(build_dir.join("debug-server.txt"));
                let mut token = None;
                if mcp_session {
                    if !has_server {
                        if o.mcp == Some(true) { eprintln!("WARNING: --mcp: {bundle_id} doesn't link DebugTraceServer; launching with --log only") }
                        mcp_session = false;
                    } else {
                        token = Some(credential.as_ref().and_then(|c| c.command_token.clone())
                            .ok_or_else(|| format!("--mcp: {bundle_id} has no signed DebugTrace CommandToken"))?);
                    }
                }
                // A DebugTrace app mirrors only its own lines to stderr, compact and redacted;
                // anything else gets the unified log mirrored (OS_ACTIVITY_DT_MODE).
                if has_debugtrace && !o.log_all { environment.push(("DEBUGTRACE_STDERR".into(), "1".into())) }
                else { environment.push(("OS_ACTIVITY_DT_MODE".into(), "YES".into())) }
                if mcp_session { environment.push(("DEBUGTRACE_SERVER".into(), "1".into())) }
                let pairs: Vec<(&str, &str)> = environment.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let pid = launch::detached(launch::Detached {
                    bundle_id: &bundle_id, device: &device.name, console: true, jit: o.jit, environment: &pairs,
                    log: &app_log, pidfile: &pidfile, wait: Duration::from_secs(20),
                })?;
                let (mut debug_api, mut note) = (None, None);
                if let (true, Some(token)) = (mcp_session, token) {
                    let address = device.name.to_lowercase();
                    match launch::debug_server_port(&app_log, Duration::from_secs(20)) {
                        None => note = Some(format!("not up yet (no 'listening on port' line in 20 s); device address: {address}")),
                        Some(port) if !launch::answers(&address, port) => note = Some(format!("listening on port {port}, but not reachable at: {address}")),
                        Some(port) => {
                            let url = launch::url(&address, port);
                            let app_name = bundle::string(&bundle::read_info(&info_path)?, "CFBundleDisplayName")
                                .or_else(|| bundle::read_info(&info_path).ok().and_then(|i| bundle::string(&i, "CFBundleName")))
                                .unwrap_or_else(|| bundle_id.rsplit('.').next().unwrap_or(&bundle_id).to_string());
                            launch::write_session(launch::Session {
                                dir: &state.join("debugtrace/sessions"), bundle_id: &bundle_id, app_name: &app_name, device: &device.name,
                                device_id: &device.udid, url: &url, pid, console_log: &app_log, project_root: &root, token: &token,
                                key_id: credential.as_ref().map(|c| c.key_id.as_str()), inject_dir: inject_dir.as_deref(),
                                platform: &platform, jit: o.jit,
                            })?;
                            fs::write(build_dir.join("debug-server.txt"), format!(
                                "# Debug server for {bundle_id} (this launch only)\nurl: {url}\nagents: use the `apps` MCP tools (apps, help, call, trace); the session is registered\nhelp: curl -s {url}/   (the rest needs the session's bearer token)\n"
                            )).map_err(|e| e.to_string())?;
                            debug_api = Some(url);
                        }
                    }
                }
                launched = Launched::Log { debug_api, note };
            } else if o.jit {
                println!("==> Launching {bundle_id} with JIT (session → {})...", app_log.display());
                fs::write(&app_log, "").map_err(|e| e.to_string())?;
                let pairs: Vec<(&str, &str)> = environment.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                launch::detached(launch::Detached {
                    bundle_id: &bundle_id, device: &device.name, console: false, jit: true, environment: &pairs,
                    log: &app_log, pidfile: &pidfile, wait: Duration::from_secs(20),
                })?;
                launched = Launched::Jit;
            } else {
                println!("==> Launching {bundle_id} (through Apparition's tunnel)...");
                let mut tunnel = crate::store().connect_verified(device).await.map_err(|e| format!("{} didn't answer: {}", device.name, api::describe(&e)))?;
                // A headset that isn't worn holds a launch without answering: give up after 30 s.
                launched = match tokio::time::timeout(Duration::from_secs(30), tunnel.launch(&bundle_id, &[], &environment, false, false)).await {
                    Ok(Ok(_)) => Launched::Run,
                    Ok(Err(e)) => Launched::Failed(format!("Failed to launch {bundle_id} on {}: {}", device.name, api::describe(&e))),
                    Err(_) => Launched::Failed(format!("{} didn't launch {bundle_id} within 30 s. Put it on or unlock it, then launch the app (it is installed).", device.name)),
                };
            }
        }

        // The report, as build-and-sign prints it.
        let line = |label: &str, value: &str| println!("  {label:<12} {value}");
        println!();
        println!("=================== Install Report ====================");
        line("App:", &bundle_id);
        if let Some(id) = &extension_id { line("Extension:", id) }
        if let Some(id) = &watch_id { line("Watch:", id) }
        if !git_stamp.is_empty() { line("Build:", &git_stamp) }
        line("Build ID:", &build_id);
        line("Manifest:", &manifest.display().to_string());
        line("Config:", o.config);
        line("Signed:", &format!("{signed_count} items"));
        if mac {
            line("App path:", &artifact.display().to_string());
            line("Next step:", "copy this .app wherever you want");
        } else {
            line("IPA:", &signed_ipa.display().to_string());
        }
        let device_label = device.as_ref().map(|d| d.name.clone()).or(device_name.clone()).unwrap_or_default();
        if installed { line("Status:", &format!("✓ installed on {device_label}")) }
        else if !mac { line("Status:", &format!("not deployed (install with: apparition push '{}' --device <name>)", signed_ipa.display())) }
        let launch_failed = match &launched { Launched::Failed(why) => Some(why.clone()), _ => None };
        match launched {
            Launched::None => {}
            Launched::Run => line("Launched:", &format!("✓ running on {device_label}")),
            Launched::Failed(why) => line("Launched:", &format!("✗ {why}")),
            Launched::Jit => {
                line("Launched:", &format!("✓ running on {device_label}, JIT served by Apparition"));
                line("JIT log:", &app_log.display().to_string());
                line("Stop JIT:", &format!("kill $(cat {})  (detaches; the app keeps running)", pidfile.display()));
            }
            Launched::Log { debug_api, note } => {
                line("Launched:", &format!("✓ running on {device_label} with console attached"));
                if o.jit { line("JIT:", "served by Apparition for as long as the console runs") }
                line("App log:", &app_log.display().to_string());
                line("Stop log:", &format!("kill $(cat {})", pidfile.display()));
                if let Some(url) = debug_api { line("Debug API:", &format!("{url}  (agents: the `apps` MCP tools)")) }
                else if let Some(note) = note { line("Debug API:", &note) }
            }
        }
        println!("=========================================================");
        if let Some(why) = launch_failed {
            EXIT_CODE.store(LAUNCH_FAILED, std::sync::atomic::Ordering::SeqCst);
            return Err(why);
        }
        Ok(())
    }
}

/// The exit status of a build that installed but couldn't launch (the Store notes it as built
/// and installed, with a warning). Any other failure is 1.
pub const LAUNCH_FAILED: u8 = 3;
static EXIT_CODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The status a failed `apparition build` exits with, when it isn't 1.
pub fn exit_code() -> Option<u8> {
    Some(EXIT_CODE.load(std::sync::atomic::Ordering::SeqCst)).filter(|c| *c != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse_as_build_and_sign_s() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let o = parse(&args(&["--debug", "--log", "--set", "A=B", "--device", "AVP", "--inject-backend", "apparition"])).unwrap().unwrap();
        assert_eq!(o.config, "Debug");
        assert!(o.run && o.log && o.inject && !o.log_all);
        assert_eq!(o.settings, ["A=B"]);
        assert_eq!(o.device.as_deref(), Some("AVP"));
        assert_eq!(o.inject_backend.as_deref(), Some("apparition"));
        let o = parse(&args(&["--ipa", "x.ipa", "--no-mcp", "--unsigned"])).unwrap().unwrap();
        assert!(o.sign_only && o.no_deploy && o.unsigned && o.mcp == Some(false));
        assert!(parse(&args(&["--help"])).unwrap().is_none());
        assert!(parse(&args(&["--plan"])).unwrap().unwrap().plan);
        assert!(parse(&args(&["--distribution"])).unwrap_err().contains("--distribution isn't supported"));
        let o = parse(&args(&["--profile", "p.mobileprovision", "--sign-dylib", "a.dylib", "b.dylib"])).unwrap().unwrap();
        assert_eq!(o.sign_dylibs, [PathBuf::from("a.dylib"), PathBuf::from("b.dylib")]);
        assert!(parse(&args(&["--sign-dylib"])).unwrap_err().contains("needs the files"));
        assert!(parse(&args(&["--bogus"])).unwrap_err().contains("Unknown option"));
        assert!(parse(&args(&["--set"])).unwrap_err().contains("needs a value"));
    }

    #[test]
    fn noise_never_counts_as_the_error() {
        assert!(is_noise("2026 DTDKRemoteDeviceConnection: failed error: x"));
        assert!(is_noise("  12  DTDeviceKitBase  0x0001 error:"));
        assert!(is_noise("3 libdispatch.dylib error:"));
        assert!(!is_noise("/p/App.swift:3:1: error: cannot find 'x' in scope"));
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        fs::write(&log, "ok\n/p/A.swift:1:1: error: nope\n4 IDEFoundation error: noise\n** BUILD FAILED **\n").unwrap();
        assert_eq!(log_excerpt(&log).unwrap(), "/p/A.swift:1:1: error: nope\n** BUILD FAILED **");
        fs::write(&log, "fine\n").unwrap();
        assert_eq!(log_excerpt(&log), None);
    }

    #[test]
    fn dates_are_utc_and_well_formed() {
        let now = utc_now();
        assert_eq!(now.seconds.len(), 20);
        assert!(now.seconds.ends_with('Z') && now.millis.ends_with('Z') && now.offset.ends_with("+00:00"));
        assert_eq!(&now.seconds[..19], &now.offset[..19]);
        let id = new_build_id();
        assert_eq!(id.len(), 32);
        assert_eq!(&id[12..13], "4");
    }

    #[test]
    fn a_profile_s_devices_come_out_of_its_envelope() {
        let profile = b"\x30\x80junk<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>ProvisionedDevices</key><array><string>00008112-A</string><string>B</string></array></dict></plist>\x00\x00tail";
        assert_eq!(profile_devices(profile), ["00008112-A", "B"]);
        assert!(profile_devices(b"nothing").is_empty());
        let wildcard = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Entitlements</key><dict><key>keychain-access-groups</key><array><string>TEAM.*</string><string>com.apple.token</string></array></dict></dict></plist>";
        let kept = profile_keychain_groups(wildcard).unwrap();
        let groups: Vec<&str> = kept["keychain-access-groups"].as_array().unwrap().iter().filter_map(|v| v.as_string()).collect();
        assert_eq!(groups, ["TEAM.*", "com.apple.token"]);
    }

    #[test]
    fn a_static_ipa_is_never_taken_from_build() {
        let dir = tempfile::tempdir().unwrap();
        let build = dir.path().join("build");
        fs::create_dir_all(&build).unwrap();
        fs::create_dir_all(dir.path().join("iOS/out")).unwrap();
        fs::write(build.join("App-signed.ipa"), "old").unwrap();
        assert_eq!(find_static_ipa(dir.path(), &build), None);
        fs::write(dir.path().join("iOS/out/App.IPA"), "new").unwrap();
        assert_eq!(find_static_ipa(dir.path(), &build), Some(dir.path().join("iOS/out/App.IPA")));
    }

    #[test]
    fn a_second_build_waits_for_the_lock_and_a_dead_holder_s_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let lock_dir = dir.path().join(".build-and-sign.lock");
        fs::create_dir(&lock_dir).unwrap();
        // A pid that isn't running, or isn't a build: stale.
        fs::write(lock_dir.join("pid"), "999999\n").unwrap();
        let lock = BuildLock::acquire(dir.path(), dir.path()).unwrap();
        assert_eq!(fs::read_to_string(lock_dir.join("pid")).unwrap().trim(), std::process::id().to_string());
        drop(lock);
        assert!(!lock_dir.exists());
    }
}
