//! What the Store hands to other devices and computers: the OTA page and its links
//! (`itms-services`), and the signed `.app.zip` downloads of Mac apps. Part of `server`, so it
//! shares the request and reply helpers.

use super::*;
use crate::altsource::{self, Outcome as AltOutcome};
use crate::macapps::{self, MacApp, MacBuild, Request as MacRequest};

const MAC_LINK_TTL_DAYS: u64 = 7;

/// The scheme and host the device used to reach this Store: behind `tailscale serve` that is
/// https and the ts.net name (`X-Forwarded-*`); a direct LAN hit falls back to the Host header.
/// Where the request's client reached the Store: what a proxy in front (`tailscale serve`) says,
/// else its Host. A request to loopback with no proxy in front (`apparition store`, the app, a
/// paired computer's channel) gets config.json's `publicUrl` when there is one, as links made
/// for it are for devices, which can't reach this computer's loopback.
pub(super) fn request_base(req: &Req, public_url: Option<&str>) -> String {
    let forwarded = req.header("x-forwarded-host");
    let host = req.header("host");
    let loopback = {
        let name = host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())).map_or(host, |(name, _)| name);
        ["127.0.0.1", "localhost", "[::1]"].contains(&name)
    };
    if forwarded.is_empty() && loopback && let Some(url) = public_url.filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
        return url.trim_end_matches('/').to_string();
    }
    let proto = match req.header("x-forwarded-proto") { "" => "http", p => p };
    let host = if forwarded.is_empty() { host } else { forwarded };
    format!("{proto}://{host}")
}

pub(super) fn xml_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&apos;").replace('"', "&quot;")
}

/// The manifest an `itms-services` install reads. It leaves out `platform-identifier`: visionOS
/// rejects Xcode-16-style manifests carrying `com.apple.platform.xros` (ASDErrorDomain 752), and
/// iOS never needed the key.
pub(super) fn ota_manifest(ipa_url: &str, bundle_id: &str, version: &str, title: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>items</key>
  <array>
    <dict>
      <key>assets</key>
      <array>
        <dict>
          <key>kind</key><string>software-package</string>
          <key>url</key><string>{}</string>
        </dict>
      </array>
      <key>metadata</key>
      <dict>
        <key>bundle-identifier</key><string>{}</string>
        <key>bundle-version</key><string>{}</string>
        <key>kind</key><string>software</string>
        <key>title</key><string>{}</string>
      </dict>
    </dict>
  </array>
</dict>
</plist>
"#, xml_esc(ipa_url), xml_esc(bundle_id), xml_esc(version), xml_esc(title))
}

/// A file as a streamed body: its length, and the body.
pub(super) fn file_body(path: &Path) -> Option<(u64, Body)> {
    let file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    Some((length, Body::from_stream(tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file)))))
}

fn text_reply(status: u16, message: &str, headers: &[(&'static str, String)]) -> Response {
    let mut res = reply(status, "text/plain; charset=utf-8", format!("{message}\n"));
    res = with_header(res, "x-content-type-options", "nosniff");
    res = with_header(res, "cache-control", "no-store");
    for (name, value) in headers {
        res = with_header(res, name, value);
    }
    res
}

/// A 307 back to the same URL with the wait counter up one: the client asks again.
pub(super) fn keep_waiting(path: &str, query: &HashMap<String, String>, job: u64) -> Response {
    let mut next: Vec<(String, String)> = query.iter().filter(|(k, _)| *k != "w").map(|(k, v)| (k.clone(), v.clone())).collect();
    next.sort();
    let waited = query.get("w").and_then(|w| w.parse::<u64>().ok()).unwrap_or(0) + 1;
    next.push(("w".into(), waited.to_string()));
    let query = next.iter().map(|(k, v)| format!("{}={}", encode_uri_component(k), encode_uri_component(v))).collect::<Vec<_>>().join("&");
    text_reply(307, &format!("still building (build {job}); follow the redirect to keep waiting"),
        &[("location", format!("{path}?{query}")), ("retry-after", "1".into())])
}

/// How long a request waits for its build before the 307: `?hold=` (1 to 3600 s), else `default`.
pub(super) fn hold_seconds(query: &HashMap<String, String>, default: u64) -> u64 {
    query.get("hold").and_then(|h| h.parse::<f64>().ok()).filter(|h| *h > 0.0).map(|h| h as u64).unwrap_or(default).clamp(1, 3600)
}

impl App {
    /// The base of links made for this request (`request_base`).
    pub(super) fn base(&self, req: &Req) -> String {
        let config = self.config();
        request_base(req, config.raw.get("publicUrl").and_then(Value::as_str))
    }

    /// The OTA copy the state lists under this slug.
    fn ota_entry(&self, slug: &str) -> Option<Value> {
        self.state.field("ota").as_object()?.values().find(|e| e["slug"].as_str() == Some(slug)).cloned()
    }

    fn signed_ota_url(&self, req: &Req, pathname: &str) -> String {
        format!("{}{pathname}?t={}", self.base(req), self.auth.sign_path(pathname, 3600))
    }

    /// `/ota`, `/ota/<slug>/manifest.plist` and `/ota/<slug>.ipa` (the last two arrive with a signed
    /// link or a session), and `/api/ota/<slug>/link`.
    pub(super) fn ota_routes(&self, req: &Req, path: &str) -> Option<Response> {
        let get = req.method == Method::GET;
        let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let slug_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        match segments.as_slice() {
            ["api", "ota", slug, "link"] if get && slug_ok(slug) => Some(match self.ota_entry(slug) {
                None => json_reply(404, json!({ "error": "unknown ipa" })),
                Some(e) => json_reply(200, json!({ "manifestUrl": self.signed_ota_url(req, &format!("/ota/{}/manifest.plist", e["slug"].as_str().unwrap_or(""))) })),
            }),
            ["ota"] if get => Some(with_header(reply(200, "text/html; charset=utf-8", self.ota_page(req)), "cache-control", "no-store")),
            ["ota", slug, "manifest.plist"] if get && slug_ok(slug) => Some(match self.ota_entry(slug) {
                None => json_reply(404, json!({ "error": "unknown ipa" })),
                Some(e) => {
                    let slug = e["slug"].as_str().unwrap_or("");
                    let plist = ota_manifest(&self.signed_ota_url(req, &format!("/ota/{slug}.ipa")), e["bundleId"].as_str().unwrap_or(""),
                        e["version"].as_str().unwrap_or("1.0"), e["name"].as_str().unwrap_or(""));
                    with_header(reply(200, "text/xml; charset=utf-8", plist), "cache-control", "no-store")
                }
            }),
            ["ota", file] if get && file.strip_suffix(".ipa").is_some_and(slug_ok) => {
                let entry = self.ota_entry(file.strip_suffix(".ipa").unwrap());
                let path = entry.as_ref().map(|e| self.queue.ota.join(format!("{}.ipa", e["slug"].as_str().unwrap_or(""))));
                Some(match path.and_then(|p| file_body(&p)) {
                    Some((length, body)) => with_header(reply(200, "application/octet-stream", body), "content-length", &length.to_string()),
                    None => json_reply(404, json!({ "error": "unknown ipa" })),
                })
            }
            _ => None,
        }
    }

    fn ota_page(&self, req: &Req) -> String {
        let base = self.base(req);
        let https = base.starts_with("https://");
        let ota = self.state.field("ota");
        let mut entries: Vec<(&String, &Value)> = ota.as_object().into_iter().flatten().collect();
        entries.sort_by_key(|(_, e)| std::cmp::Reverse(e["at"].as_u64().unwrap_or(0)));
        let rows: Vec<String> = entries.iter().map(|(key, e)| {
            let bundle_id = e["bundleId"].as_str().map(String::from).unwrap_or_else(|| key.split('|').next().unwrap_or("").to_string());
            let platform = key.split('|').nth(1).unwrap_or("");
            let slug = e["slug"].as_str().unwrap_or("");
            let manifest_url = self.signed_ota_url(req, &format!("/ota/{slug}/manifest.plist"));
            let itms = format!("itms-services://?action=download-manifest&url={}", encode_uri_component(&manifest_url));
            let mb = e["size"].as_f64().unwrap_or(0.0) / 1048576.0;
            let when = e["at"].as_u64().map(|ms| rfc3339_minutes(ms)).unwrap_or_default();
            let branch = e["branch"].as_str().filter(|b| !b.is_empty()).map(|b| format!("{}@", xml_esc(b))).unwrap_or_default();
            format!("<tr>\n      <td><b>{}</b><br><small>{}</small></td>\n      <td>{} · {}<br><small>{branch}{}{}</small></td>\n      <td>{mb:.1} MB<br><small>{when}</small></td>\n      <td>{}</td>\n    </tr>",
                xml_esc(e["name"].as_str().unwrap_or("")), xml_esc(&bundle_id), xml_esc(e["repo"].as_str().unwrap_or("")), xml_esc(platform),
                xml_esc(e["commit"].as_str().unwrap_or("?")), if e["backfilled"] == Value::Bool(true) { " · pre-existing" } else { "" },
                if https { format!("<a class=\"btn\" href=\"{}\">Install</a>", xml_esc(&itms)) } else { "—".into() })
        }).collect();
        let warn = if https { String::new() } else { "<div class=\"warn\">Opened over plain HTTP — itms-services needs a trusted HTTPS origin. Use the tailnet URL instead.</div>".into() };
        format!(r#"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Apparition Store OTA</title>
<style>
  body{{font:15px -apple-system,system-ui;background:#111;color:#eee;margin:2em auto;max-width:720px;padding:0 1em}}
  table{{border-collapse:collapse;width:100%}}
  td,th{{padding:.6em .5em;border-bottom:1px solid #333;text-align:left;vertical-align:top}}
  small{{color:#888}}
  .btn{{display:inline-block;background:#0a84ff;color:#fff;text-decoration:none;padding:.45em 1.1em;border-radius:8px}}
  .warn{{background:#3a2a00;border:1px solid #7a5c00;padding:.7em 1em;border-radius:8px;margin-bottom:1em}}
</style></head><body>
<h2>OTA install</h2>
{warn}
<table><tr><th>App</th><th>Source</th><th>IPA</th><th></th></tr>
{}
</table>
<p><small>Latest published build per app/platform. Install requires this device's UDID in the app's provisioning profile.</small></p>
{}
</body></html>"#, rows.join("\n"), self.alt_source_block(req))
    }


    // ---------- AltSource ----------

    pub(super) fn alt_source_on(&self) -> bool {
        self.config().raw.pointer("/altSource/enabled") == Some(&Value::Bool(true))
    }

    fn alt_cfg(&self) -> Value {
        self.config().raw.get("altSource").cloned().unwrap_or(Value::Null)
    }

    /// The feed URL is added to a client once, so it carries a lasting token: `altSource.token`
    /// when set, else one derived from `data/secret`.
    fn alt_token(&self) -> String {
        match self.alt_cfg().get("token").and_then(Value::as_str) {
            Some(t) if t.len() >= 16 => t.to_string(),
            _ => self.auth.derive("altsource-feed"),
        }
    }

    fn alt_url(&self, req: &Req, platform: Option<&str>) -> String {
        format!("{}/altsource/{}.json?k={}", self.base(req), platform.map_or("source".to_string(), encode_uri_component), encode_uri_component(&self.alt_token()))
    }

    fn alt_links(&self, req: &Req) -> Value {
        let one = |platform: Option<&str>, url: String| json!({
            "platform": platform, "altstore": format!("altstore://source?url={}", encode_uri_component(&url)),
            "sidestore": format!("sidestore://source?url={}", encode_uri_component(&url)), "url": url,
        });
        let config = self.config();
        json!({
            "sources": altsource::source_platforms(&config.raw).iter().map(|p| one(Some(p), self.alt_url(req, Some(p)))).collect::<Vec<_>>(),
            "combined": one(None, self.alt_url(req, None)),
        })
    }

    /// Links inside the feed are signed per path, good for `linkTtlDays` (the feed is fetched on
    /// every refresh, so they never get old in practice).
    fn alt_link(&self, req: &Req, pathname: &str) -> String {
        let days = self.alt_cfg().get("linkTtlDays").and_then(Value::as_f64).filter(|d| *d > 0.0).unwrap_or(7.0);
        format!("{}{pathname}?t={}", self.base(req), self.auth.sign_path(pathname, (days * 86400.0) as u64))
    }

    /// The OTA page's AltStore section (empty while the mode is off).
    pub(super) fn alt_source_block(&self, req: &Req) -> String {
        if !self.alt_source_on() {
            return String::new();
        }
        let links = self.alt_links(req);
        let block = |label: &str, s: &Value| format!("<h4>{}</h4>\n<p><input readonly style=\"width:100%;background:#222;color:#eee;border:1px solid #444;padding:.4em;border-radius:6px\" value=\"{}\" onclick=\"this.select()\"></p>\n<p><a class=\"btn\" href=\"{}\">Add to AltStore</a> <a class=\"btn\" href=\"{}\">Add to SideStore</a></p>",
            xml_esc(label), xml_esc(s["url"].as_str().unwrap_or("")), xml_esc(s["altstore"].as_str().unwrap_or("")), xml_esc(s["sidestore"].as_str().unwrap_or("")));
        let sources: Vec<String> = links["sources"].as_array().into_iter().flatten().map(|s| block(s["platform"].as_str().unwrap_or(""), s)).collect();
        format!("<h3>AltStore sources</h3>\n<p><small>One per platform: add the one for this device. Every app is at the head of its source branch; a download builds that commit if it isn't built yet and answers when the build is done.</small></p>\n{}\n{}",
            sources.join("\n"), block("All platforms (for clients that filter by the app's platform key)", &links["combined"]))
    }

    /// `GET /api/altsource`: whether it is on, the feed links, and what is building.
    pub(super) fn alt_source_api(&self, req: &Req) -> Response {
        if !self.alt_source_on() {
            return json_reply(200, json!({ "enabled": false }));
        }
        let mut body = self.alt_links(req);
        body["enabled"] = json!(true);
        body["building"] = json!(self.alt.building().iter().map(|b| json!({ "key": b.key, "jobId": b.job_id })).collect::<Vec<_>>());
        json_reply(200, body)
    }

    /// The store's own icon: the branding icon, else the page's touch icon.
    fn store_icon(&self) -> Option<(Vec<u8>, &'static str)> {
        let config = self.config();
        if let Some(file) = self.brand_icon_file(&config) {
            return Some((fs::read(&file).ok()?, image_type(file.to_str()?).unwrap_or("image/png")));
        }
        let bytes = match &self.settings.public_dir {
            Some(dir) => fs::read(dir.join("apple-touch-icon.png")).ok()?,
            None => crate::assets::get("apple-touch-icon.png")?.to_vec(),
        };
        Some((bytes, "image/png"))
    }

    fn icon_reply(bytes: Vec<u8>, content_type: &str) -> Response {
        let res = reply(200, content_type, bytes);
        let res = with_header(res, "x-content-type-options", "nosniff");
        with_header(res, "content-security-policy", "default-src 'none'; style-src 'unsafe-inline'")
    }

    /// `/altsource/*`, all of it gone while the mode is off. Clients have no cookie: the feed takes
    /// its lasting token, the links in it their own signatures. The IPA waits on its build, so it
    /// is handed back as an async route.
    pub(super) fn alt_source_routes(&self, req: &Req) -> Option<Result<Response, (String, String, String)>> {
        let rest = req.path.strip_prefix("/altsource/")?;
        if req.method != Method::GET {
            return None;
        }
        if !self.alt_source_on() {
            return Some(Ok(json_reply(404, json!({ "error": "not found" }))));
        }
        let name_ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if let Some(name) = rest.strip_suffix(".json").filter(|n| name_ok(n)) {
            let given = req.query.get("k").map_or("", String::as_str);
            let want = self.alt_token();
            if !constant_eq(given, &want) {
                return Some(Ok(json_reply(403, json!({ "error": "wrong or missing feed token" }))));
            }
            let config = self.config();
            let mut platform = None;
            if name != "source" {
                platform = altsource::source_platforms(&config.raw).into_iter().find(|p| p.eq_ignore_ascii_case(name));
                if platform.is_none() {
                    return Some(Ok(json_reply(404, json!({ "error": format!("no apps for platform {name}"), "platforms": altsource::source_platforms(&config.raw) }))));
                }
            }
            let title = self.brand().title;
            let doc = self.alt.source(&title, &self.alt_cfg(), platform.as_deref(), &|path| self.alt_link(req, path));
            let res = reply(200, "application/json; charset=utf-8", serde_json::to_string_pretty(&doc).unwrap_or_default());
            return Some(Ok(with_header(res, "cache-control", "no-store")));
        }
        if !req.query.get("t").is_some_and(|t| self.auth.verify_path(&req.path, t)) {
            return Some(Ok(json_reply(403, json!({ "error": "link expired or not signed" }))));
        }
        if rest == "icon.png" {
            return Some(Ok(match self.store_icon() { Some((bytes, kind)) => Self::icon_reply(bytes, kind), None => json_reply(404, json!({ "error": "not found" })) }));
        }
        let parts: Vec<&str> = rest.split('/').collect();
        match parts.as_slice() {
            [slug, "icon.png"] if name_ok(slug) => {
                let own = self.alt.icon_file(slug).and_then(|f| fs::read(f).ok()).map(|b| (b, "image/png"));
                Some(Ok(match own.or_else(|| self.store_icon()) { Some((bytes, kind)) => Self::icon_reply(bytes, kind), None => json_reply(404, json!({ "error": "not found" })) }))
            }
            [slug, file] if name_ok(slug) && file.strip_suffix(".ipa").is_some_and(|c| (4..=40).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))) => {
                Some(Err((slug.to_string(), file.strip_suffix(".ipa").unwrap().to_string(), req.path.clone())))
            }
            _ => Some(Ok(json_reply(404, json!({ "error": "not found" })))),
        }
    }

    /// A clean LAN or OTA build already stamped with exactly this commit is the same app.
    fn adopt_ota_ipa(&self, app: &altsource::SourceApp, commit: &str) -> Option<PathBuf> {
        let ota = self.state.field("ota");
        let entry = ota.get(format!("{}|{}", app.bundle_id, app.platform))?;
        if entry["backfilled"] == Value::Bool(true) || entry["buildNo"].as_str() != Some(commit) {
            return None;
        }
        let file = self.queue.ota.join(format!("{}.ipa", entry["slug"].as_str()?));
        file.is_file().then_some(file)
    }

    /// The long poll. Built: the IPA. Building: hold the request up to `hold` seconds (`?hold=` or
    /// `altSource.holdSeconds`, default 50: under URLSession's 60 s idle timeout), then 307 back to
    /// the same URL so the client asks again. Nothing is sent while holding, since any byte would
    /// commit the response to the IPA.
    pub(super) async fn serve_alt_ipa(self: &Arc<Self>, path: String, query: HashMap<String, String>, slug: String, commit: String) -> Response {
        let default = self.alt_cfg().get("holdSeconds").and_then(Value::as_f64).filter(|h| *h > 0.0).map_or(50, |h| h as u64);
        let hold = hold_seconds(&query, default);
        let (app, s, c) = (self.clone(), slug.clone(), commit.clone());
        let mut result = tokio::task::spawn_blocking(move || {
            let adopt = |a: &altsource::SourceApp, commit: &str| app.adopt_ota_ipa(a, commit);
            app.alt.request(&s, &c, Some(&adopt))
        }).await.unwrap_or(AltOutcome::Failed { status: 500, error: "internal error".into() });
        if let AltOutcome::Building(job) = result {
            tokio::select! {
                _ = self.queue.wait(job) => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(hold)) => {}
            }
            result = self.alt.outcome(&slug, &commit, job);
            if let AltOutcome::Building(job) = result {
                return keep_waiting(&path, &query, job);
            }
        }
        match result {
            AltOutcome::Ready(entry) => {
                let Some((length, body)) = file_body(&self.alt.file(&entry)) else { return text_reply(500, "the IPA is gone", &[]) };
                let name = altsource::slug_of(&format!("{}-{}", if entry.name.is_empty() { &slug } else { &entry.name }, if entry.build_no.is_empty() { &commit } else { &entry.build_no }));
                let res = with_header(reply(200, "application/octet-stream", body), "content-length", &length.to_string());
                let res = with_header(res, "content-disposition", &format!("attachment; filename=\"{name}.ipa\""));
                with_header(res, "cache-control", "no-store")
            }
            AltOutcome::Failed { status, error } => text_reply(status, &error, &[]),
            AltOutcome::Building(_) => text_reply(500, "no IPA", &[]),
        }
    }

    // ---------- Mac apps ----------

    /// The branch a Mac download builds: one the repo offers, or the one its checkout has out
    /// (`current`); anything else is refused.
    fn mac_branch(&self, app: &MacApp, wanted: Option<&str>) -> Result<String, (u16, String)> {
        let config = self.config();
        let offered: Vec<String> = config.raw.get("repos").and_then(Value::as_array).into_iter().flatten()
            .find(|r| r["name"].as_str() == Some(&app.repo)).and_then(|r| r["branches"].as_array())
            .into_iter().flatten().filter_map(Value::as_str).map(String::from).collect();
        let branch = wanted.filter(|b| !b.is_empty()).unwrap_or(&app.branch);
        if branch == "current" {
            let out = crate::git::run(Path::new(&app.path), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
            if out.is_empty() || out == "HEAD" {
                return Err((409, format!("{}'s checkout has no branch checked out; pick one", app.repo)));
            }
            return Ok(out);
        }
        if branch == app.branch || offered.iter().any(|b| b == branch) {
            return Ok(branch.to_string());
        }
        Err((400, format!("{} does not offer branch {branch}", app.repo)))
    }

    /// The commit at a branch's tip: `(short sha, date, subject)`.
    pub(super) fn branch_head(path: &str, branch: &str) -> Result<(String, String, String), String> {
        let out = crate::git::run(Path::new(path), &["log", "-1", "--format=%h%x09%cI%x09%s", &format!("refs/heads/{branch}")])
            .ok_or_else(|| format!("no branch {branch}"))?;
        let mut parts = out.splitn(3, '\t');
        Ok((parts.next().unwrap_or("").into(), parts.next().unwrap_or("").into(), parts.next().unwrap_or("").into()))
    }

    fn mac_zip_url(slug: &str, commit: &str, branch: &str) -> String {
        format!("/mac/{slug}/{commit}.app.zip?branch={}", encode_uri_component(branch))
    }

    /// A link for a Mac with no session: `latest` follows the branch, so it stays good for
    /// seven days. The branch is part of what is signed.
    fn signed_mac_latest(&self, req: &Req, slug: &str, branch: &str) -> String {
        let signed = format!("/mac/{slug}/latest.app.zip?branch={}", encode_uri_component(branch));
        format!("{}{signed}&t={}", self.base(req), self.auth.sign_path(&signed, MAC_LINK_TTL_DAYS * 86400))
    }

    /// Whether a request for `/mac/<slug>/<latest|commit>.app.zip` carries a valid signed link.
    pub(super) fn mac_link_signed(&self, req: &Req) -> Option<(String, String, bool)> {
        if req.method != Method::GET {
            return None;
        }
        let rest = req.path.strip_prefix("/mac/")?;
        let (slug, file) = rest.split_once('/')?;
        let which = file.strip_suffix(".app.zip")?;
        let slug_ok = !slug.is_empty() && slug.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        let which_ok = which == "latest" || ((4..=40).contains(&which.len()) && which.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        if !slug_ok || !which_ok {
            return None;
        }
        let signed = req.query.get("t").is_some_and(|t| {
            let covered = if which == "latest" {
                format!("{}?branch={}", req.path, encode_uri_component(req.query.get("branch").map_or("", String::as_str)))
            } else {
                req.path.clone()
            };
            self.auth.verify_path(&covered, t)
        });
        Some((slug.to_string(), which.to_string(), signed))
    }

    /// `/api/mac/<slug>/build` (the card's button) and `/api/mac/<slug>/link`.
    pub(super) fn mac_api(&self, req: &Req, segments: &[&str]) -> Option<Response> {
        let ["api", "mac", slug, action @ ("build" | "link")] = segments else { return None };
        let Some(app) = self.mac.app_by_slug(slug) else { return Some(json_reply(404, json!({ "error": "unknown Mac app" }))) };
        let body = if *action == "build" && req.method == Method::POST { Some(req.json_body()) } else { None };
        let wanted = match &body { Some(b) => b.get("branch").and_then(Value::as_str).map(String::from), None => req.query.get("branch").cloned() };
        let branch = match self.mac_branch(&app, wanted.as_deref()) { Ok(b) => b, Err((status, message)) => return Some(json_reply(status, json!({ "error": message }))) };
        if *action == "link" && req.method == Method::GET {
            let link = self.signed_mac_latest(req, &app.slug, &branch);
            let zip = format!("{}.app.zip", app.name.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect::<String>());
            return Some(json_reply(200, json!({ "url": link, "days": MAC_LINK_TTL_DAYS,
                "curl": format!("curl -fL --max-redirs 100 -o '{zip}' '{link}&hold=600' && ditto -x -k '{zip}' .") })));
        }
        if body.is_none() {
            return Some(json_reply(405, json!({ "error": "POST to build" })));
        }
        let (commit, _, _) = match Self::branch_head(&app.path, &branch) { Ok(h) => h, Err(e) => return Some(json_reply(500, json!({ "error": e }))) };
        Some(match self.mac.request(&app.slug, &commit, Some(&branch)) {
            MacRequest::Ready(entry) => json_reply(200, json!({ "ready": true, "url": Self::mac_zip_url(&app.slug, &entry.commit, &branch), "entry": entry_json(&entry), "branch": branch })),
            MacRequest::Building(job) => json_reply(200, json!({ "ready": false, "jobId": job, "commit": commit, "branch": branch })),
            MacRequest::Failed { status, error } => json_reply(status, json!({ "error": error })),
        })
    }

    /// `/mac/<slug>/latest.app.zip`: a redirect to the head's zip. The zip itself holds the
    /// request while it builds (`serve_mac_zip`).
    pub(super) fn mac_latest(&self, req: &Req, slug: &str, signed: bool) -> Response {
        let Some(app) = self.mac.app_by_slug(slug) else { return text_reply(404, "unknown Mac app", &[]) };
        let branch = match self.mac_branch(&app, req.query.get("branch").map(String::as_str)) {
            Ok(b) => b,
            Err((status, message)) => return text_reply(status, &message, &[]),
        };
        let (commit, _, _) = match Self::branch_head(&app.path, &branch) { Ok(h) => h, Err(e) => return json_reply(500, json!({ "error": e })) };
        let mut location = Self::mac_zip_url(slug, &commit, &branch);
        if let Some(hold) = req.query.get("hold") {
            location.push_str(&format!("&hold={}", encode_uri_component(hold)));
        }
        if signed {
            location.push_str(&format!("&t={}", self.auth.sign_path(&format!("/mac/{slug}/{commit}.app.zip"), MAC_LINK_TTL_DAYS * 86400)));
        }
        with_header(with_header(reply(302, "text/plain", ""), "location", &location), "cache-control", "no-store")
    }

    pub(super) async fn serve_mac_zip(self: &Arc<Self>, path: String, query: HashMap<String, String>, slug: String, commit: String) -> Response {
        let Some(app) = self.mac.app_by_slug(&slug) else { return text_reply(404, "unknown Mac app", &[]) };
        let branch = match self.mac_branch(&app, query.get("branch").map(String::as_str)) {
            Ok(b) => b,
            Err((status, message)) => return text_reply(status, &message, &[]),
        };
        let hold = hold_seconds(&query, 50);
        let mut result = self.mac.request(&slug, &commit, Some(&branch));
        if let MacRequest::Building(job) = result {
            tokio::select! {
                _ = self.queue.wait(job) => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(hold)) => {}
            }
            self.mac.sweep();
            result = self.mac.outcome(&slug, &commit, job);
            if let MacRequest::Building(job) = result {
                return keep_waiting(&path, &query, job);
            }
        }
        match result {
            MacRequest::Ready(entry) => {
                let file = self.mac.file(&entry);
                let Some((length, body)) = file_body(&file) else { return text_reply(500, "the zip is gone", &[]) };
                let name = macapps::slug_of(&format!("{}-{}", entry.bundle.trim_end_matches(".app"), if entry.build_no.is_empty() { &entry.commit } else { &entry.build_no }));
                let res = reply(200, "application/zip", body);
                let res = with_header(res, "content-length", &length.to_string());
                let res = with_header(res, "content-disposition", &format!("attachment; filename=\"{name}.app.zip\""));
                with_header(res, "cache-control", "no-store")
            }
            MacRequest::Failed { status, error } => text_reply(status, &error, &[]),
            MacRequest::Building(_) => text_reply(500, "no app", &[]),
        }
    }
}

fn entry_json(entry: &MacBuild) -> Value {
    let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
    if let Some(o) = value.as_object_mut() { o.remove("file"); }
    value
}

/// `2026-11-04 12:30` (UTC), as the OTA page shows an entry's age.
fn rfc3339_minutes(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86400, secs % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let (y, doy) = (yoe + era * 400, doe - (365 * yoe + yoe / 4 - yoe / 100));
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ota_manifest_escapes_what_it_is_given_and_names_no_platform() {
        let plist = ota_manifest("https://h/ota/a.ipa?t=1&x=2", "com.example.a", "1.0", "A & <B>");
        assert!(plist.contains("<string>https://h/ota/a.ipa?t=1&amp;x=2</string>"));
        assert!(plist.contains("<string>A &amp; &lt;B&gt;</string>"));
        assert!(!plist.contains("platform-identifier"));
    }

    #[test]
    fn dates_are_shown_in_utc() {
        assert_eq!(rfc3339_minutes(0), "1970-01-01 00:00");
        assert_eq!(rfc3339_minutes(1_793_795_400_000), "2026-11-04 12:30");
    }

    #[test]
    fn a_hold_is_clamped_and_defaults() {
        let q = |h: &str| HashMap::from([("hold".to_string(), h.to_string())]);
        assert_eq!(hold_seconds(&HashMap::new(), 50), 50);
        assert_eq!(hold_seconds(&q("600"), 50), 600);
        assert_eq!(hold_seconds(&q("99999"), 50), 3600);
        assert_eq!(hold_seconds(&q("nope"), 50), 50);
    }
}
