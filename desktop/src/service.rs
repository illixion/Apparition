//! The authenticated Mac build service. Credentials stay in Rust memory; the
//! webview gets only capabilities and results from a bounded set of operations.
use apparition_core::api::DeviceStore;
use reqwest::{Client, Method, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::{sync::Mutex, time::Duration};
use tauri_plugin_dialog::DialogExt;

#[derive(Clone)]
struct Session {
    base: Url,
    token: String,
    client: Client,
}

#[derive(Default)]
pub struct BuildService {
    session: Mutex<Option<Session>>,
}

fn address(raw: &str) -> Result<Url, String> {
    let url =
        Url::parse(raw.trim()).map_err(|_| "Enter the build service's full URL.".to_string())?;
    let local = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || !(url.scheme() == "https" || (url.scheme() == "http" && local))
    {
        return Err(
            "Use an HTTPS service URL, or HTTP on localhost, without a path or credentials.".into(),
        );
    }
    Ok(url)
}

fn client() -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(95))
        .connect_timeout(Duration::from_secs(10))
        // Never forward a service password or bearer token through a redirect.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "Couldn't create the service connection.".into())
}

async fn response(request: reqwest::RequestBuilder) -> Result<Value, String> {
    let response = request.send().await.map_err(|_| {
        "Couldn't reach the build service. Check its address and connection.".to_string()
    })?;
    let status = response.status();
    // Limit diagnostic payloads and fail explicitly rather than truncating JSON.
    if response
        .content_length()
        .is_some_and(|n| n > 8 * 1024 * 1024)
    {
        return Err("The service response is too large.".into());
    }
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "The service connection was interrupted.".to_string())?
    {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err("The service response is too large.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(&bytes)}));
    if !status.is_success() {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or(if status.as_u16() == 401 {
                "The service session expired. Connect again."
            } else {
                "The service refused the request."
            })
            .to_string());
    }
    Ok(value)
}

fn settings() -> std::path::PathBuf {
    DeviceStore::default().dir.join("build-service.json")
}

#[tauri::command]
pub fn service_settings() -> Value {
    std::fs::read(settings())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({}))
}

#[tauri::command]
pub async fn service_connect(
    state: tauri::State<'_, BuildService>,
    url: String,
    password: String,
) -> Result<Value, String> {
    let base = address(&url)?;
    let client = client()?;
    let reply = response(
        client
            .post(base.join("api/service/connect").unwrap())
            .json(&json!({"password": password})),
    )
    .await?;
    if reply["protocol"] != "apparition-service" || reply["version"] != 1 {
        return Err("This server doesn't support Apparition service protocol 1.".into());
    }
    let token = reply["token"]
        .as_str()
        .filter(|t| {
            t.len() == 43
                && t.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        })
        .ok_or("The service didn't return a valid session.")?
        .to_string();
    let old = state.session.lock().unwrap().replace(Session {
        base: base.clone(),
        token,
        client,
    });
    if let Some(old) = old {
        let _ = old
            .client
            .post(old.base.join("api/service/v1/disconnect").unwrap())
            .bearer_auth(&old.token)
            .send()
            .await;
    }
    let path = settings();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, json!({"url": base.as_str()}).to_string());
    // The session token is never returned to the webview or persisted on disk.
    Ok(
        json!({"url": base.as_str(), "capabilities": reply["capabilities"], "version": 1, "hostPlatform": reply["hostPlatform"]}),
    )
}

#[tauri::command]
pub async fn service_disconnect(state: tauri::State<'_, BuildService>) -> Result<(), String> {
    let session = state.session.lock().unwrap().take();
    if let Some(s) = session {
        response(
            s.client
                .post(s.base.join("api/service/v1/disconnect").unwrap())
                .bearer_auth(&s.token)
                .json(&json!({})),
        )
        .await?;
    }
    Ok(())
}

// Only these operations can carry the credential. No arbitrary URL, headers or
// filesystem path from the webview is ever passed to the network client.
fn operation(name: &str, args: &Value) -> Result<(Method, String, Option<Value>), String> {
    let id = || {
        args["id"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("Select a build first.")
    };
    let key = || {
        args["id"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
            })
            .ok_or("Invalid report or app identifier.")
    };
    let get = |p: String| Ok((Method::GET, p, None));
    match name {
        "state" | "traces" | "crashlogs" | "artifacts" => get(name.into()),
        "build" => {
            if !args["repo"].is_string() || !args["device"].is_string() {
                return Err("Select a project and target first.".into());
            }
            Ok((Method::POST, "build".into(), Some(args.clone())))
        }
        "cancel" => Ok((
            Method::POST,
            format!("builds/{}/cancel", id()?),
            Some(json!({})),
        )),
        "buildLog" => get(format!(
            "builds/{}/log?offset={}",
            id()?,
            args["offset"].as_u64().unwrap_or(0)
        )),
        "crashFile" => get(format!(
            "crashlogs/{}/{}",
            key()?,
            if args["raw"] == true {
                "ips"
            } else {
                "symbolicated"
            }
        )),
        "traceFile" => {
            let path = args["path"].as_str().ok_or("Select a report file first.")?;
            if path.is_empty()
                || path
                    .split('/')
                    .any(|s| s.is_empty() || s == "." || s == "..")
            {
                return Err("Invalid report file.".into());
            }
            let mut url = Url::parse("https://service.invalid/").unwrap();
            url.path_segments_mut()
                .unwrap()
                .extend(["traces", key()?, "files"])
                .extend(path.split('/'));
            get(url.path().trim_start_matches('/').to_string())
        }
        "debugApps" => get("debug/apps".into()),
        "artifactManifest" => get(format!("artifacts/{}/manifest", key()?)),
        "debugHelp" => get(format!("debug/{}/help", key()?)),
        "debugCall" => Ok((
            Method::POST,
            format!("debug/{}/call", key()?),
            Some(json!({"endpoint": args["endpoint"], "arguments": args["arguments"]})),
        )),
        _ => Err("Unknown service operation.".into()),
    }
}

async fn download_to(
    session: &Session,
    id: &str,
    artifact: &Value,
    destination: &std::path::Path,
) -> Result<(), String> {
    let expected_bytes = artifact["bytes"]
        .as_u64()
        .ok_or("The artifact size is missing.")?;
    let expected_hash = artifact["sha256"]
        .as_str()
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or("The artifact digest is missing.")?;
    let url = session
        .base
        .join(&format!("api/service/v1/artifacts/{id}/download"))
        .map_err(|_| "Invalid build identity.")?;
    let mut reply = session
        .client
        .get(url)
        .bearer_auth(&session.token)
        .timeout(Duration::from_secs(30 * 60))
        .send()
        .await
        .map_err(|_| "The artifact download could not connect.".to_string())?;
    if !reply.status().is_success() {
        return Err(
            "The service refused the artifact download. Connect again if the session expired."
                .into(),
        );
    }
    let mut file = tempfile::NamedTempFile::new_in(
        destination.parent().ok_or("Choose a destination folder.")?,
    )
    .map_err(|_| "Couldn't create the download file.".to_string())?;
    let mut count = 0u64;
    let mut hash = Sha256::new();
    while let Some(chunk) = reply
        .chunk()
        .await
        .map_err(|_| "The artifact download was interrupted.".to_string())?
    {
        count = count
            .checked_add(chunk.len() as u64)
            .ok_or("The artifact size overflowed.")?;
        if count > expected_bytes {
            return Err("The downloaded artifact is larger than its manifest.".into());
        }
        file.write_all(&chunk)
            .map_err(|_| "Couldn't write the downloaded artifact.".to_string())?;
        hash.update(&chunk);
    }
    if count != expected_bytes || format!("{:x}", hash.finalize()) != expected_hash.to_lowercase() {
        return Err(
            "The artifact does not match its manifest. The destination file was preserved.".into(),
        );
    }
    file.as_file()
        .sync_all()
        .map_err(|_| "Couldn't save the completed artifact.".to_string())?;
    file.persist(destination)
        .map_err(|_| "Couldn't move the verified artifact to its destination.".to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn service_download(
    app: tauri::AppHandle,
    state: tauri::State<'_, BuildService>,
    id: String,
) -> Result<Value, String> {
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return Err("Invalid build identity.".into());
    }
    let session = state
        .session
        .lock()
        .unwrap()
        .clone()
        .ok_or("Connect to a build service first.")?;
    let manifest = response(
        session
            .client
            .get(
                session
                    .base
                    .join(&format!("api/service/v1/artifacts/{id}/manifest"))
                    .unwrap(),
            )
            .bearer_auth(&session.token),
    )
    .await?;
    if manifest["format"] != "apparition-build/1" || manifest["buildId"] != id {
        return Err("The service returned a different build identity.".into());
    }
    let extension = match manifest["artifact"]["kind"].as_str() {
        Some("ipa") => "ipa",
        Some("app.zip") => "app.zip",
        _ => return Err("Unknown artifact type.".into()),
    };
    let name = manifest["app"]["name"]
        .as_str()
        .unwrap_or("App")
        .chars()
        .take(60)
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>();
    let filename = format!("{name}-{id}.{extension}");
    let destination = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Save build artifact")
            .set_file_name(&filename)
            .blocking_save_file()
            .and_then(|file| file.into_path().ok())
    })
    .await
    .map_err(|_| "Couldn't open the save dialog.".to_string())?;
    let Some(destination) = destination else {
        return Ok(json!({"cancelled":true}));
    };
    download_to(&session, &id, &manifest["artifact"], &destination).await?;
    Ok(json!({"path": destination.to_string_lossy(), "sha256": manifest["artifact"]["sha256"]}))
}

#[tauri::command]
pub async fn service_request(
    state: tauri::State<'_, BuildService>,
    operation: String,
    arguments: Value,
) -> Result<Value, String> {
    let session = state
        .session
        .lock()
        .unwrap()
        .clone()
        .ok_or("Connect to a build service first.")?;
    let (method, path, body) = self::operation(&operation, &arguments)?;
    let mut request = session
        .client
        .request(
            method,
            session
                .base
                .join(&format!("api/service/v1/{path}"))
                .unwrap(),
        )
        .bearer_auth(&session.token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    response(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verified_download_preserves_destination_on_corruption() {
        use std::io::Read;
        use std::net::TcpListener;
        apparition_core::api::init();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("artifact.ipa");
        for (corrupt, expected_bytes) in [(true, 5), (false, 4), (false, 6), (false, 5)] {
            std::fs::write(&destination, b"existing user file").unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0; 4096];
                let count = socket.read(&mut request).unwrap();
                assert!(
                    String::from_utf8_lossy(&request[..count])
                        .contains("GET /api/service/v1/artifacts/build/download")
                );
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                    )
                    .unwrap();
            });
            let artifact = json!({"bytes":expected_bytes,"sha256":format!("{:x}", Sha256::digest(if corrupt { b"other" } else { b"hello" }))});
            let session = Session {
                base,
                token: "fixture-token".into(),
                client: client().unwrap(),
            };
            let result = runtime.block_on(download_to(&session, "build", &artifact, &destination));
            server.join().unwrap();
            if corrupt || expected_bytes != 5 {
                assert!(result.is_err());
                assert_eq!(std::fs::read(&destination).unwrap(), b"existing user file");
            } else {
                assert!(result.is_ok());
                assert_eq!(std::fs::read(&destination).unwrap(), b"hello");
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }
    #[test]
    fn transport_reads_json_and_text_and_refuses_redirects_and_expired_sessions() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        apparition_core::api::init();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        for (status, headers, body, expected) in [
            (
                "200 OK",
                "Content-Type: application/json\r\n",
                "{\"value\":1}",
                Some(json!({"value": 1})),
            ),
            (
                "200 OK",
                "Content-Type: text/plain\r\n",
                "plain diagnostic",
                Some(json!({"text": "plain diagnostic"})),
            ),
            (
                "302 Found",
                "Location: http://127.0.0.1:9/credential-sink\r\n",
                "{}",
                None,
            ),
            ("401 Unauthorized", "", "{}", None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!(
                "http://{}/api/service/v1/state",
                listener.local_addr().unwrap()
            );
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0; 4096];
                let count = socket.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                assert!(request.starts_with("GET /api/service/v1/state HTTP/1.1"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer test-session")
                );
                write!(socket, "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let result = runtime.block_on(async {
                response(client().unwrap().get(&url).bearer_auth("test-session")).await
            });
            server.join().unwrap();
            if let Some(expected) = expected {
                assert_eq!(result.unwrap(), expected);
            } else {
                assert!(result.is_err());
            }
        }
    }
    #[test]
    fn credentials_cannot_be_sent_to_insecure_or_redirected_urls() {
        for url in [
            "http://example.com",
            "https://user:password@example.com",
            "https://example.com/path",
            "https://example.com/?token=x",
            "file:///tmp/server",
        ] {
            assert!(address(url).is_err(), "{url}");
        }
        for url in [
            "https://example.com",
            "http://localhost:8484",
            "http://127.0.0.1:8484",
            "http://[::1]:8484",
        ] {
            assert!(address(url).is_ok(), "{url}");
        }
    }
    #[test]
    fn operations_cannot_escape_the_service_namespace() {
        assert!(operation("https://example.com", &json!({})).is_err());
        assert!(operation("traceFile", &json!({"id":"id", "path":"../secret"})).is_err());
        assert!(operation("debugHelp", &json!({"id":"../../secret"})).is_err());
        assert!(operation("cancel", &json!({"id":-1})).is_err());
        let (_, path, _) =
            operation("traceFile", &json!({"id":"id", "path":"logs/a?#.txt"})).unwrap();
        assert_eq!(path, "traces/id/files/logs/a%3F%23.txt");
    }
}
