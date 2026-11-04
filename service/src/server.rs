//! The HTTP front of the service: the web gate, login, static files and the bearer API that
//! native clients and the hosting app use. It mirrors the dispatch order of `store/server.js`
//! so the two answer the same way while both exist. Routes not ported yet answer 501.

use crate::{
    auth::{Auth, COOKIE, Login, cookie_value, hash_password, system_ms},
    config::Config,
    artifacts,
    state::{State as StoreState, hostname},
    traces,
};
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
use subtle::ConstantTimeEq;

const NOT_YET: &str = "Not available in the Rust service yet.";
const WEB_UI_OFF: &str = "The web UI is off. Turn it on in Apparition: Developer › Devices & signing › Web UI and other devices.";
const PUBLIC_FILES: [&str; 6] = ["/login.html", "/favicon.svg", "/favicon.ico", "/apple-touch-icon.png", "/mask-icon.svg", "/branding/icon"];
const SERVICE_PREFIX: &str = "/api/service/v1";
/// Routes the native clients may call; the rest of the prefix is 404, ported or not.
const SERVICE_ROUTES: [(&str, &str); 19] = [
    ("GET", "/state"), ("GET", "/artifacts"), ("POST", "/build"), ("POST", "/inject"), ("POST", "/installs"),
    ("POST", "/projects"), ("POST", "/projects/discover"), ("GET", "/resign/options"), ("POST", "/resign"),
    ("GET", "/builds/*/log"), ("GET", "/artifacts/*/manifest"), ("POST", "/builds/*/cancel"), ("GET", "/traces"),
    ("GET", "/traces/*/files"), ("GET", "/crashlogs"), ("GET", "/crashlogs/*/ips"), ("GET", "/debug/apps"),
    ("GET", "/debug/*/help"), ("POST", "/debug/*/call"),
];

pub struct Settings {
    pub config_path: PathBuf,
    pub data_dir: PathBuf,
    /// `store/public`, read at run time until the cutover embeds it.
    pub public_dir: PathBuf,
    /// The hosting app's token (43 characters). Present means hosted.
    pub host_token: Option<String>,
    /// Where `build-and-sign` records trace signing keys (`traces::default_key_dir()`).
    pub key_dir: PathBuf,
}

pub struct App {
    settings: Settings,
    auth: Auth,
    state: StoreState,
    config: Mutex<(Option<SystemTime>, Arc<Config>)>,
    host_sessions: Mutex<HashSet<String>>,
    host_codes: Mutex<HashMap<String, u64>>,
}

pub fn router(settings: Settings) -> std::io::Result<Router> {
    let auth = Auth::open(&settings.data_dir)?;
    let state = StoreState::load(&settings.data_dir);
    let app = Arc::new(App {
        settings,
        auth,
        state,
        config: Mutex::new((None, Arc::new(Config::default()))),
        host_sessions: Mutex::default(),
        host_codes: Mutex::default(),
    });
    Ok(Router::new().fallback(handle).with_state(app))
}

struct Req {
    method: Method,
    path: String,
    query: HashMap<String, String>,
    headers: HeaderMap,
    body: Vec<u8>,
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
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
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
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
    res
}

fn json_reply(status: u16, value: Value) -> Response {
    reply(status, "application/json", value.to_string())
}

fn text(status: u16, message: &str) -> Response {
    reply(status, "text/plain; charset=utf-8", message.to_string())
}

fn with_header(mut res: Response, name: &'static str, value: &str) -> Response {
    res.headers_mut().append(name, HeaderValue::from_str(value).unwrap());
    res
}

fn redirect(location: &str) -> Response {
    let mut res = reply(302, "text/plain", "");
    res.headers_mut().insert(header::LOCATION, HeaderValue::from_str(location).unwrap());
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

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn image_type(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "svg" => "image/svg+xml", "png" => "image/png", "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif", "webp" => "image/webp", "ico" => "image/x-icon", _ => return None,
    })
}

struct Brand {
    title: String,
    name: String,
    mark: String,
    head: String,
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
            (None, hosted) => !hosted,
        }
    }

    fn brand_icon_file(&self, config: &Config) -> Option<PathBuf> {
        let icon = config.branding.as_ref()?.icon.as_deref()?;
        image_type(icon)?;
        let icon = match (icon.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => PathBuf::from(icon),
        };
        let file = self.settings.config_path.parent()?.join(icon);
        file.is_file().then_some(file)
    }

    fn brand(&self) -> Brand {
        let config = self.config();
        let b = config.branding.as_ref();
        let title = b.and_then(|b| b.title.clone().filter(|t| !t.is_empty()))
            .or_else(|| b.and_then(|b| b.org.clone().filter(|o| !o.is_empty())).map(|o| format!("{o}'s Apparition Store")))
            .unwrap_or_else(|| "Apparition Store".into());
        let name = match (b.and_then(|b| b.title.as_deref()).filter(|t| !t.is_empty()), b.and_then(|b| b.org.as_deref()).filter(|o| !o.is_empty())) {
            (Some(t), _) => format!("<b>{}</b>", escape_html(t)),
            (None, Some(o)) => format!("<span class=\"org\">{}&#39;s</span> <b>Apparition Store</b>", escape_html(o)),
            _ => "<b>Apparition Store</b>".into(),
        };
        let icon = b.and_then(|b| b.icon.as_deref()).map(str::trim).unwrap_or("");
        let (mark, head);
        if let Some(file) = self.brand_icon_file(&config) {
            let version = fs::metadata(&file).and_then(|m| m.modified()).ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis());
            let href = format!("/branding/icon?v={version}");
            mark = format!("<img class=\"brandicon\" src=\"{href}\" alt=\"\">");
            head = format!("<link rel=\"icon\" href=\"{href}\"><link rel=\"apple-touch-icon\" href=\"{href}\">");
        } else if !icon.is_empty() {
            mark = format!("<span class=\"brandicon\">{}</span>", escape_html(icon));
            let svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\"><text x=\"50\" y=\"50\" font-size=\"80\" text-anchor=\"middle\" dominant-baseline=\"central\">{}</text></svg>", escape_html(icon));
            head = format!("<link rel=\"icon\" href=\"data:image/svg+xml,{}\">", encode_uri_component(&svg));
        } else {
            mark = "<span class=\"tri\"></span>".into();
            head = "<link rel=\"icon\" href=\"/favicon.svg\" type=\"image/svg+xml\"><link rel=\"alternate icon\" href=\"/favicon.ico\" sizes=\"48x48 32x32 16x16\"><link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">".into();
        }
        Brand { title, name, mark, head }
    }

    /// `public/<rel>` with the brand placeholders filled in HTML, or None when it is not a file
    /// under `public/`.
    fn static_file(&self, rel: &str) -> Option<Response> {
        let rel = Path::new(rel);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return None;
        }
        let file = self.settings.public_dir.join(rel);
        if !file.is_file() {
            return None;
        }
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
        let content_type = match ext {
            "html" => "text/html; charset=utf-8", "js" => "text/javascript", "css" => "text/css",
            "svg" => "image/svg+xml", "png" => "image/png", "ico" => "image/x-icon", _ => "application/octet-stream",
        };
        let res = if ext == "html" {
            let brand = self.brand();
            let page = fs::read_to_string(&file).ok()?
                .replace("{{BRAND_TITLE}}", &escape_html(&brand.title))
                .replace("{{BRAND_NAME}}", &brand.name)
                .replace("{{BRAND_MARK}}", &brand.mark)
                .replace("{{BRAND_HEAD}}", &brand.head);
            with_header(reply(200, content_type, page), "cache-control", "no-store")
        } else {
            reply(200, content_type, fs::read(&file).ok()?)
        };
        Some(with_header(res, "x-content-type-options", "nosniff"))
    }

    /// Rewrites `config.json` with `change`, atomically, keeping every key it does not touch.
    fn update_config(&self, change: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Result<(), String> {
        let path = &self.settings.config_path;
        let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let object = value.as_object_mut().ok_or("config.json is not an object")?;
        change(object);
        let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        {
            use std::{io::Write, os::unix::fs::OpenOptionsExt};
            let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| e.to_string())?;
            file.write_all((serde_json::to_string_pretty(&value).map_err(|e| e.to_string())? + "\n").as_bytes()).map_err(|e| e.to_string())?;
        }
        fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        self.config.lock().unwrap().0 = None; // reread on the next request
        Ok(())
    }

    /// Kept builds: config `buildManifestDir` (relative to config.json), else the default store.
    fn build_store(&self) -> PathBuf {
        let config_dir = self.settings.config_path.parent().unwrap_or(Path::new("."));
        match self.config().raw.get("buildManifestDir").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            Some(dir) => config_dir.join(dir),
            None => artifacts::default_store(),
        }
    }

    fn traces_dir(&self) -> PathBuf {
        self.settings.data_dir.join("traces")
    }

    fn receive_trace(&self, req: &Req) -> Response {
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
        match traces::receive(&req.body, &up) {
            Ok((status, meta)) => {
                if status == 201 {
                    eprintln!("trace {} from {} ({} bytes)", meta["id"].as_str().unwrap_or(""), meta["app"]["bundleId"].as_str().unwrap_or(""), req.body.len());
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
        match segments.as_slice() {
            ["api", "state"] if get => Some(self.state_response()),
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
            ["api", "traces"] if get => Some(json_reply(200, json!({ "traces": traces::list(&traces) }))),
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

    fn state_response(&self) -> Response {
        json_reply(200, self.state.bundle(&self.config(), system_ms(), &hostname()))
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
            if let Some(hash) = hash {
                c.insert("passwordHash".into(), Value::String(hash));
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
        "capabilities": [], "authentication": "password-session" })
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let upload = parts.method == Method::POST && parts.uri.path() == "/api/traces";
    let limit = if upload { traces::MAX_UPLOAD } else { 1_000_000 };
    let body = match to_bytes(body, limit).await {
        Ok(b) => b.to_vec(),
        Err(_) if upload => return json_reply(413, json!({ "error": format!("trace is larger than {} MB", traces::MAX_UPLOAD >> 20) })),
        Err(_) => Vec::new(),
    };
    let query = parts.uri.query().unwrap_or("").split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (percent_decode(k), percent_decode(v))
    }).collect();
    let req = Req { method: parts.method, path: parts.uri.path().to_string(), query, headers: parts.headers, body };
    // blocking work (scrypt, file reads) stays off the async threads
    tokio::task::spawn_blocking(move || dispatch(&app, &req)).await.unwrap_or_else(|_| json_reply(500, json!({ "error": "internal error" })))
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
        || (req.method == Method::GET && ["/ota/", "/altsource/", "/mac/"].iter().any(|p| req.path.starts_with(p)));
    if !app.web_ui_on() && !machine_route && !app.host_session(req) {
        return text(404, WEB_UI_OFF);
    }

    // ---- reachable without a session ----
    let config = app.config();
    match (req.method.as_str(), req.path.as_str()) {
        ("POST", "/api/login") => {
            let Some(hash) = &config.password_hash else { return json_reply(200, json!({ "ok": true, "open": true })) };
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
        ("GET", "/branding/icon") => {
            let Some(file) = app.brand_icon_file(&config) else { return json_reply(404, json!({ "error": "no branding icon configured" })) };
            let Ok(bytes) = fs::read(&file) else { return json_reply(404, json!({ "error": "no branding icon configured" })) };
            let res = reply(200, image_type(file.to_str().unwrap_or("")).unwrap_or("application/octet-stream"), bytes);
            // an SVG served from here must not run script of its own
            let res = with_header(res, "content-security-policy", "default-src 'none'; style-src 'unsafe-inline'; img-src data:");
            return with_header(with_header(res, "x-content-type-options", "nosniff"), "cache-control", "no-cache");
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

    // ---- everything below needs a session (or a signed link, once those are ported) ----
    if req.method == Method::POST && req.path == "/api/traces" {
        return app.receive_trace(req);
    }
    if machine_route {
        return json_reply(501, json!({ "error": NOT_YET }));
    }
    if !app.signed_in(req) {
        if req.path.starts_with("/api/") {
            return json_reply(401, json!({ "error": "sign in first", "login": "/login" }));
        }
        let target = format!("{}{}", req.path, req.query_string());
        return redirect(&format!("/login?next={}", encode_uri_component(&target)));
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
    if req.path.starts_with("/api/") {
        return json_reply(501, json!({ "error": NOT_YET }));
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
            return Some(json_reply(409, json!({ "error": "Set a service password with node server.js set-password first." })));
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
    let suffix = req.path.strip_prefix(SERVICE_PREFIX).filter(|s| s.is_empty() || s.starts_with('/'))?;
    let bearer = req.header("authorization").strip_prefix("Bearer ").filter(|t| {
        t.len() == 43 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    let is_host = matches!((bearer, &app.settings.host_token), (Some(t), Some(h)) if constant_eq(t, h));
    let session = bearer.is_some_and(|t| config.password_hash.as_deref().is_some_and(|h| app.auth.check(t, h)));
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
        ("GET", "/artifacts") => app.api(req, "/api/artifacts").unwrap_or_else(unknown),
        ("GET", s) if s.starts_with("/artifacts/") => app.api(req, &format!("/api{s}")).unwrap_or_else(unknown),
        ("GET", "/traces") => app.api(req, "/api/traces").unwrap_or_else(unknown),
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
            if !is_host {
                app.auth.logout(bearer.unwrap_or(""));
            }
            json_reply(200, json!({ "ok": true }))
        }
        (method, path) if SERVICE_ROUTES.iter().any(|(m, pattern)| *m == method && route_matches(pattern, path)) => {
            json_reply(501, json!({ "error": NOT_YET }))
        }
        _ => unknown(),
    })
}

/// `*` stands for one path segment; a trailing `/files` also takes what follows it.
fn route_matches(pattern: &str, path: &str) -> bool {
    let (p, q): (Vec<&str>, Vec<&str>) = (pattern.split('/').collect(), path.split('/').collect());
    if pattern.ends_with("/files") && q.len() > p.len() {
        return p.iter().zip(&q).all(|(a, b)| *a == "*" && !b.is_empty() || a == b);
    }
    p.len() == q.len() && p.iter().zip(&q).all(|(a, b)| *a == "*" && !b.is_empty() || a == b)
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
