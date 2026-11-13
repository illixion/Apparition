//! The HTTP front of the service: the web gate, login, static files and the bearer API that
//! native clients and the hosting app use. Its dispatch order is the Store's contract: the hosted
//! app token and loopback rules first, then signed links, then the session.

use crate::{
    altsource::{self, AltSource},
    auth::{Auth, COOKIE, Login, cookie_value, hash_password, system_ms},
    config::Config,
    artifacts, crashlogs, deploy, devices,
    debug::{self, Debug},
    macapps::{self, MacBuilds},
    mcp::Mcp,
    projects,
    queue::{self, Queue},
    state::{State as StoreState, hostname},
    traces,
};
mod delivery;
mod routes;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use futures_util::StreamExt;
use subtle::ConstantTimeEq;

const WEB_UI_OFF: &str = "The web UI is off. Turn it on in Apparition: Developer › Devices & signing › Web UI and other devices.";
const PUBLIC_FILES: [&str; 5] = ["/login.html", "/logo.svg", "/favicon.ico", "/apple-touch-icon.png", "/fonts/Fraunces-Soft.woff2"];
const SERVICE_PREFIX: &str = "/api/service/v1";

pub struct Settings {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    /// A directory to serve the web UI from (while working on it), or None for the copy compiled
    /// into the binary.
    pub public_dir: Option<PathBuf>,
    /// The hosting app's token (43 characters). Present means hosted.
    pub host_token: Option<String>,
    /// Where `build-and-sign` records trace signing keys (`traces::default_key_dir()`).
    pub key_dir: PathBuf,
    /// Where the keys that open encrypted problem reports are kept (`reports::default_key_dir()`).
    pub report_key_dir: PathBuf,
    /// DebugTrace's state directory (`sessions/` inside): `DEBUGTRACE_STATE_DIR` or
    /// `~/.local/state/debugtrace`.
    pub debug_state_dir: PathBuf,
    /// Where running apps are found; Bonjour unless a test supplies a list.
    pub browser: Arc<dyn debug::Browse>,
    /// Named in a device's "Allow debug access?" prompt.
    pub client_name: String,
    /// The devices this Mac reaches through its own pairings; the hosting app supplies them.
    pub devices: Option<Arc<dyn devices::Devices>>,
    /// The loopback port to listen on (config.json's `port`, 8484 by default: what apps upload
    /// traces to and what `tailscale serve` points at). Another free port when it is taken.
    pub port: Option<u16>,
}

impl Settings {
    pub fn new(config_path: PathBuf, data_dir: PathBuf, public_dir: PathBuf) -> Settings {
        let home = crate::home();
        let host = hostname();
        Settings {
            config_path, data_dir, public_dir: Some(public_dir), host_token: None,
            key_dir: traces::default_key_dir(),
            report_key_dir: crate::reports::default_key_dir(),
            debug_state_dir: std::env::var_os("DEBUGTRACE_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".local/state/debugtrace")),
            browser: Arc::new(Arc::new(debug::Mdns::default())),
            client_name: format!("Apparition Store page on {}", host.split('.').next().unwrap_or("")),
            devices: None,
            port: None,
        }
    }
}

pub struct App {
    settings: Settings,
    auth: Auth,
    state: Arc<StoreState>,
    debug: Arc<Debug>,
    mcp: Mcp,
    queue: Arc<Queue>,
    alt: AltSource,
    mac: MacBuilds,
    deliver: Arc<dyn queue::Deliver>,
    /// `bundle@device` of the patches in progress: one at a time per app and device.
    patching: Mutex<HashSet<String>>,
    config: Mutex<(Option<SystemTime>, Arc<Config>)>,
    host_sessions: Mutex<HashSet<String>>,
    host_codes: Mutex<HashMap<String, u64>>,
}

/// A debug route needs the network, so the synchronous dispatch hands it to `handle`, which runs
/// it on the async side. It rides on an otherwise empty response.
#[derive(Clone)]
enum Deferred {
    Apps,
    Help(String),
    Call(String, Value),
    Panels(String),
    Logs(String, String, Option<String>),
    Mcp(Value),
    /// Ask the devices what they have installed, then plan.
    UpdatesCheck,
    /// `POST /api/refresh-devices`: look at the devices in the background.
    RefreshDevices(bool),
    /// The AltSource IPA and the Mac zip wait on their builds.
    AltIpa { path: String, query: HashMap<String, String>, slug: String, commit: String },
    MacZip { path: String, query: HashMap<String, String>, slug: String, commit: String },
    /// `POST /api/service/v1/inject`.
    Inject(Value),
}

fn defer(route: Deferred) -> Response {
    let mut res = Response::new(Body::empty());
    res.extensions_mut().insert(route);
    res
}

/// Serves the Store on a free loopback port, on the current tokio runtime, until the task is
/// dropped or aborted. Other computers reach it only through the companion channel.
pub async fn serve_loopback(settings: Settings) -> std::io::Result<(std::net::SocketAddr, Served)> {
    let wanted = settings.port;
    let app = build(settings)?;
    let mut moved = None;
    let listener = match wanted {
        Some(port) => match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(e) => {
                let note = format!("Port {port} is taken ({e}), so the Store listens on another one: apps' trace uploads and links made for port {port} don't reach it. Quit what holds {port} and start the Store again.");
                eprintln!("{note}");
                moved = Some(note);
                tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?
            }
        },
        None => tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?,
    };
    let address = listener.local_addr()?;
    let router = Router::new().fallback(handle).with_state(app.clone());
    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            eprintln!("the Store stopped: {error}");
        }
    });
    Ok((address, Served { task, app, moved }))
}

/// A Store `serve_loopback` started.
pub struct Served {
    task: tokio::task::JoinHandle<()>,
    app: Arc<App>,
    /// Why it isn't on the port asked for, when it isn't.
    pub moved: Option<String>,
}

impl Served {
    /// Stops it: no new connections, the queue's builds stopped and noted as failed, and every
    /// request still arriving on an open connection answered 503. Another Store can then take
    /// the same folder.
    pub fn stop(&self) {
        self.app.queue.shutdown();
        self.task.abort();
    }

    /// Runs until the server stops by itself (only on an error).
    pub async fn wait(&mut self) {
        let _ = (&mut self.task).await;
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the AltSource feed and the Mac downloads ask of the Store: the live config, a branch
/// head and the build queue.
struct Feeds(std::sync::Weak<App>);

impl Feeds {
    fn app(&self) -> Option<Arc<App>> {
        self.0.upgrade()
    }
}

fn job_view(job: Option<queue::Job>) -> Option<(String, Option<String>)> {
    job.map(|j| (j.status, j.error))
}

impl altsource::Deps for Feeds {
    fn apps(&self, platform: Option<&str>) -> Vec<altsource::SourceApp> {
        self.app().map(|a| altsource::source_apps(&a.config().raw, platform)).unwrap_or_default()
    }
    fn all_apps(&self) -> Vec<altsource::SourceApp> {
        self.app().map(|a| altsource::all_source_apps(&a.config().raw)).unwrap_or_default()
    }
    fn head(&self, app: &altsource::SourceApp) -> Result<altsource::Head, String> {
        let (commit, date, subject) = App::branch_head(&app.path, &app.branch)?;
        Ok(altsource::Head { commit, date, subject })
    }
    fn enqueue(&self, app: &altsource::SourceApp, commit: &str) -> u64 {
        let Some(a) = self.app() else { return 0 };
        let ctx = a.queue_context();
        a.queue.enqueue_alt_source(&ctx, &app.repo, &app.branch, &app.platform, app.device.as_deref().unwrap_or(""), &app.bundle_id, &app.slug, commit).id
    }
    fn job(&self, id: u64) -> Option<altsource::JobView> {
        job_view(self.app()?.queue.job(id)).map(|(status, error)| altsource::JobView { status, error })
    }
}

impl macapps::Deps for Feeds {
    fn apps(&self) -> Vec<macapps::MacApp> {
        self.app().map(|a| macapps::mac_apps(&a.config().raw)).unwrap_or_default()
    }
    fn enqueue(&self, app: &macapps::MacApp, commit: &str, branch: &str) -> u64 {
        let Some(a) = self.app() else { return 0 };
        let ctx = a.queue_context();
        a.queue.enqueue_mac(&ctx, &app.repo, branch, &app.platform, &app.bundle_id, &app.slug, commit).id
    }
    fn job(&self, id: u64) -> Option<macapps::JobView> {
        job_view(self.app()?.queue.job(id)).map(|(status, error)| macapps::JobView { status, error })
    }
}

/// A finished build is handed to the feed or the Mac downloads here.
struct Deliveries(std::sync::Weak<App>);

impl queue::Deliver for Deliveries {
    fn alt_source_built(&self, slug: &str, rev: &str, ipa: &Path) -> Result<(), String> {
        let app = self.0.upgrade().ok_or("the Store is stopping")?;
        app.alt.record(slug, rev, ipa).map(|_| ())
    }
    fn mac_built(&self, slug: &str, rev: &str, branch: &str, kept: &artifacts::Kept) -> Result<String, String> {
        let app = self.0.upgrade().ok_or("the Store is stopping")?;
        let bundle = kept.manifest.pointer("/app/bundleName").and_then(Value::as_str).unwrap_or("");
        app.mac.record(slug, rev, branch, Path::new(bundle), Some(kept)).map(|e| e.build_no)
    }
}

pub fn router(settings: Settings) -> std::io::Result<Router> {
    Ok(Router::new().fallback(handle).with_state(build(settings)?))
}

fn build(settings: Settings) -> std::io::Result<Arc<App>> {
    let auth = Auth::open(&settings.data_dir)?;
    let state = Arc::new(StoreState::load(&settings.data_dir));
    let debug = Arc::new(Debug::new(settings.key_dir.clone(), &settings.debug_state_dir, settings.browser.clone(), settings.client_name.clone()));
    let max = Config::load(&settings.config_path).map(|c| queue::max_builds(&c)).unwrap_or(2);
    let queue = Arc::new(Queue::open(&settings.data_dir, max));
    let data = settings.data_dir.clone();
    let mut failed = None;
    let app = Arc::new_cyclic(|me: &std::sync::Weak<App>| {
        let feeds = Arc::new(Feeds(me.clone()));
        let log = |m: &str| eprintln!("{m}");
        let alt = AltSource::new(data.join("altsource"), feeds.clone()).map(|a| a.with_log(log));
        let mac = MacBuilds::new(data.join("macapps"), Box::new(Feeds(me.clone()))).map(|m| m.with_log(log));
        let (alt, mac) = match (alt, mac) {
            (Ok(a), Ok(m)) => (Some(a), Some(m)),
            (a, m) => { failed = Some(a.err().or(m.err()).unwrap()); (None, None) }
        };
        App {
            settings, auth, state, queue,
            alt: alt.unwrap_or_else(|| AltSource::new(std::env::temp_dir().join("apparition-none"), feeds.clone()).unwrap()),
            mac: mac.unwrap_or_else(|| MacBuilds::new(std::env::temp_dir().join("apparition-none-mac"), Box::new(Feeds(me.clone()))).unwrap()),
            deliver: Arc::new(Deliveries(me.clone())),
            patching: Mutex::default(),
            mcp: Mcp::new(debug.clone()),
            debug,
            config: Mutex::new((None, Arc::new(Config::default()))),
            host_sessions: Mutex::default(),
            host_codes: Mutex::default(),
        }
    });
    if let Some(error) = failed {
        return Err(error);
    }
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(automatic_updates(Arc::downgrade(&app)));
        // A first look at the devices, so the page has an inventory to judge freshness by.
        let (queue, ctx) = (app.queue.clone(), app.queue_context());
        runtime.spawn(async move { queue.refresh_devices(&ctx, false).await });
    }
    Ok(app)
}

/// The automatic mode (config `autoUpdate.enabled`): once a minute, builds and installs what is
/// behind once its sources have settled; asks the devices what they have every 15 minutes. Also
/// clears build clones unused for a week, hourly, whether the mode is on or not.
async fn automatic_updates(app: std::sync::Weak<App>) {
    let (mut asked, mut reaped) = (0u64, 0u64);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let Some(app) = app.upgrade().filter(|a| !a.queue.stopped()) else { return };
        let ctx = app.queue_context();
        let now = system_ms();
        if now - reaped > 3_600_000 {
            reaped = now;
            let (q, c) = (app.queue.clone(), ctx.clone());
            let _ = tokio::task::spawn_blocking(move || q.reap(&c)).await;
        }
        let (enabled, settle) = queue::auto_settings(&ctx.config);
        if !enabled {
            continue;
        }
        if now - asked > 15 * 60_000 {
            asked = now;
            app.queue.refresh_devices(&ctx, false).await;
        }
        let (q, c) = (app.queue.clone(), ctx.clone());
        let started = tokio::task::spawn_blocking(move || q.auto_tick(&c, settle)).await.unwrap_or_default();
        for job in started {
            eprintln!("automatic update: {} for {} ({})", job.repo, job.devices.join(", "), job.why.join("; "));
        }
    }
}

struct Req {
    method: Method,
    path: String,
    query: HashMap<String, String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
}

impl Req {
    fn header(&self, name: &str) -> &str {
        self.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
    }
    fn is_https(&self) -> bool {
        let proto = self.header("x-forwarded-proto");
        proto == "https"
    }
    fn session_token(&self) -> String {
        cookie_value(self.header("cookie"), COOKIE).unwrap_or_default()
    }
    fn json_body(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    /// The request names this Mac's loopback as its host.
    fn loopback_host(&self) -> bool {
        let host = self.header("host");
        let name = match host.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or(""),
            None => host.rsplit_once(':').map_or(host, |(name, port)| if port.bytes().all(|b| b.is_ascii_digit()) { name } else { host }),
        }.to_ascii_lowercase();
        matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1") || name.ends_with(".localhost")
    }
    /// A non-GET with a foreign `Origin` is refused (SameSite=Strict already keeps cookies off).
    fn cross_site(&self) -> bool {
        if matches!(self.method, Method::GET | Method::HEAD) || self.header("origin").is_empty() {
            return false;
        }
        let host = match self.header("x-forwarded-host") {
            "" => self.header("host"),
            forwarded => forwarded,
        };
        let origin = self.header("origin");
        let origin_host = origin.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or("")).unwrap_or("\0");
        origin_host != host
    }
}

fn percent_decode(s: &str) -> String {
    percent_decode_with(s, true)
}

fn percent_decode_with(s: &str, plus_is_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((high * 16 + low) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn reply(status: u16, content_type: &str, body: impl Into<Body>) -> Response {
    let mut res = Response::new(body.into());
    *res.status_mut() = StatusCode::from_u16(status).unwrap();
    res.headers_mut().insert(header::CONTENT_TYPE, header_value(content_type));
    res
}

fn json_reply(status: u16, value: Value) -> Response {
    reply(status, "application/json", value.to_string())
}

fn text(status: u16, message: &str) -> Response {
    reply(status, "text/plain; charset=utf-8", message.to_string())
}

/// A header value as it goes on the wire: anything outside visible ASCII percent-encoded, so a
/// non-ASCII `next` or file name can't fail the response.
fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or_else(|_| {
        let mut out = String::with_capacity(value.len());
        for b in value.bytes() {
            if (0x20..0x7f).contains(&b) { out.push(b as char) } else { out.push_str(&format!("%{b:02X}")) }
        }
        HeaderValue::from_str(&out).unwrap_or_else(|_| HeaderValue::from_static(""))
    })
}

fn with_header(mut res: Response, name: &'static str, value: &str) -> Response {
    res.headers_mut().append(name, header_value(value));
    res
}

fn redirect(location: &str) -> Response {
    let mut res = reply(302, "text/plain", "");
    res.headers_mut().insert(header::LOCATION, header_value(location));
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    res
}

/// A same-site path only. Browsers read "/\host" like "//host", and a control character would
/// break the header, so neither is allowed through.
fn safe_next(next: &str) -> &str {
    let ok = next.starts_with('/')
        && !next[1..].starts_with(['/', '\\'])
        && !next.chars().any(|c| c.is_control());
    if ok { next } else { "/" }
}

impl App {
    /// The config, reread when the file changes, as the Node store's watcher does.
    fn config(&self) -> Arc<Config> {
        let mtime = fs::metadata(&self.settings.config_path).and_then(|m| m.modified()).ok();
        let mut cached = self.config.lock().unwrap();
        if cached.0 != mtime || mtime.is_none() {
            if let Ok(config) = Config::load(&self.settings.config_path) {
                *cached = (mtime, Arc::new(config));
            }
        }
        cached.1.clone()
    }

    fn hosted(&self) -> bool {
        self.settings.host_token.is_some()
    }

    fn web_ui_on(&self) -> bool {
        self.config().web_ui_on(self.hosted())
    }

    fn host_session(&self, req: &Req) -> bool {
        self.host_sessions.lock().unwrap().contains(&req.session_token())
    }

    fn signed_in(&self, req: &Req) -> bool {
        if self.host_session(req) {
            return true;
        }
        let config = self.config();
        match (&config.password_hash, self.hosted()) {
            (Some(hash), _) => self.auth.check(&req.session_token(), hash),
            // No password: open, as the Node store is, but only to pages that name this Mac's
            // loopback. A web page elsewhere can point its own name at 127.0.0.1 (DNS
            // rebinding); its requests then carry that name, not localhost. Nor to another
            // site's page asking for localhost directly: a download link builds on first fetch,
            // and an <img> there would start that build.
            (None, hosted) => !hosted && req.loopback_host() && req.header("sec-fetch-site") != "cross-site",
        }
    }

    /// `public/<rel>`, or None when it is not a file under `public/`.
    fn static_file(&self, rel: &str) -> Option<Response> {
        let rel = Path::new(rel);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        let bytes: Vec<u8> = match &self.settings.public_dir {
            Some(dir) => fs::read(dir.join(rel)).ok().filter(|_| dir.join(rel).is_file())?,
            None => crate::assets::get(rel.to_str()?)?.to_vec(),
        };
        let ext = rel.extension().and_then(|e| e.to_str()).unwrap_or("");
        let content_type = match ext {
            "html" => "text/html; charset=utf-8", "js" => "text/javascript", "css" => "text/css",
            "svg" => "image/svg+xml", "png" => "image/png", "ico" => "image/x-icon", "woff2" => "font/woff2",
            "txt" => "text/plain; charset=utf-8", _ => "application/octet-stream",
        };
        let res = if ext == "html" {
            with_header(reply(200, content_type, bytes), "cache-control", "no-store")
        } else {
            reply(200, content_type, bytes)
        };
        Some(with_header(res, "x-content-type-options", "nosniff"))
    }

    /// Rewrites `config.json` with `change`, atomically, keeping every key it does not touch.
    /// A change made meanwhile (a project registered, an edit by hand) is read again and the
    /// change applied over it, rather than written over.
    fn update_config(&self, change: impl Fn(&mut serde_json::Map<String, Value>)) -> Result<(), String> {
        let path = &self.settings.config_path;
        for _ in 0..5 {
            let original = fs::read(path).map_err(|e| e.to_string())?;
            let mut value: Value = serde_json::from_slice(&original).map_err(|e| e.to_string())?;
            let object = value.as_object_mut().ok_or("config.json is not an object")?;
            change(object);
            let tmp = path.with_extension(format!("json.tmp-{}-{}", std::process::id(), crate::auth::new_token()));
            {
                use std::io::Write;
                let mut file = crate::private_file().write(true).create(true).truncate(true).open(&tmp).map_err(|e| e.to_string())?;
                file.write_all((serde_json::to_string_pretty(&value).map_err(|e| e.to_string())? + "\n").as_bytes()).map_err(|e| e.to_string())?;
            }
            if fs::read(path).ok().as_deref() != Some(original.as_slice()) {
                let _ = fs::remove_file(&tmp);
                continue;
            }
            fs::rename(&tmp, path).map_err(|e| e.to_string())?;
            self.config.lock().unwrap().0 = None; // reread on the next request
            return Ok(());
        }
        Err("config.json keeps changing; try again".into())
    }

    /// Kept builds: config `buildManifestDir` (relative to config.json), else the default store.
    fn build_store(&self) -> PathBuf {
        let config_dir = self.settings.config_path.parent().unwrap_or(Path::new("."));
        match self.config().raw.get("buildManifestDir").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            Some(dir) => config_dir.join(dir),
            None => artifacts::default_store(),
        }
    }

    fn queue_context(&self) -> queue::Context {
        let config = self.config();
        queue::Context {
            cli: queue::cli(&config), build_store: self.build_store(), devices: self.settings.devices.clone(),
            state: self.state.clone(), deliver: Some(self.deliver.clone()), config,
        }
    }

    /// `/api/updates*` and `/api/builds/<id>/{log,cancel}`.
    fn updates_api(&self, req: &Req, segments: &[&str]) -> Option<Response> {
        let (get, post) = (req.method == Method::GET, req.method == Method::POST);
        match segments {
            ["api", "updates"] if get => Some(json_reply(200, self.queue.report(&self.queue_context()))),
            ["api", "updates", "check"] if post => Some(defer(Deferred::UpdatesCheck)),
            ["api", "updates", "run"] if post => {
                let ctx = self.queue_context();
                let body = req.json_body();
                let only: Option<Vec<String>> = body.get("only").and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect());
                let rows: Vec<_> = self.queue.plan(&ctx).into_iter()
                    .filter(|r| only.as_ref().is_none_or(|o| o.contains(&format!("{}|{}", r.device, r.bundle_id)))).collect();
                let groups = self.queue.groups(&ctx, &rows);
                let jobs = self.queue.enqueue(&ctx, groups, false);
                Some(json_reply(200, json!({ "ok": true, "jobs": jobs })))
            }
            ["api", "updates", "auto"] if post => {
                let body = req.json_body();
                let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
                    return Some(json_reply(400, json!({ "error": "Send enabled, and settleMinutes if you change it." })));
                };
                let settle = body.get("settleMinutes").and_then(Value::as_u64);
                let saved = self.update_config(|c| {
                    let auto = c.entry("autoUpdate").or_insert_with(|| json!({}));
                    if !auto.is_object() { *auto = json!({}); }
                    auto["enabled"] = Value::Bool(enabled);
                    if let Some(minutes) = settle { auto["settleMinutes"] = json!(minutes.min(24 * 60)); }
                });
                Some(match saved {
                    Ok(()) => json_reply(200, json!({ "ok": true, "auto": { "enabled": enabled, "settleMinutes": queue::auto_settings(&self.config()).1 } })),
                    Err(_) => json_reply(500, json!({ "error": "Could not change the setting." })),
                })
            }
            ["api", "builds", id, "log"] if get => {
                // The Node store's reply: what follows `offset` (in characters), for a page that
                // polls the log as it grows. Logs of either store's builds are in the same folder.
                let id: u64 = id.parse().ok()?;
                let bytes = fs::read(self.queue.log_path(id)).unwrap_or_default();
                // A build may be mid-way through writing a character: leave its first bytes for the
                // next poll, rather than failing the read and sending the client back to the start.
                let whole = match std::str::from_utf8(&bytes) {
                    Ok(_) => bytes.len(),
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    Err(_) => bytes.len(),
                };
                let text = String::from_utf8_lossy(&bytes[..whole]);
                let offset: usize = req.query.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
                let total = text.chars().count();
                let chunk: String = text.chars().skip(offset).collect();
                let status = self.queue.job(id).map(|j| j.status).unwrap_or_else(|| "unknown".into());
                Some(json_reply(200, json!({ "chunk": chunk, "nextOffset": total, "status": status })))
            }
            ["api", "builds", id, "cancel"] if post => {
                let id: u64 = id.parse().ok()?;
                // Builds the Node store made are long over.
                Some(match self.queue.cancel(id) {
                    Some(job) => json_reply(200, json!({ "ok": true, "job": job })),
                    None => json_reply(200, json!({ "ok": false })),
                })
            }
            _ => None,
        }
    }

    fn crash_logs(&self) -> crashlogs::CrashLogs {
        crashlogs::CrashLogs::new(self.settings.data_dir.join("crashlogs"))
    }

    fn traces_dir(&self) -> PathBuf {
        self.settings.data_dir.join("traces")
    }

    /// `POST /api/traces` (a signed trace, from an app or dropped on the page), or with `report`,
    /// `POST /api/traces/report` (an encrypted problem report, from a signed-in page).
    fn receive_trace(&self, req: &Req, report: bool) -> Response {
        let config = self.config();
        let repo_for_bundle = |bundle: &str| -> Option<String> {
            config.raw.get("repos")?.as_array()?.iter().find(|r| {
                ["platforms", "macApps"].iter().filter_map(|k| r.get(k)?.as_array()).flatten().any(|p| p.get("bundleId").and_then(Value::as_str) == Some(bundle))
            })?.get("name")?.as_str().map(String::from)
        };
        let uploader = req.header("tailscale-user-login");
        let up = traces::Upload {
            dir: &self.traces_dir(), key_dir: &self.settings.key_dir, now_ms: system_ms(),
            via_web: req.header("x-debugtrace-source") == "web", uploader: (!uploader.is_empty()).then_some(uploader),
            repo_for_bundle: &repo_for_bundle,
        };
        // An encrypted report has its own way in, behind a session: this route takes anything
        // from anyone, and a report carries no signature to judge it by.
        let received = if report {
            traces::receive_report(&req.body, &up, &self.settings.report_key_dir)
        } else if crate::reports::is_report(&req.body) {
            Err(traces::TraceError(400, "this is an encrypted report (.trace), which only a person at this Store can add: drop it on the Store's Crashes & traces page, or run `apparition crash-reports add <file>`".into()))
        } else {
            traces::receive(&req.body, &up)
        };
        match received {
            Ok((status, meta)) => {
                if status == 201 {
                    // a crash log pulled before this trace arrived
                    let metas: Vec<Value> = traces::list(&self.traces_dir()).into_iter().take(100).collect();
                    self.crash_logs().link(&crashlogs::crash_refs(&self.traces_dir(), &metas));
                    eprintln!("{} {} from {} ({} bytes)", if report { "report" } else { "trace" }, meta["id"].as_str().unwrap_or(""), meta["app"]["bundleId"].as_str().unwrap_or(""), req.body.len());
                }
                json_reply(status, json!({ "ok": true, "trace": meta }))
            }
            Err(traces::TraceError(status, message)) => json_reply(status, json!({ "error": message })),
        }
    }

    /// The routes behind a session (or the service API's bearer): `path` is `/api/...`.
    fn api(&self, req: &Req, path: &str) -> Option<Response> {
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let is_id = |s: &str| s.len() == 36 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-');
        let get = req.method == Method::GET;
        let traces = self.traces_dir();
        if let Some(res) = self.updates_api(req, &segments) {
            return Some(res);
        }
        if let Some(res) = self.ota_routes(req, path).or_else(|| self.mac_api(req, &segments)) {
            return Some(res);
        }
        let post = req.method == Method::POST;
        match segments.as_slice() {
            ["api", "altsource"] if get => Some(self.alt_source_api(req)),
            ["api", "build"] if post => {
                let ctx = self.queue_context();
                Some(match self.queue.enqueue_deploy(&ctx, &req.json_body()) {
                    Ok(jobs) => json_reply(200, json!({ "jobs": jobs })),
                    Err(error) => json_reply(400, json!({ "error": error })),
                })
            }
            ["api", "resign", "options"] if get => Some(self.resign_options()),
            ["api", "resign"] if post => Some(self.resign(req)),
            ["api", "refresh-devices"] if post => {
                // Devices the Store leaves alone on a plain refresh are named in the reply.
                let force = req.json_body().get("force").and_then(Value::as_bool) == Some(true);
                Some(defer(Deferred::RefreshDevices(force)))
            }
            ["api", "debug", "apps"] if get => Some(defer(Deferred::Apps)),
            ["api", "debug", id, action @ ("help" | "call" | "logs" | "panels")] if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) => {
                let id = id.to_string();
                match (*action, &req.method) {
                    ("help", &Method::GET) => Some(defer(Deferred::Help(id))),
                    ("panels", &Method::GET) => Some(defer(Deferred::Panels(id))),
                    ("call", &Method::POST) => {
                        let body = req.json_body();
                        let ok = body.get("endpoint").and_then(Value::as_str).is_some_and(debug::valid_endpoint);
                        if !ok {
                            return Some(json_reply(400, json!({ "ok": false, "error": { "message": "body must be {\"endpoint\": \"<name>\", \"arguments\": {...}}" } })));
                        }
                        Some(defer(Deferred::Call(id, body)))
                    }
                    ("logs", &Method::GET) => {
                        let level = req.query.get("level").cloned().unwrap_or_else(|| "debug".into());
                        if !["debug", "info", "notice", "error", "fault"].contains(&level.as_str()) {
                            return Some(json_reply(400, json!({ "error": "unknown level" })));
                        }
                        Some(defer(Deferred::Logs(id, level, req.query.get("contains").filter(|c| !c.is_empty()).cloned())))
                    }
                    _ => None,
                }
            }
            ["api", "state"] if get => {
                // A page the desktop app opened signs in by itself (host-login), so it has no use for Sign out.
                let mut bundle = self.state_bundle();
                // Only then, so a browser's bundle keeps the Node store's exact fields.
                if self.host_session(req) { bundle["appWindow"] = json!(true); }
                Some(json_reply(200, bundle))
            }
            ["api", "artifacts"] if get => Some(json_reply(200, json!({ "builds": artifacts::list_builds(&self.build_store(), 40) }))),
            ["api", "artifacts", id, which @ ("manifest" | "download")] if get && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') => {
                let Ok(kept) = artifacts::read_build(id, &self.build_store()) else {
                    return Some(json_reply(404, json!({ "error": "Build artifact not found or invalid." })));
                };
                if *which == "manifest" {
                    return Some(json_reply(200, kept.manifest));
                }
                let name = kept.manifest["artifact"]["file"].as_str().unwrap_or("artifact");
                let length = kept.manifest["artifact"]["bytes"].as_u64().unwrap_or(0);
                let file = fs::File::open(&kept.file).ok()?;
                let body = Body::from_stream(tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file)));
                let mut res = reply(200, "application/octet-stream", body);
                res = with_header(res, "content-length", &length.to_string());
                res = with_header(res, "content-disposition", &format!("attachment; filename=\"{name}\""));
                res = with_header(res, "cache-control", "no-store");
                Some(with_header(res, "x-content-type-options", "nosniff"))
            }
            ["api", "crashlogs"] if get => {
                let logs = self.crash_logs();
                Some(json_reply(200, json!({ "crashLogs": logs.list(), "pull": logs.status() })))
            }
            ["api", "crashlogs", "refresh"] if req.method == Method::POST => Some(json_reply(200, json!({ "ok": false, "disabled": true }))),
            ["api", "crashlogs", id, which @ ("ips" | "symbolicated")] if get && (8..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-') => {
                let logs = self.crash_logs();
                let unknown = || json_reply(404, json!({ "error": "unknown crash log" }));
                let entry = logs.get(id).unwrap_or(Value::Null);
                let Some(rel) = entry[if *which == "ips" { "file" } else { "symbolicated" }].as_str() else { return Some(unknown()) };
                let Some(file) = logs.resolve(rel).filter(|f| f.is_file()) else { return Some(unknown()) };
                let Ok(bytes) = fs::read(file) else { return Some(unknown()) };
                // As text, never HTML: the content came from a device.
                let res = with_header(reply(200, "text/plain; charset=utf-8", bytes), "x-content-type-options", "nosniff");
                Some(with_header(res, "content-security-policy", "default-src 'none'"))
            }
            ["api", "traces"] if get => Some(json_reply(200, json!({ "traces": traces::list(&traces) }))),
            ["api", "traces", "report"] if post => Some(self.receive_trace(req, true)),
            ["api", "traces", file] if get && file.strip_suffix(".zip").is_some_and(is_id) => {
                let id = file.strip_suffix(".zip").unwrap();
                let Ok(bytes) = fs::read(traces.join(id).join("trace.zip")) else { return Some(json_reply(404, json!({ "error": "unknown trace" }))) };
                let meta: Value = fs::read_to_string(traces.join(id).join("meta.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null);
                let app_name = meta["app"]["name"].as_str().unwrap_or("app");
                let name = safe_slug(&format!("{app_name}-trace-{}.zip", &id[..8]));
                let res = with_header(reply(200, "application/zip", bytes), "content-disposition", &format!("attachment; filename=\"{name}\""));
                Some(res)
            }
            ["api", "traces", id, "files", rest @ ..] if get && is_id(id) && !rest.is_empty() => {
                let rel = percent_decode_with(&rest.join("/"), false);
                let base = traces.join(id).join("files");
                let file = base.join(&rel);
                let unknown = || json_reply(404, json!({ "error": "unknown file" }));
                if !traces::safe_trace_path(&rel) || !file.is_file() {
                    return Some(unknown());
                }
                let Ok(bytes) = fs::read(&file) else { return Some(unknown()) };
                // Text as text so the browser shows it; anything else downloads. Never rendered
                // as HTML: the content came from a device.
                let ext = file.extension().and_then(|e| e.to_str()).map(|e| format!(".{}", e.to_ascii_lowercase())).unwrap_or_default();
                let is_text = [".txt", ".json", ".jsonl", ".md", ".log", ".csv"].contains(&ext.as_str());
                let mut res = reply(200, if is_text { "text/plain; charset=utf-8" } else { "application/octet-stream" }, bytes);
                res = with_header(res, "x-content-type-options", "nosniff");
                res = with_header(res, "content-security-policy", "default-src 'none'");
                if !is_text {
                    let leaf = file.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                    res = with_header(res, "content-disposition", &format!("attachment; filename=\"{}\"", safe_slug(leaf)));
                }
                Some(res)
            }
            ["api", "traces", id, "delete"] if req.method == Method::POST && is_id(id) => {
                let dir = traces.join(id);
                if !dir.exists() {
                    return Some(json_reply(404, json!({ "error": "unknown trace" })));
                }
                let _ = fs::remove_dir_all(dir);
                Some(json_reply(200, json!({ "ok": true })))
            }
            _ => None,
        }
    }

    fn state_bundle(&self) -> Value {
        let mut bundle = self.state.bundle(&self.config(), system_ms(), &hostname(), &|repo| json!(self.mac.list(repo)));
        // The queue's own builds beside the Node store's, newest first.
        let jobs = self.queue.jobs();
        let ids = |status: &str| jobs.iter().filter(|j| j.status == status).map(|j| json!(j.id)).collect::<Vec<_>>();
        bundle["running"] = Value::Array(ids("running"));
        bundle["queued"] = Value::Array(ids("queued"));
        let mut builds: Vec<Value> = jobs.iter().map(|j| serde_json::to_value(j).unwrap_or(Value::Null)).collect();
        builds.extend(bundle["builds"].as_array().cloned().unwrap_or_default());
        builds.sort_by_key(|b| std::cmp::Reverse(b["queuedAt"].as_u64().unwrap_or(0)));
        builds.truncate(40);
        bundle["builds"] = Value::Array(builds);
        // The last discovery; a fresh one runs in the background, so this never waits on Bonjour.
        bundle["debugApps"] = self.debug.cached_apps();
        self.debug.refresh_in_background();
        bundle
    }

    fn state_response(&self) -> Response {
        json_reply(200, self.state_bundle())
    }

    fn host_status(&self) -> Value {
        let config = self.config();
        json!({
            "hosted": self.hosted(), "port": config.port, "webUi": config.web_ui,
            "passwordSet": config.password_hash.is_some(), "webUiOn": self.web_ui_on(),
            "configPath": self.settings.config_path, "dataDir": self.settings.data_dir,
        })
    }

    fn set_web_ui(&self, body: &Value) -> Result<(), (u16, String)> {
        let enabled = body.get("enabled").and_then(Value::as_bool);
        let password = match body.get("password") {
            None | Some(Value::Null) => None,
            Some(Value::String(p)) if (8..=256).contains(&p.chars().count()) => Some(p.as_str()),
            Some(_) => return Err((400, "Send enabled, and a password of 8 to 256 characters if you set one.".into())),
        };
        let Some(enabled) = enabled else {
            return Err((400, "Send enabled, and a password of 8 to 256 characters if you set one.".into()));
        };
        if enabled && password.is_none() && self.config().password_hash.is_none() {
            return Err((400, "Set a password to turn the web UI on.".into()));
        }
        let hash = password.map(hash_password).transpose().map_err(|e| (400, e.to_string()))?;
        self.update_config(|c| {
            c.insert("webUi".into(), Value::Bool(enabled));
            if let Some(hash) = &hash {
                c.insert("passwordHash".into(), Value::String(hash.clone()));
            }
        })
        .map_err(|_| (500, "Could not change the web UI setting.".to_string()))
    }
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn capabilities() -> Value {
    // Only what this service really does. Clients enable features from this list.
    json!({ "protocol": "apparition-service", "version": 1, "hostPlatform": std::env::consts::OS,
        "capabilities": ["projects", "devices", "builds", "build-logs", "cancel-builds", "traces", "crash-logs", "debug-endpoints",
            "artifact-manifests", "artifact-downloads", "project-registration", "project-discovery", "resign", "resign-build", "entry-patching", "install-modes",
            "report-install"], "authentication": "password-session" })
}

/// Whether the request carries the service API's bearer: Some(true) for the hosting app's own
/// token, Some(false) for a password session, None for neither.
fn bearer_access(app: &Arc<App>, parts: &axum::http::request::Parts) -> Option<bool> {
    let header = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
    let bearer = header.strip_prefix("Bearer ").unwrap_or("");
    let is_host = matches!(&app.settings.host_token, Some(h) if constant_eq(bearer, h));
    let session = !bearer.is_empty() && app.config().password_hash.as_deref().is_some_and(|h| app.auth.check(bearer, h));
    (is_host || session).then_some(is_host)
}

/// Where another computer sends a build to install (`deploy.rs`).
pub const DEPLOY_PATH: &str = "/api/service/v1/deploy";

/// `POST /api/service/v1/deploy?file=<name>&device=<name>` with the IPA as the body, streamed to
/// disk: the bearer of the service API, then a kept build the device's Apparition installs.
async fn deploy_route(app: &Arc<App>, parts: axum::http::request::Parts, body: Body) -> Response {
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Some(is_host) = bearer_access(app, &parts) else {
        return json_reply(401, json!({ "error": "Connect to the build service again." }));
    };
    let query: HashMap<String, String> = parts.uri.query().unwrap_or("").split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (percent_decode(k), percent_decode(v))
    }).collect();
    // The computer's name comes from the companion channel, which sets it with the app's token.
    let computer = if is_host { Some(header("x-apparition-computer")).filter(|c| !c.is_empty()) } else { None };
    let origin = deploy::Origin { computer, file: query.get("file").cloned(), device: query.get("device").cloned().filter(|d| !d.is_empty()) };
    match deploy::receive(&app.build_store(), body, origin).await {
        Ok(manifest) => {
            let name = manifest.pointer("/app/name").and_then(Value::as_str).or_else(|| manifest.pointer("/app/bundleId").and_then(Value::as_str)).unwrap_or("The app").to_string();
            let device = manifest.pointer("/origin/device").and_then(Value::as_str).map(String::from);
            let open_it = |device: &Option<String>| match device {
                Some(device) => format!("Open Apparition on {device} to install it."),
                None => "Open Apparition on the device to install it.".to_string(),
            };
            eprintln!("deploy {} {} from {}", manifest["buildId"].as_str().unwrap_or(""), manifest.pointer("/app/bundleId").and_then(Value::as_str).unwrap_or(""),
                manifest.pointer("/origin/computer").and_then(Value::as_str).unwrap_or("this computer"));
            // The sender may ask the Mac to finish the install itself, through its own pairing with
            // the device. Whatever stops that, the build is kept and the device's Apparition can take it.
            let wants_install = matches!(query.get("install").map(String::as_str), Some("1" | "auto"));
            let install = match (wants_install, &device) {
                (true, Some(target)) => match install_kept(app, manifest["buildId"].as_str().unwrap_or(""), target).await {
                    Ok(done) => json!({ "via": "store", "installed": true, "device": done.device, "seconds": done.seconds, "queued": done.queued, "signedWith": done.signed_with,
                        "message": if done.queued { format!("{name} is on {}; it finishes installing when the app is closed there.", done.device) } else { format!("{name} is installed on {}.", done.device) } }),
                    Err(error) => json!({ "via": "device", "installed": false, "error": error,
                        "message": format!("{name} is on {}, but it couldn't install it itself: {error} {}", crate::HERE, open_it(&device)) }),
                },
                (true, None) => json!({ "via": "device", "installed": false, "error": "no device was named",
                    "message": format!("{name} is on {here}. Name a device to install it from {here}, or open Apparition on the device to install it.", here = crate::HERE) }),
                (false, _) => json!({ "via": "device", "message": format!("{name} is on {}. {}", crate::HERE, open_it(&device)) }),
            };
            json_reply(201, json!({ "ok": true, "buildId": manifest["buildId"], "app": manifest["app"], "platform": manifest["platform"], "install": install }))
        }
        Err(deploy::Refused { status, error }) => json_reply(status, json!({ "ok": false, "error": error })),
    }
}

/// Installs a kept build on a device through this Mac's own pairing. The platform must fit
/// (visionOS also runs iOS apps).
async fn install_kept(app: &Arc<App>, build_id: &str, device: &str) -> Result<devices::Installed, String> {
    install_kept_status(app, build_id, device).await.map_err(|(_, e)| e)
}

/// `install_kept` with the status a route answers: 404 for a build that isn't kept, 409 for one
/// that can't go to that device.
async fn install_kept_status(app: &Arc<App>, build_id: &str, device: &str) -> Result<devices::Installed, (u16, String)> {
    let backend = app.settings.devices.clone().ok_or((501, "this Store has no devices.".to_string()))?;
    let store = app.build_store();
    let kept = artifacts::read_build(build_id, &store).map_err(|e| {
        let missing = !artifacts::valid_build_id(build_id) || !store.join(build_id).join("manifest.json").is_file();
        (if missing { 404 } else { 409 }, format!("{e}."))
    })?;
    install_on(backend, kept, device).await.map_err(|e| (409, e))
}

async fn install_on(backend: Arc<dyn devices::Devices>, kept: artifacts::Kept, device: &str) -> Result<devices::Installed, String> {
    let known = backend.list().await?;
    let target = devices::find(&known, device).map_err(|e| format!("{e}."))?;
    let built = kept.manifest.get("platform").and_then(Value::as_str).unwrap_or("").to_lowercase();
    let fits = built.is_empty() || built == target.platform.to_lowercase() || (built == "ios" && target.platform == "visionOS");
    if !fits {
        return Err(format!("the build is for {}, and {} runs {}.", kept.manifest["platform"].as_str().unwrap_or(""), target.name, target.platform));
    }
    backend.install(target.udid.clone(), kept.file).await.map_err(|e| if e.ends_with('.') { e } else { format!("{e}.") })
}

/// `GET /api/service/v1/devices`, `GET .../devices/<device>/apps` and
/// `POST .../builds/<id>/install?device=<device>`: what this Mac does with devices itself.
/// None when the path is not one of those.
async fn devices_route(app: &Arc<App>, parts: &axum::http::request::Parts) -> Option<Response> {
    let suffix = parts.uri.path().strip_prefix(SERVICE_PREFIX)?;
    let segments: Vec<&str> = suffix.trim_start_matches('/').split('/').collect();
    let get = parts.method == Method::GET;
    let post = parts.method == Method::POST;
    let listing = get && segments == ["devices"];
    let apps = get && segments.len() == 3 && segments[0] == "devices" && segments[2] == "apps";
    let install = post && segments.len() == 3 && segments[0] == "builds" && segments[2] == "install";
    if !(listing || apps || install) {
        return None;
    }
    if bearer_access(app, parts).is_none() {
        return Some(json_reply(401, json!({ "error": "Connect to the build service again." })));
    }
    let Some(backend) = app.settings.devices.clone() else {
        return Some(json_reply(501, json!({ "error": "This Mac's Store reaches no devices." })));
    };
    let fail = |status: u16, error: String| json_reply(status, json!({ "ok": false, "error": error }));
    if listing {
        return Some(match backend.list().await {
            Ok(list) => json_reply(200, json!({ "devices": list })),
            Err(e) => fail(502, e),
        });
    }
    if apps {
        let wanted = percent_decode(segments[1]);
        let list = match backend.list().await { Ok(l) => l, Err(e) => return Some(fail(502, e)) };
        let target = match devices::find(&list, &wanted) { Ok(t) => t.clone(), Err(e) => return Some(fail(404, e)) };
        return Some(match backend.apps(target.udid.clone()).await {
            Ok(apps) => json_reply(200, json!({ "device": target, "apps": apps })),
            Err(e) => fail(502, e),
        });
    }
    let query: HashMap<String, String> = parts.uri.query().unwrap_or("").split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (percent_decode(k), percent_decode(v))
    }).collect();
    let Some(device) = query.get("device").filter(|d| !d.is_empty()) else {
        return Some(fail(400, "name the device with ?device=".into()));
    };
    Some(match install_kept_status(app, segments[1], device).await {
        Ok(done) => json_reply(200, json!({ "ok": true, "installed": done })),
        Err((status, e)) => fail(status, e),
    })
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    if app.queue.stopped() {
        return json_reply(503, json!({ "error": "This Store has stopped." }));
    }
    let (parts, body) = request.into_parts();
    if parts.method == Method::POST && parts.uri.path() == DEPLOY_PATH {
        return deploy_route(&app, parts, body).await;
    }
    if let Some(res) = devices_route(&app, &parts).await {
        return res;
    }
    let upload = parts.method == Method::POST && ["/api/traces", "/api/traces/report"].contains(&parts.uri.path());
    // Uploads are read and verified in memory (up to the inflated cap each): a few at a time,
    // and the turn is taken before the body is read, so waiting ones hold no memory.
    static UPLOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let _upload = if upload { UPLOADS.acquire().await.ok() } else { None };
    let limit = if upload { traces::MAX_UPLOAD } else { 1_000_000 };
    let body = match to_bytes(body, limit).await {
        Ok(b) => b,
        Err(_) if upload => return json_reply(413, json!({ "error": format!("trace is larger than {} MB", traces::MAX_UPLOAD >> 20) })),
        Err(_) => return json_reply(413, json!({ "error": "request body is too large" })),
    };
    let query = parts.uri.query().unwrap_or("").split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (percent_decode(k), percent_decode(v))
    }).collect();
    let req = Req { method: parts.method, path: parts.uri.path().to_string(), query, headers: parts.headers, body };
    // blocking work (scrypt, file reads) stays off the async threads
    let dispatcher = app.clone();
    let res = tokio::task::spawn_blocking(move || dispatch(&dispatcher, &req)).await.unwrap_or_else(|_| json_reply(500, json!({ "error": "internal error" })));
    match res.extensions().get::<Deferred>().cloned() {
        Some(route) => run_debug(&app, route).await,
        None => res,
    }
}

fn reply_of(reply: debug::Reply) -> Response {
    if reply.body.is_some() || reply.raw.is_empty() {
        return json_reply(reply.status, reply.body.unwrap_or(Value::Null));
    }
    // An image or file from the app: passed through, never as HTML.
    let ct = reply.content_type.to_lowercase();
    let allowed = ["image/png", "image/jpeg", "image/gif", "image/webp", "image/heic", "application/zip", "application/octet-stream", "text/plain"].iter().any(|t| ct.starts_with(t));
    let res = reply_status(reply.status, if allowed { &reply.content_type } else { "application/octet-stream" }, reply.raw);
    let res = with_header(res, "x-content-type-options", "nosniff");
    with_header(res, "content-security-policy", "default-src 'none'")
}

fn reply_status(status: u16, content_type: &str, body: impl Into<Body>) -> Response {
    reply(status, content_type, body)
}

async fn run_debug(app: &Arc<App>, route: Deferred) -> Response {
    match route {
        Deferred::Apps => json_reply(200, app.debug.apps().await),
        Deferred::Help(id) => reply_of(app.debug.help(&id).await),
        Deferred::Call(id, body) => {
            let args = body.get("arguments").filter(|a| a.is_object()).cloned().unwrap_or_else(|| json!({}));
            reply_of(app.debug.call(&id, body["endpoint"].as_str().unwrap_or(""), args).await)
        }
        Deferred::Panels(id) => reply_of(app.debug.panels(&id).await),
        Deferred::UpdatesCheck => {
            let ctx = app.queue_context();
            app.queue.refresh_devices(&ctx, false).await;
            let (queue, c) = (app.queue.clone(), ctx);
            let report = tokio::task::spawn_blocking(move || queue.report(&c)).await.unwrap_or(Value::Null);
            json_reply(200, report)
        }
        Deferred::RefreshDevices(force) => {
            let config = app.config();
            let mut configured: Vec<String> = queue::targets(&config).into_iter().map(|t| t.device).collect();
            configured.sort();
            configured.dedup();
            let skipped = if force { Vec::new() } else { let skip = config.skipped_devices(); configured.into_iter().filter(|d| skip.contains(d)).collect() };
            let (queue, ctx) = (app.queue.clone(), app.queue_context());
            tokio::spawn(async move { queue.refresh_devices(&ctx, force).await });
            json_reply(200, json!({ "ok": true, "skipped": skipped }))
        }
        Deferred::AltIpa { path, query, slug, commit } => app.serve_alt_ipa(path, query, slug, commit).await,
        Deferred::MacZip { path, query, slug, commit } => app.serve_mac_zip(path, query, slug, commit).await,
        Deferred::Inject(job) => app.inject_run(job).await,
        Deferred::Mcp(message) => {
            if message.is_array() {
                return json_reply(200, json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "batches are not supported" } }));
            }
            app.mcp.set_cli(queue::cli(&app.config()));
            match app.mcp.handle(&message).await {
                Some(answer) => json_reply(200, answer),
                None => reply(202, "text/plain", ""),
            }
        }
        Deferred::Logs(id, level, contains) => {
            let rx = app.debug.stream_logs(id, level, contains);
            let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::convert::Infallible>);
            let mut res = reply(200, "text/event-stream", Body::from_stream(stream));
            res = with_header(res, "cache-control", "no-store");
            res = with_header(res, "connection", "keep-alive");
            with_header(res, "x-accel-buffering", "no")
        }
    }
}

fn dispatch(app: &App, req: &Req) -> Response {
    if req.cross_site() {
        return json_reply(403, json!({ "error": "cross-site request refused" }));
    }
    if let Some(res) = service_api(app, req) {
        return res;
    }
    if req.path == "/host-login" && req.method == Method::GET {
        let code = req.query.get("code").cloned().unwrap_or_default();
        let expiry = app.host_codes.lock().unwrap().remove(&code);
        if expiry.is_none_or(|e| e < system_ms()) {
            return text(403, "This link has expired. Open it from Apparition again.");
        }
        let token = crate::auth::new_token();
        app.host_sessions.lock().unwrap().insert(token.clone());
        // a session cookie: no Max-Age, so it ends with the window
        let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict");
        let next = req.query.get("next").map(String::as_str).unwrap_or("/");
        return with_header(redirect(safe_next(next)), "set-cookie", &cookie);
    }
    let machine_route = (req.method == Method::POST && req.path == "/api/traces")
        || req.path == "/mcp"
        || (req.method == Method::GET && ["/ota/", "/altsource/", "/mac/"].iter().any(|p| req.path.starts_with(p)));
    if !app.web_ui_on() && !machine_route && !app.host_session(req) {
        return text(404, WEB_UI_OFF);
    }

    // ---- reachable without a session ----
    let config = app.config();
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/login") => {
            let Some(hash) = &config.password_hash else {
                // Open only to this Mac's own pages; elsewhere there is nothing to sign in with.
                return if app.signed_in(req) { json_reply(200, json!({ "ok": true, "open": true })) }
                    else { json_reply(403, json!({ "error": "This Store has no password. Set one under Web UI and other devices in Apparition, or `apparition store web-ui on`." })) };
            };
            let password = req.json_body().get("password").and_then(Value::as_str).unwrap_or("").to_string();
            let user = req.header("tailscale-user-login");
            return match app.auth.login(&password, hash, (!user.is_empty()).then_some(user)) {
                Login::Ok(token) => with_header(json_reply(200, json!({ "ok": true })), "set-cookie", &Auth::cookie(&token, req.is_https())),
                Login::Refused { status, error } => json_reply(status, json!({ "error": error })),
            };
        }
        ("POST", "/api/logout") => {
            let token = req.session_token();
            app.host_sessions.lock().unwrap().remove(&token);
            app.auth.logout(&token);
            return with_header(json_reply(200, json!({ "ok": true })), "set-cookie", &Auth::clear_cookie(req.is_https()));
        }
        ("GET", "/api/auth") => {
            return json_reply(200, json!({ "required": config.password_hash.is_some(), "signedIn": app.signed_in(req) }));
        }
        _ => {}
    }
    if req.method == Method::GET && (req.path == "/login" || PUBLIC_FILES.contains(&req.path.as_str())) {
        if req.path == "/login" && app.signed_in(req) {
            return redirect(safe_next(req.query.get("next").map(String::as_str).unwrap_or("/")));
        }
        let file = if req.path == "/login" { "login.html" } else { &req.path[1..] };
        if let Some(res) = app.static_file(file) {
            return res;
        }
    }

    // ---- everything below needs a session, or one of the signed links ----
    if req.method == Method::POST && req.path == "/api/traces" {
        return app.receive_trace(req, false);
    }
    // AltSource: clients have no cookie; the feed has its lasting token, its links their own.
    if let Some(routed) = app.alt_source_routes(req) {
        return match routed {
            Ok(res) => res,
            Err((slug, commit, path)) => defer(Deferred::AltIpa { path, query: req.query.clone(), slug, commit }),
        };
    }
    // installd can't send the cookie, so the OTA files also take a signed `?t=`; so do the Mac
    // downloads, for a Mac with no session.
    let ota_signed = req.method == Method::GET && req.path.strip_prefix("/ota/").is_some_and(|rest| {
        let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        let name = rest.strip_suffix(".ipa").map(|s| ok(s) && !s.contains('/'))
            .or_else(|| rest.strip_suffix("/manifest.plist").map(ok)).unwrap_or(false);
        name && req.query.get("t").is_some_and(|t| app.auth.verify_path(&req.path, t))
    });
    let mac_link = app.mac_link_signed(req);
    let mac_signed = mac_link.as_ref().is_some_and(|m| m.2);
    if mac_link.is_some() && req.query.contains_key("t") && !mac_signed && !app.signed_in(req) {
        return text(403, "link expired or not signed");
    }
    if !(ota_signed || mac_signed) && !app.signed_in(req) {
        if req.path.starts_with("/api/") {
            return json_reply(401, json!({ "error": "sign in first", "login": "/login" }));
        }
        let target = format!("{}{}", req.path, req.query_string());
        return redirect(&format!("/login?next={}", encode_uri_component(&target)));
    }
    if let Some((slug, which, signed)) = mac_link {
        return if which == "latest" {
            app.mac_latest(req, &slug, signed)
        } else {
            defer(Deferred::MacZip { path: req.path.clone(), query: req.query.clone(), slug, commit: which })
        };
    }
    if req.method == Method::GET {
        let file = if req.path == "/" { "index.html" } else { &req.path[1..] };
        if let Some(res) = app.static_file(file) {
            return res;
        }
    }
    if let Some(res) = app.api(req, &req.path) {
        return res;
    }
    json_reply(404, json!({ "error": "not found" }))
}

impl Req {
    fn query_string(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        let mut pairs: Vec<_> = self.query.iter().collect();
        pairs.sort();
        format!("?{}", pairs.iter().map(|(k, v)| format!("{}={}", encode_uri_component(k), encode_uri_component(v))).collect::<Vec<_>>().join("&"))
    }
}

/// `/api/service*`: the native clients' API. None when the path is not theirs.
fn service_api(app: &App, req: &Req) -> Option<Response> {
    let config = app.config();
    if req.path == "/api/service" && req.method == Method::GET {
        return Some(json_reply(200, capabilities()));
    }
    if req.path == "/api/service/connect" && req.method == Method::POST {
        let Some(hash) = &config.password_hash else {
            return Some(json_reply(409, json!({ "error": "Set a password under Web UI and other devices in Apparition first." })));
        };
        let password = req.json_body().get("password").and_then(Value::as_str).unwrap_or("").to_string();
        return Some(match app.auth.login(&password, hash, None) {
            Login::Ok(token) => {
                let mut body = capabilities();
                body["token"] = Value::String(token);
                json_reply(200, body)
            }
            Login::Refused { status, error } => json_reply(status, json!({ "error": error })),
        });
    }
    if req.path == "/mcp" {
        // The MCP endpoint for agents anywhere: the same bearer as the service API.
        let bearer = req.header("authorization").strip_prefix("Bearer ").unwrap_or("");
        let is_host = matches!(&app.settings.host_token, Some(h) if constant_eq(bearer, h));
        let session = config.password_hash.as_deref().is_some_and(|h| app.auth.check(bearer, h));
        if !is_host && !session {
            return Some(json_reply(401, json!({ "error": "Connect to the build service again." })));
        }
        if req.method != Method::POST {
            return Some(with_header(json_reply(405, json!({ "error": "POST JSON-RPC messages to /mcp" })), "allow", "POST"));
        }
        return Some(match serde_json::from_slice::<Value>(&req.body) {
            Ok(message) => defer(Deferred::Mcp(message)),
            Err(_) => json_reply(200, json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })),
        });
    }
    let suffix = req.path.strip_prefix(SERVICE_PREFIX).filter(|s| s.is_empty() || s.starts_with('/'))?;
    let bearer = req.header("authorization").strip_prefix("Bearer ").filter(|t| {
        t.len() == 43 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    let token = matches!((bearer, &app.settings.host_token), (Some(t), Some(h)) if constant_eq(t, h));
    // The companion channel brings paired computers in with the app's token, and always names
    // the computer: they drive builds and deploys, but the Store's own settings (its password,
    // the web pages, sign-in links) stay the owner's, as for a password session.
    let paired = token && !req.header("x-apparition-computer").is_empty();
    let is_host = token && !paired;
    let session = paired || bearer.is_some_and(|t| config.password_hash.as_deref().is_some_and(|h| app.auth.check(t, h)));
    if !is_host && !session {
        return Some(json_reply(401, json!({ "error": "Connect to the build service again." })));
    }
    // Apparition names its device on every request while it is open, so a build asked for
    // `install: auto` goes to it and not through the Xcode tunnel; a closed one goes quiet.
    if let Some(name) = device_header(req.header("x-apparition-device")) {
        app.state.presence.touch(&name, system_ms());
    }
    let unknown = || json_reply(404, json!({ "error": "Unknown service operation." }));
    Some(match (req.method.as_str(), suffix) {
        ("GET", "" | "/capabilities") => json_reply(200, capabilities()),
        ("GET", "/state") => app.state_response(),
        ("GET", "/debug/apps") => app.api(req, "/api/debug/apps").unwrap_or_else(unknown),
        ("GET", s) if s.starts_with("/debug/") && s.ends_with("/help") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("POST", s) if s.starts_with("/debug/") && s.ends_with("/call") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("GET", "/crashlogs") => app.api(req, "/api/crashlogs").unwrap_or_else(unknown),
        ("GET", s) if s.starts_with("/crashlogs/") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("GET", "/artifacts") => app.api(req, "/api/artifacts").unwrap_or_else(unknown),
        ("GET", s) if s.starts_with("/artifacts/") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("GET", "/traces") => app.api(req, "/api/traces").unwrap_or_else(unknown),
        ("GET", "/updates") => app.api(req, "/api/updates").unwrap_or_else(unknown),
        ("POST", "/updates/run" | "/updates/check" | "/updates/auto") => app.api(req, &format!("/api{suffix}")).unwrap_or_else(unknown),
        ("GET", s) if s.starts_with("/builds/") && s.ends_with("/log") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("POST", s) if s.starts_with("/builds/") && s.ends_with("/cancel") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("GET", s) if s.starts_with("/traces/") && s.contains("/files/") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        (_, "/host" | "/host/web-ui" | "/host/web-link") if !is_host => unknown(),
        ("GET", "/host") => json_reply(200, app.host_status()),
        ("POST", "/host/web-link") => {
            let code = crate::auth::new_token();
            let now = system_ms();
            let mut codes = app.host_codes.lock().unwrap();
            codes.retain(|_, expiry| *expiry >= now);
            codes.insert(code.clone(), now + 60_000);
            json_reply(200, json!({ "path": format!("/host-login?code={code}") }))
        }
        ("POST", "/host/web-ui") => match app.set_web_ui(&req.json_body()) {
            Ok(()) => json_reply(200, app.host_status()),
            Err((status, message)) => json_reply(status, json!({ "error": message })),
        },
        ("POST", "/disconnect") => {
            if !token {
                app.auth.logout(bearer.unwrap_or(""));
            }
            json_reply(200, json!({ "ok": true }))
        }
        ("GET", "/resign/options") => app.api(req, "/api/resign/options").unwrap_or_else(unknown),
        ("POST", "/resign") => app.api(req, "/api/resign").unwrap_or_else(unknown),
        ("POST", "/build") => app.service_build(req),
        ("POST", "/inject") => app.service_inject(req),
        ("POST", "/installs") => app.service_installs(req),
        ("POST", "/projects/discover") => app.service_discover(req),
        ("POST", "/projects") => app.service_register(req),
        // The rest of what the Store's page does (refreshing devices, OTA, AltStore and Mac
        // links, Mac download builds, debug panels and logs, trace downloads and deletion), as
        // the page asks for it.
        _ => app.api(req, &format!("/api{suffix}")).unwrap_or_else(unknown),
    })
}

/// The name a client sends for the device it runs on, percent-encoded because headers are ASCII.
fn device_header(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 400 {
        return None;
    }
    let decoded = percent_decode(&value.replace('+', "%2B"));
    Some(decoded)
}

fn safe_slug(s: &str) -> String {
    let mut out = String::new();
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
