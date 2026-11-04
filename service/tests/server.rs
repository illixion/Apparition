//! The HTTP layer, driven in process.

use apparition_service::{auth::hash_password, server::{Settings, router}};
use axum::{Router, body::Body, http::{Request, Response, StatusCode}};
use serde_json::{Value, json};
use std::path::PathBuf;
use tower::ServiceExt;

const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNO12"; // 43 characters

struct Fixture {
    dir: tempfile::TempDir,
    app: Router,
}

fn fixture(config: Value, hosted: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    let public = dir.path().join("public");
    std::fs::create_dir(&public).unwrap();
    std::fs::write(public.join("index.html"), "<title>{{BRAND_TITLE}}</title>{{BRAND_NAME}}").unwrap();
    std::fs::write(public.join("login.html"), "<title>{{BRAND_TITLE}}</title>{{BRAND_MARK}}{{BRAND_HEAD}}").unwrap();
    std::fs::write(public.join("favicon.svg"), "<svg/>").unwrap();
    std::fs::write(dir.path().join("secret-notes.txt"), "no").unwrap();
    let app = router(Settings {
        host_token: hosted.then(|| TOKEN.to_string()),
        key_dir: dir.path().join("keys"),
        debug_state_dir: dir.path().join("debugtrace"),
        browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), public)
    })
    .unwrap();
    Fixture { dir, app }
}

async fn send(app: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> (StatusCode, Vec<(String, String)>, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        builder = builder.header("host", "localhost:8484");
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let res: Response<Body> = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let headers = res.headers().iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string())).collect();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn cookie_of(headers: &[(String, String)]) -> String {
    header(headers, "set-cookie").unwrap().split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn open_store_without_password_serves_pages_and_reports_auth() {
    let f = fixture(json!({ "port": 8484 }), false);
    let (status, _, body) = send(&f.app, "GET", "/api/auth", &[], None).await;
    assert_eq!((status, serde_json::from_str::<Value>(&body).unwrap()), (StatusCode::OK, json!({ "required": false, "signedIn": true })));
    let (status, headers, body) = send(&f.app, "GET", "/", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "<title>Apparition Store</title><b>Apparition Store</b>");
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    // Open only to pages naming this Mac's loopback: a site that points its own name at
    // 127.0.0.1 (DNS rebinding) is not signed in.
    for host in ["127.0.0.1:8484", "[::1]:8484", "localhost"] {
        let (_, _, body) = send(&f.app, "GET", "/api/auth", &[("host", host)], None).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["signedIn"], true, "{host}");
    }
    for host in ["evil.example:8484", "mac.local", "localhost.evil.example", "127.0.0.1.nip.io"] {
        let (_, _, body) = send(&f.app, "GET", "/api/auth", &[("host", host)], None).await;
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["signedIn"], false, "{host}");
    }
}

#[tokio::test]
async fn branding_fills_the_page_and_escapes() {
    let f = fixture(json!({ "branding": { "org": "Ixion <3", "icon": "🚀" } }), false);
    let (_, _, body) = send(&f.app, "GET", "/login.html", &[], None).await;
    assert!(body.starts_with("<title>Ixion &lt;3&#39;s Apparition Store</title><span class=\"brandicon\">🚀</span>"), "{body}");
    assert!(body.contains("data:image/svg+xml,%3Csvg"));
}

#[tokio::test]
async fn password_login_sets_a_session_that_the_gate_accepts() {
    let hash = hash_password("correct horse").unwrap();
    let f = fixture(json!({ "passwordHash": hash }), false);
    let (status, _, body) = send(&f.app, "GET", "/api/state", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, ..) = send(&f.app, "GET", "/api/state", &[("cookie", "store_session=bogus")], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, headers, _) = send(&f.app, "GET", "/", &[], None).await;
    assert_eq!((status, header(&headers, "location")), (StatusCode::FOUND, Some("/login?next=%2F")));
    let (status, ..) = send(&f.app, "POST", "/api/login", &[], Some(json!({ "password": "nope nope" }))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, headers, _) = send(&f.app, "POST", "/api/login", &[("x-forwarded-proto", "https")], Some(json!({ "password": "correct horse" }))).await;
    assert_eq!(status, StatusCode::OK);
    let set = header(&headers, "set-cookie").unwrap();
    assert!(set.contains("HttpOnly; SameSite=Strict; Max-Age=2592000; Secure"), "{set}");
    let cookie = cookie_of(&headers);
    let (_, _, body) = send(&f.app, "GET", "/api/auth", &[("cookie", &cookie)], None).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!({ "required": true, "signedIn": true }));
    let (status, ..) = send(&f.app, "GET", "/api/resign/options", &[("cookie", &cookie)], None).await;
    assert_eq!(status, StatusCode::OK, "signed in reaches the route");
    let (_, headers, _) = send(&f.app, "GET", "/login?next=/builds", &[("cookie", &cookie)], None).await;
    assert_eq!(header(&headers, "location"), Some("/builds"));
    let (_, headers, _) = send(&f.app, "GET", "/login?next=//evil.test", &[("cookie", &cookie)], None).await;
    assert_eq!(header(&headers, "location"), Some("/"), "an off-site next is dropped");
    send(&f.app, "POST", "/api/logout", &[("cookie", &cookie)], None).await;
    let (_, _, body) = send(&f.app, "GET", "/api/auth", &[("cookie", &cookie)], None).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["signedIn"], false);
}

#[tokio::test]
async fn cross_site_writes_are_refused() {
    let f = fixture(json!({}), false);
    let (status, ..) = send(&f.app, "POST", "/api/logout", &[("origin", "https://evil.test")], None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, ..) = send(&f.app, "POST", "/api/logout", &[("origin", "http://localhost:8484")], None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn static_files_stay_inside_public() {
    let f = fixture(json!({}), false);
    let (status, ..) = send(&f.app, "GET", "/favicon.svg", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    for uri in ["/../secret-notes.txt", "/%2e%2e/secret-notes.txt", "/..%2fsecret-notes.txt"] {
        let (status, ..) = send(&f.app, "GET", uri, &[], None).await;
        assert_ne!(status, StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn hosted_web_pages_are_off_until_the_app_turns_them_on() {
    let f = fixture(json!({ "future": { "keep": true } }), true);
    let (status, _, body) = send(&f.app, "GET", "/", &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.starts_with("The web UI is off."));
    let (status, ..) = send(&f.app, "GET", "/ota/x.ipa", &[], None).await;
    assert_eq!(status, StatusCode::FOUND, "machine routes pass the web-UI gate, then want a signed link or a session");

    let host = [("authorization", &*format!("Bearer {TOKEN}"))];
    let (status, _, body) = send(&f.app, "GET", "/api/service/v1/host", &host, None).await;
    assert_eq!(status, StatusCode::OK);
    let status_json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((status_json["hosted"].clone(), status_json["webUiOn"].clone()), (json!(true), json!(false)));

    let (status, ..) = send(&f.app, "POST", "/api/service/v1/host/web-ui", &host, Some(json!({ "enabled": true }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "needs a password");
    let (status, ..) = send(&f.app, "POST", "/api/service/v1/host/web-ui", &host, Some(json!({ "enabled": true, "password": "short" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, body) = send(&f.app, "POST", "/api/service/v1/host/web-ui", &host, Some(json!({ "enabled": true, "password": "long enough pw" }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["webUiOn"], true);

    let written: Value = serde_json::from_str(&std::fs::read_to_string(f.dir.path().join("config.json")).unwrap()).unwrap();
    assert_eq!(written["future"], json!({ "keep": true }), "keys the service does not know survive");
    assert!(written["passwordHash"].as_str().unwrap().starts_with("scrypt$32768$8$1$"));

    let (status, ..) = send(&f.app, "GET", "/", &[], None).await;
    assert_eq!(status, StatusCode::FOUND, "now the pages ask for a login");
}

#[tokio::test]
async fn the_app_opens_its_own_window_with_a_one_time_link_even_with_the_web_ui_off() {
    let f = fixture(json!({}), true);
    let host = [("authorization", &*format!("Bearer {TOKEN}"))];
    let (_, _, body) = send(&f.app, "POST", "/api/service/v1/host/web-link", &host, None).await;
    let path = serde_json::from_str::<Value>(&body).unwrap()["path"].as_str().unwrap().to_string();
    let (status, headers, _) = send(&f.app, "GET", &path, &[], None).await;
    assert_eq!((status, header(&headers, "location")), (StatusCode::FOUND, Some("/")));
    let cookie = cookie_of(&headers);
    assert!(!header(&headers, "set-cookie").unwrap().contains("Max-Age"));
    let (status, ..) = send(&f.app, "GET", "/", &[("cookie", &cookie)], None).await;
    assert_eq!(status, StatusCode::OK, "the window is signed in with the web UI off");
    let (status, ..) = send(&f.app, "GET", &path, &[], None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a code works once");
}

#[tokio::test]
async fn the_service_api_wants_a_bearer_and_only_the_host_reaches_host_routes() {
    let hash = hash_password("correct horse").unwrap();
    let f = fixture(json!({ "passwordHash": hash }), true);
    let (status, ..) = send(&f.app, "GET", "/api/service/v1/state", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, _, body) = send(&f.app, "POST", "/api/service/connect", &[], Some(json!({ "password": "correct horse" }))).await;
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"].as_str().unwrap().to_string();
    let bearer = format!("Bearer {token}");
    let device = [("authorization", &*bearer)];
    let (status, ..) = send(&f.app, "GET", "/api/service/v1/resign/options", &device, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = send(&f.app, "GET", "/api/service/v1/state", &device, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["authRequired"], true);
    let (status, ..) = send(&f.app, "GET", "/api/service/v1/nonsense", &device, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = send(&f.app, "GET", "/api/service/v1/host", &device, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "a device is not the host");
    let (status, ..) = send(&f.app, "POST", "/api/service/v1/disconnect", &device, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, ..) = send(&f.app, "GET", "/api/service/v1/state", &device, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "disconnect revoked it");
    let (status, ..) = send(&f.app, "GET", "/api/service/v1/state", &[("authorization", "Bearer short")], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, body) = send(&f.app, "GET", "/api/service", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["protocol"], "apparition-service");
    let _: PathBuf = f.dir.path().to_path_buf();
}

#[tokio::test]
async fn without_a_directory_the_compiled_in_ui_is_served() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), "{}").unwrap();
    let app = router(Settings {
        public_dir: None,
        browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("unused"))
    }).unwrap();
    let (status, headers, body) = send(&app, "GET", "/", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>Apparition Store</title>") && !body.contains("{{BRAND_"), "brand placeholders are filled");
    assert_eq!(header(&headers, "cache-control"), Some("no-store"));
    let (status, headers, _) = send(&app, "GET", "/shared/projects.js", &[], None).await;
    assert_eq!((status, header(&headers, "content-type")), (StatusCode::OK, Some("text/javascript")));
    assert_eq!(send(&app, "GET", "/shared/nope.js", &[], None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_paired_computer_drives_the_store_but_not_its_settings() {
    let f = fixture(json!({ "repos": [] }), true);
    let bearer = format!("Bearer {TOKEN}");
    let paired = [("authorization", bearer.as_str()), ("x-apparition-computer", "Gaming PC")];
    let (status, _, _) = send(&f.app, "GET", "/api/service/v1/state", &paired, None).await;
    assert_eq!(status, StatusCode::OK);
    for (method, path) in [("GET", "/api/service/v1/host"), ("POST", "/api/service/v1/host/web-link"), ("POST", "/api/service/v1/host/web-ui")] {
        let (status, _, _) = send(&f.app, method, path, &paired, Some(json!({ "enabled": true, "password": "a password here" }))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} is the owner's");
    }
    let config: Value = serde_json::from_str(&std::fs::read_to_string(f.dir.path().join("config.json")).unwrap()).unwrap();
    assert!(config.get("passwordHash").is_none());
    // The app itself still is the owner.
    let (status, _, _) = send(&f.app, "GET", "/api/service/v1/host", &[("authorization", bearer.as_str())], None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_service_api_reaches_what_the_page_does_and_links_use_the_public_url() {
    let f = fixture(json!({ "repos": [], "publicUrl": "https://mac.example.ts.net/", "altSource": { "enabled": true } }), true);
    let bearer = format!("Bearer {TOKEN}");
    let auth = [("authorization", bearer.as_str()), ("host", "127.0.0.1:8484")];
    let (status, _, body) = send(&f.app, "GET", "/api/service/v1/altsource", &auth, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let links: Value = serde_json::from_str(&body).unwrap();
    assert!(links["combined"]["url"].as_str().unwrap().starts_with("https://mac.example.ts.net/altsource/source.json?k="), "{links}");
    // Through a proxy, the proxy's name wins.
    let proxied = [("authorization", bearer.as_str()), ("host", "127.0.0.1:8484"), ("x-forwarded-host", "other.example"), ("x-forwarded-proto", "https")];
    let (_, _, body) = send(&f.app, "GET", "/api/service/v1/altsource", &proxied, None).await;
    assert!(body.contains("https://other.example/altsource/"), "{body}");
    let (status, _, _) = send(&f.app, "POST", "/api/service/v1/refresh-devices", &auth, Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&f.app, "GET", "/api/service/v1/nothing-like-this", &auth, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_stopped_store_answers_nothing_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), json!({ "repos": [] }).to_string()).unwrap();
    let mut settings = Settings::new(dir.path().join("config.json"), dir.path().join("data"), dir.path().join("public"));
    settings.host_token = Some(TOKEN.into());
    settings.key_dir = dir.path().join("keys");
    settings.debug_state_dir = dir.path().join("debugtrace");
    settings.browser = std::sync::Arc::new(apparition_service::debug::Fixed(vec![]));
    let (address, served) = apparition_service::server::serve_loopback(settings).await.unwrap();
    let ask = |path: &'static str| async move {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut text = String::new();
        let _ = stream.read_to_string(&mut text).await;
        text
    };
    assert!(ask("/api/service/v1/state").await.starts_with("HTTP/1.1 200"));
    served.stop();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(tokio::net::TcpStream::connect(address).await.is_err(), "no new connections after stop");
}

#[tokio::test]
async fn ota_links_come_through_the_service_api_too() {
    let f = fixture(json!({ "repos": [], "publicUrl": "https://mac.example.ts.net" }), true);
    let data = f.dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("state.json"), json!({ "ota": { "com.example.app|iOS": { "slug": "com.example.app-iOS", "bundleId": "com.example.app" } } }).to_string()).unwrap();
    let f = Fixture { app: router(Settings {
        host_token: Some(TOKEN.to_string()), key_dir: f.dir.path().join("keys"), debug_state_dir: f.dir.path().join("debugtrace"),
        browser: std::sync::Arc::new(apparition_service::debug::Fixed(vec![])),
        ..Settings::new(f.dir.path().join("config.json"), data, f.dir.path().join("public"))
    }).unwrap(), dir: f.dir };
    let bearer = format!("Bearer {TOKEN}");
    let (status, _, body) = send(&f.app, "GET", "/api/service/v1/ota/com.example.app-iOS/link", &[("authorization", bearer.as_str())], None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("https://mac.example.ts.net/ota/com.example.app-iOS/manifest.plist?t="), "{body}");
}
