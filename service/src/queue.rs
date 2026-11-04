//! Every build the Store makes goes through here: updates, the page's deploys, AltSource and Mac
//! downloads, and the builds a re-sign waits for. Jobs live in `data/queue.json`; what each device
//! runs is the install ledger in `state.json` (`state.rs`). Job ids start at 1000000, clear of
//! the ids the Node store used, so a log or a link made earlier still means what it did.
//!
//! Efficient with many apps at once:
//! - one build per platform build, not per device: platform rows whose conf resolves alike
//!   (`apparition build --plan`: the `iPad` alias is the iOS build) share it, so Hypnos for the
//!   iPhone and the iPad is one build installed twice;
//! - at most `maxConcurrentBuilds` (config, default 2) builds at once, and one at a time per
//!   checkout or build clone, since each holds one `build/` folder;
//! - clones are kept, so each build checks out only the new commits and Xcode builds
//!   incrementally;
//! - installs go one device at a time (the devices' own rule) after the build, while other
//!   builds go on; a device that can't be reached keeps the build for its Apparition to install.
//!
//! A deploy from the page builds in the repo's own checkout when it asks for the branch that is
//! checked out (or `current`: its uncommitted state is meant); any other branch, an update, an
//! AltSource or a Mac download builds in a clone at an exact commit.

use crate::{
    artifacts,
    auth::system_ms,
    clones,
    config::Config,
    devices::{self, Devices},
    install_mode,
    state::State,
    updates::{self, Installed, Row, Target},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const FIRST_ID: u64 = 1_000_000;
const KEEP_JOBS: usize = 200;
/// A group whose build failed isn't tried again on its own for this long, unless its sources move.
const RETRY_AFTER_MS: u64 = 6 * 3600 * 1000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Install {
    pub device: String,
    pub ok: bool,
    #[serde(default)]
    pub queued: bool,
    #[serde(default)]
    pub error: Option<String>,
    pub at: u64,
}

/// A queued build, in the shape the web page reads jobs in, plus the devices it goes to and why.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Job {
    pub id: u64,
    pub batch_id: Option<u64>,
    /// "update", "deploy", "altsource", "mac" or "resign".
    pub kind: String,
    pub repo: String,
    /// The branch asked for; "current" is whatever the checkout has out.
    pub branch: String,
    pub platform: String,
    pub device: String,
    pub devices: Vec<String>,
    pub bundle_id: String,
    /// The app commit to build, when one is pinned (empty: the branch's tip, or the checkout).
    pub rev: String,
    pub status: String,
    pub queued_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub commit: Option<String>,
    pub built_branch: Option<String>,
    pub build_id: Option<String>,
    /// The signed build kept before an install that failed, for the device's Apparition to take.
    pub kept_build_id: Option<String>,
    pub build_no: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
    /// What made it due: "vergence: Double 50-60 fps video…".
    pub why: Vec<String>,
    /// Started by the automatic mode, not a person.
    pub auto: bool,
    pub installs: Vec<Install>,
    /// "mac" (this computer installs), "device" (its Apparition does), "none", or "store" for an
    /// update this Store installs itself.
    pub install_via: String,
    pub no_deploy: bool,
    pub run: bool,
    pub mcp: bool,
    pub jit: bool,
    pub build_debug: bool,
    pub inject: bool,
    pub inject_backend: Option<String>,
    pub build_settings: Vec<String>,
    /// The AltSource slug this build is for.
    pub alt_source: Option<String>,
    /// The Mac download this build is for.
    pub mac_app: Option<String>,
    /// A build made only for the re-sign queued after it.
    pub for_resign: bool,
    /// A re-sign: `{from, after, profile, dataMode, dryRun, ...}`.
    pub resign: Option<Value>,
    pub artifact: Option<Value>,
    pub source: Option<Value>,
}

impl Job {
    fn new(kind: &str, repo: &str, branch: &str, platform: &str, device: &str, bundle_id: &str) -> Job {
        Job {
            kind: kind.into(), repo: repo.into(), branch: branch.into(), platform: platform.into(),
            device: device.into(), devices: if device.is_empty() { Vec::new() } else { vec![device.into()] },
            bundle_id: bundle_id.into(), status: "queued".into(), queued_at: system_ms(), ..Job::default()
        }
    }

    pub fn active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    next_id: u64,
    jobs: Vec<Job>,
}

/// One build that updates one or more devices.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub repo: String,
    pub branch: String,
    pub platform: String,
    pub bundle_id: String,
    pub devices: Vec<String>,
    pub rev: String,
    pub why: Vec<String>,
    /// The newest change that made it due, in ms.
    pub latest_at: u64,
    /// Identifies the build: repo, branch and what the conf resolves to.
    pub key: String,
}

/// What a finished build hands to the parts that publish it.
pub trait Deliver: Send + Sync {
    /// An AltSource build is done: take its IPA for the feed.
    fn alt_source_built(&self, slug: &str, rev: &str, ipa: &Path) -> Result<(), String>;
    /// A Mac download build is done: `kept` is the build as kept (the `.app` zipped, or the app's path).
    fn mac_built(&self, slug: &str, rev: &str, branch: &str, kept: &artifacts::Kept) -> Result<String, String>;
}

/// What a run needs from the Store around it.
#[derive(Clone)]
pub struct Context {
    pub config: Arc<Config>,
    pub build_store: PathBuf,
    pub cli: PathBuf,
    pub devices: Option<Arc<dyn Devices>>,
    pub state: Arc<State>,
    pub deliver: Option<Arc<dyn Deliver>>,
}

/// How long one update install may take before it is given up on.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// `apparition build`'s exit status when it installed but couldn't launch (`builder::LAUNCH_FAILED`).
const LAUNCH_FAILED: u8 = 3;

/// A job's turn: its checkout or clone, and a build slot.
struct Turn {
    _lock: tokio::sync::OwnedMutexGuard<()>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

pub struct Queue {
    file: PathBuf,
    pub logs: PathBuf,
    pub clones: PathBuf,
    pub ota: PathBuf,
    saved: Mutex<Saved>,
    slots: Arc<tokio::sync::Semaphore>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pids: Mutex<HashMap<u64, u32>>,
    manifests: updates::Manifests,
    plans: Mutex<HashMap<(String, String), (u64, Option<Value>)>>,
    /// group key + commit -> when its build failed.
    failures: Mutex<HashMap<String, u64>>,
    /// Slots a build is using now, which `reap` leaves alone.
    busy: Mutex<Vec<PathBuf>>,
    /// Device names being asked for their apps right now.
    asking: Mutex<Vec<String>>,
    /// Set by `shutdown`: nothing more starts, and queue.json is no longer written, so a Store
    /// started after this one over the same folder owns it alone.
    stopped: std::sync::atomic::AtomicBool,
}

pub fn max_builds(config: &Config) -> usize {
    config.raw.get("maxConcurrentBuilds").and_then(Value::as_u64).filter(|n| (1..=16).contains(n)).unwrap_or(2) as usize
}

/// The command line that builds: config `apparitionCli`, else the one beside this app, else the
/// user's install, else PATH.
pub fn cli(config: &Config) -> PathBuf {
    if let Some(path) = config.raw.get("apparitionCli").and_then(Value::as_str).filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("APPARITION_CLI") {
        return PathBuf::from(path);
    }
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("apparition-cli"))).filter(|p| p.is_file()) {
        return beside;
    }
    let local = crate::home().join(".local/bin/apparition");
    if local.is_file() { local } else { PathBuf::from("apparition") }
}

/// Every device row of every repo in config.json, watch rows aside (a watch app goes with its
/// iPhone app).
pub fn targets(config: &Config) -> Vec<Target> {
    let mut out = Vec::new();
    for repo in config.raw.get("repos").and_then(Value::as_array).into_iter().flatten() {
        let (Some(name), Some(path)) = (repo["name"].as_str(), repo["path"].as_str()) else { continue };
        let first_branch = repo["branches"].as_array().and_then(|b| b.first()).and_then(Value::as_str).unwrap_or("main");
        for row in repo["platforms"].as_array().into_iter().flatten() {
            let (Some(platform), Some(bundle), Some(device)) = (row["platform"].as_str(), row["bundleId"].as_str(), row["device"].as_str()) else { continue };
            if platform.eq_ignore_ascii_case("watchos") {
                continue;
            }
            let branch = repo.pointer(&format!("/deviceBranches/{}", device.replace('~', "~0").replace('/', "~1"))).and_then(Value::as_str).unwrap_or(first_branch);
            out.push(Target {
                repo: name.into(), repo_path: PathBuf::from(path), platform: platform.into(), device: device.into(),
                bundle_id: bundle.into(), default_branch: branch.into(),
            });
        }
    }
    out
}

fn repo_of<'a>(config: &'a Config, name: &str) -> Option<&'a Value> {
    config.raw.get("repos")?.as_array()?.iter().find(|r| r["name"].as_str() == Some(name))
}

/// Extra xcodebuild settings (`--set KEY=VALUE`), from a repo's `buildSettings`: an array for
/// every branch, or `{branch: [...], "*": [...]}`; either form's value may itself be
/// `{platform: [...], "*": [...]}`.
pub fn build_settings(repo: &Value, branch: &str, platform: &str) -> Vec<String> {
    fn strings(v: &Value) -> Vec<String> {
        v.as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect()
    }
    let Some(settings) = repo.get("buildSettings").filter(|s| !s.is_null()) else { return Vec::new() };
    let for_branch = if settings.is_array() { settings } else { settings.get(branch).or_else(|| settings.get("*")).unwrap_or(&Value::Null) };
    if for_branch.is_array() {
        return strings(for_branch);
    }
    strings(for_branch.get(platform).or_else(|| for_branch.get("*")).unwrap_or(&Value::Null))
}

/// The automatic mode's settings: config `autoUpdate` `{enabled, settleMinutes}`.
pub fn auto_settings(config: &Config) -> (bool, u64) {
    let auto = config.raw.get("autoUpdate");
    let enabled = auto.and_then(|a| a.get("enabled")).and_then(Value::as_bool).unwrap_or(false);
    let settle = auto.and_then(|a| a.get("settleMinutes")).and_then(Value::as_u64).unwrap_or(5).min(24 * 60);
    (enabled, settle)
}

fn why(reason: &updates::Reason) -> String {
    match (reason.rewritten, reason.commits) {
        (true, _) => format!("{}: history rewritten since {}", reason.source, reason.from),
        (false, 1) => format!("{}: {}", reason.source, reason.latest),
        (false, n) => format!("{}: {n} commits, latest {}", reason.source, reason.latest),
    }
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A job's build: where it runs and at which commit.
struct Workdir {
    dir: PathBuf,
    /// The repo's own checkout: its uncommitted state is part of the build.
    direct: bool,
    branch: String,
    /// Keys the lock that keeps two builds out of one `build/` folder.
    key: String,
}

impl Queue {
    pub fn open(data_dir: &Path, max: usize) -> Queue {
        let file = data_dir.join("queue.json");
        let mut saved: Saved = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        saved.next_id = saved.next_id.max(FIRST_ID);
        for job in saved.jobs.iter_mut().filter(|j| j.active()) {
            job.status = "failed".into();
            job.error = Some("the Store stopped during the build".into());
        }
        Queue {
            file, logs: data_dir.join("logs"), clones: data_dir.join("clones"), ota: data_dir.join("ota"),
            saved: Mutex::new(saved), slots: Arc::new(tokio::sync::Semaphore::new(max)),
            locks: Mutex::default(), pids: Mutex::default(),
            manifests: updates::Manifests::default(), plans: Mutex::default(), failures: Mutex::default(), busy: Mutex::default(),
            asking: Mutex::default(), stopped: Default::default(),
        }
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The Store is stopping: what is queued or running is noted as failed, the builds running
    /// are stopped (their whole process group), and nothing starts after this.
    pub fn shutdown(&self) {
        let pids: Vec<u32> = {
            let mut saved = self.saved.lock().unwrap();
            if self.stopped() {
                return;
            }
            for job in saved.jobs.iter_mut().filter(|j| j.active()) {
                job.status = "failed".into();
                job.error = Some("the Store stopped during the build".into());
                job.finished_at = Some(system_ms());
            }
            self.save(&saved);
            self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            self.pids.lock().unwrap().values().copied().collect()
        };
        for pid in pids {
            stop(pid);
        }
    }

    fn save(&self, saved: &Saved) {
        if self.stopped() {
            return;
        }
        let Ok(text) = serde_json::to_string_pretty(saved) else { return };
        let tmp = self.file.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    pub fn update(&self, id: u64, change: impl FnOnce(&mut Job)) -> Option<Job> {
        let mut saved = self.saved.lock().unwrap();
        let job = saved.jobs.iter_mut().find(|j| j.id == id)?;
        change(job);
        let job = job.clone();
        self.save(&saved);
        Some(job)
    }

    pub fn jobs(&self) -> Vec<Job> {
        self.saved.lock().unwrap().jobs.clone()
    }

    pub fn job(&self, id: u64) -> Option<Job> {
        self.saved.lock().unwrap().jobs.iter().find(|j| j.id == id).cloned()
    }

    pub fn log_path(&self, id: u64) -> PathBuf {
        self.logs.join(format!("build-{id}.log"))
    }

    /// Settles when the job ends, however it ends (finished, failed, cancelled while queued).
    pub async fn wait(&self, id: u64) -> Option<Job> {
        loop {
            match self.job(id) {
                Some(job) if job.active() => tokio::time::sleep(Duration::from_millis(250)).await,
                other => return other,
            }
        }
    }

    /// What is installed where, the newest word wins: the device itself, or the install ledger
    /// (an OTA offer nobody confirmed doesn't count).
    pub fn installed(&self, ctx: &Context) -> HashMap<(String, String), Installed> {
        let mut out: HashMap<(String, String), Installed> = HashMap::new();
        fn offer(out: &mut HashMap<(String, String), Installed>, device: &str, bundle: &str, i: Installed) {
            let key = (device.to_string(), bundle.to_string());
            if out.get(&key).is_none_or(|old| i.at >= old.at) {
                out.insert(key, i);
            }
        }
        for (device, apps) in ctx.state.field("installs").as_object().into_iter().flatten() {
            for (bundle, e) in apps.as_object().into_iter().flatten() {
                let Some(build_no) = e["buildNo"].as_str().or(e["commit"].as_str()) else { continue };
                if e["confirmed"] == Value::Bool(false) {
                    continue;
                }
                offer(&mut out, device, bundle, Installed {
                    build_no: build_no.into(), build_id: e["buildId"].as_str().map(String::from),
                    branch: e["branch"].as_str().map(String::from), at: e["at"].as_u64().unwrap_or(0), from: "ledger",
                });
            }
        }
        for (device, inventory) in ctx.state.field("deviceApps").as_object().into_iter().flatten() {
            let fetched = inventory["fetchedAt"].as_u64().unwrap_or(0);
            for (bundle, app) in inventory["apps"].as_object().into_iter().flatten() {
                let Some(build) = app["buildNo"].as_str() else { continue };
                // The device knows what is there, but not which build that was: keep a ledger's
                // build ID when its build number agrees.
                let key = (device.clone(), bundle.clone());
                let same = out.get(&key).filter(|l| l.build_no == build).cloned();
                match same {
                    Some(l) => offer(&mut out, device, bundle, Installed { at: fetched.max(l.at), from: "device", ..l }),
                    None => offer(&mut out, device, bundle, Installed { build_no: build.into(), build_id: None, branch: None, at: fetched, from: "device" }),
                }
            }
        }
        out
    }

    /// Every target, planned.
    pub fn plan(&self, ctx: &Context) -> Vec<Row> {
        let installed = self.installed(ctx);
        let manifests = self.manifests.all(&ctx.build_store);
        let roots: Vec<PathBuf> = [self.clones.clone(), self.clones.with_file_name("build-clones")].into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p)).collect();
        let git = updates::Git::default();
        targets(&ctx.config).iter()
            .map(|t| updates::plan(t, installed.get(&(t.device.clone(), t.bundle_id.clone())), &manifests, &roots, &git))
            .collect()
    }

    /// What the conf resolves `platform` of `repo` to (`apparition build --plan`), kept ten
    /// minutes. None when it can't say: that row then builds on its own.
    fn build_plan(&self, ctx: &Context, repo: &Path, platform: &str) -> Option<Value> {
        let key = (repo.display().to_string(), platform.to_string());
        let now = system_ms();
        if let Some((at, plan)) = self.plans.lock().unwrap().get(&key) {
            if now - at < 600_000 {
                return plan.clone();
            }
        }
        let out = std::process::Command::new(&ctx.cli).args(["build", "--plan"]).current_dir(repo).env("PLATFORM", platform)
            .stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).output().ok();
        let plan = out.filter(|o| o.status.success()).and_then(|o| String::from_utf8_lossy(&o.stdout).lines().last().and_then(|l| serde_json::from_str::<Value>(l).ok()));
        self.plans.lock().unwrap().insert(key, (now, plan.clone()));
        plan
    }

    /// The builds that bring `rows` (those behind) up to date: one per repo, branch and resolved build.
    pub fn groups(&self, ctx: &Context, rows: &[Row]) -> Vec<Group> {
        let mut groups: Vec<Group> = Vec::new();
        let git = updates::Git::default();
        for row in rows.iter().filter(|r| r.status == "behind") {
            let Some(target) = targets(&ctx.config).into_iter().find(|t| t.device == row.device && t.bundle_id == row.bundle_id && t.repo == row.repo) else { continue };
            let Some(rev) = git.run(&target.repo_path, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}^{{commit}}", row.branch)]) else { continue };
            let resolved = self.build_plan(ctx, &target.repo_path, &row.platform).map(|mut p| {
                if let Some(o) = p.as_object_mut() { o.remove("device"); o.remove("profileDevices"); }
                p.to_string()
            }).unwrap_or_else(|| format!("row:{}:{}", row.platform, row.device));
            let key = format!("{}|{}|{resolved}", row.repo, row.branch);
            let latest_at = row.reasons.iter().map(|r| r.latest_at).max().unwrap_or(0);
            match groups.iter_mut().find(|g| g.key == key) {
                Some(g) => {
                    g.devices.push(row.device.clone());
                    for w in row.reasons.iter().map(why) {
                        if !g.why.contains(&w) { g.why.push(w) }
                    }
                    g.latest_at = g.latest_at.max(latest_at);
                }
                None => groups.push(Group {
                    repo: row.repo.clone(), branch: row.branch.clone(), platform: row.platform.clone(), bundle_id: row.bundle_id.clone(),
                    devices: vec![row.device.clone()], rev, why: row.reasons.iter().map(why).collect(), latest_at, key,
                }),
            }
        }
        groups
    }

    /// Gives `jobs` their ids (one batch when there are several), keeps them and starts them.
    fn push(self: &Arc<Self>, ctx: &Context, mut jobs: Vec<Job>) -> Vec<Job> {
        {
            let mut saved = self.saved.lock().unwrap();
            let batch = (jobs.len() > 1).then_some(saved.next_id);
            // A re-sign waits for the build queued just before it.
            if jobs.len() == 2 && jobs[1].kind == "resign" && jobs[0].for_resign {
                let first = saved.next_id;
                if let Some(r) = jobs[1].resign.as_mut().and_then(Value::as_object_mut) { r.insert("after".into(), json!(first)); }
            }
            for job in &mut jobs {
                job.id = saved.next_id;
                job.batch_id = job.batch_id.or(batch);
                saved.next_id += 1;
                saved.jobs.push(job.clone());
            }
            let excess = saved.jobs.len().saturating_sub(KEEP_JOBS);
            saved.jobs.drain(..excess);
            self.save(&saved);
        }
        for job in &jobs {
            let (queue, ctx, id) = (self.clone(), ctx.clone(), job.id);
            tokio::spawn(async move { queue.run(ctx, id).await });
        }
        jobs
    }

    /// Queues a build per group, except one already queued or running for the same build.
    pub fn enqueue(self: &Arc<Self>, ctx: &Context, groups: Vec<Group>, auto: bool) -> Vec<Job> {
        let existing = self.jobs();
        let jobs: Vec<Job> = groups.into_iter().filter(|g| {
            !existing.iter().any(|j| j.active() && j.repo == g.repo && j.branch == g.branch && j.bundle_id == g.bundle_id
                && j.rev == g.rev && g.devices.iter().all(|d| j.devices.contains(d)))
        }).map(|g| {
            let mut job = Job::new("update", &g.repo, &g.branch, &g.platform, g.devices.first().map_or("", String::as_str), &g.bundle_id);
            job.devices = g.devices;
            job.rev = g.rev;
            job.why = g.why;
            job.auto = auto;
            job.install_via = "store".into();
            job
        }).collect();
        self.push(ctx, jobs)
    }

    /// A deploy from the page or the service API: `{repo, targets: [{device, platform?, branch?}],
    /// run, mcp, jit, buildDebug, inject, injectBackend, deploy, install}`. The error is what the
    /// route answers with.
    pub fn enqueue_deploy(self: &Arc<Self>, ctx: &Context, body: &Value) -> Result<Vec<Job>, String> {
        let flag = |k: &str| body.get(k).and_then(Value::as_bool).unwrap_or(false);
        let inject_backend = body.get("injectBackend").and_then(Value::as_str).unwrap_or("dynamic");
        if !["dynamic", "apparition"].contains(&inject_backend) {
            return Err("injectBackend must be dynamic or apparition".into());
        }
        let install = body.get("install").and_then(Value::as_str);
        if body.get("install").is_some_and(|i| !i.is_null() && install.is_none_or(|m| !install_mode::MODES.contains(&m))) {
            return Err("install must be auto, mac, device or none".into());
        }
        let name = body.get("repo").and_then(Value::as_str).unwrap_or("");
        let repo = repo_of(&ctx.config, name).ok_or_else(|| format!("unknown repo: {name}"))?;
        let rows = repo["platforms"].as_array().filter(|p| !p.is_empty()).ok_or_else(|| format!("{name} has no device platforms to deploy to"))?;
        let wanted: Vec<Value> = match body.get("targets").and_then(Value::as_array).filter(|t| !t.is_empty()) {
            Some(targets) => targets.clone(),
            None => vec![json!({ "device": body.get("device").and_then(Value::as_str).unwrap_or_else(|| rows[0]["device"].as_str().unwrap_or("")), "branch": body.get("branch") })],
        };
        let launch = flag("run");
        let now = system_ms();
        let mut jobs = Vec::new();
        for t in &wanted {
            let device = t["device"].as_str().unwrap_or("");
            let platform = t["platform"].as_str().filter(|p| !p.is_empty());
            let row = rows.iter().find(|p| p["device"].as_str() == Some(device) && platform.is_none_or(|w| p["platform"].as_str() == Some(w)))
                .ok_or_else(|| format!("{name} has no target {device}{}", platform.map(|p| format!(" ({p})")).unwrap_or_default()))?;
            let via = install_mode::resolve(install, body.get("deploy").and_then(Value::as_bool), device, &ctx.state.presence, launch, now)?;
            let branch = t["branch"].as_str().filter(|b| !b.is_empty()).unwrap_or("current");
            let mut job = Job::new("deploy", name, branch, row["platform"].as_str().unwrap_or(""), device, row["bundleId"].as_str().unwrap_or(""));
            job.install_via = via.into();
            // Only `mac` deploys inside the build. Any other mode passes --no-deploy, so a device
            // this computer can't reach can't fail the build, and the launch and the install
            // ledger are skipped too: nothing landed on the device from here.
            job.no_deploy = via != "mac";
            job.mcp = launch && flag("mcp") && via == "mac";
            job.jit = launch && flag("jit") && via == "mac";
            job.run = launch && !flag("mcp") && !flag("jit") && via == "mac";
            job.build_debug = flag("buildDebug");
            job.inject = flag("inject");
            job.inject_backend = Some(inject_backend.into());
            jobs.push(job);
        }
        Ok(self.push(ctx, jobs))
    }

    /// An AltSource build: exactly `rev` of the app's branch, in a clean clone, signed and kept,
    /// never installed from here and never in the install ledger.
    pub fn enqueue_alt_source(self: &Arc<Self>, ctx: &Context, repo: &str, branch: &str, platform: &str, device: &str, bundle_id: &str, slug: &str, rev: &str) -> Job {
        let mut job = Job::new("altsource", repo, branch, platform, device, bundle_id);
        job.rev = rev.into();
        job.alt_source = Some(slug.into());
        job.no_deploy = true;
        job.install_via = "none".into();
        self.push(ctx, vec![job]).remove(0)
    }

    /// A Mac app's download build: exactly `rev`, in a clean clone, signed and zipped. It has no device.
    pub fn enqueue_mac(self: &Arc<Self>, ctx: &Context, repo: &str, branch: &str, platform: &str, bundle_id: &str, slug: &str, rev: &str) -> Job {
        let mut job = Job::new("mac", repo, branch, platform, "", bundle_id);
        job.rev = rev.into();
        job.mac_app = Some(slug.into());
        job.no_deploy = true;
        job.install_via = "none".into();
        self.push(ctx, vec![job]).remove(0)
    }

    /// Queues prepared jobs as they are (the re-sign and the build it waits for).
    pub fn enqueue_jobs(self: &Arc<Self>, ctx: &Context, jobs: Vec<Job>) -> Vec<Job> {
        self.push(ctx, jobs)
    }

    /// A blank job of `kind` for the caller to fill in (re-sign builds it by hand).
    pub fn blank(kind: &str, repo: &str, branch: &str, platform: &str, device: &str, bundle_id: &str) -> Job {
        Job::new(kind, repo, branch, platform, device, bundle_id)
    }

    pub fn cancel(&self, id: u64) -> Option<Job> {
        let job = self.update(id, |j| if j.active() {
            j.status = "cancelled".into();
            j.finished_at = Some(system_ms());
        })?;
        if let Some(pid) = self.pids.lock().unwrap().get(&id).copied() {
            stop(pid);
        }
        Some(job)
    }

    /// Where a job builds, and the lock for it.
    fn workdir(&self, ctx: &Context, job: &Job) -> Result<Workdir, String> {
        let repo = repo_of(&ctx.config, &job.repo).ok_or("the repo is no longer in config.json")?;
        let path = PathBuf::from(repo["path"].as_str().ok_or("the repo has no path")?);
        let out = git_out(&path, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
        // A re-sign runs `apparition build --ipa` in the checkout, so it shares the build's lock.
        let direct = job.kind == "resign" || job.kind == "deploy" && (job.branch == "current" || job.branch == out);
        if direct {
            let key = format!("{}|checkout", job.repo);
            return Ok(Workdir { dir: path, direct, branch: out, key });
        }
        let branch = if job.branch == "current" { out } else { job.branch.clone() };
        // Keyed by the slot's folder: `feature/x` and `feature-x` share one.
        let dir = clones::slot(&self.clones, &path, &branch);
        Ok(Workdir { key: dir.display().to_string(), dir, direct, branch })
    }

    async fn run(self: Arc<Self>, ctx: Context, id: u64) {
        let Some(job) = self.job(id) else { return };
        let work = match self.workdir(&ctx, &job) {
            Ok(w) => w,
            Err(error) => { self.update(id, |j| { j.status = "failed".into(); j.error = Some(error); j.finished_at = Some(system_ms()); }); return }
        };
        // A re-sign waits for its build before taking the repo's turn, which that build needs.
        if let Some(after) = job.resign.as_ref().and_then(|r| r["after"].as_u64()) {
            self.wait(after).await;
        }
        let lock = self.locks.lock().unwrap().entry(work.key.clone()).or_default().clone();
        let lock = lock.lock_owned().await;
        let Ok(permit) = self.slots.clone().acquire_owned().await else { return };
        if self.stopped() {
            return;
        }
        // Queued to running in one step, so a cancel can't land between the look and the change.
        let started = self.update(id, |j| if j.status == "queued" { j.status = "running".into(); j.started_at = Some(system_ms()); });
        if started.is_none_or(|j| j.status != "running") {
            return;
        }
        let turn = Turn { _lock: lock, _permit: permit };
        let result = if job.kind == "resign" { self.clone().resign(&ctx, &job).await.map(|()| drop(turn)) } else { self.clone().build(&ctx, &job, work, turn).await };
        if let Err(error) = result {
            let cancelled = self.job(id).is_some_and(|j| j.status == "cancelled");
            if !cancelled {
                self.failures.lock().unwrap().insert(format!("{}@{}", job.devices.join(","), job.rev), system_ms());
                self.update(id, |j| { j.status = "failed".into(); j.error = Some(error); j.finished_at = Some(system_ms()); });
            }
        }
        if let Some(done) = self.job(id).filter(|j| j.status == "success") {
            // Something just landed on those devices, so they are awake: look at what they have now.
            let dry = done.resign.as_ref().is_some_and(|r| r["dryRun"] == Value::Bool(true));
            if !dry && (!done.no_deploy && done.kind != "update" || done.resign.is_some()) {
                for device in &done.devices {
                    self.refresh_device_apps(&ctx, device, true).await;
                }
            }
        }
    }

    /// A re-sign (`resign.rs`) as a job. The build it waits for, if any, ran first on the same
    /// repo's queue, so it has finished by now.
    async fn resign(self: Arc<Self>, ctx: &Context, job: &Job) -> Result<(), String> {
        let id = job.id;
        std::fs::create_dir_all(&self.logs).map_err(|e| e.to_string())?;
        let file = Arc::new(Mutex::new(std::fs::File::create(self.log_path(id)).map_err(|e| e.to_string())?));
        let log: crate::resign::Log = { let file = file.clone(); Arc::new(move |text: &str| { let mut f = file.lock().unwrap(); let _ = f.write_all(text.as_bytes()); if !text.ends_with('\n') { let _ = f.write_all(b"\n"); } }) };
        let spec = job.resign.clone().unwrap_or(Value::Null);
        let dry_run = spec["dryRun"].as_bool() == Some(true);
        let mut from = spec["from"].as_str().map(String::from);
        if let Some(after) = spec["after"].as_u64() {
            let build = self.job(after).filter(|b| b.status == "success" && b.build_id.is_some()).ok_or_else(|| {
                let cancelled = self.job(after).is_some_and(|b| b.status == "cancelled");
                format!("build #{after} {}, so there is nothing to re-sign; the app was not changed", if cancelled { "was cancelled" } else { "didn't produce a kept IPA" })
            })?;
            log(&format!("==> Using build #{after} ({}@{}), kept as {}", build.built_branch.clone().unwrap_or_default(), build.commit.clone().unwrap_or_default(), build.build_id.clone().unwrap_or_default()));
            from = build.build_id;
        }
        let kept = match &from { Some(id) => Some(artifacts::read_build(id, &ctx.build_store)?), None => None };
        if kept.is_none() && !dry_run {
            return Err("no kept build to re-sign".into());
        }
        let profile = crate::resign::find_profile(spec["profile"].as_str().unwrap_or("")).ok_or("the profile is no longer in the build-signing folder")?;
        log(&format!("==> Re-signing {}{} for {} with {} [{}]", job.bundle_id, from.as_ref().map(|f| format!(" (kept build {f})")).unwrap_or_default(), job.device, profile.name, profile.app_id));
        let repo = repo_of(&ctx.config, &job.repo).ok_or("the repo is no longer in config.json")?;
        let pids = self.clone();
        let hook: crate::resign::PidHook = Arc::new(move |pid| match pid {
            Some(pid) => { pids.pids.lock().unwrap().insert(id, pid); }
            None => { pids.pids.lock().unwrap().remove(&id); }
        });
        let exec = crate::resign::CliExec { cli: ctx.cli.clone(), log: log.clone(), on_pid: Some(hook) };
        let mut options = crate::resign::Options::new(kept.as_ref().map(|k| k.file.clone()).unwrap_or_default(), &job.bundle_id, &job.device, profile.file.clone());
        options.cwd = repo["path"].as_str().map(PathBuf::from);
        options.env = vec![("PLATFORM".into(), job.platform.clone()), ("APPARITION_BUILD_DIR".into(), ctx.build_store.display().to_string())];
        options.data_mode = crate::resign::DataMode::parse(spec["dataMode"].as_str().unwrap_or("keep")).unwrap_or(crate::resign::DataMode::Keep);
        options.dry_run = dry_run;
        let outcome = crate::resign::perform(&options, &exec, &*log).await?;
        let text = std::fs::read_to_string(self.log_path(id)).unwrap_or_default();
        let build_id = build_id_in(&text);
        self.update(id, |j| {
            j.build_id = build_id.clone();
            if let Some(r) = j.resign.as_mut().and_then(Value::as_object_mut) {
                r.insert("result".into(), json!(outcome.mode.as_str()));
                if let Some(backup) = &outcome.backup { r.insert("backup".into(), json!(backup)); }
            }
        });
        if !dry_run && let Some(build_id) = build_id {
            let mut entry = ctx.state.installed(&job.device, &job.bundle_id).unwrap_or_else(|| json!({ "repo": job.repo, "platform": job.platform }));
            entry["buildId"] = json!(build_id);
            entry["at"] = json!(system_ms());
            entry["confirmed"] = json!(true);
            entry["resigned"] = json!(profile.id);
            ctx.state.record_install(&job.device, &job.bundle_id, entry);
        }
        log(&format!("\n==> Store: {}", if dry_run { "dry run finished".to_string() } else { format!("re-signed {} on {} ({})", job.bundle_id, job.device, outcome.mode.as_str()) }));
        self.update(id, |j| { j.status = "success".into(); j.finished_at = Some(system_ms()); });
        Ok(())
    }

    /// A re-sign: of a kept build (`kept`), or of a fresh build of the project target, which is
    /// queued first on the same repo and signed but not deployed. A dry run only probes the device.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_resign(self: &Arc<Self>, ctx: &Context, repo: &str, platform: &str, device: &str, bundle_id: &str, kept: Option<&str>,
        branch: Option<&str>, profile_id: &str, data_mode: &str, dry_run: bool) -> Vec<Job> {
        let mut jobs = Vec::new();
        if kept.is_none() && !dry_run {
            let mut build = Job::new("deploy", repo, branch.unwrap_or("current"), platform, device, bundle_id);
            build.no_deploy = true;
            build.for_resign = true;
            build.install_via = "none".into();
            jobs.push(build);
        }
        let mut job = Job::new("resign", repo, "current", platform, device, bundle_id);
        job.resign = Some(json!({ "from": kept, "after": null, "profile": profile_id, "dataMode": data_mode, "dryRun": dry_run }));
        jobs.push(job);
        self.push(ctx, jobs)
    }

    /// Ends a job as a success, unless it was cancelled meanwhile.
    fn succeed(&self, id: u64) {
        self.update(id, |j| if j.status == "running" { j.status = "success".into(); j.finished_at = Some(system_ms()); });
    }

    fn cancelled(&self, id: u64) -> bool {
        self.job(id).is_some_and(|j| j.status == "cancelled")
    }

    /// `turn` (the checkout's lock and a build slot) is let go once the build process is done:
    /// the installs that follow don't keep other builds waiting.
    async fn build(self: Arc<Self>, ctx: &Context, job: &Job, work: Workdir, turn: Turn) -> Result<(), String> {
        let id = job.id;
        std::fs::create_dir_all(&self.logs).map_err(|e| e.to_string())?;
        let log_file = std::fs::File::create(self.log_path(id)).map_err(|e| e.to_string())?;
        let log = Arc::new(Mutex::new(log_file));
        let say = |text: String| { let _ = writeln!(log.lock().unwrap(), "{text}"); };
        let repo = repo_of(&ctx.config, &job.repo).ok_or("the repo is no longer in config.json")?.clone();
        let repo_path = PathBuf::from(repo["path"].as_str().unwrap_or(""));
        let exclude: Vec<String> = repo["cloneExclude"].as_array().into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        let what = match job.kind.as_str() {
            "altsource" => "AltSource build", "mac" => "Mac download build", "update" => "Update build", _ => "Build",
        };

        // Where it builds: the checkout itself, or a clone at an exact commit.
        let app_dir = if work.direct {
            say(format!("==> {what} of {} [{}] in the checkout", job.repo, work.branch));
            work.dir.clone()
        } else {
            let rev = if job.rev.is_empty() {
                git_out(&repo_path, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}^{{commit}}", work.branch)])
                    .ok_or_else(|| format!("{} has no branch {}", job.repo, work.branch))?
            } else { job.rev.clone() };
            say(format!("==> {what} of {} [{}@{}] in a clean build clone{}", job.repo, work.branch, &rev[..rev.len().min(10)],
                if job.devices.is_empty() { String::new() } else { format!(" for {}", job.devices.join(", ")) }));
            let (clones_dir, branch, log2, from) = (self.clones.clone(), work.branch.clone(), log.clone(), repo_path.clone());
            self.busy.lock().unwrap().push(work.dir.clone());
            let prepared = tokio::task::spawn_blocking(move || {
                let mut writer = LogWriter(log2);
                clones::prepare(&clones_dir, &from, &branch, &rev, &exclude, &mut writer)
            }).await.map_err(|e| e.to_string());
            match prepared {
                Ok(Ok(dir)) => dir,
                Ok(Err(e)) | Err(e) => { self.busy.lock().unwrap().retain(|s| s != &work.dir); return Err(e) }
            }
        };
        for w in &job.why { say(format!("    {w}")); }
        let release = |queue: &Queue| queue.busy.lock().unwrap().retain(|s| s != &work.dir);

        let short = git_out(&app_dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        let settings = build_settings(&repo, &work.branch, &job.platform);
        self.update(id, |j| { j.commit = Some(short.clone()); j.built_branch = Some(work.branch.clone()); j.build_settings = settings.clone(); });
        if !settings.is_empty() { say(format!("    build settings: {}", settings.join("  "))); }

        // The build.
        let mut args: Vec<String> = vec!["build".into()];
        if job.no_deploy || job.kind == "update" { args.push("--no-deploy".into()); }
        // The Mac only compiles; the device signs with its own identity.
        if job.install_via == "device" { args.push("--unsigned".into()); }
        if job.mcp { args.extend(["--run".into(), "--mcp".into()]); }
        else if job.run { args.extend(["--run".into(), "--no-mcp".into()]); }
        else if !job.no_deploy && job.kind != "update" || job.kind == "update" { args.push("--no-mcp".into()); }
        // The row's device, not whichever DEVICE_NAME the conf names.
        if !job.no_deploy && job.kind == "deploy" && !job.device.is_empty() { args.extend(["--device".into(), job.device.clone()]); }
        if job.jit { args.push("--jit".into()); }
        if job.build_debug { args.push("--debug".into()); }
        if job.inject { args.extend(["--inject-backend".into(), job.inject_backend.clone().unwrap_or_else(|| "dynamic".into())]); }
        for s in &settings { args.extend(["--set".into(), s.clone()]); }
        if self.cancelled(id) {
            release(&self);
            return Err("cancelled".into());
        }
        say(format!("==> {} {} (PLATFORM={})\n", ctx.cli.display(), args.join(" "), job.platform));
        let mut command = tokio::process::Command::new(&ctx.cli);
        command.args(&args).current_dir(&app_dir).env("PATH", child_path())
            .env("PLATFORM", &job.platform).env("APPARITION_BUILD_DIR", &ctx.build_store)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(tool) = ctx.config.raw.get("debugtraceInjectTool").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            command.env("DEBUGTRACE_INJECT_TOOL", tool);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| { release(&self); format!("couldn't start {}: {e}", ctx.cli.display()) })?;
        if let Some(pid) = child.id() { self.pids.lock().unwrap().insert(id, pid); }
        let output = Arc::new(Mutex::new(String::new()));
        let pump = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>| {
            let (log, output) = (log.clone(), output.clone());
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let Some(stream) = stream else { return };
                let mut lines = tokio::io::BufReader::new(stream).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = writeln!(log.lock().unwrap(), "{line}");
                    let mut out = output.lock().unwrap();
                    out.push_str(&line);
                    out.push('\n');
                }
            })
        };
        let mut a = pump(child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>));
        let mut b = pump(child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>));
        let status = child.wait().await;
        // What the build left running (a console, a hook's daemon) may hold the pipes open: the
        // job ends with the process, after a moment for the last lines.
        let _ = tokio::time::timeout(Duration::from_secs(2), async { let _ = (&mut a).await; let _ = (&mut b).await; }).await;
        a.abort();
        b.abort();
        self.pids.lock().unwrap().remove(&id);
        release(&self);
        drop(turn);
        let output = output.lock().unwrap().clone();
        // `apparition build` keeps the signed build before it installs, so one it couldn't install
        // is still there for the device's Apparition to take.
        let kept_id = build_id_in(&output);
        if self.job(id).is_some_and(|j| j.status == "cancelled") {
            return Err("cancelled".into());
        }
        let status = status.map_err(|e| e.to_string())?;
        let launch_failed = status.code() == Some(LAUNCH_FAILED.into()) && kept_id.is_some();
        if launch_failed {
            say("==> Store: installed, but the app didn't launch (see above); the build counts as installed".into());
        }
        if !status.success() && !launch_failed {
            if kept_id.is_some() && !job.no_deploy && job.kind == "deploy" {
                self.update(id, |j| { j.build_id = kept_id.clone(); j.kept_build_id = kept_id.clone(); });
                say("==> The signed build was kept and can be installed from the device".into());
            }
            return Err(format!("apparition build exited with {}", status.code().map(|c| c.to_string()).unwrap_or_else(|| "a signal".into())));
        }
        let build_id = kept_id.ok_or("the build didn't say its Build ID")?;
        let kept = artifacts::read_build(&build_id, &ctx.build_store)?;
        let app = &kept.manifest["app"];
        if app["bundleId"].as_str() != Some(job.bundle_id.as_str()) && app["originalBundleId"].as_str() != Some(job.bundle_id.as_str()) {
            return Err(format!("the build is of {}, not the requested {}", app["bundleId"].as_str().unwrap_or("?"), job.bundle_id));
        }
        let build_no = app["build"].as_str().unwrap_or_default().to_string();
        let version = app["version"].as_str().map(String::from);
        let source = kept.manifest.get("source").filter(|s| !s.is_null()).map(|s| json!({
            "fingerprint": s.pointer("/before/fingerprint"), "stable": s.get("stable"), "anyDirty": s.pointer("/before/anyDirty"),
        }));
        if (job.alt_source.is_some() || job.mac_app.is_some()) && kept.manifest.pointer("/source/stable") == Some(&Value::Bool(false)) {
            return Err("source changed during compilation; refusing to publish this download".into());
        }
        self.update(id, |j| {
            j.build_id = Some(build_id.clone()); j.build_no = Some(build_no.clone()); j.version = version.clone();
            j.artifact = Some(kept.manifest["artifact"].clone()); j.source = source.clone();
        });

        // A Mac app is done once its zip is kept: no IPA, no device, no ledger.
        if let Some(slug) = &job.mac_app {
            let deliver = ctx.deliver.as_ref().ok_or("this Store doesn't publish Mac downloads")?;
            deliver.mac_built(slug, &job.rev, &work.branch, &kept)?;
            say(format!("\n==> Store: {} {} ({}@{}) ready to download", job.bundle_id, version.clone().unwrap_or_default(), work.branch, short));
            self.succeed(id);
            return Ok(());
        }
        if kept.manifest["artifact"]["kind"].as_str() != Some("ipa") {
            return Err("the build manifest doesn't describe an IPA".into());
        }
        if let Some(slug) = &job.alt_source {
            let deliver = ctx.deliver.as_ref().ok_or("this Store doesn't publish an AltSource")?;
            deliver.alt_source_built(slug, &job.rev, &kept.file)?;
            say(format!("==> Store: AltSource has {} @ {} ({build_no})", job.bundle_id, &job.rev[..job.rev.len().min(10)]));
        }
        // A durable copy for the OTA page: the build's own folder is the source of truth, but a
        // copy under data/ota/ is what an itms-services link serves.
        match self.publish_ota(ctx, job, &work.branch, &short, &kept, &version, &build_no) {
            Ok(slug) => say(format!("==> Store: published OTA copy {slug}.ipa")),
            Err(e) => say(format!("==> Store: WARNING failed to publish an OTA copy: {e}")),
        }

        match job.kind.as_str() {
            "update" => {
                // The installs, one device at a time, exactly as signed.
                let mut installs = Vec::new();
                for device in &job.devices {
                    if self.cancelled(id) {
                        say(format!("==> Cancelled: not installed on {device}"));
                        break;
                    }
                    let outcome = match &ctx.devices {
                        None => Err("this Store reaches no devices itself".to_string()),
                        Some(backend) => tokio::time::timeout(INSTALL_TIMEOUT, install_signed(backend.clone(), &kept, device)).await
                            .unwrap_or_else(|_| Err(format!("{device} didn't finish the install in {} minutes", INSTALL_TIMEOUT.as_secs() / 60))),
                    };
                    let at = system_ms();
                    match outcome {
                        Ok(done) => {
                            say(format!("==> Installed on {device}{}", if done.queued { " (it finishes when the app is closed there)" } else { "" }));
                            self.record(ctx, device, job, &work.branch, &short, &build_id, &version, &build_no, true);
                            installs.push(Install { device: device.clone(), ok: true, queued: done.queued, error: None, at });
                        }
                        Err(error) => {
                            say(format!("==> Not installed on {device}: {error}. The build is kept, so Apparition on {device} can install it."));
                            installs.push(Install { device: device.clone(), ok: false, queued: false, error: Some(error), at });
                        }
                    }
                }
                // Installed nowhere: the automatic mode leaves this commit alone for a while rather
                // than building it again every minute while the devices are away.
                if !installs.is_empty() && installs.iter().all(|i| !i.ok) {
                    self.failures.lock().unwrap().insert(format!("{}@{}", job.devices.join(","), job.rev), system_ms());
                }
                self.update(id, |j| j.installs = installs);
            }
            "deploy" if job.install_via != "device" => {
                // `mac` installed inside the build; `none` only kept it, which the ledger notes as a guess.
                self.record(ctx, &job.device, job, &work.branch, &short, &build_id, &version, &build_no, !job.no_deploy);
                say(if job.no_deploy {
                    format!("\n==> Store: OTA-only build of {} {} ({}@{short}), not deployed to {}", job.bundle_id, version.clone().unwrap_or_default(), work.branch, job.device)
                } else {
                    format!("\n==> Store: recorded install of {} {} ({}@{short}) on {}", job.bundle_id, version.clone().unwrap_or_default(), work.branch, job.device)
                });
            }
            "deploy" => say(format!("\n==> Store: built {} {} ({}@{short}); {}'s Apparition installs it", job.bundle_id, version.clone().unwrap_or_default(), work.branch, job.device)),
            _ => say(format!("\n==> Store: built {} {} ({}@{short})", job.bundle_id, version.clone().unwrap_or_default(), work.branch)),
        }
        self.succeed(id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn record(&self, ctx: &Context, device: &str, job: &Job, branch: &str, commit: &str, build_id: &str, version: &Option<String>, build_no: &str, confirmed: bool) {
        if job.alt_source.is_some() || job.for_resign {
            return;
        }
        ctx.state.record_install(device, &job.bundle_id, json!({
            "repo": job.repo, "branch": branch, "commit": commit, "buildId": build_id, "platform": job.platform,
            "version": version, "buildNo": build_no, "at": system_ms(), "confirmed": confirmed,
        }));
        // What the device reports now includes this build, so a plan made before its next look is right.
        ctx.state.update(|s| {
            if let Some(apps) = s.get_mut("deviceApps").and_then(|d| d.get_mut(device)).and_then(|d| d.get_mut("apps")).and_then(Value::as_object_mut) {
                if confirmed { apps.insert(job.bundle_id.clone(), json!({ "name": job.repo, "version": version, "buildNo": build_no })); }
            }
        });
    }

    /// Keeps the IPA under `data/ota/<bundle>-<platform>.ipa` and notes it in the state, as the
    /// OTA page and the installs from other devices expect.
    #[allow(clippy::too_many_arguments)]
    fn publish_ota(&self, ctx: &Context, job: &Job, branch: &str, commit: &str, kept: &artifacts::Kept, version: &Option<String>, build_no: &str) -> Result<String, String> {
        std::fs::create_dir_all(&self.ota).map_err(|e| e.to_string())?;
        let slug = clones::slug(&format!("{}-{}", job.bundle_id, job.platform));
        let dest = self.ota.join(format!("{slug}.ipa"));
        let _ = std::fs::remove_file(&dest);
        std::fs::copy(&kept.file, &dest).map_err(|e| e.to_string())?;
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        let name = kept.manifest["app"]["name"].as_str().unwrap_or(&job.repo).to_string();
        ctx.state.update(|s| {
            let ota = s.entry("ota").or_insert_with(|| json!({}));
            if !ota.is_object() { *ota = json!({}); }
            ota[format!("{}|{}", job.bundle_id, job.platform)] = json!({
                "slug": slug, "bundleId": job.bundle_id, "repo": job.repo, "branch": branch, "commit": commit,
                "name": name, "version": version, "buildNo": build_no, "size": size, "at": system_ms(),
            });
        });
        Ok(slug)
    }

    /// Asks the paired devices what they have installed and what they are, except those
    /// config.json says to leave alone (`ledgerOnlyDevices`) unless `force`.
    pub async fn refresh_devices(&self, ctx: &Context, force: bool) {
        let Some(backend) = ctx.devices.clone() else { return };
        if let Ok(paired) = backend.list().await {
            ctx.state.update(|s| {
                s.insert("devices".into(), json!({ "fetchedAt": system_ms(), "list": paired.iter().map(|d| json!({
                    "name": d.name, "model": d.model, "state": "connected", "udid": d.udid, "platform": d.platform,
                })).collect::<Vec<_>>() }));
            });
        }
        let mut names: Vec<String> = targets(&ctx.config).into_iter().map(|t| t.device).collect();
        names.sort();
        names.dedup();
        futures_util::future::join_all(names.iter().map(|d| self.refresh_device_apps(ctx, d, force))).await;
    }

    /// One device's installed apps (those of the repos' `bundlePrefix`) into the state.
    pub async fn refresh_device_apps(&self, ctx: &Context, device: &str, force: bool) {
        let Some(backend) = ctx.devices.clone() else { return };
        if !force && ctx.config.skipped_devices().iter().any(|d| d == device) {
            return;
        }
        {
            let mut asking = self.asking.lock().unwrap();
            if asking.iter().any(|d| d == device) { return }
            asking.push(device.to_string());
        }
        let answer = async {
            let known = backend.list().await?;
            let target = devices::find(&known, device)?;
            tokio::time::timeout(Duration::from_secs(60), backend.apps(target.udid.clone())).await
                .unwrap_or_else(|_| Err(format!("{device} didn't answer in 60 s")))
        }.await;
        let prefixes = &ctx.config.bundle_prefix;
        ctx.state.update(|s| {
            let all = s.entry("deviceApps").or_insert_with(|| json!({}));
            if !all.is_object() { *all = json!({}); }
            match answer {
                Ok(apps) => {
                    let apps: serde_json::Map<String, Value> = apps.into_iter()
                        .filter(|a| prefixes.is_empty() || prefixes.iter().any(|p| a.bundle_id.starts_with(p)))
                        .map(|a| (a.bundle_id, json!({ "name": a.name, "version": a.version, "buildNo": a.build }))).collect();
                    all[device] = json!({ "fetchedAt": system_ms(), "apps": apps });
                }
                Err(error) => {
                    let mut entry = all.get(device).cloned().filter(Value::is_object).unwrap_or_else(|| json!({ "apps": {} }));
                    entry["error"] = json!(error);
                    entry["erroredAt"] = json!(system_ms());
                    all[device] = entry;
                }
            }
        });
        self.asking.lock().unwrap().retain(|d| d != device);
    }

    /// One pass of the automatic mode: builds what is behind once its sources have been still
    /// for `settle` minutes, except a build that failed for the same commit lately.
    pub fn auto_tick(self: &Arc<Self>, ctx: &Context, settle_minutes: u64) -> Vec<Job> {
        let now = system_ms();
        let rows = self.plan(ctx);
        let due: Vec<Group> = self.groups(ctx, &rows).into_iter().filter(|g| {
            let settled = now.saturating_sub(g.latest_at) >= settle_minutes * 60_000;
            let failed = self.failures.lock().unwrap().get(&format!("{}@{}", g.devices.join(","), g.rev)).is_some_and(|at| now - at < RETRY_AFTER_MS);
            settled && !failed
        }).collect();
        if due.is_empty() { Vec::new() } else { self.enqueue(ctx, due, true) }
    }

    /// Clears slots unused for `cloneMaxAgeDays` (config, default 7).
    pub fn reap(&self, ctx: &Context) {
        let days = ctx.config.raw.get("cloneMaxAgeDays").and_then(Value::as_u64).filter(|d| *d > 0).unwrap_or(7);
        let busy = self.busy.lock().unwrap().clone();
        for dir in clones::reap(&self.clones, Duration::from_secs(days * 86400), &busy) {
            eprintln!("cleared the unused build clone {}", dir.display());
        }
    }

    /// The `/api/updates` reply.
    pub fn report(&self, ctx: &Context) -> Value {
        let rows = self.plan(ctx);
        let groups = self.groups(ctx, &rows);
        let (enabled, settle) = auto_settings(&ctx.config);
        let mut jobs = self.jobs();
        jobs.reverse();
        jobs.truncate(40);
        json!({
            "rows": rows, "groups": groups, "builds": jobs,
            "auto": { "enabled": enabled, "settleMinutes": settle },
            "maxConcurrentBuilds": max_builds(&ctx.config),
            "devices": ctx.devices.is_some(),
        })
    }
}

/// The kept build's ID from `apparition build`'s output ("  Build ID:    <id>").
pub fn build_id_in(output: &str) -> Option<String> {
    output.lines().rev().find_map(|l| l.trim().strip_prefix("Build ID:").map(|id| id.trim().to_string()))
        .filter(|id| artifacts::valid_build_id(id))
}

/// Installs a signed kept build as it is (it was signed for its devices at build time).
async fn install_signed(backend: Arc<dyn Devices>, kept: &artifacts::Kept, device: &str) -> Result<devices::Installed, String> {
    let known = backend.list().await?;
    let target = devices::find(&known, device)?;
    let built = kept.manifest["platform"].as_str().unwrap_or("").to_lowercase();
    let fits = built.is_empty() || built == target.platform.to_lowercase() || (built == "ios" && target.platform == "visionOS");
    if !fits {
        return Err(format!("the build is for {built}, and {} runs {}", target.name, target.platform));
    }
    backend.install_signed(target.udid.clone(), kept.file.clone()).await
}

#[cfg(unix)]
fn stop(pid: u32) {
    // The build's own process group: `apparition build` and the xcodebuild it started.
    // SAFETY: killpg only sends a signal to the group this queue created for the child.
    unsafe { libc::killpg(pid as libc::pid_t, libc::SIGTERM) };
}

#[cfg(not(unix))]
fn stop(_pid: u32) {}

/// PATH for what the Store runs: the Store's own, plus Homebrew's and the system's folders. An
/// app opened from the Finder or at login has only launchd's PATH (`/usr/bin:/bin:...`), and
/// build phases call Homebrew tools (swiftlint, bash 5 through `env bash`).
pub fn child_path() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    #[cfg(unix)]
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !dirs.iter().any(|x| x == Path::new(d)) {
            dirs.push(PathBuf::from(d));
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

struct LogWriter(Arc<Mutex<std::fs::File>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.0.lock().unwrap().write(buf) }
    fn flush(&mut self) -> std::io::Result<()> { self.0.lock().unwrap().flush() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_id_comes_from_the_report() {
        assert_eq!(build_id_in("==> Signing...\n  Build ID:    0123abcd\n  Done\n").as_deref(), Some("0123abcd"));
        assert_eq!(build_id_in("nothing here"), None);
    }

    #[test]
    fn targets_follow_device_branches_and_leave_watch_rows_out() {
        let config: Config = serde_json::from_value(json!({})).unwrap();
        let config = Config { raw: json!({ "repos": [{
            "name": "App", "path": "/p/App", "branches": ["main", "dev"], "deviceBranches": { "AVP": "dev" },
            "platforms": [
                { "platform": "visionOS", "bundleId": "a", "device": "AVP" },
                { "platform": "iOS", "bundleId": "a", "device": "ixPhone" },
                { "platform": "watchOS", "bundleId": "a.watch", "device": "iWatch" },
            ],
        }] }), ..config };
        let t = targets(&config);
        assert_eq!(t.len(), 2);
        assert_eq!((t[0].device.as_str(), t[0].default_branch.as_str()), ("AVP", "dev"));
        assert_eq!((t[1].device.as_str(), t[1].default_branch.as_str()), ("ixPhone", "main"));
        assert_eq!(auto_settings(&config), (false, 5));
        assert_eq!(max_builds(&config), 2);
    }

    #[test]
    fn build_settings_scope_by_branch_then_platform() {
        let all = json!({ "buildSettings": ["A=1"] });
        assert_eq!(build_settings(&all, "any", "iOS"), ["A=1"]);
        let by_branch = json!({ "buildSettings": { "dev": ["B=2"], "*": ["C=3"] } });
        assert_eq!(build_settings(&by_branch, "dev", "iOS"), ["B=2"]);
        assert_eq!(build_settings(&by_branch, "main", "iOS"), ["C=3"]);
        let by_platform = json!({ "buildSettings": { "*": { "visionOS": ["D=4"], "*": ["E=5"] } } });
        assert_eq!(build_settings(&by_platform, "main", "visionOS"), ["D=4"]);
        assert_eq!(build_settings(&by_platform, "main", "iOS"), ["E=5"]);
        assert!(build_settings(&json!({}), "main", "iOS").is_empty());
    }
}
