//! Live debugging of running dev builds: finding them, and talking to their DebugTrace servers.
//!
//! This is `DebugTrace/Tools/debugtrace-mcp`'s discovery and `store/lib/debug.js`'s proxy, in the
//! process that already holds the Mac's Local Network permission, so a LAN app is reached at the
//! address Bonjour advertises (the Python tool could not, and guessed tailnet names).
//!
//! Apps are found two ways. `build-and-sign --run --mcp` records a session in
//! `<state>/sessions/<bundle id>@<device>.json` (URL, the console streamer's pid, inject paths).
//! And every dev build signed by `build-and-sign` advertises `_debugtrace._tcp` with a random
//! nonce and a tag over it under the build's command token, so only the key ledger that holds the
//! token can say which app it is. A page or an agent addresses an app by its build's key id; the
//! URL comes from discovery and the token from the ledger, never from the caller.

use futures_util::future::BoxFuture;
use ring::hmac;
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const SERVICE_TYPE: &str = "_debugtrace._tcp.local.";
const DISCOVERY_TTL: Duration = Duration::from_secs(5);
/// One call may wait for the person at the device to answer "Allow debug access?" (DebugTrace
/// gives up after 60 s), and `_profile` then samples for up to 10 s.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// A session as the Python tool keeps it: a loose record, because launch records carry fields
/// this module only passes along.
pub type Session = Map<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub instance: String,
    pub host: String,
    pub port: u16,
    pub txt: HashMap<String, String>,
}

/// Where Bonjour answers come from; a fixed list in tests.
pub trait Browse: Send + Sync + 'static {
    fn browse(&self) -> BoxFuture<'static, Vec<Service>>;
}

/// A fixed list: tests, and anything that supplies its own discovery.
pub struct Fixed(pub Vec<Service>);

impl Browse for Fixed {
    fn browse(&self) -> BoxFuture<'static, Vec<Service>> {
        let services = self.0.clone();
        Box::pin(async move { services })
    }
}

/// The real thing: one daemon for the process, kept browsing, so a call never waits for a
/// multicast round trip. A suspended app that drops its advertisement leaves the map.
#[derive(Default)]
pub struct Mdns {
    live: Arc<Mutex<HashMap<String, Service>>>,
    started: Mutex<Option<mdns_sd::ServiceDaemon>>,
}

impl Mdns {
    fn start(&self) {
        let mut started = self.started.lock().unwrap();
        if started.is_some() {
            return;
        }
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return };
        let Ok(events) = daemon.browse(SERVICE_TYPE) else { return };
        let live = self.live.clone();
        tokio::spawn(async move {
            while let Ok(event) = events.recv_async().await {
                match event {
                    mdns_sd::ServiceEvent::ServiceResolved(info) => {
                        let txt = info.get_properties().iter().map(|p| (p.key().to_string(), p.val_str().to_string())).collect();
                        let fullname = info.get_fullname().to_string();
                        let instance = fullname.strip_suffix(&format!(".{SERVICE_TYPE}")).unwrap_or(&fullname).to_string();
                        let host = info.get_hostname().trim_end_matches('.').to_string();
                        live.lock().unwrap().insert(fullname, Service { instance, host, port: info.get_port(), txt });
                    }
                    mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                        live.lock().unwrap().remove(&fullname);
                    }
                    _ => {}
                }
            }
        });
        *started = Some(daemon);
    }
}

impl Browse for Arc<Mdns> {
    fn browse(&self) -> BoxFuture<'static, Vec<Service>> {
        self.start();
        let this = self.clone();
        Box::pin(async move {
            // The first call after start has had no time to hear anything.
            tokio::time::sleep(Duration::from_millis(if this.live.lock().unwrap().is_empty() { 1500 } else { 0 })).await;
            let mut found: Vec<Service> = this.live.lock().unwrap().values().cloned().collect();
            found.sort_by(|a, b| a.instance.cmp(&b.instance));
            found
        })
    }
}

/// The `t` a server with this token advertises for nonce `n` (`DebugTraceServer.advertisementTag`).
pub fn advertisement_tag(token: &str, nonce: &str) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    let key = hmac::Key::new(hmac::HMAC_SHA256, token.as_bytes());
    let tag = hmac::sign(&key, format!("debugtrace-advert|{nonce}").as_bytes());
    URL_SAFE_NO_PAD.encode(&tag.as_ref()[..12])
}

fn valid_key_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn str_of<'a>(session: &'a Session, key: &str) -> Option<&'a str> {
    session.get(key).and_then(Value::as_str)
}

fn port_of(url: &str) -> &str {
    url.rsplit(':').next().unwrap_or("").trim_end_matches('/')
}

pub struct Debug {
    key_dir: PathBuf,
    sessions_dir: PathBuf,
    panels_dir: PathBuf,
    browser: Arc<dyn Browse>,
    pub client_name: String,
    cache: tokio::sync::Mutex<Option<(Instant, Vec<Session>)>>,
    last: Mutex<Vec<Session>>,
    http: reqwest::Client,
}

pub struct Reply {
    pub status: u16,
    pub body: Option<Value>,
    pub raw: Vec<u8>,
    pub content_type: String,
}

impl Reply {
    fn error(status: u16, message: &str) -> Reply {
        Reply { status, body: Some(json!({ "ok": false, "error": { "message": message } })), raw: Vec::new(), content_type: "application/json".into() }
    }
}

impl Debug {
    pub fn new(key_dir: PathBuf, state_dir: &Path, browser: Arc<dyn Browse>, client_name: String) -> Debug {
        Debug {
            panels_dir: key_dir.parent().map_or_else(|| PathBuf::from("panels"), |p| p.join("panels")),
            key_dir,
            sessions_dir: state_dir.join("sessions"),
            browser,
            client_name,
            cache: tokio::sync::Mutex::new(None),
            last: Mutex::default(),
            http: reqwest::Client::builder().build().expect("http client"),
        }
    }

    fn ledger_record(&self, key_id: &str) -> Option<Value> {
        if !valid_key_id(key_id) {
            return None;
        }
        serde_json::from_str(&std::fs::read_to_string(self.key_dir.join(format!("{key_id}.json"))).ok()?).ok()
    }

    /// The command token of a build, from the ledger.
    pub fn token(&self, key_id: &str) -> Option<String> {
        self.ledger_record(key_id)?.get("commandToken")?.as_str().filter(|t| !t.is_empty()).map(String::from)
    }

    /// Every ledger record that holds a command token.
    fn ledger_records(&self) -> Vec<Value> {
        let Ok(names) = std::fs::read_dir(&self.key_dir) else { return Vec::new() };
        let mut ids: Vec<String> = names.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(String::from)).collect();
        ids.sort();
        ids.iter().filter_map(|id| self.ledger_record(id)).filter(|r| r["commandToken"].as_str().is_some_and(|t| !t.is_empty())).collect()
    }

    /// The ledger record behind an advertisement. Current builds are matched by their tag;
    /// builds from before it still name their `keyId`.
    fn recognise(&self, txt: &HashMap<String, String>, records: &[Value]) -> Option<Value> {
        if let (Some(nonce), Some(tag)) = (txt.get("n").filter(|n| !n.is_empty()), txt.get("t").filter(|t| !t.is_empty())) {
            return records.iter().find(|r| {
                let expected = advertisement_tag(r["commandToken"].as_str().unwrap_or(""), nonce);
                { use subtle::ConstantTimeEq; expected.len() == tag.len() && bool::from(expected.as_bytes().ct_eq(tag.as_bytes())) }
            }).cloned();
        }
        self.ledger_record(txt.get("keyId")?)
    }

    /// Where to reach an advertised server: the device's tailnet name when that answers (its name
    /// lowercased, as `bas --mcp` tries first), else the advertised host.
    async fn bonjour_url(&self, service: &Service, device: &str) -> String {
        let mut host = service.host.clone();
        if !device.is_empty() {
            let name = device.to_lowercase();
            let attempt = tokio::net::TcpStream::connect((name.as_str(), service.port));
            if matches!(tokio::time::timeout(Duration::from_secs(1), attempt).await, Ok(Ok(_))) {
                host = name;
            }
        }
        format!("http://{host}:{}", service.port)
    }

    /// Advertised servers as sessions. `attachable` is false, with a reason, when this Mac's
    /// ledger has no token for that build.
    pub async fn discovered_sessions(&self, services: &[Service]) -> Vec<Session> {
        let records = if services.is_empty() { Vec::new() } else { self.ledger_records() };
        let mut sessions = Vec::new();
        for service in services {
            let record = self.recognise(&service.txt, &records).unwrap_or(Value::Null);
            let rec = |k: &str| record.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            let txt = |k: &str| service.txt.get(k).filter(|s| !s.is_empty()).cloned();
            let device = rec("device").unwrap_or_else(|| service.host.trim_end_matches(".local").to_string());
            let project = rec("project").and_then(|p| Path::new(&p).file_name().map(|n| n.to_string_lossy().into_owned()));
            let has_token = rec("commandToken").is_some();
            let mut s = Session::new();
            s.insert("appName".into(), json!(txt("name").or(project).or(rec("bundleId")).unwrap_or_else(|| "An unrecognised app".into())));
            s.insert("bundleId".into(), json!(rec("bundleId").or(txt("bundleId"))));
            s.insert("build".into(), json!(rec("build").or(txt("build"))));
            s.insert("version".into(), json!(txt("version")));
            s.insert("keyId".into(), json!(rec("keyId").or(txt("keyId"))));
            s.insert("device".into(), json!(device));
            s.insert("startedAt".into(), Value::Null);
            s.insert("source".into(), json!("bonjour"));
            // Probed only for an app this Mac can attach to: nobody calls the others.
            let url = if has_token { self.bonjour_url(service, &device).await } else { format!("http://{}:{}", service.host, service.port) };
            s.insert("url".into(), json!(url));
            if has_token {
                s.insert("token".into(), json!(rec("commandToken")));
                s.insert("attachable".into(), json!(true));
            } else {
                s.insert("attachable".into(), json!(false));
                s.insert("reason".into(), json!(if record.is_null() {
                    "built on another Mac, or before per-build tokens: not in this Mac's key ledger"
                } else {
                    "this build has no command token; rebuild it with build-and-sign"
                }));
            }
            sessions.push(s);
        }
        sessions
    }

    /// A launch record is live while the process streaming the app's log runs. A shell can reap
    /// that process while the app keeps serving, so a record without one is kept and only
    /// revived when Bonjour shows the same build.
    fn session_records(&self) -> Vec<Session> {
        let Ok(names) = std::fs::read_dir(&self.sessions_dir) else { return Vec::new() };
        let mut names: Vec<PathBuf> = names.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "json")).collect();
        names.sort();
        names.into_iter().filter_map(|path| {
            let Value::Object(mut session) = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()? else { return None };
            session.insert("_path".into(), json!(path));
            if !pid_is_console_streamer(session.get("pid").and_then(Value::as_i64)) {
                session.insert("_consoleEnded".into(), json!(true));
            }
            Some(session)
        }).collect()
    }

    /// Live console records plus Bonjour finds, recovering a dead console's metadata only when
    /// the exact recorded build (bundle, device and key id) is still advertising.
    pub async fn all_sessions(&self) -> Vec<Session> {
        let records = self.session_records();
        let (inactive, mut sessions): (Vec<Session>, Vec<Session>) = records.into_iter().partition(|s| s.contains_key("_consoleEnded"));
        let seen: Vec<(Option<String>, String)> = sessions.iter().map(|s| (str_of(s, "bundleId").map(String::from), port_of(str_of(s, "url").unwrap_or("")).to_string())).collect();
        for found in self.recent_discoveries().await {
            let key = (str_of(&found, "bundleId").map(String::from), port_of(str_of(&found, "url").unwrap_or("")).to_string());
            if seen.contains(&key) {
                continue;
            }
            let recovered = inactive.iter().find(|r| {
                str_of(r, "bundleId") == str_of(&found, "bundleId")
                    && str_of(r, "device").unwrap_or("").to_lowercase() == str_of(&found, "device").unwrap_or("").to_lowercase()
                    && str_of(r, "keyId").is_some_and(|k| !k.is_empty())
                    && str_of(r, "keyId") == str_of(&found, "keyId")
            });
            let mut merged = found.clone();
            if let Some(recovered) = recovered {
                for k in ["projectRoot", "injectDir", "consoleLog", "startedAt", "_path"] {
                    if let Some(v) = recovered.get(k).filter(|v| !v.is_null()) {
                        merged.insert(k.into(), v.clone());
                    }
                }
            }
            sessions.push(merged);
        }
        sessions
    }

    async fn recent_discoveries(&self) -> Vec<Session> {
        let mut cache = self.cache.lock().await;
        if let Some((at, sessions)) = cache.as_ref().filter(|(at, _)| at.elapsed() < DISCOVERY_TTL) {
            let _ = at;
            return sessions.clone();
        }
        let services = self.browser.browse().await;
        let sessions = self.discovered_sessions(&services).await;
        *cache = Some((Instant::now(), sessions.clone()));
        sessions
    }

    /// The apps for the page: never the token.
    pub fn public_app(&self, s: &Session) -> Value {
        let key_id = str_of(s, "keyId").filter(|k| !k.is_empty());
        let usable = key_id.is_some_and(|k| str_of(s, "url").is_some_and(|u| !u.is_empty()) && self.token(k).is_some());
        let mut app = json!({
            "id": key_id, "appName": s.get("appName"), "bundleId": s.get("bundleId"), "build": s.get("build").filter(|b| !b.is_null()),
            "device": s.get("device"), "source": str_of(s, "source").unwrap_or("session"),
            "attachable": usable && s.get("attachable").and_then(Value::as_bool) != Some(false),
        });
        if s.get("consoleLog").is_some_and(|c| !c.is_null() && c.as_str() != Some("")) {
            app["consoleLog"] = json!(true);
        }
        if !usable {
            app["reason"] = json!(str_of(s, "reason").filter(|r| !r.is_empty()).map(String::from).unwrap_or_else(|| if key_id.is_some() {
                "no command token for this build in the key ledger".into()
            } else {
                "launched before per-build keys; relaunch it with build-and-sign".into()
            }));
        }
        app
    }

    /// Running apps for `/api/debug/apps`, with the discovery error, if any.
    pub async fn apps(&self) -> Value {
        let sessions = self.all_sessions().await;
        let apps: Vec<Value> = sessions.iter().map(|s| self.public_app(s)).collect();
        *self.last.lock().unwrap() = sessions;
        json!({ "apps": apps, "error": null, "clientName": self.client_name })
    }

    /// What the last discovery found, for `/api/state`, which never waits on Bonjour.
    pub fn cached_apps(&self) -> Value {
        let apps: Vec<Value> = self.last.lock().unwrap().iter().map(|s| self.public_app(s)).collect();
        json!({ "apps": apps, "error": null })
    }

    /// Starts a fresh discovery in the background, so the next `cached_apps` is current.
    pub fn refresh_in_background(self: &Arc<Self>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let this = self.clone();
            handle.spawn(async move { this.apps().await; });
        }
    }

    async fn find(&self, id: &str) -> Result<(Session, String), Reply> {
        let sessions = self.all_sessions().await;
        let app = sessions.into_iter().find(|s| str_of(s, "keyId") == Some(id))
            .ok_or_else(|| Reply::error(404, "that app is no longer running, or is not on this network"))?;
        let token = self.token(id).ok_or_else(|| Reply::error(403, "no command token for this build in the key ledger"))?;
        Ok((app, token))
    }

    /// One request to the app.
    async fn request(&self, app: &Session, token: &str, post: Option<Value>, path: &str, timeout: Duration) -> Result<Reply, reqwest::Error> {
        let base = str_of(app, "url").unwrap_or("").trim_end_matches('/');
        let mut req = self.http.request(if post.is_some() { reqwest::Method::POST } else { reqwest::Method::GET }, format!("{base}{path}"))
            .timeout(timeout)
            // Named in the app's "Allow debug access?" prompt.
            .header("Authorization", format!("Bearer {token}")).header("Accept", "application/json").header("X-DebugTrace-Client", &self.client_name);
        if let Some(body) = post {
            req = req.json(&body);
        }
        let res = req.send().await?;
        let status = res.status().as_u16();
        let content_type = res.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let raw = res.bytes().await?.to_vec();
        let body = content_type.contains("json").then(|| serde_json::from_slice(&raw).ok()).flatten();
        Ok(Reply { status, body, raw, content_type })
    }

    fn unreachable(&self, app: &Session, e: &reqwest::Error) -> Reply {
        let name = str_of(app, "appName").unwrap_or("The app");
        let timeout = e.is_timeout();
        Reply {
            status: if timeout { 504 } else { 502 },
            body: Some(json!({ "ok": false, "error": {
                "code": if timeout { "timeout" } else { "unreachable" },
                "message": format!("{name} did not answer: {e}"),
                "hint": if timeout {
                    format!("if the app is showing \"Allow debug access?\" for \"{}\", choose Allow and run it again; otherwise bring the app to the foreground", self.client_name)
                } else { "iOS suspends apps in the background; bring it to the foreground".to_string() },
            } })),
            raw: Vec::new(),
            content_type: "application/json".into(),
        }
    }

    pub async fn call(&self, id: &str, endpoint: &str, args: Value) -> Reply {
        let (app, token) = match self.find(id).await { Ok(f) => f, Err(r) => return r };
        match self.request(&app, &token, Some(json!({ "name": endpoint, "arguments": args })), "/_call", CALL_TIMEOUT).await {
            Ok(r) => r,
            Err(e) => self.unreachable(&app, &e),
        }
    }

    pub async fn help(&self, id: &str) -> Reply {
        let (app, token) = match self.find(id).await { Ok(f) => f, Err(r) => return r };
        match self.request(&app, &token, None, "/", CALL_TIMEOUT).await {
            Ok(r) => r,
            Err(e) => self.unreachable(&app, &e),
        }
    }

    /// The app's saved tuning panels, newest first, read from disk on every request so an
    /// agent's edit shows on the page's next poll. Malformed files are skipped.
    pub async fn panels(&self, id: &str) -> Reply {
        let (app, _) = match self.find(id).await { Ok(f) => f, Err(r) => return r };
        let ok = |body: Value| Reply { status: 200, body: Some(body), raw: Vec::new(), content_type: "application/json".into() };
        let bundle = str_of(&app, "bundleId").unwrap_or("");
        if bundle.is_empty() || bundle.len() > 200 || !bundle.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
            return ok(json!({ "ok": true, "panels": [] }));
        }
        let mut list: Vec<Value> = Vec::new();
        let mut names: Vec<String> = std::fs::read_dir(self.panels_dir.join(bundle)).into_iter().flatten().flatten()
            .filter_map(|e| e.file_name().to_str().map(String::from)).filter(|n| valid_panel_file(n)).collect();
        names.sort();
        for name in names {
            let Some(Value::Object(mut panel)) = std::fs::read_to_string(self.panels_dir.join(bundle).join(&name)).ok().and_then(|t| serde_json::from_str(&t).ok()) else { continue };
            if panel.get("controls").is_some_and(Value::is_array) {
                panel.insert("name".into(), json!(name.trim_end_matches(".json")));
                list.push(Value::Object(panel));
            }
        }
        list.sort_by(|a, b| b["updatedAt"].as_str().unwrap_or("").cmp(a["updatedAt"].as_str().unwrap_or("")));
        ok(json!({ "ok": true, "bundleId": bundle, "panels": list }))
    }

    /// Server-sent events: `backlog` once, then a `lines` event per batch, and `status` when the
    /// app stops or starts answering. Long-polls the app's `_logs afterSequence=` so a line shows
    /// up within a fraction of a second. Ends when the receiver is dropped.
    pub fn stream_logs(self: &Arc<Self>, id: String, level: String, contains: Option<String>) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(64);
        let this = self.clone();
        tokio::spawn(async move {
            let send = |event: &str, data: Value| { let tx = tx.clone(); let msg = format!("event: {event}\ndata: {data}\n\n"); async move { tx.send(msg).await.is_ok() } };
            let (mut cursor, mut first, mut reachable): (i64, bool, Option<bool>) = (0, true, None);
            let mut filter = json!({ "level": level, "format": "json" });
            if let Some(c) = &contains {
                filter["contains"] = json!(c);
            }
            let heartbeat_tx = tx.clone();
            let heartbeat = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(15)).await;
                    if heartbeat_tx.send(": keep-alive\n\n".into()).await.is_err() { break; }
                }
            });
            'outer: while !tx.is_closed() {
                let (app, token) = match this.find(&id).await {
                    Ok(f) => f,
                    Err(r) => {
                        if reachable != Some(false) {
                            let message = r.body.as_ref().and_then(|b| b["error"]["message"].as_str()).unwrap_or("").to_string();
                            if !send("status", json!({ "state": "gone", "message": message })).await { break 'outer; }
                        }
                        reachable = Some(false);
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        continue;
                    }
                };
                let mut args = filter.clone();
                if first {
                    args["afterSequence"] = json!(0);
                    args["limit"] = json!(300);
                } else {
                    args["afterSequence"] = json!(cursor);
                    args["waitSeconds"] = json!(20);
                    args["limit"] = json!(1000);
                }
                let reply = match this.request(&app, &token, Some(json!({ "name": "_logs", "arguments": args })), "/_call", CALL_TIMEOUT).await {
                    Ok(r) => r,
                    Err(e) => {
                        if reachable != Some(false) && !send("status", json!({ "state": "unreachable", "message": e.to_string() })).await { break 'outer; }
                        reachable = Some(false);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                if reachable != Some(true) {
                    if !send("status", json!({ "state": "live" })).await { break 'outer; }
                    reachable = Some(true);
                }
                let body = reply.body.clone().unwrap_or(Value::Null);
                let data = body.get("data").filter(|d| !d.is_null());
                if body["ok"].as_bool() != Some(true) || data.is_none() {
                    let message = body["error"]["message"].as_str().map(String::from).unwrap_or_else(|| format!("HTTP {}", reply.status));
                    if !send("status", json!({ "state": "error", "message": message })).await { break 'outer; }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                let data = data.unwrap();
                let last = data["lastSequence"].as_i64().unwrap_or(cursor);
                // A smaller cursor than ours means the app relaunched: start over.
                if !first && last < cursor {
                    cursor = 0;
                    first = true;
                    if !send("status", json!({ "state": "relaunched" })).await { break 'outer; }
                    continue;
                }
                cursor = last;
                let entries = data["entries"].clone();
                if first || entries.as_array().is_some_and(|e| !e.is_empty()) {
                    if !send(if first { "backlog" } else { "lines" }, json!({ "entries": entries, "truncated": data["truncated"] })).await { break 'outer; }
                }
                first = false;
            }
            heartbeat.abort();
        });
        rx
    }
}

fn valid_panel_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else { return false };
    let mut chars = stem.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && stem.len() <= 48 && stem.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A launch record's pid is live while it is the process streaming the app's log: `devicectl
/// --console`, or `apparition launch`, which serves quiet devices and `--jit` (the Python tool
/// knew only devicectl and treated those launches as ended until Bonjour recovered them).
fn pid_is_console_streamer(pid: Option<i64>) -> bool {
    let Some(pid) = pid.filter(|p| *p > 0 && *p <= i32::MAX as i64) else { return false };
    // SAFETY: signal 0 only checks that the process exists and can be signalled.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "command="]).output()
        .map(|o| { let c = String::from_utf8_lossy(&o.stdout); c.contains("devicectl") || c.contains("apparition launch") })
        .unwrap_or(false)
}
