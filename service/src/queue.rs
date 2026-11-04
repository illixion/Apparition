//! The Store's own build queue, for updates: it builds apps that are behind (`updates.rs`) in
//! build clones (`clones.rs`) with `apparition build`, then installs each build on its devices
//! through this computer's own pairings.
//!
//! Efficient with many apps at once:
//! - one build per platform build, not per device: platform rows whose conf resolves alike
//!   (`apparition build --plan`: the `iPad` alias is the iOS build) share it, so Hypnos for the
//!   iPhone and the iPad is one build installed twice;
//! - at most `maxConcurrentBuilds` (config, default 2) builds at once, and one at a time per
//!   repo and branch, since a slot holds one checkout;
//! - slots are kept, so each build checks out only the new commits and Xcode builds
//!   incrementally;
//! - installs go one device at a time (the devices' own rule) after the build, while other
//!   builds go on; a device that can't be reached keeps the build for its Apparition to install.
//!
//! The queue keeps its own file, `data/queue.json` (jobs, and a ledger of what it installed), so
//! it never races the Node store over `state.json` while both run. Job ids start at 1000000, clear
//! of the Node store's.

use crate::{
    artifacts,
    auth::system_ms,
    clones,
    config::Config,
    devices::{self, Devices},
    updates::{self, Installed, Row, Target},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
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

/// A queued build, in the shape the web page reads the Node store's jobs in, plus the devices it
/// goes to and why.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: u64,
    #[serde(default)]
    pub batch_id: Option<u64>,
    pub kind: String,
    pub repo: String,
    pub branch: String,
    pub platform: String,
    pub device: String,
    pub devices: Vec<String>,
    pub bundle_id: String,
    /// The app commit to build.
    pub rev: String,
    pub status: String,
    pub queued_at: u64,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub built_branch: Option<String>,
    #[serde(default)]
    pub build_id: Option<String>,
    #[serde(default)]
    pub build_no: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// What made it due: "vergence: Double 50-60 fps video…".
    #[serde(default)]
    pub why: Vec<String>,
    /// Started by the automatic mode, not a person.
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub installs: Vec<Install>,
    pub install_via: String,
}

impl Job {
    fn active(&self) -> bool {
        matches!(self.status.as_str(), "queued" | "running")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    pub repo: String,
    pub branch: String,
    pub commit: String,
    pub build_id: String,
    pub build_no: String,
    pub platform: String,
    pub at: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Saved {
    next_id: u64,
    jobs: Vec<Job>,
    /// device -> bundle id -> what this queue installed there.
    ledger: BTreeMap<String, BTreeMap<String, Ledger>>,
}

/// What a device reported having installed (its build numbers), when it was last asked.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub fetched_at: u64,
    pub apps: HashMap<String, String>,
    pub error: Option<String>,
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

/// What a run needs from the Store around it.
#[derive(Clone)]
pub struct Context {
    pub config: Arc<Config>,
    pub build_store: PathBuf,
    pub cli: PathBuf,
    pub devices: Option<Arc<dyn Devices>>,
    /// The Node store's `state.json`, for its install ledger while it still runs.
    pub node_state: PathBuf,
}

pub struct Queue {
    file: PathBuf,
    pub logs: PathBuf,
    pub clones: PathBuf,
    saved: Mutex<Saved>,
    slots: Arc<tokio::sync::Semaphore>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pids: Mutex<HashMap<u64, u32>>,
    inventory: Mutex<HashMap<String, Inventory>>,
    manifests: updates::Manifests,
    plans: Mutex<HashMap<(String, String), (u64, Option<Value>)>>,
    /// group key + commit -> when its build failed.
    failures: Mutex<HashMap<String, u64>>,
    /// Slots a build is using now, which `reap` leaves alone.
    busy: Mutex<Vec<PathBuf>>,
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

fn ledger_only(config: &Config) -> Vec<String> {
    config.raw.get("ledgerOnlyDevices").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(String::from).collect()
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
            file, logs: data_dir.join("logs"), clones: data_dir.join("clones"),
            saved: Mutex::new(saved), slots: Arc::new(tokio::sync::Semaphore::new(max)),
            locks: Mutex::default(), pids: Mutex::default(), inventory: Mutex::default(),
            manifests: updates::Manifests::default(), plans: Mutex::default(), failures: Mutex::default(), busy: Mutex::default(),
        }
    }

    fn save(&self, saved: &Saved) {
        let Ok(text) = serde_json::to_string_pretty(saved) else { return };
        let tmp = self.file.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.file);
        }
    }

    fn update(&self, id: u64, change: impl FnOnce(&mut Job)) -> Option<Job> {
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

    pub fn inventory(&self) -> HashMap<String, Inventory> {
        self.inventory.lock().unwrap().clone()
    }

    /// What is installed where, the newest word wins: the device itself, this queue's ledger, the
    /// Node store's (an OTA offer it never saw installed doesn't count).
    pub fn installed(&self, ctx: &Context) -> HashMap<(String, String), Installed> {
        let mut out: HashMap<(String, String), Installed> = HashMap::new();
        fn offer(out: &mut HashMap<(String, String), Installed>, device: &str, bundle: &str, i: Installed) {
            let key = (device.to_string(), bundle.to_string());
            if out.get(&key).is_none_or(|old| i.at >= old.at) {
                out.insert(key, i);
            }
        }
        let node: Value = std::fs::read_to_string(&ctx.node_state).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
        for (device, apps) in node["installs"].as_object().into_iter().flatten() {
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
        for (device, apps) in &self.saved.lock().unwrap().ledger {
            for (bundle, e) in apps {
                offer(&mut out, device, bundle, Installed { build_no: e.build_no.clone(), build_id: Some(e.build_id.clone()), branch: Some(e.branch.clone()), at: e.at, from: "ledger" });
            }
        }
        for (device, inventory) in self.inventory.lock().unwrap().iter() {
            for (bundle, build) in &inventory.apps {
                // The device knows what is there, but not which build that was: keep a ledger's
                // build ID when its build number agrees.
                let key = (device.clone(), bundle.clone());
                let same = out.get(&key).filter(|l| &l.build_no == build).cloned();
                match same {
                    Some(l) => offer(&mut out, device, bundle, Installed { at: inventory.fetched_at.max(l.at), from: "device", ..l }),
                    None => offer(&mut out, device, bundle, Installed { build_no: build.clone(), build_id: None, branch: None, at: inventory.fetched_at, from: "device" }),
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

    /// Queues a build per group, except one already queued or running for the same build.
    pub fn enqueue(self: &Arc<Self>, ctx: &Context, groups: Vec<Group>, auto: bool) -> Vec<Job> {
        let mut made = Vec::new();
        {
            let mut saved = self.saved.lock().unwrap();
            let batch = (groups.len() > 1).then_some(saved.next_id);
            for g in groups {
                let duplicate = saved.jobs.iter().any(|j| j.active() && j.repo == g.repo && j.branch == g.branch && j.bundle_id == g.bundle_id
                    && j.rev == g.rev && g.devices.iter().all(|d| j.devices.contains(d)));
                if duplicate {
                    continue;
                }
                let job = Job {
                    id: saved.next_id, batch_id: batch, kind: "update".into(), repo: g.repo, branch: g.branch, platform: g.platform,
                    device: g.devices.first().cloned().unwrap_or_default(), devices: g.devices, bundle_id: g.bundle_id, rev: g.rev,
                    status: "queued".into(), queued_at: system_ms(), started_at: None, finished_at: None, commit: None, built_branch: None,
                    build_id: None, build_no: None, error: None, why: g.why, auto, installs: Vec::new(), install_via: "store".into(),
                };
                saved.next_id += 1;
                saved.jobs.push(job.clone());
                made.push(job);
            }
            let excess = saved.jobs.len().saturating_sub(KEEP_JOBS);
            saved.jobs.drain(..excess);
            self.save(&saved);
        }
        for job in &made {
            let (queue, ctx, id) = (self.clone(), ctx.clone(), job.id);
            tokio::spawn(async move { queue.run(ctx, id).await });
        }
        made
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

    async fn run(self: Arc<Self>, ctx: Context, id: u64) {
        let Some(job) = self.job(id) else { return };
        let lock = self.locks.lock().unwrap().entry(format!("{}|{}", job.repo, job.branch)).or_default().clone();
        let _slot_turn = lock.lock().await;
        let Ok(_permit) = self.slots.clone().acquire_owned().await else { return };
        if self.job(id).is_none_or(|j| j.status != "queued") {
            return;
        }
        self.update(id, |j| { j.status = "running".into(); j.started_at = Some(system_ms()); });
        let result = self.clone().build(&ctx, &job).await;
        match result {
            Ok(()) => {}
            Err(error) => {
                let cancelled = self.job(id).is_some_and(|j| j.status == "cancelled");
                if !cancelled {
                    self.failures.lock().unwrap().insert(format!("{}@{}", job.devices.join(","), job.rev), system_ms());
                    self.update(id, |j| { j.status = "failed".into(); j.error = Some(error); j.finished_at = Some(system_ms()); });
                }
            }
        }
    }

    async fn build(self: Arc<Self>, ctx: &Context, job: &Job) -> Result<(), String> {
        let id = job.id;
        std::fs::create_dir_all(&self.logs).map_err(|e| e.to_string())?;
        let log_file = std::fs::File::create(self.log_path(id)).map_err(|e| e.to_string())?;
        let log = Arc::new(Mutex::new(log_file));
        let say = |text: String| { let _ = writeln!(log.lock().unwrap(), "{text}"); };
        let target = targets(&ctx.config).into_iter().find(|t| t.repo == job.repo).ok_or("the repo is no longer in config.json")?;
        let exclude: Vec<String> = ctx.config.raw.get("repos").and_then(Value::as_array).into_iter().flatten()
            .find(|r| r["name"].as_str() == Some(&job.repo)).and_then(|r| r["cloneExclude"].as_array())
            .into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        say(format!("==> Update build of {} [{}@{}] for {}", job.repo, job.branch, &job.rev[..job.rev.len().min(10)], job.devices.join(", ")));
        for w in &job.why { say(format!("    {w}")); }

        // The clone, off the async threads.
        let (clones_dir, repo, branch, rev, log2) = (self.clones.clone(), target.repo_path.clone(), job.branch.clone(), job.rev.clone(), log.clone());
        let slot = clones::slot(&clones_dir, &repo, &branch);
        self.busy.lock().unwrap().push(slot.clone());
        let prepared = tokio::task::spawn_blocking(move || {
            let mut writer = LogWriter(log2);
            clones::prepare(&clones_dir, &repo, &branch, &rev, &exclude, &mut writer)
        }).await.map_err(|e| e.to_string());
        let app_dir = match prepared {
            Ok(Ok(dir)) => dir,
            Ok(Err(e)) | Err(e) => { self.busy.lock().unwrap().retain(|s| s != &slot); return Err(e) }
        };
        let short = updates::Git::default().run(&app_dir, &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        self.update(id, |j| { j.commit = Some(short.clone()); j.built_branch = Some(job.branch.clone()); });

        // The build.
        say(format!("==> {} build --no-deploy --no-mcp (PLATFORM={})\n", ctx.cli.display(), job.platform));
        let mut command = tokio::process::Command::new(&ctx.cli);
        command.args(["build", "--no-deploy", "--no-mcp"]).current_dir(&app_dir)
            .env("PLATFORM", &job.platform).env("APPARITION_BUILD_DIR", &ctx.build_store)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|e| { self.busy.lock().unwrap().retain(|s| s != &slot); format!("couldn't start {}: {e}", ctx.cli.display()) })?;
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
        let a = pump(child.stdout.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>));
        let b = pump(child.stderr.take().map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>));
        let status = child.wait().await;
        let _ = (a.await, b.await);
        self.pids.lock().unwrap().remove(&id);
        self.busy.lock().unwrap().retain(|s| s != &slot);
        if self.job(id).is_some_and(|j| j.status == "cancelled") {
            return Err("cancelled".into());
        }
        let status = status.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("apparition build exited with {}", status.code().map(|c| c.to_string()).unwrap_or_else(|| "a signal".into())));
        }
        let build_id = build_id_in(&output.lock().unwrap()).ok_or("the build didn't say its Build ID")?;
        let kept = artifacts::read_build(&build_id, &ctx.build_store)?;
        let build_no = kept.manifest.pointer("/app/build").and_then(Value::as_str).unwrap_or_default().to_string();
        self.update(id, |j| { j.build_id = Some(build_id.clone()); j.build_no = Some(build_no.clone()); });

        // The installs, one device at a time.
        let mut installs = Vec::new();
        for device in &job.devices {
            let outcome = match &ctx.devices {
                None => Err(format!("this Store reaches no devices; Apparition on {device} can install it")),
                Some(backend) => install_signed(backend.clone(), &kept, device).await,
            };
            let at = system_ms();
            match outcome {
                Ok(done) => {
                    say(format!("==> Installed on {device}{}", if done.queued { " (it finishes when the app is closed there)" } else { "" }));
                    self.record(device, &job.bundle_id, Ledger {
                        repo: job.repo.clone(), branch: job.branch.clone(), commit: short.clone(), build_id: build_id.clone(),
                        build_no: build_no.clone(), platform: job.platform.clone(), at,
                    });
                    installs.push(Install { device: device.clone(), ok: true, queued: done.queued, error: None, at });
                }
                Err(error) => {
                    say(format!("==> Not installed on {device}: {error}. The build is kept, so Apparition on {device} can install it."));
                    installs.push(Install { device: device.clone(), ok: false, queued: false, error: Some(error), at });
                }
            }
        }
        self.update(id, |j| { j.installs = installs; j.status = "success".into(); j.finished_at = Some(system_ms()); });
        Ok(())
    }

    fn record(&self, device: &str, bundle: &str, entry: Ledger) {
        if let Some(inventory) = self.inventory.lock().unwrap().get_mut(device) {
            inventory.apps.insert(bundle.to_string(), entry.build_no.clone());
        }
        let mut saved = self.saved.lock().unwrap();
        saved.ledger.entry(device.to_string()).or_default().insert(bundle.to_string(), entry);
        self.save(&saved);
    }

    /// Asks each paired device what it has installed, except those config.json says not to wake
    /// (`ledgerOnlyDevices`): what the ledgers say stands for those.
    pub async fn refresh_inventory(&self, ctx: &Context) {
        let Some(backend) = ctx.devices.clone() else { return };
        let Ok(paired) = backend.list().await else { return };
        let skip = ledger_only(&ctx.config);
        let wanted: Vec<String> = targets(&ctx.config).into_iter().map(|t| t.device).collect();
        let asks = paired.into_iter().filter(|d| wanted.contains(&d.name) && !skip.contains(&d.name)).map(|d| {
            let backend = backend.clone();
            async move {
                let answer = tokio::time::timeout(Duration::from_secs(30), backend.apps(d.udid.clone())).await
                    .unwrap_or_else(|_| Err(format!("{} didn't answer in 30 s", d.name)));
                (d.name, answer)
            }
        });
        for (name, answer) in futures_util::future::join_all(asks).await {
            let mut inventory = self.inventory.lock().unwrap();
            let entry = inventory.entry(name).or_default();
            match answer {
                Ok(apps) => {
                    *entry = Inventory {
                        fetched_at: system_ms(), error: None,
                        apps: apps.into_iter().filter_map(|a| Some((a.bundle_id, a.build?))).collect(),
                    };
                }
                Err(error) => entry.error = Some(error),
            }
        }
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
            "inventory": self.inventory(),
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
}
