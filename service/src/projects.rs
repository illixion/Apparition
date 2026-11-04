//! Projects: what a checkout on this computer builds, adding it to `config.json`, the install a
//! device reports for itself, and the Xcode handoff's optional scheme overlay. Port of the Node
//! store's `project-discovery.js`, `project-registration.js`, `installs.js`, `projects.js` and the
//! pure part of `xcode.js`.
//!
//! Everything here takes and returns plain JSON values and errors that carry the HTTP status the
//! route replies with. Reads only until registration: `git` and `xcodebuild -list/-showBuildSettings`.
//! Discovery never builds, signs or sources a conf. The generated `build-signing.conf` is the
//! device-side minimum (project, scheme, platform, device): no signing material, and kept out of Git
//! through the clone's local exclude.
//!
//! What is not here from `xcode.js`: `capture`, `register`, `signature` and `platformOf` (an
//! existing Xcode product handed to the kept builds). They need the artifact snapshot, publish and
//! Mach-O index code, which belong to `artifacts.rs`. The local package references (`localPackageRefs`)
//! are `clones::package_refs`; `sibling_packages` below builds on it.

use crate::{clones::package_refs, updates::normalize};
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// The aliases the build and install code already uses (see examples/build-signing.conf.example).
const PLATFORM_ALIASES: &[(&str, &str)] = &[("iphoneos", "iOS"), ("xros", "visionOS"), ("appletvos", "tvOS")];
const MAX_SCHEMES: usize = 8;
const CONFS: [&str; 2] = ["build-signing.conf", "scripts/build-signing.conf"];

/// A refused checkout; `status` is the HTTP status of the reply.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveryError {
    pub status: u16,
    pub message: String,
}

/// A refused registration; `status` is the HTTP status of the reply (400, 409, or 500 when the
/// configuration could not be read or written).
#[derive(Debug, Clone, PartialEq)]
pub struct RegistrationError {
    pub status: u16,
    pub message: String,
}

/// A refused install report; `status` is the HTTP status of the reply.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportError {
    pub status: u16,
    pub message: String,
}

macro_rules! error_type {
    ($($name:ident),*) => {$(
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
        }
        impl std::error::Error for $name {}
        impl $name {
            fn new(status: u16, message: impl Into<String>) -> Self { Self { status, message: message.into() } }
        }
    )*};
}
error_type!(DiscoveryError, RegistrationError, ReportError);

impl From<DiscoveryError> for RegistrationError {
    fn from(e: DiscoveryError) -> Self {
        Self { status: e.status, message: e.message }
    }
}

fn bad(message: &str) -> DiscoveryError {
    DiscoveryError::new(400, message)
}

// ---- shared checks ----

fn is_control(c: char) -> bool {
    c <= '\u{1f}' || c == '\u{7f}'
}

/// The JS `text()`: a non-empty string of at most `max` UTF-16 units without control characters.
fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.encode_utf16().count() <= max && !s.chars().any(is_control)
}

fn text(value: Option<&Value>, max: usize) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| valid_text(s, max))
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]*$`
fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) && chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// `^[A-Za-z0-9]+(?:[.-][A-Za-z0-9]+)*$`
fn is_bundle_id(s: &str) -> bool {
    !s.is_empty() && s.split(['.', '-']).all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Runs a command to completion with a deadline. `None` when it could not start, timed out or failed.
fn run(command: &str, args: &[&str], dir: Option<&Path>, timeout: Duration) -> Result<Option<Vec<u8>>, std::io::Error> {
    let mut cmd = Command::new(command);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                // Our own child, by its handle.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(15)),
        }
    };
    let out = reader.join().unwrap_or_default();
    Ok(status.filter(|s| s.success()).map(|_| out))
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut full = vec!["-C", dir.to_str()?];
    full.extend_from_slice(args);
    run("git", &full, None, Duration::from_secs(10)).ok().flatten().map(|o| String::from_utf8_lossy(&o).into_owned())
}

fn home_relative(requested: &str) -> PathBuf {
    match requested.strip_prefix("~/") {
        Some(rest) => crate::home().join(rest),
        None => PathBuf::from(requested),
    }
}

/// The canonical checkout directory named by a request's `path` value.
fn resolve_checkout(input: Option<&Value>) -> Result<PathBuf, DiscoveryError> {
    let Some(input) = text(input, 4096) else { return Err(bad("Enter the checkout path on the Mac.")) };
    let requested = home_relative(input);
    if !requested.is_absolute() {
        return Err(bad("Use an absolute checkout path on the Mac."));
    }
    fs::canonicalize(&requested).ok().filter(|d| d.is_dir()).ok_or_else(|| bad("The checkout directory does not exist on the Mac."))
}

/// The checkout must be the top of a Git work tree.
fn require_git_root(directory: &Path) -> Result<(), DiscoveryError> {
    let top = git(directory, &["rev-parse", "--show-toplevel"]);
    match top.as_deref().map(str::trim).map(fs::canonicalize) {
        Some(Ok(top)) if top == directory => Ok(()),
        _ => Err(bad("Choose the root of an existing Git checkout.")),
    }
}

fn has_conf(directory: &Path) -> bool {
    CONFS.iter().any(|name| directory.join(name).is_file())
}

// ---- discovery ----

/// Workspaces first (they carry the packages), then projects, at the root only. An .xcodeproj
/// inside a workspace's own bundle is not a candidate.
fn containers(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory).into_iter().flatten().flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
    names.sort();
    let found: Vec<String> = names.iter().filter(|n| n.ends_with(".xcworkspace")).chain(names.iter().filter(|n| n.ends_with(".xcodeproj"))).cloned().collect();
    // A workspace that only wraps one project of the same name adds nothing.
    found.iter().filter(|n| match n.strip_suffix(".xcodeproj") {
        Some(stem) => !found.contains(&format!("{stem}.xcworkspace")),
        None => true,
    }).cloned().collect()
}

/// `xcodebuild ... -json` parsed; `None` when it failed or printed something else.
fn xcodebuild(directory: &Path, args: &[&str], timeout: Duration) -> Result<Option<Value>, DiscoveryError> {
    match run("xcodebuild", args, Some(directory), timeout) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(bad("Xcode is not installed on this Mac.")),
        Err(_) => Ok(None),
        Ok(out) => Ok(out.and_then(|o| serde_json::from_slice(&o).ok())),
    }
}

fn project_name(directory: &Path) -> String {
    let base = directory.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let cleaned: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    let cleaned = cleaned.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if cleaned.is_empty() { "Project".into() } else { cleaned.into() }
}

/// What the checkout at `input` (the request's `path` value, `~/` allowed) builds: its branches,
/// whether it has a conf or is already in `repos`, and each scheme that builds an app.
///
/// Reply: `{name, path, branches, currentBranch, hasConf, registered, container, containers,
/// apps: [{scheme, product, bundleId, platforms}], note}`.
pub fn discover(input: &Value, repos: &Value) -> Result<Value, DiscoveryError> {
    let directory = resolve_checkout(Some(input))?;
    require_git_root(&directory)?;
    let branches: Vec<String> = git(&directory, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]).unwrap_or_default()
        .split('\n').filter(|b| !b.is_empty()).map(String::from).collect();
    let current = git(&directory, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map(|b| b.trim().to_string());
    let registered = repos.as_array().is_some_and(|repos| repos.iter().any(|repo| {
        repo.get("path").and_then(Value::as_str).is_some_and(|p| fs::canonicalize(p).map_or(Path::new(p) == directory, |c| c == directory))
    }));
    let found = containers(&directory);
    let mut reply = json!({
        "name": project_name(&directory), "path": directory.to_string_lossy(), "branches": branches,
        "currentBranch": current, "hasConf": has_conf(&directory), "registered": registered,
        "container": found.first(), "containers": found, "apps": [], "note": null,
    });
    let Some(first) = found.first() else {
        reply["note"] = if directory.join("Package.swift").exists() {
            "This is a Swift package without an Xcode project. Apparition builds apps from an .xcodeproj or .xcworkspace."
        } else {
            "No Xcode project or workspace at the root of this checkout."
        }.into();
        return Ok(reply);
    };
    let flag = if first.ends_with(".xcworkspace") { "-workspace" } else { "-project" };
    let listing = xcodebuild(&directory, &[flag, first, "-list", "-json"], Duration::from_secs(60))?;
    let Some(container) = listing.as_ref().and_then(|l| l.get("workspace").or_else(|| l.get("project"))).filter(|c| !c.is_null()) else {
        reply["note"] = format!("Xcode could not read {first}. Open it in Xcode once, then try again.").into();
        return Ok(reply);
    };
    let schemes: Vec<String> = container.get("schemes").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|s| s.as_str().map(String::from)).take(MAX_SCHEMES).collect();
    let mut apps = Vec::new();
    for scheme in schemes {
        let settings = xcodebuild(&directory, &[flag, first, "-scheme", &scheme, "-showBuildSettings", "-json"], Duration::from_secs(90))?;
        // The scheme's first product that is an app (not a framework, test bundle or extension).
        let app = settings.as_ref().and_then(Value::as_array).into_iter().flatten()
            .filter_map(|entry| entry.get("buildSettings").and_then(Value::as_object))
            .find(|s| s.get("WRAPPER_EXTENSION").and_then(Value::as_str) == Some("app")
                && s.get("PRODUCT_BUNDLE_IDENTIFIER").and_then(Value::as_str).is_some_and(|b| !b.is_empty()));
        let Some(app) = app else { continue };
        let mut platforms: Vec<&str> = Vec::new();
        for p in app.get("SUPPORTED_PLATFORMS").and_then(Value::as_str).unwrap_or("").split_whitespace() {
            if let Some((_, alias)) = PLATFORM_ALIASES.iter().find(|(k, _)| *k == p) {
                if !platforms.contains(alias) {
                    platforms.push(alias);
                }
            }
        }
        let product = app.get("FULL_PRODUCT_NAME").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| format!("{scheme}.app"));
        apps.push(json!({ "scheme": scheme, "product": product, "bundleId": app["PRODUCT_BUNDLE_IDENTIFIER"], "platforms": platforms }));
    }
    if apps.is_empty() {
        reply["note"] = format!("No scheme in {first} builds an app.").into();
    }
    reply["apps"] = Value::Array(apps);
    Ok(reply)
}

/// What registration generates when a checkout has no conf.
#[derive(Debug, Clone, PartialEq)]
pub struct Generation {
    pub container: String,
    pub scheme: String,
    pub platform: String,
    pub device: String,
}

fn shell_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Validates what registration may generate: a container at the checkout root and a scheme name.
/// Nothing here runs; the values are quoted when written. `platforms` is the validated targets
/// (the first one names the platform and device).
pub fn validate_generation(directory: &Path, generate: &Value, platforms: &[Value]) -> Result<Generation, DiscoveryError> {
    let ok = generate.as_object().is_some_and(|g| g.keys().all(|k| k == "container" || k == "scheme"));
    if !ok {
        return Err(bad("Choose the project and scheme to build."));
    }
    let container = text(generate.get("container"), 255).filter(|c| {
        let stem = c.strip_suffix(".xcodeproj").or_else(|| c.strip_suffix(".xcworkspace"));
        !c.contains('/') && stem.is_some_and(|s| !s.is_empty()) && directory.join(c).exists()
    });
    let Some(container) = container else { return Err(bad("That Xcode project is not at the root of the checkout.")) };
    let Some(scheme) = text(generate.get("scheme"), 255) else { return Err(bad("Choose the scheme to build.")) };
    let first = platforms.first();
    let field = |key: &str| first.and_then(|p| p.get(key)).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(Generation { container: container.into(), scheme: scheme.into(), platform: field("platform"), device: field("device") })
}

/// The text of a generated `build-signing.conf`.
pub fn conf_text(g: &Generation) -> String {
    let device: String = g.device.chars().flat_map(|c| if "\"\\$`".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
    [
        "# Written by Apparition. Holds no signing material: builds kept for device-side signing need none.".to_string(),
        "# Add TEAM_ID, DEV_P12_PATH and DEV_PROFILE_PATH here only for installs signed on this Mac.".into(),
        format!("PROJECT_PATH={}", shell_value(&g.container)),
        format!("SCHEME_NAME={}", shell_value(&g.scheme)),
        "# Respect the caller's PLATFORM and DEVICE_NAME (Apparition Store sets them per target).".into(),
        format!("PLATFORM=\"${{PLATFORM:-{}}}\"", g.platform),
        format!("DEVICE_NAME=\"${{DEVICE_NAME:-{device}}}\""),
        String::new(),
    ].join("\n")
}

/// Undoes `write_conf`: removes the conf and restores the clone's local exclude file.
pub struct UndoConf {
    file: PathBuf,
    exclude: Option<(PathBuf, String)>,
}

impl UndoConf {
    pub fn undo(self) {
        let _ = fs::remove_file(&self.file);
        if let Some((file, before)) = self.exclude {
            let _ = fs::write(file, before);
        }
    }
}

/// Writes the conf (never over an existing one) and keeps it out of Git locally.
pub fn write_conf(directory: &Path, g: &Generation) -> std::io::Result<UndoConf> {
    let file = directory.join("build-signing.conf");
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&file)?.write_all(conf_text(g).as_bytes())?;
    let exclude = (|| {
        let path = git(directory, &["rev-parse", "--git-path", "info/exclude"])?;
        let exclude = directory.join(path.trim());
        let before = fs::read_to_string(&exclude).unwrap_or_default();
        if !before.split('\n').any(|l| l == "/build-signing.conf") {
            fs::create_dir_all(exclude.parent()?).ok()?;
            let gap = if !before.is_empty() && !before.ends_with('\n') { "\n" } else { "" };
            fs::write(&exclude, format!("{before}{gap}/build-signing.conf\n")).ok()?;
        }
        Some((exclude, before))
    })();
    Ok(UndoConf { file, exclude })
}

// ---- registration ----

fn refused(message: &str) -> RegistrationError {
    RegistrationError::new(400, message)
}

/// Validates a registration request against the configured projects. Returns the project to
/// add, `{name, path, branches, platforms}` with the canonical checkout path, and what to
/// generate when the checkout has no conf. Nothing is written.
pub fn validate(input: &Value, existing: &[Value]) -> Result<(Value, Option<Generation>), RegistrationError> {
    let Some(request) = input.as_object() else { return Err(refused("Enter a project and at least one target.")) };
    if request.keys().any(|k| !["name", "path", "branches", "platforms", "generate"].contains(&k.as_str())) {
        return Err(refused("Unknown project field."));
    }
    let name = text(request.get("name"), 80).filter(|n| is_identifier(n));
    let Some(name) = name else { return Err(refused("Use a project name with letters, numbers, dots, underscores or hyphens.")) };
    let directory = resolve_checkout(request.get("path"))?;
    require_git_root(&directory)?;
    let conf = has_conf(&directory);
    // JS truthiness: absent, null, false, 0 and "" mean "do not generate".
    let generate = request.get("generate").filter(|g| !matches!(g, Value::Null | Value::Bool(false)) && g.as_str() != Some("") && g.as_f64() != Some(0.0));
    if !conf && generate.is_none() {
        return Err(refused("Set up build-signing.conf in this checkout before registering it, or choose its project and scheme so Apparition can write one."));
    }
    let same_checkout = |repo: &Value| repo.get("path").and_then(Value::as_str).is_some_and(|p| fs::canonicalize(p).unwrap_or_else(|_| absolute(Path::new(p))) == directory);
    if existing.iter().any(|repo| repo.get("name").and_then(Value::as_str) == Some(name) || same_checkout(repo)) {
        return Err(RegistrationError::new(409, "That project name or checkout is already registered."));
    }
    let targets = request.get("platforms").and_then(Value::as_array).filter(|t| !t.is_empty() && t.len() <= 32);
    let Some(targets) = targets else { return Err(refused("Add between one and 32 targets.")) };
    let mut pairs = HashSet::new();
    let mut platforms = Vec::new();
    for target in targets {
        let invalid = || refused("Each target needs a platform alias, device name and bundle identifier.");
        let target = target.as_object().filter(|t| t.keys().all(|k| ["platform", "device", "bundleId"].contains(&k.as_str()))).ok_or_else(invalid)?;
        let platform = text(target.get("platform"), 40).filter(|p| is_identifier(p));
        let device = text(target.get("device"), 128);
        let bundle_id = text(target.get("bundleId"), 255).filter(|b| is_bundle_id(b));
        let (Some(platform), Some(device), Some(bundle_id)) = (platform, device, bundle_id) else { return Err(invalid()) };
        if !pairs.insert((platform.to_string(), device.to_string())) {
            return Err(refused("Each platform and device pair must be unique."));
        }
        platforms.push(json!({ "platform": platform, "device": device, "bundleId": bundle_id }));
    }
    let branches: Option<Vec<Value>> = match request.get("branches") {
        None => Some(git(&directory, &["symbolic-ref", "--quiet", "--short", "HEAD"]).map(|b| vec![json!(b.trim())]).unwrap_or_default()),
        Some(value) => value.as_array().cloned(),
    };
    let existing_branch = |b: &Value| text(Some(b), 255).is_some_and(|b| b != "current"
        && git(&directory, &["show-ref", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_some());
    let branches = branches.filter(|b| b.len() <= 64 && b.iter().all(existing_branch));
    let Some(branches) = branches else {
        return Err(refused("Choose existing local branches; current checkout is always offered separately."));
    };
    let mut unique: Vec<Value> = Vec::new();
    for b in branches {
        if !unique.contains(&b) {
            unique.push(b);
        }
    }
    let project = json!({ "name": name, "path": directory.to_string_lossy(), "branches": unique, "platforms": platforms });
    // An existing conf is never replaced: the person's setup wins over a generated one.
    let generation = match (conf, generate) {
        (false, Some(generate)) => Some(validate_generation(&directory, generate, &platforms)?),
        _ => None,
    };
    Ok((project, generation))
}

fn absolute(path: &Path) -> PathBuf {
    normalize(&if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) })
}

fn unavailable() -> RegistrationError {
    RegistrationError::new(500, "Could not register the project on the Mac.")
}

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Adds the project in `input` to `config_path`'s `repos` and returns it. Every unrelated
/// configuration value is kept (and its order); the file is replaced atomically (written beside
/// it, mode 0600, then renamed) and only if nobody changed it meanwhile (409). A conf generated
/// for the checkout is removed again when the registration does not complete. Never sources a
/// signing config, builds, installs or contacts Apple's services.
pub fn register_project(config_path: &Path, input: &Value) -> Result<Value, RegistrationError> {
    register_with(config_path, input, &|| {})
}

fn register_with(file: &Path, input: &Value, before_rename: &dyn Fn()) -> Result<Value, RegistrationError> {
    let original = fs::read(file).map_err(|_| unavailable())?;
    let mut config: Map<String, Value> = match serde_json::from_slice(&original) {
        Ok(Value::Object(config)) => config,
        _ => return Err(unavailable()),
    };
    let repos: Vec<Value> = config.get("repos").and_then(Value::as_array).cloned().unwrap_or_default();
    let (project, generation) = validate(input, &repos)?;
    let mut next_repos = repos;
    next_repos.push(project.clone());
    config.insert("repos".into(), Value::Array(next_repos));
    let mut serialized = serde_json::to_string_pretty(&Value::Object(config)).map_err(|_| unavailable())?;
    serialized.push('\n');

    let mut name = file.as_os_str().to_owned();
    name.push(format!(".incoming-{}-{}-{}", std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()), TEMPORARY.fetch_add(1, Ordering::Relaxed)));
    let temporary = PathBuf::from(name);
    let mut undo: Option<UndoConf> = None;
    let result = (|| -> Result<(), RegistrationError> {
        if let Some(generation) = &generation {
            let directory = Path::new(project["path"].as_str().unwrap_or_default());
            undo = Some(write_conf(directory, generation).map_err(|_| unavailable())?);
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut handle = options.open(&temporary).map_err(|_| unavailable())?;
        handle.write_all(serialized.as_bytes()).and_then(|_| handle.sync_all()).map_err(|_| unavailable())?;
        drop(handle);
        before_rename();
        if fs::read(file).ok().as_deref() != Some(original.as_slice()) {
            return Err(RegistrationError::new(409, "The Mac configuration changed; try registering again."));
        }
        fs::rename(&temporary, file).map_err(|_| unavailable())
    })();
    let _ = fs::remove_file(&temporary);
    match result {
        Ok(()) => Ok(project),
        Err(error) => {
            if let Some(undo) = undo {
                undo.undo();
            }
            Err(error)
        }
    }
}

// ---- installs a device made itself ----

fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    matches!((a.and_then(Value::as_str), b.and_then(Value::as_str)), (Some(a), Some(b)) if a.to_lowercase() == b.to_lowercase())
}

/// The install ledger entry for an install the device made for itself (it downloaded a kept
/// build and signed it). `report` is `{device, bundleId, buildId, platform?}`; `repos` the
/// configured projects, `builds` the service's jobs, `read_build(buildId)` the kept build's
/// manifest (an error when it is not kept). The client names a configured target and a kept
/// build; everything else in the entry is read from the service's own records, never taken from
/// the request. Returns `(device, bundleId, entry)`.
pub fn ledger_entry_from_report(
    report: &Value,
    repos: &Value,
    builds: &[Value],
    read_build: &dyn Fn(&str) -> Result<Value, String>,
    now_ms: u64,
) -> Result<(String, String, Value), ReportError> {
    let build_id = report.get("buildId").and_then(Value::as_str).filter(|id| (1..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    let valid = report.as_object().is_some_and(|r| r.keys().all(|k| ["device", "bundleId", "buildId", "platform"].contains(&k.as_str())))
        && text(report.get("device"), 128).is_some() && text(report.get("bundleId"), 255).is_some() && build_id.is_some()
        && report.get("platform").is_none_or(|p| text(Some(p), 40).is_some());
    let (true, Some(build_id)) = (valid, build_id) else {
        return Err(ReportError::new(400, "Name a configured device, its bundle identifier and a kept build."));
    };
    let found = repos.as_array().into_iter().flatten().flat_map(|repo| {
        repo.get("platforms").and_then(Value::as_array).into_iter().flatten().map(move |target| (repo, target))
    }).find(|(_, target)| target.get("device") == report.get("device") && target.get("bundleId") == report.get("bundleId")
        && report.get("platform").is_none_or(|p| same(target.get("platform"), Some(p))));
    let Some((repo, target)) = found else { return Err(ReportError::new(400, "That is not a configured target.")) };
    let manifest = read_build(build_id).map_err(|_| ReportError::new(404, "That build is not kept on the Mac."))?;
    let app = manifest.get("app").filter(|a| a.is_object()).unwrap_or(&Value::Null);
    let bundle_id = target.get("bundleId");
    if app.get("bundleId") != bundle_id && app.get("originalBundleId") != bundle_id {
        return Err(ReportError::new(400, "That build is not of this target."));
    }
    if !same(manifest.get("platform"), target.get("platform")) {
        return Err(ReportError::new(400, "That build is for another platform."));
    }
    let job = builds.iter().find(|b| b.get("buildId").and_then(Value::as_str) == Some(build_id) && b.get("repo") == repo.get("name"));
    let value = |v: Option<&Value>| v.cloned().unwrap_or(Value::Null);
    let entry = json!({
        "repo": value(repo.get("name")),
        "branch": value(job.and_then(|j| j.get("builtBranch"))),
        "commit": value(job.and_then(|j| j.get("commit"))),
        "buildId": build_id,
        "platform": value(target.get("platform")),
        "version": value(app.get("version")),
        "buildNo": value(app.get("build")),
        "at": now_ms,
        // The device itself did this; no guess, and no `devicectl` involved.
        "confirmed": true,
        "via": "apparition",
    });
    let string = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    Ok((string(target.get("device")), string(bundle_id), entry))
}

// ---- packages and the Xcode scheme overlay ----

/// The names in `parent` (the real checkout's parent folder) of the packages a clone at `clone`
/// builds against, transitively, sorted: `["DebugTrace", "RAVESDK", ...]`. The clone's own
/// references resolve inside its slot; a package's own, inside `parent`. Anything outside that
/// layout cannot be mirrored and is left alone.
pub fn sibling_packages(clone: &Path, parent: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(clone.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let mut queue = vec![(clone.to_path_buf(), clone.parent().map(Path::to_path_buf).unwrap_or_default())];
    while let Some((dir, base)) = queue.pop() {
        for reference in package_refs(&dir) {
            let Ok(rel) = reference.strip_prefix(&base) else { continue };
            let Some(top) = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()) else { continue };
            if !seen.insert(top.clone()) {
                continue;
            }
            if !parent.join(&top).exists() {
                continue;
            }
            names.push(top.clone());
            queue.push((parent.join(&top), parent.to_path_buf()));
        }
    }
    names.sort();
    names
}

const ENVIRONMENT: &str = "export PATH=\"$PATH:$HOME/.local/bin:$HOME/bin:/opt/homebrew/bin:/usr/local/bin\"\nif [ -z \"$APPARITION_CLI\" ]; then APPARITION_CLI=apparition; fi\n";

/// The optional XcodeGen spec that adds an "<target> (Apparition)" scheme to a project: a
/// pre-build action capturing the source context and a post-build one registering the product.
/// The original spec is included, never modified. Returns `{spec, scheme, originalSpec}`; with
/// `generate`, also runs `xcodegen` on it.
pub fn configure(spec: &Path, target: &str, output: Option<&Path>, generate: bool) -> Result<Value, String> {
    let spec = absolute(spec);
    if target.is_empty() || target.contains(['\r', '\n']) {
        return Err("Choose an Xcode target.".into());
    }
    let root = spec.parent().map(Path::to_path_buf).unwrap_or_default();
    let output = output.map(absolute).unwrap_or_else(|| root.join("project.apparition.yml"));
    if output == spec {
        return Err("The optional spec must be separate from the original.".into());
    }
    let before = format!("{ENVIRONMENT}set -e\n\"$APPARITION_CLI\" xcode snapshot --project \"$SRCROOT\" --output \"$DERIVED_FILE_DIR/apparition/source-before.json\"\n");
    let after = format!("{ENVIRONMENT}set -e\nset -- xcode register --app \"$TARGET_BUILD_DIR/$FULL_PRODUCT_NAME\" --project \"$SRCROOT\" --configuration \"$CONFIGURATION\" --context \"$DERIVED_FILE_DIR/apparition/source-before.json\"\nif [ -d \"$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME\" ]; then set -- \"$@\" --symbols \"$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME\"; fi\n\"$APPARITION_CLI\" \"$@\"\n");
    let name = format!("{target} (Apparition)");
    let document = json!({
        "include": [{ "path": spec.to_string_lossy(), "relativePaths": false }],
        "schemes": { name.clone(): { "build": {
            "targets": { target: "all" },
            "preActions": [{ "name": "Capture Apparition source context", "settingsTarget": target, "script": before }],
            "postActions": [{ "name": "Register with Apparition", "settingsTarget": target, "script": after }],
        } } },
    });
    let content = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    if output.exists() && fs::read_to_string(&output).ok().as_deref() != Some(content.as_str()) {
        return Err("The optional spec already exists with different content; choose another output.".into());
    }
    if let Some(dir) = output.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(&output, content).map_err(|e| e.to_string())?;
    if generate {
        let (output_arg, root_arg) = (output.to_string_lossy(), root.to_string_lossy());
        let ran = run("xcodegen", &["--spec", &output_arg, "--project", &root_arg, "--project-root", &root_arg], None, Duration::from_secs(300));
        if !matches!(ran, Ok(Some(_))) {
            return Err("xcodegen failed during Xcode handoff".into());
        }
    }
    Ok(json!({ "spec": output.to_string_lossy(), "scheme": name, "originalSpec": spec.to_string_lossy() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_run(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args)
            .env("GIT_AUTHOR_NAME", "T").env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T").env("GIT_COMMITTER_EMAIL", "t@example.com").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn repo(parent: &Path, name: &str) -> PathBuf {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        git_run(&dir, &["init", "-q", "-b", "main"]);
        fs::write(dir.join("main.txt"), "fixture").unwrap();
        git_run(&dir, &["add", "main.txt"]);
        git_run(&dir, &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "Fixture"]);
        dir
    }

    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let p = fs::canonicalize(t.path()).unwrap();
        (t, p)
    }

    fn err(r: Result<impl std::fmt::Debug, DiscoveryError>) -> String {
        r.unwrap_err().message
    }

    #[test]
    fn discovery_reports_a_checkout_with_no_xcode_project() {
        let (_t, root) = scratch();
        let dir = repo(&root, "Repo");
        fs::write(dir.join("Package.swift"), "// swift-tools-version:5.9\n").unwrap();
        let found = discover(&json!(dir), &json!([])).unwrap();
        assert_eq!(found["container"], Value::Null);
        assert!(found["note"].as_str().unwrap().contains("Swift package"));
        assert_eq!(found["hasConf"], false);
        assert_eq!(found["registered"], false);
        assert_eq!(found["branches"], json!(["main"]));
        assert_eq!(found["currentBranch"], "main");
        assert_eq!(found["name"], "Repo");
        assert_eq!(discover(&json!(dir), &json!([{ "path": dir }])).unwrap()["registered"], true);
        fs::remove_file(dir.join("Package.swift")).unwrap();
        assert!(discover(&json!(dir), &json!([])).unwrap()["note"].as_str().unwrap().starts_with("No Xcode project"));
    }

    #[test]
    fn discovery_refuses_a_subdirectory_a_relative_path_and_a_missing_directory() {
        let (_t, root) = scratch();
        let dir = repo(&root, "Repo");
        fs::create_dir(dir.join("Sub")).unwrap();
        assert!(err(discover(&json!(dir.join("Sub")), &json!([]))).contains("root"));
        assert!(err(discover(&json!("relative"), &json!([]))).contains("absolute"));
        assert!(err(discover(&json!(dir.join("Nope")), &json!([]))).contains("does not exist"));
        assert!(err(discover(&Value::Null, &json!([]))).contains("Enter the checkout path"));
        assert!(err(discover(&json!("/tmp/a\nb"), &json!([]))).contains("Enter the checkout path"));
        assert_eq!(discover(&json!("relative"), &json!([])).unwrap_err().status, 400);
    }

    #[test]
    fn discovery_prefers_workspaces_and_drops_a_workspace_wrapped_project() {
        let (_t, root) = scratch();
        for name in ["App.xcodeproj", "App.xcworkspace", "Other.xcodeproj", ".Hidden.xcodeproj"] {
            fs::create_dir(root.join(name)).unwrap();
        }
        assert_eq!(containers(&root), ["App.xcworkspace", "Other.xcodeproj"]);
        assert_eq!(project_name(Path::new("/x/_my app!")), "my-app-");
        assert_eq!(project_name(Path::new("/")), "Project");
    }

    #[test]
    fn a_generated_conf_quotes_its_values_and_holds_no_signing_material() {
        let g = Generation { container: "A B.xcodeproj".into(), scheme: "it's $(x)".into(), platform: "visionOS".into(), device: "Vision \"Pro\" $x".into() };
        let text = conf_text(&g);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.contains(&"PROJECT_PATH='A B.xcodeproj'"));
        assert!(lines.contains(&"SCHEME_NAME='it'\\''s $(x)'"));
        assert!(lines.contains(&"PLATFORM=\"${PLATFORM:-visionOS}\""));
        assert!(lines.contains(&"DEVICE_NAME=\"${DEVICE_NAME:-Vision \\\"Pro\\\" \\$x}\""));
        assert!(!lines.iter().any(|l| ["TEAM_ID=", "DEV_P12_PATH=", "DEV_PROFILE_PATH="].iter().any(|k| l.starts_with(k))));
        assert!(text.ends_with('\n'));
    }

    struct Fixture {
        _t: tempfile::TempDir,
        root: PathBuf,
        repo: PathBuf,
        file: PathBuf,
        config: Value,
        input: Value,
    }

    fn fixture() -> Fixture {
        let (t, root) = scratch();
        let repo = repo(&root, "Repo");
        fs::write(repo.join("build-signing.conf"), "exit 77 # Must never be sourced by registration\n").unwrap();
        let config = json!({ "repos": [], "passwordHash": "fixture-hash", "buildSettings": { "untouched": true }, "bundlePrefix": ["com.example"] });
        let file = root.join("config.json");
        fs::write(&file, config.to_string()).unwrap();
        let input = json!({ "name": "Example", "path": repo, "platforms": [{ "platform": "visionOS", "device": "Headset", "bundleId": "com.example.app" }] });
        Fixture { _t: t, root, repo, file, config, input }
    }

    fn stored(f: &Fixture) -> Value {
        serde_json::from_slice(&fs::read(&f.file).unwrap()).unwrap()
    }

    fn with(input: &Value, key: &str, value: Value) -> Value {
        let mut input = input.clone();
        input[key] = value;
        input
    }

    #[test]
    fn registration_atomically_preserves_configuration_and_records_a_canonical_checkout() {
        let f = fixture();
        let alias = f.root.join("Alias");
        std::os::unix::fs::symlink(&f.repo, &alias).unwrap();
        let project = register_project(&f.file, &with(&f.input, "path", json!(alias))).unwrap();
        assert_eq!(project["path"], json!(f.repo));
        assert_eq!(project["branches"], json!(["main"]));
        let mut expected = f.config.clone();
        expected["repos"] = json!([project]);
        assert_eq!(stored(&f), expected);
        // Key order is the file's own, with the new project appended.
        let text = fs::read_to_string(&f.file).unwrap();
        assert!(text.find("passwordHash").unwrap() < text.find("buildSettings").unwrap());
        assert!(text.ends_with("}\n"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&f.file).unwrap().permissions().mode() & 0o777, 0o600);
        let again = register_project(&f.file, &with(&f.input, "name", json!("Another"))).unwrap_err();
        assert_eq!((again.status, again.message.as_str()), (409, "That project name or checkout is already registered."));
        assert_eq!(stored(&f), expected);
        let leftovers = fs::read_dir(&f.root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains(".incoming-")).count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn registration_rejects_unsafe_names_missing_setup_duplicate_targets_and_nonexistent_branches() {
        let f = fixture();
        let target = f.input["platforms"][0].clone();
        let nested = f.repo.join("Nested");
        fs::create_dir(&nested).unwrap();
        for input in [
            with(&f.input, "name", json!("../bad")), with(&f.input, "path", json!("relative")),
            with(&f.input, "path", json!(f.repo.join("subdirectory"))), with(&f.input, "path", json!(nested)),
            with(&f.input, "branches", json!(["missing"])), with(&f.input, "branches", json!(["current"])),
            with(&f.input, "branches", json!("main")), with(&f.input, "branches", json!(null)),
            with(&f.input, "platforms", json!([])), with(&f.input, "platforms", json!([target, target])),
            with(&f.input, "platforms", json!([with(&target, "device", json!("AVP\nother"))])),
            with(&f.input, "platforms", json!([with(&target, "bundleId", json!("com..x"))])),
            with(&f.input, "platforms", json!([with(&target, "extra", json!(1))])),
            with(&f.input, "platforms", json!(["text"])),
            with(&f.input, "buildSettings", json!(["UNEXPECTED=1"])), json!(["array"]), Value::Null,
        ] {
            assert_eq!(validate(&input, &[]).unwrap_err().status, 400, "{input}");
        }
        fs::remove_file(f.repo.join("build-signing.conf")).unwrap();
        let e = register_project(&f.file, &f.input).unwrap_err();
        assert!(e.message.contains("build-signing.conf"));
        assert_eq!(stored(&f), f.config);
    }

    #[test]
    fn registration_can_write_a_minimal_conf_keeps_it_out_of_git_and_undoes_it_when_the_config_changed() {
        let f = fixture();
        fs::remove_file(f.repo.join("build-signing.conf")).unwrap();
        fs::create_dir(f.repo.join("App.xcodeproj")).unwrap();
        let bad = with(&f.input, "generate", json!({ "container": "../x.xcodeproj", "scheme": "S" }));
        let e = register_project(&f.file, &bad).unwrap_err();
        assert!(e.message.contains("not at the root"), "{e}");
        assert!(!f.repo.join("build-signing.conf").exists());
        for generate in [json!({ "container": "App.xcodeproj" }), json!({ "container": "App.xcodeproj", "scheme": "S", "x": 1 })] {
            assert_eq!(register_project(&f.file, &with(&f.input, "generate", generate)).unwrap_err().status, 400);
        }
        assert!(!f.repo.join("build-signing.conf").exists());

        // The config changes between validation and the rename: nothing is left behind.
        let generate = json!({ "container": "App.xcodeproj", "scheme": "My App's" });
        let input = with(&f.input, "generate", generate);
        let changed = register_with(&f.file, &input, &|| fs::write(&f.file, "{\"repos\":[],\"changed\":1}").unwrap()).unwrap_err();
        assert_eq!(changed.status, 409);
        assert!(!f.repo.join("build-signing.conf").exists());
        let exclude = fs::read_to_string(f.repo.join(".git/info/exclude")).unwrap();
        assert!(!exclude.contains("build-signing.conf"));
        fs::write(&f.file, f.config.to_string()).unwrap();

        let project = register_project(&f.file, &input).unwrap();
        let conf = fs::read_to_string(f.repo.join("build-signing.conf")).unwrap();
        assert!(conf.lines().any(|l| l == "PROJECT_PATH='App.xcodeproj'"));
        assert!(conf.lines().any(|l| l == "SCHEME_NAME='My App'\\''s'"));
        assert!(conf.contains("PLATFORM=\"${PLATFORM:-visionOS}\""));
        let status = Command::new("git").arg("-C").arg(&f.repo).args(["status", "--porcelain", "--untracked-files=all"]).output().unwrap();
        assert!(!String::from_utf8_lossy(&status.stdout).contains("build-signing.conf"));
        assert!(!fs::read_to_string(&f.file).unwrap().contains("generate"));
        assert_eq!(stored(&f)["repos"][0], project);
        assert!(project.get("generate").is_none());
    }

    #[test]
    fn registration_never_replaces_an_existing_conf_with_a_generated_one() {
        let f = fixture();
        fs::create_dir(f.repo.join("App.xcodeproj")).unwrap();
        let input = with(&f.input, "generate", json!({ "container": "App.xcodeproj", "scheme": "App" }));
        register_project(&f.file, &input).unwrap();
        assert!(fs::read_to_string(f.repo.join("build-signing.conf")).unwrap().contains("exit 77"));
    }

    #[test]
    fn registration_checks_branches_and_an_unreadable_config() {
        let f = fixture();
        git_run(&f.repo, &["branch", "dev"]);
        let project = register_project(&f.file, &with(&f.input, "branches", json!(["dev", "main", "dev"]))).unwrap();
        assert_eq!(project["branches"], json!(["dev", "main"]));
        let e = register_project(&f.root.join("missing.json"), &f.input).unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (500, "Could not register the project on the Mac."));
        fs::write(&f.file, "not json").unwrap();
        assert_eq!(register_project(&f.file, &f.input).unwrap_err().status, 500);
    }

    // ---- installs ----

    fn repos() -> Value {
        json!([{ "name": "App", "path": "/x", "platforms": [
            { "platform": "visionOS", "device": "Headset", "bundleId": "com.example.app" },
            { "platform": "iOS", "device": "Phone", "bundleId": "com.example.app" }] }])
    }

    fn manifest() -> Value {
        json!({ "platform": "visionOS", "app": { "bundleId": "com.example.app", "version": "1.2", "build": "34" } })
    }

    fn report(report: Value, builds: &[Value], read: &dyn Fn(&str) -> Result<Value, String>) -> Result<(String, String, Value), ReportError> {
        ledger_entry_from_report(&report, &repos(), builds, read, 1234)
    }

    #[test]
    fn a_report_becomes_a_ledger_entry_from_the_services_own_records() {
        let builds = [json!({ "buildId": "abc-1", "repo": "App", "builtBranch": "main", "commit": "deadbeef" }), json!({ "buildId": "abc-1", "repo": "Other", "commit": "no" })];
        let read = |id: &str| { assert_eq!(id, "abc-1"); Ok(manifest()) };
        let (device, bundle, entry) = report(json!({ "device": "Headset", "bundleId": "com.example.app", "buildId": "abc-1" }), &builds, &read).unwrap();
        assert_eq!((device.as_str(), bundle.as_str()), ("Headset", "com.example.app"));
        assert_eq!(entry, json!({ "repo": "App", "branch": "main", "commit": "deadbeef", "buildId": "abc-1", "platform": "visionOS",
            "version": "1.2", "buildNo": "34", "at": 1234, "confirmed": true, "via": "apparition" }));
        // No job on record, a platform compared case-insensitively, the original bundle id accepted.
        let read = |_: &str| Ok(json!({ "platform": "VisionOS", "app": { "bundleId": "com.other.signed", "originalBundleId": "com.example.app" } }));
        let (_, _, entry) = report(json!({ "device": "Headset", "bundleId": "com.example.app", "buildId": "zz", "platform": "visionos" }), &[], &read).unwrap();
        assert_eq!((entry["branch"].clone(), entry["commit"].clone(), entry["version"].clone(), entry["buildNo"].clone()), (Value::Null, Value::Null, Value::Null, Value::Null));
        assert_eq!(entry["platform"], "visionOS");
    }

    #[test]
    fn a_report_is_refused_unless_it_names_a_configured_target_and_a_kept_build() {
        let ok = json!({ "device": "Headset", "bundleId": "com.example.app", "buildId": "abc" });
        let read = |_: &str| Ok(manifest());
        let invalid = "Name a configured device, its bundle identifier and a kept build.";
        for body in [
            Value::Null, json!([]), json!("x"), json!({}),
            with(&ok, "extra", json!(1)), with(&ok, "device", json!("")), with(&ok, "device", json!(7)),
            with(&ok, "device", json!("a".repeat(129))), with(&ok, "device", json!("H\nx")),
            with(&ok, "bundleId", json!("b".repeat(256))), with(&ok, "buildId", json!("../x")), with(&ok, "buildId", json!("")),
            with(&ok, "buildId", json!("a".repeat(65))), with(&ok, "buildId", Value::Null), with(&ok, "platform", Value::Null),
            with(&ok, "platform", json!("")), with(&ok, "platform", json!("p".repeat(41))),
        ] {
            let e = report(body.clone(), &[], &read).unwrap_err();
            assert_eq!((e.status, e.message.as_str()), (400, invalid), "{body}");
        }
        for body in [with(&ok, "device", json!("Nope")), with(&ok, "bundleId", json!("com.other")), with(&ok, "platform", json!("tvOS")),
                     with(&ok, "device", json!("Phone")) /* Phone is iOS: the device and bundle id match another target only */] {
            let wanted = body["device"] == "Phone";
            let result = report(body.clone(), &[], &read);
            if wanted {
                // Matches the iOS target, whose platform the visionOS build does not have.
                let e = result.unwrap_err();
                assert_eq!((e.status, e.message.as_str()), (400, "That build is for another platform."));
            } else {
                let e = result.unwrap_err();
                assert_eq!((e.status, e.message.as_str()), (400, "That is not a configured target."), "{body}");
            }
        }
        let missing = |_: &str| Err("no such build".to_string());
        let e = report(ok.clone(), &[], &missing).unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (404, "That build is not kept on the Mac."));
        for other in [json!({ "platform": "visionOS", "app": { "bundleId": "com.other" } }), json!({ "platform": "visionOS" }), json!({ "platform": "visionOS", "app": "x" })] {
            let read = move |_: &str| Ok(other.clone());
            let e = report(ok.clone(), &[], &read).unwrap_err();
            assert_eq!((e.status, e.message.as_str()), (400, "That build is not of this target."));
        }
        for other in [json!({ "platform": "iOS", "app": { "bundleId": "com.example.app" } }), json!({ "app": { "bundleId": "com.example.app" } })] {
            let read = move |_: &str| Ok(other.clone());
            let e = report(ok.clone(), &[], &read).unwrap_err();
            assert_eq!((e.status, e.message.as_str()), (400, "That build is for another platform."));
        }
        // The platform in the report narrows the match to its own target.
        let read = |_: &str| Ok(json!({ "platform": "iOS", "app": { "bundleId": "com.example.app" } }));
        let phone = json!({ "device": "Phone", "bundleId": "com.example.app", "buildId": "abc", "platform": "iOS" });
        assert!(report(phone, &[], &read).is_ok());
        let wrong = json!({ "device": "Headset", "bundleId": "com.example.app", "buildId": "abc", "platform": "iOS" });
        assert_eq!(report(wrong, &[], &read).unwrap_err().message, "That is not a configured target.");
    }

    // ---- packages and the scheme overlay ----

    #[test]
    fn sibling_packages_follow_references_transitively_inside_the_projects_folder() {
        let (_t, root) = scratch();
        let parent = root.join("Projects");
        let slot = root.join("slot");
        let write = |path: PathBuf, text: &str| { fs::create_dir_all(path.parent().unwrap()).unwrap(); fs::write(path, text).unwrap() };
        write(slot.join("App/Package.swift"), ".package(path: \"../Kit\"), .package(path: \"../../outside\"), .package(path: \"../Missing\")");
        write(parent.join("Kit/Package.swift"), ".package(name: \"Base\", path: \"../Base\"), .package(path: \"../App\")");
        write(parent.join("Base/Package.swift"), "Package()");
        assert_eq!(sibling_packages(&slot.join("App"), &parent), ["Base", "Kit"]);
        write(slot.join("App/App.xcodeproj/project.pbxproj"), "isa = XCLocalSwiftPackageReference;\n relativePath = ../Extra;\n");
        write(parent.join("Extra/Package.swift"), "Package()");
        assert_eq!(sibling_packages(&slot.join("App"), &parent), ["Base", "Extra", "Kit"]);
    }

    #[test]
    fn the_optional_xcodegen_spec_preserves_the_original_and_scopes_both_actions_to_the_target() {
        let (_t, root) = scratch();
        let spec = root.join("project.yml");
        let original = "name: Fixture\ntargets: {}\n";
        fs::write(&spec, original).unwrap();
        let generated = configure(&spec, "Fixture", None, false).unwrap();
        assert_eq!(fs::read_to_string(&spec).unwrap(), original);
        let output = PathBuf::from(generated["spec"].as_str().unwrap());
        let overlay: Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(overlay["include"], json!([{ "path": spec, "relativePaths": false }]));
        let build = &overlay["schemes"]["Fixture (Apparition)"]["build"];
        assert_eq!(build["preActions"][0]["settingsTarget"], "Fixture");
        assert_eq!(build["postActions"][0]["settingsTarget"], "Fixture");
        assert!(build["preActions"][0]["script"].as_str().unwrap().contains("xcode snapshot"));
        let post = build["postActions"][0]["script"].as_str().unwrap();
        assert!(post.contains("xcode register"));
        for word in ["xcodebuild", "--run", "--jit", "codesign"] {
            assert!(!post.contains(word));
        }
        assert_eq!(configure(&spec, "Fixture", None, false).unwrap(), generated);
        fs::write(&output, "# user modification").unwrap();
        assert!(configure(&spec, "Fixture", None, false).unwrap_err().contains("different content"));
        assert_eq!(fs::read_to_string(&output).unwrap(), "# user modification");
        assert!(configure(&spec, "Fixture", Some(&spec), false).unwrap_err().contains("separate"));
        assert!(configure(&spec, "A\nB", None, false).unwrap_err().contains("Choose an Xcode target"));
    }
}
