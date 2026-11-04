//! Signing in to another computer's Store with its password: `apparition store login <address>`.
//!
//! The Store answers a correct password with a session token (30 days, renewed whenever it is
//! used). That token, not the password, is kept here, in a file only this user reads, and the
//! commands that talk to a Store (`apparition store`, `deploy`, `updates`, `mcp`) send it as a
//! bearer. A session reaches builds, deploys, updates and debugging; the Store's own settings
//! (its password, its web pages) stay with the Mac it runs on, as for any password session.
//!
//! The address is HTTPS (a `tailscale serve` address, a reverse proxy), or HTTP on this computer
//! (`http://127.0.0.1:8484`): the password and the token never cross a network unencrypted, and
//! a redirect is never followed, so neither goes anywhere the person didn't type.

use std::path::PathBuf;
use std::time::Duration;

use apparition_core::api::paired_mac::{Reply, Upload};
use reqwest::Url;
use serde_json::{Value, json};

use crate::{Error, anisette_dir, fail};

/// Next to the pairing a Mac gave this computer: another way to be let in.
fn file() -> PathBuf {
    anisette_dir().join("store-login.json")
}

/// A Store this computer has signed in to.
#[derive(Clone, Debug)]
pub(crate) struct Login {
    pub(crate) url: Url,
    token: String,
}

/// The address as typed: `https://` when no scheme is given; no credentials, path, query or
/// fragment; and HTTPS unless it is this computer.
pub(crate) fn address(raw: &str) -> Result<Url, Error> {
    let raw = raw.trim();
    let full = if raw.contains("://") { raw.to_string() } else { format!("https://{raw}") };
    let url = Url::parse(&full).map_err(|_| fail("Enter the Store's address, like https://mac.your-tailnet.ts.net"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err(fail("Use an https:// address without a path or credentials (http:// only for this computer). A Store on another network is reached with `tailscale serve`, or `apparition mac pair`."));
    }
    Ok(url)
}

fn client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .user_agent(concat!("Apparition/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        // A Store's slowest answers (a debug call into an app) take up to ~90 s; an upload of
        // an IPA takes as long as it takes, so there is no limit on the whole request.
        .read_timeout(Duration::from_secs(130))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| fail(format!("Couldn't set up the connection: {e}")))
}

/// The saved sign-in, if there is one.
pub(crate) fn load() -> Option<Login> {
    let value: Value = serde_json::from_slice(&std::fs::read(file()).ok()?).ok()?;
    let url = address(value["url"].as_str()?).ok()?;
    let token = value["token"].as_str().filter(|t| valid_token(t))?.to_string();
    Some(Login { url, token })
}

fn valid_token(token: &str) -> bool {
    token.len() == 43 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn save(login: &Login) -> Result<(), Error> {
    use std::io::Write;
    let path = file();
    std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
    let temporary = path.with_extension("new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&temporary)?;
    out.write_all(json!({ "url": login.url.as_str(), "token": login.token }).to_string().as_bytes())?;
    drop(out);
    std::fs::rename(&temporary, path)?;
    Ok(())
}

/// Signs in to the Store at `url` and keeps the session. Returns the Store's capabilities.
pub(crate) async fn connect(url: &Url, password: &str) -> Result<Value, Error> {
    let client = client()?;
    let reply = client.post(url.join("api/service/connect").unwrap()).json(&json!({ "password": password }))
        .send().await.map_err(|e| fail(format!("Couldn't reach {url}: {}", describe(&e))))?;
    let status = reply.status();
    let body: Value = reply.bytes().await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    if !status.is_success() {
        let why = body["error"].as_str().unwrap_or("it refused the sign-in");
        return Err(fail(format!("{url}: {why} (HTTP {})", status.as_u16())));
    }
    if body["protocol"] != "apparition-service" || body["version"] != 1 {
        return Err(fail(format!("{url} isn't an Apparition Store this version understands.")));
    }
    let token = body["token"].as_str().filter(|t| valid_token(t)).ok_or_else(|| fail("The Store didn't return a valid session."))?;
    let login = Login { url: url.clone(), token: token.to_string() };
    // A second sign-in replaces the first; the old session is ended on its Store.
    let old = load();
    save(&login)?;
    if let Some(old) = old {
        let _ = old.request("POST", "/api/service/v1/disconnect", Some("application/json"), Upload::Bytes(b"{}")).await;
    }
    Ok(body)
}

/// Ends the saved session on its Store (when that answers) and forgets it. Whether there was one.
pub(crate) async fn disconnect() -> Result<Option<Url>, Error> {
    let Some(login) = load() else { return Ok(None) };
    let _ = login.request("POST", "/api/service/v1/disconnect", Some("application/json"), Upload::Bytes(b"{}")).await;
    std::fs::remove_file(file())?;
    Ok(Some(login.url))
}

fn describe(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() { "timed out" } else if error.is_connect() { "nothing answered there" } else { "the connection failed" }
}

impl Login {
    /// What requests to this Store are called.
    pub(crate) fn name(&self) -> String {
        self.url.host_str().unwrap_or("the Store").to_string()
    }

    /// One request, with the session as the bearer; the whole answer.
    pub(crate) async fn request(&self, method: &str, path: &str, content_type: Option<&str>, body: Upload<'_>) -> Result<Reply, Error> {
        if !path.starts_with('/') || path.contains(['\r', '\n', ' ']) {
            return Err(fail(format!("invalid request {method} {path}")));
        }
        let url = self.url.join(path.trim_start_matches('/')).map_err(|_| fail(format!("invalid request {method} {path}")))?;
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| fail(format!("invalid method {method}")))?;
        let mut request = client()?.request(method, url).bearer_auth(&self.token);
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        request = match body {
            Upload::None => request,
            Upload::Bytes(bytes) => request.body(bytes.to_vec()),
            Upload::File(path) => {
                let file = tokio::fs::File::open(path).await.map_err(|e| fail(format!("Can't read {}: {e}", path.display())))?;
                let length = file.metadata().await.map_err(|e| fail(e.to_string()))?.len();
                let stream = tokio_util::io::ReaderStream::new(file);
                request.header(reqwest::header::CONTENT_LENGTH, length).body(reqwest::Body::wrap_stream(stream))
            }
        };
        let reply = request.send().await.map_err(|e| fail(format!("{} didn't answer: {}", self.name(), describe(&e))))?;
        let status = reply.status().as_u16();
        let content_type = reply.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = reply.bytes().await.map_err(|_| fail(format!("{}'s answer was cut off", self.name())))?.to_vec();
        if status == 401 {
            return Err(fail(format!("{}'s Store ended this sign-in (sessions last 30 days, and end when the password changes). Sign in again: apparition store login {}", self.name(), self.url)));
        }
        Ok(Reply { status, content_type, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_only_go_to_https_or_this_computer() {
        for bad in ["http://example.com", "https://user:pw@example.com", "https://example.com/path", "https://example.com/?t=x", "file:///tmp/x", "ftp://example.com", "http://localhost.evil.example", "http://127.0.0.1.nip.io:8484"] {
            assert!(address(bad).is_err(), "{bad}");
        }
        for good in ["https://mac.ts.net", "mac.ts.net", "mac.ts.net:8443", "http://localhost:8484", "http://127.0.0.1:8484", "http://[::1]:8484"] {
            assert!(address(good).is_ok(), "{good}");
        }
        assert_eq!(address("mac.ts.net").unwrap().as_str(), "https://mac.ts.net/");
    }

    #[test]
    fn tokens_have_the_stores_shape() {
        assert!(valid_token(&"a".repeat(43)));
        assert!(!valid_token(&"a".repeat(42)));
        assert!(!valid_token(&format!("{}/", "a".repeat(42))));
    }
}
