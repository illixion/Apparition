//! Mac apps: a repo's `macApps` entries, downloaded as a signed `.app.zip` built on demand
//! (`store/lib/macapps.js`).
//!
//! config.json, per repo: `macApps: [{name, platform, bundleId, branch}]`. `platform` (default
//! "macOS") reaches build-and-sign as PLATFORM, so a repo with several Mac apps names each one
//! with an alias its build-signing.conf maps to a scheme and bundle id. `branch` is the one
//! offered by default; a download may name any branch the repo offers.
//!
//! A download names a commit (`/mac/<slug>/<commit>.app.zip`). If that commit is built, the
//! zip is served; otherwise the Store builds exactly that commit in a clean build clone (so the
//! CFBundleVersion stamp is never `-dirty` and the cache can trust the SHA), zips the signed
//! `.app` and keeps the newest [`KEEP_PER_APP`] per app. Requests for a commit being built join
//! that build. Everything touching the build queue is behind [`Deps`], so tests drive it
//! without Xcode. The long-poll hold and the 307/302 redirects belong to the HTTP layer.

use crate::artifacts::Kept;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub const KEEP_PER_APP: usize = 3;
/// A failed build is reported to further requests for this long instead of starting another.
pub const FAILURE_MEMORY_MS: u64 = 2 * 60_000;

/// `<bundleId>-<platform>` with anything outside `A-Za-z0-9._-` collapsed to `-`.
pub fn slug_of(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "._-".contains(c) {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out
}

/// Short SHAs from two places may be abbreviated to different lengths.
pub fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MacApp {
    pub slug: String,
    pub repo: String,
    pub path: String,
    pub name: String,
    pub platform: String,
    pub bundle_id: String,
    pub branch: String,
}

/// The branch a repo's entry follows: its own `altSource.branch`, else main/master when offered,
/// else the first branch offered.
fn source_branch(rc: &Value) -> String {
    if let Some(b) = rc.pointer("/altSource/branch").and_then(Value::as_str) {
        return b.to_string();
    }
    let branches: Vec<&str> = rc.get("branches").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    ["main", "master"].into_iter().find(|b| branches.contains(b))
        .or(branches.first().copied()).unwrap_or("main").to_string()
}

/// Every configured Mac app, keyed `<bundleId>-<platform>` like the OTA and AltSource slugs.
/// Entries without a bundleId are skipped, and the first of two with one slug wins.
pub fn mac_apps(cfg: &Value) -> Vec<MacApp> {
    let mut out: Vec<MacApp> = Vec::new();
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    for rc in cfg.get("repos").and_then(Value::as_array).into_iter().flatten() {
        let repo = rc.get("name").and_then(Value::as_str).unwrap_or_default();
        for m in rc.get("macApps").and_then(Value::as_array).into_iter().flatten() {
            let Some(bundle_id) = text(m, "bundleId") else { continue };
            let platform = text(m, "platform").unwrap_or_else(|| "macOS".into());
            let slug = slug_of(&format!("{bundle_id}-{platform}"));
            if out.iter().any(|a| a.slug == slug) {
                continue;
            }
            out.push(MacApp {
                slug, repo: repo.to_string(),
                path: rc.get("path").and_then(Value::as_str).unwrap_or_default().to_string(),
                name: text(m, "name").unwrap_or_else(|| repo.to_string()), platform, bundle_id,
                branch: m.get("branch").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| source_branch(rc)),
            });
        }
    }
    out
}

/// A build job as far as this module cares: `status` is the queue's word for it ("queued",
/// "running", "success", "failed", "cancelled", ...).
#[derive(Clone, Debug)]
pub struct JobView {
    pub status: String,
    pub error: Option<String>,
}

impl JobView {
    fn active(&self) -> bool {
        self.status == "queued" || self.status == "running"
    }
}

/// What the build queue gives this module. Called with the module's lock held, so an
/// implementation must not call back into [`MacBuilds`].
pub trait Deps: Send + Sync {
    /// [`mac_apps`] of the live config.
    fn apps(&self) -> Vec<MacApp>;
    /// Queue a build of `commit` of `branch` for `app`; returns the job id.
    fn enqueue(&self, app: &MacApp, commit: &str, branch: &str) -> u64;
    /// The job, or `None` once the queue has forgotten it.
    fn job(&self, id: u64) -> Option<JobView>;
}

/// One kept build, as stored in `index.json` (the JS store's format) and listed on the page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MacBuild {
    pub commit: String,
    pub branch: String,
    #[serde(default)]
    pub build_no: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    /// The `.app` folder name inside the zip, e.g. `Fake.app`.
    #[serde(default)]
    pub bundle: String,
    #[serde(default)]
    pub min_os_version: Option<String>,
    pub file: String,
    #[serde(default)]
    pub size: u64,
    /// Milliseconds since the epoch.
    #[serde(default)]
    pub at: u64,
    #[serde(default)]
    pub build_id: Option<String>,
    #[serde(default)]
    pub artifact_sha256: Option<String>,
}

/// What a request for `slug`@`commit` gets right now.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Built: serve [`MacBuilds::file`] of the entry.
    Ready(MacBuild),
    /// A build is queued or running (this job id): wait on it, then call `outcome`.
    Building(u64),
    /// Unknown app (404) or its build failed (502).
    Failed { status: u16, error: String },
}

#[derive(Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    apps: BTreeMap<String, Vec<MacBuild>>,
}

#[derive(Default)]
struct Inner {
    index: Index,
    /// (slug, commit) -> job id
    in_flight: HashMap<(String, String), u64>,
    /// (slug, commit) -> (error, at ms)
    failed: HashMap<(String, String), (String, u64)>,
}

pub struct MacBuilds {
    dir: PathBuf,
    deps: Box<dyn Deps>,
    log: Box<dyn Fn(&str) + Send + Sync>,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    inner: Mutex<Inner>,
}

fn clock() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl MacBuilds {
    /// Keeps its zips and `index.json` in `dir` (`<data>/macapps/`), created if missing.
    pub fn new(dir: impl Into<PathBuf>, deps: Box<dyn Deps>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let index = fs::read_to_string(dir.join("index.json")).ok()
            .and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Ok(Self { dir, deps, log: Box::new(|_| {}), now: Box::new(clock),
            inner: Mutex::new(Inner { index, ..Inner::default() }) })
    }

    pub fn with_log(mut self, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Box::new(log);
        self
    }

    /// Replaces the clock (milliseconds since the epoch), for tests of the failure memory.
    pub fn with_clock(mut self, now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, inner: &Inner) -> io::Result<()> {
        let path = self.dir.join("index.json");
        let tmp = self.dir.join("index.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&inner.index).map_err(io::Error::other)?)?;
        fs::rename(tmp, path)
    }

    /// The kept builds of an app whose zip is still on disk, newest first.
    fn entries(&self, inner: &Inner, slug: &str) -> Vec<MacBuild> {
        inner.index.apps.get(slug).into_iter().flatten()
            .filter(|e| self.dir.join(&e.file).exists()).cloned().collect()
    }

    fn cached_in(&self, inner: &Inner, slug: &str, commit: &str) -> Option<MacBuild> {
        self.entries(inner, slug).into_iter().find(|e| same_commit(&e.commit, commit))
    }

    /// Moves builds the queue has finished out of `in_flight`, remembering the failed ones. The
    /// JS store did this from a `waitFor` callback; the server may call this when a build ends
    /// to date the failure exactly, and every other method calls it first.
    pub fn sweep(&self) {
        let mut inner = self.lock();
        self.sweep_in(&mut inner);
    }

    fn sweep_in(&self, inner: &mut Inner) {
        let keys: Vec<_> = inner.in_flight.keys().cloned().collect();
        for key in keys {
            let id = inner.in_flight[&key];
            let job = self.deps.job(id);
            if job.as_ref().is_some_and(JobView::active) {
                continue;
            }
            inner.in_flight.remove(&key);
            if let Some(done) = job.filter(|j| j.status != "success") {
                let error = format!("build {}: {}", done.status, done.error.as_deref().filter(|e| !e.is_empty()).unwrap_or("no app"));
                inner.failed.insert(key, (error, (self.now)()));
            }
        }
    }

    pub fn app_by_slug(&self, slug: &str) -> Option<MacApp> {
        self.deps.apps().into_iter().find(|a| a.slug == slug)
    }

    /// The kept build of `commit` (abbreviations of either length match), if its zip exists.
    pub fn cached(&self, slug: &str, commit: &str) -> Option<MacBuild> {
        self.cached_in(&self.lock(), slug, commit)
    }

    /// The newest kept build of an app.
    pub fn latest(&self, slug: &str) -> Option<MacBuild> {
        self.entries(&self.lock(), slug).into_iter().next()
    }

    /// Where an entry's zip is.
    pub fn file(&self, entry: &MacBuild) -> PathBuf {
        self.dir.join(&entry.file)
    }

    /// What a download of `slug`@`commit` gets right now. `branch` is the branch to build when it
    /// has to be (default: the app's own).
    pub fn request(&self, slug: &str, commit: &str, branch: Option<&str>) -> Request {
        let Some(app) = self.app_by_slug(slug) else {
            return Request::Failed { status: 404, error: "unknown Mac app".into() };
        };
        let mut inner = self.lock();
        self.sweep_in(&mut inner);
        if let Some(hit) = self.cached_in(&inner, slug, commit) {
            return Request::Ready(hit);
        }
        let key = (slug.to_string(), commit.to_string());
        if let Some(&id) = inner.in_flight.get(&key) {
            return Request::Building(id);
        }
        if let Some((error, at)) = inner.failed.get(&key) {
            if (self.now)().saturating_sub(*at) < FAILURE_MEMORY_MS {
                return Request::Failed { status: 502, error: error.clone() };
            }
        }
        let id = self.deps.enqueue(&app, commit, branch.filter(|b| !b.is_empty()).unwrap_or(&app.branch));
        inner.in_flight.insert(key.clone(), id);
        inner.failed.remove(&key);
        Request::Building(id)
    }

    /// After waiting on a build: the zip, or why there isn't one (still building: `Building`
    /// again, for the 307).
    pub fn outcome(&self, slug: &str, commit: &str, job_id: u64) -> Request {
        let mut inner = self.lock();
        self.sweep_in(&mut inner);
        if let Some(hit) = self.cached_in(&inner, slug, commit) {
            return Request::Ready(hit);
        }
        match self.deps.job(job_id) {
            Some(j) if j.active() => Request::Building(job_id),
            Some(j) => Request::Failed { status: 502, error: format!("build {}: {}", j.status,
                j.error.as_deref().filter(|e| !e.is_empty()).unwrap_or("it produced no app")) },
            None => Request::Failed { status: 502, error: "build lost".into() },
        }
    }

    /// A repo's Mac apps for the page (`repos[].macApps` of `/api/state`), each with its kept
    /// builds (newest first, without the file name) and the builds in progress.
    pub fn list(&self, repo: &str) -> Vec<Value> {
        let apps = self.deps.apps();
        let mut inner = self.lock();
        self.sweep_in(&mut inner);
        apps.iter().filter(|a| a.repo == repo).map(|a| {
            let built: Vec<Value> = self.entries(&inner, &a.slug).iter().map(|e| {
                let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    o.remove("file");
                }
                v
            }).collect();
            let mut building: Vec<(&String, u64)> = inner.in_flight.iter()
                .filter(|((slug, _), _)| *slug == a.slug).map(|((_, commit), id)| (commit, *id)).collect();
            building.sort_by_key(|(_, id)| *id);
            json!({
                "slug": a.slug, "name": a.name, "platform": a.platform, "bundleId": a.bundle_id, "branch": a.branch,
                "built": built,
                "building": building.iter().map(|(commit, id)| json!({"commit": commit, "jobId": id})).collect::<Vec<_>>(),
            })
        }).collect()
    }

    /// Zip a signed `.app` as this app's build of `rev`, newest first. Refuses an app that isn't
    /// the one configured: a conf alias that fell through to another scheme would otherwise be
    /// cached and served under this app's name.
    ///
    /// With `kept` (the registered build the manifest describes) its artifact is copied as it is
    /// and its manifest supplies the metadata, and `app_path` only names the bundle; without, the
    /// `.app` at `app_path` is zipped under its product name (build-and-sign's `<Product>-macOS.app`
    /// loses the suffix) and its Info.plist read. Blocking: call it from `spawn_blocking`.
    pub fn record(&self, slug: &str, rev: &str, branch: &str, app_path: &Path, kept: Option<&Kept>) -> Result<MacBuild, String> {
        let app = self.app_by_slug(slug).ok_or_else(|| format!("unknown Mac app {slug}"))?;
        let meta = match kept {
            Some(k) => {
                if k.manifest.pointer("/artifact/kind").and_then(Value::as_str) != Some("app.zip") {
                    return Err("Mac build manifest does not describe an app zip".into());
                }
                let text = |key: &str| k.manifest.pointer(&format!("/app/{key}")).and_then(Value::as_str).map(str::to_string);
                AppMeta { name: text("name").unwrap_or_default(), bundle_id: text("bundleId").unwrap_or_default(),
                    version: text("version").unwrap_or_default(), build_no: text("build").unwrap_or_default(),
                    min_os_version: text("minOSVersion") }
            }
            None => inspect_app(app_path)?,
        };
        if meta.bundle_id != app.bundle_id {
            return Err(format!("the build produced {}, not {}: check the {} block of {}'s build-signing.conf",
                if meta.bundle_id.is_empty() { "an app with no bundle id" } else { &meta.bundle_id },
                app.bundle_id, app.platform, app.repo));
        }
        let file = format!("{slug}-{}.app.zip", slug_of(rev));
        let tmp = self.dir.join(format!(".incoming-{file}"));
        let _ = fs::remove_file(&tmp);
        let packed = match kept {
            Some(k) => fs::copy(&k.file, &tmp).map(|_| file_name(app_path)).map_err(|e| format!("could not copy the registered artifact: {e}")),
            None => zip_app(app_path, &tmp),
        };
        let bundle = packed.inspect_err(|_| { let _ = fs::remove_file(&tmp); })?;
        let dest = self.dir.join(&file);
        fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        let entry = MacBuild {
            commit: rev.to_string(), branch: branch.to_string(), build_no: meta.build_no, version: meta.version,
            name: meta.name, bundle, min_os_version: meta.min_os_version, file: file.clone(),
            size: fs::metadata(&dest).map_or(0, |m| m.len()), at: (self.now)(),
            build_id: kept.and_then(|k| k.manifest.get("buildId").and_then(Value::as_str)).map(str::to_string),
            artifact_sha256: kept.and_then(|k| k.manifest.pointer("/artifact/sha256").and_then(Value::as_str)).map(str::to_string),
        };
        {
            let mut inner = self.lock();
            let mut list = vec![entry.clone()];
            list.extend(inner.index.apps.get(slug).into_iter().flatten().filter(|e| e.file != file).cloned());
            for old in list.iter().skip(KEEP_PER_APP) {
                let _ = fs::remove_file(self.dir.join(&old.file));
            }
            list.truncate(KEEP_PER_APP);
            inner.index.apps.insert(slug.to_string(), list);
            self.save(&inner).map_err(|e| format!("could not save index.json: {e}"))?;
        }
        if !entry.build_no.is_empty() && !same_commit(entry.build_no.trim_end_matches("-dirty"), rev) {
            (self.log)(&format!("macapps: {slug} built for {rev} is stamped {}", entry.build_no));
        }
        Ok(entry)
    }
}

struct AppMeta {
    name: String,
    bundle_id: String,
    version: String,
    build_no: String,
    min_os_version: Option<String>,
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The `.app` name inside the zip: build-and-sign's `<Product>-macOS.app` copy loses the suffix.
fn product_bundle_name(app_path: &Path) -> String {
    let name = file_name(app_path);
    match name.strip_suffix("-macOS.app") {
        Some(stem) => format!("{stem}.app"),
        None => name,
    }
}

/// Name, bundle id, version and stamp of a built `.app`, from `Contents/Info.plist`.
fn inspect_app(app_path: &Path) -> Result<AppMeta, String> {
    let unreadable = || format!("could not read the Info.plist of {}", file_name(app_path));
    let info = plist::Value::from_file(app_path.join("Contents/Info.plist")).map_err(|_| unreadable())?;
    let info = info.as_dictionary().ok_or_else(unreadable)?;
    let text = |key: &str| info.get(key).and_then(plist::Value::as_string).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(AppMeta {
        name: text("CFBundleDisplayName").or_else(|| text("CFBundleName"))
            .unwrap_or_else(|| file_name(app_path).trim_end_matches(".app").to_string()),
        bundle_id: text("CFBundleIdentifier").unwrap_or_default(),
        version: text("CFBundleShortVersionString").unwrap_or_else(|| "1.0".into()),
        build_no: text("CFBundleVersion").unwrap_or_default(),
        min_os_version: text("LSMinimumSystemVersion"),
    })
}

/// Zips `app_path` to `dest` with one top-level folder, the product name (what `ditto
/// --keepParent` made). Unix permissions and symlinks are kept: a bundle needs both, or its
/// signature and its frameworks break. Returns that folder's name.
fn zip_app(app_path: &Path, dest: &Path) -> Result<String, String> {
    let bundle = product_bundle_name(app_path);
    let file = fs::File::create(dest).map_err(|e| format!("could not zip {bundle}: {e}"))?;
    let mut zip = zip::ZipWriter::new(io::BufWriter::new(file));
    add_tree(&mut zip, app_path, &bundle).and_then(|()| zip.finish().map_err(io::Error::other)?.flush())
        .map_err(|e| format!("could not zip {bundle}: {e}"))?;
    Ok(bundle)
}

fn options(meta: &fs::Metadata, default_mode: u32) -> zip::write::SimpleFileOptions {
    #[cfg(unix)]
    let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o7777;
    #[cfg(not(unix))]
    let mode = default_mode;
    let _ = default_mode;
    zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(mode).large_file(meta.len() >= 0xFFFF_FFFF)
}

fn add_tree<W: io::Write + io::Seek>(zip: &mut zip::ZipWriter<W>, dir: &Path, name: &str) -> io::Result<()> {
    let meta = fs::metadata(dir)?;
    zip.add_directory(format!("{name}/"), options(&meta, 0o755)).map_err(io::Error::other)?;
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|c| c.file_name());
    for child in children {
        let path = child.path();
        let entry = format!("{name}/{}", child.file_name().to_string_lossy());
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            zip.add_symlink(entry, target.to_string_lossy().replace('\\', "/"), options(&meta, 0o755)).map_err(io::Error::other)?;
        } else if meta.is_dir() {
            add_tree(zip, &path, &entry)?;
        } else if meta.is_file() {
            zip.start_file(entry, options(&meta, 0o644)).map_err(io::Error::other)?;
            io::copy(&mut fs::File::open(&path)?, zip)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn config() -> Value {
        json!({ "repos": [
            { "name": "A", "path": "/a", "branches": ["dev", "main"], "platforms": [],
              "macApps": [{ "name": "A for Mac", "bundleId": "com.example.a.mac" },
                { "name": "A Helper", "platform": "macOS-helper", "bundleId": "com.example.a.helper", "branch": "dev" },
                { "name": "no id" }] },
            { "name": "B", "path": "/b", "branches": ["trunk"], "platforms": [{ "platform": "iOS", "bundleId": "com.example.b", "device": "P" }] },
        ] })
    }

    #[test]
    fn one_app_per_entry_with_a_bundle_id_keyed_by_bundle_id_and_platform() {
        let apps = mac_apps(&config());
        assert_eq!(apps.iter().map(|a| a.slug.as_str()).collect::<Vec<_>>(), ["com.example.a.mac-macOS", "com.example.a.helper-macOS-helper"]);
        assert_eq!(apps[0].platform, "macOS");
        assert_eq!(apps[0].name, "A for Mac");
        assert_eq!(apps[0].repo, "A");
        assert_eq!(apps[0].path, "/a");
    }

    #[test]
    fn branch_is_its_own_else_main_master_else_the_first_offered() {
        let apps = mac_apps(&config());
        assert_eq!(apps[0].branch, "main");
        assert_eq!(apps[1].branch, "dev");
        let cfg = json!({ "repos": [
            { "name": "T", "path": "/t", "branches": ["trunk", "x"], "macApps": [{ "bundleId": "t" }] },
            { "name": "M", "path": "/m", "branches": ["x", "master"], "macApps": [{ "bundleId": "m" }] },
            { "name": "S", "path": "/s", "branches": ["x"], "altSource": { "branch": "x" }, "macApps": [{ "bundleId": "s" }] },
        ] });
        let apps = mac_apps(&cfg);
        assert_eq!(apps.iter().map(|a| a.branch.as_str()).collect::<Vec<_>>(), ["trunk", "master", "x"]);
        assert_eq!(apps[0].name, "T", "the repo's name stands in for a missing one");
    }

    #[test]
    fn a_slug_collapses_runs_of_unsafe_characters() {
        assert_eq!(slug_of("com.x.y-macOS wrong/é!"), "com.x.y-macOS-wrong-");
    }

    #[derive(Default)]
    struct Queue {
        jobs: Mutex<HashMap<u64, JobView>>,
        enqueued: Mutex<Vec<String>>,
        next: AtomicU64,
    }

    struct Fake(Arc<Queue>);

    impl Deps for Fake {
        fn apps(&self) -> Vec<MacApp> {
            vec![MacApp { slug: "com.example.a-macOS".into(), repo: "A".into(), path: "/a".into(), name: "A".into(),
                platform: "macOS".into(), bundle_id: "com.example.a".into(), branch: "main".into() }]
        }
        fn enqueue(&self, _: &MacApp, commit: &str, branch: &str) -> u64 {
            let id = self.0.next.fetch_add(1, Ordering::SeqCst) + 1;
            self.0.jobs.lock().unwrap().insert(id, JobView { status: "queued".into(), error: None });
            self.0.enqueued.lock().unwrap().push(format!("{branch}@{commit}"));
            id
        }
        fn job(&self, id: u64) -> Option<JobView> {
            self.0.jobs.lock().unwrap().get(&id).cloned()
        }
    }

    const SLUG: &str = "com.example.a-macOS";

    struct Harness {
        s: MacBuilds,
        queue: Arc<Queue>,
        dir: tempfile::TempDir,
        /// A synthetic signed-looking app, named as build-and-sign names it.
        app: PathBuf,
    }

    /// A `.app` with an executable, a framework with a `Versions/Current` symlink and an Info.plist.
    fn make_app(root: &Path, bundle_id: &str, build: &str) -> PathBuf {
        let app = root.join("A-macOS.app");
        let _ = fs::remove_dir_all(&app);
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        fs::create_dir_all(app.join("Contents/Frameworks/Lib.framework/Versions/A")).unwrap();
        std::os::unix::fs::symlink("A", app.join("Contents/Frameworks/Lib.framework/Versions/Current")).unwrap();
        fs::write(app.join("Contents/MacOS/A"), "bin").unwrap();
        fs::set_permissions(app.join("Contents/MacOS/A"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        fs::write(app.join("Contents/Info.plist"), format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>{bundle_id}</string>
<key>CFBundleName</key><string>Fake</string>
<key>CFBundleShortVersionString</key><string>3.0</string>
<key>CFBundleVersion</key><string>{build}</string>
<key>LSMinimumSystemVersion</key><string>14.0</string>
</dict></plist>"#)).unwrap();
        app
    }

    fn harness(bundle_id: &str) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(Queue::default());
        let s = MacBuilds::new(dir.path().join("macapps"), Box::new(Fake(queue.clone()))).unwrap();
        let app = make_app(dir.path(), bundle_id, "abc1234");
        Harness { s, queue, dir, app }
    }

    impl Harness {
        fn finish(&self, id: u64, status: &str, ok: bool) {
            if ok {
                self.s.record(SLUG, "abc1234", "main", &self.app, None).unwrap();
            }
            let error = (status == "failed").then(|| "boom".to_string());
            self.queue.jobs.lock().unwrap().insert(id, JobView { status: status.into(), error });
            self.s.sweep();
        }
        fn zips(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.s.dir()).unwrap().flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".zip")).collect();
            names.sort();
            names
        }
    }

    #[test]
    fn unknown_apps_are_404() {
        let h = harness("com.example.a");
        assert_eq!(h.s.request("nope", "abc1234", Some("main")), Request::Failed { status: 404, error: "unknown Mac app".into() });
    }

    #[test]
    fn concurrent_requests_for_one_commit_share_one_build_then_get_the_zip() {
        let h = harness("com.example.a");
        assert_eq!(h.s.request(SLUG, "abc1234", Some("main")), Request::Building(1));
        assert_eq!(h.s.request(SLUG, "abc1234", Some("main")), Request::Building(1));
        assert_eq!(*h.queue.enqueued.lock().unwrap(), ["main@abc1234"]);
        assert_eq!(h.s.list("A")[0]["building"], json!([{ "commit": "abc1234", "jobId": 1 }]));
        h.finish(1, "success", true);
        let Request::Ready(entry) = h.s.outcome(SLUG, "abc1234", 1) else { panic!("not ready") };
        assert!(fs::metadata(h.s.file(&entry)).unwrap().len() > 0);
        assert_eq!(entry.bundle, "A.app");
        assert!(matches!(h.s.request(SLUG, "abc1234", Some("main")), Request::Ready(_)));
        let listed = &h.s.list("A")[0];
        assert_eq!(listed["building"].as_array().unwrap().len(), 0);
        assert_eq!(listed["built"][0]["commit"], "abc1234");
        assert_eq!(listed["built"][0]["version"], "3.0");
        assert!(listed["built"][0].get("file").is_none());
        // a shorter or longer abbreviation of the same commit is the same build
        assert!(h.s.cached(SLUG, "abc1234def").is_some());
        assert!(h.s.cached(SLUG, "abc").is_some());
        assert!(h.s.cached(SLUG, "abd").is_none());
    }

    #[test]
    fn the_branch_defaults_to_the_apps_own() {
        let h = harness("com.example.a");
        h.s.request(SLUG, "abc1234", None);
        h.s.request(SLUG, "def5678", Some("dev"));
        assert_eq!(*h.queue.enqueued.lock().unwrap(), ["main@abc1234", "dev@def5678"]);
    }

    #[test]
    fn a_failed_build_is_reported_not_retried_straight_away() {
        let h = harness("com.example.a");
        h.s.request(SLUG, "abc1234", Some("main"));
        h.finish(1, "failed", false);
        let Request::Failed { error, .. } = h.s.outcome(SLUG, "abc1234", 1) else { panic!("not failed") };
        assert!(error.contains("build failed: boom"), "{error}");
        assert!(matches!(h.s.request(SLUG, "abc1234", Some("main")), Request::Failed { status: 502, .. }));
        assert_eq!(h.queue.enqueued.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failure_is_forgotten_after_two_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(Queue::default());
        let now = Arc::new(AtomicU64::new(1_000_000));
        let clock = now.clone();
        let s = MacBuilds::new(dir.path(), Box::new(Fake(queue.clone()))).unwrap().with_clock(move || clock.load(Ordering::SeqCst));
        s.request(SLUG, "abc1234", None);
        queue.jobs.lock().unwrap().insert(1, JobView { status: "failed".into(), error: None });
        assert_eq!(s.request(SLUG, "abc1234", None), Request::Failed { status: 502, error: "build failed: no app".into() });
        now.fetch_add(FAILURE_MEMORY_MS + 1, Ordering::SeqCst);
        assert_eq!(s.request(SLUG, "abc1234", None), Request::Building(2));
    }

    #[test]
    fn a_job_the_queue_forgot_is_lost() {
        let h = harness("com.example.a");
        h.s.request(SLUG, "abc1234", None);
        h.queue.jobs.lock().unwrap().clear();
        assert_eq!(h.s.outcome(SLUG, "abc1234", 1), Request::Failed { status: 502, error: "build lost".into() });
    }

    #[test]
    fn an_app_with_another_bundle_id_is_refused_not_cached() {
        let h = harness("com.example.other");
        let err = h.s.record(SLUG, "abc1234", "main", &h.app, None).unwrap_err();
        assert!(err.contains("produced com.example.other, not com.example.a"), "{err}");
        assert!(h.s.cached(SLUG, "abc1234").is_none());
        assert_eq!(h.zips(), Vec::<String>::new());
        assert!(h.s.record("nope", "abc1234", "main", &h.app, None).unwrap_err().contains("unknown Mac app"));
    }

    #[test]
    fn keeps_three_builds_per_app() {
        let h = harness("com.example.a");
        for c in ["aaaa111", "bbbb222", "cccc333", "dddd444"] {
            h.s.record(SLUG, c, "main", &h.app, None).unwrap();
        }
        assert!(h.s.cached(SLUG, "dddd444").is_some());
        assert!(h.s.cached(SLUG, "bbbb222").is_some());
        assert!(h.s.cached(SLUG, "aaaa111").is_none());
        assert!(!h.s.dir().join("com.example.a-macOS-aaaa111.app.zip").exists());
        assert_eq!(h.s.latest(SLUG).unwrap().commit, "dddd444");
    }

    #[test]
    fn the_index_survives_a_restart() {
        let h = harness("com.example.a");
        h.s.record(SLUG, "abc1234", "main", &h.app, None).unwrap();
        let again = MacBuilds::new(h.s.dir(), Box::new(Fake(h.queue.clone()))).unwrap();
        assert_eq!(again.cached(SLUG, "abc1234").unwrap().version, "3.0");
    }

    #[test]
    fn a_registered_artifact_is_copied_exactly_without_inspecting_or_repacking() {
        let h = harness("com.example.wrong-mutable-output");
        let file = h.dir.path().join("registered.zip");
        fs::write(&file, "registered artifact").unwrap();
        let kept = Kept { file, manifest: json!({ "buildId": "build-id", "artifact": { "kind": "app.zip", "sha256": "ab" },
            "app": { "bundleId": "com.example.a", "build": "abc1234", "version": "2.0", "name": "Display Name", "minOSVersion": "14.0" } }) };
        let entry = h.s.record(SLUG, "abc1234", "main", Path::new("ActualProduct.app"), Some(&kept)).unwrap();
        assert_eq!(fs::read_to_string(h.s.file(&entry)).unwrap(), "registered artifact");
        assert_eq!(entry.bundle, "ActualProduct.app");
        assert_eq!(entry.build_id.as_deref(), Some("build-id"));
        assert_eq!(entry.artifact_sha256.as_deref(), Some("ab"));
        assert_eq!(entry.min_os_version.as_deref(), Some("14.0"));
        assert_eq!((entry.version.as_str(), entry.name.as_str()), ("2.0", "Display Name"));

        let wrong_kind = Kept { file: kept.file.clone(), manifest: json!({ "artifact": { "kind": "ipa" } }) };
        assert!(h.s.record(SLUG, "def5678", "main", Path::new("A.app"), Some(&wrong_kind)).unwrap_err().contains("does not describe an app zip"));
    }

    #[test]
    fn the_zip_holds_the_app_under_its_product_name_with_symlinks_and_modes_intact() {
        let h = harness("com.example.a");
        let entry = h.s.record(SLUG, "abc1234", "main", &h.app, None).unwrap();
        assert_eq!((entry.bundle.as_str(), entry.name.as_str(), entry.build_no.as_str()), ("A.app", "Fake", "abc1234"));
        assert_eq!(entry.min_os_version.as_deref(), Some("14.0"));
        assert_eq!(entry.size, fs::metadata(h.s.file(&entry)).unwrap().len());

        let mut zip = zip::ZipArchive::new(fs::File::open(h.s.file(&entry)).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().unwrap().to_string()).collect();
        assert!(names.iter().all(|n| n.starts_with("A.app/")), "{names:?}");
        assert!(names.contains(&"A.app/Contents/Info.plist".to_string()));
        {
            let link = zip.by_name("A.app/Contents/Frameworks/Lib.framework/Versions/Current").unwrap();
            assert!(link.is_symlink());
        }
        let mut target = String::new();
        io::Read::read_to_string(&mut zip.by_name("A.app/Contents/Frameworks/Lib.framework/Versions/Current").unwrap(), &mut target).unwrap();
        assert_eq!(target, "A");
        let exe = zip.by_name("A.app/Contents/MacOS/A").unwrap();
        assert_eq!(exe.unix_mode().unwrap() & 0o777, 0o755);
        assert!(!exe.is_symlink());
    }

    #[test]
    fn a_bundle_without_an_info_plist_is_refused() {
        let h = harness("com.example.a");
        fs::remove_file(h.app.join("Contents/Info.plist")).unwrap();
        assert!(h.s.record(SLUG, "abc1234", "main", &h.app, None).unwrap_err().contains("could not read the Info.plist of A-macOS.app"));
    }

    #[test]
    fn a_stamp_for_another_commit_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(Queue::default());
        let logged = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = logged.clone();
        let s = MacBuilds::new(dir.path().join("m"), Box::new(Fake(queue))).unwrap().with_log(move |m| sink.lock().unwrap().push(m.into()));
        let app = make_app(dir.path(), "com.example.a", "abc1234-dirty");
        s.record(SLUG, "abc1234", "main", &app, None).unwrap();
        assert!(logged.lock().unwrap().is_empty(), "a dirty stamp of the same commit is not news");
        s.record(SLUG, "ffff999", "main", &app, None).unwrap();
        assert_eq!(logged.lock().unwrap().len(), 1);
    }
}
