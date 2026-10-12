//! The anisette v3 protocol, served from the loaded ADI libraries.
//!
//! This is the protocol isideload's `RemoteV3AnisetteProvider` speaks (MIT licensed, a dependency
//! of this crate), so what it expects is what's implemented, and a server of another make
//! (the Docker image the Companion runs) answers the same requests:
//!
//! - `GET /v3/client_info`: the `client_info` and `user_agent` strings to send to Apple;
//! - `GET /v3/provisioning_session` (WebSocket): provisions a device, with the client making the
//!   requests to Apple. The server asks, in turn, for the identifier (16 bytes), Apple's
//!   `spim`, and Apple's `ptm` and `tk`, and answers with the `cpim` and the resulting `adi.pb`;
//! - `POST /v3/get_headers`: for an identifier and its `adi.pb`, the `X-Apple-I-MD`,
//!   `X-Apple-I-MD-M` and `X-Apple-I-MD-RINFO` headers.
//!
//! The server keeps nothing between requests: the client stores its `adi.pb` and identifier. Each
//! request gets a folder of its own for the library to read and write, removed afterwards.
//!
//! Facts taken from the Dadoum `anisette-v3-server` (no licence stated; behaviour only): the
//! ADI identifier is the UUID made of the client's 16 bytes, written in capitals, cut to 16
//! characters; `X-Apple-I-MD-RINFO` is the constant 17106176; the client info is a MacBook Pro
//! running Xcode, which is what Apple's anisette validation expects.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rootcause::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::adi::Adi;
use super::apk;

const RINFO: &str = "17106176";
const CLIENT_INFO: &str = "<MacBookPro13,2> <macOS;13.1;22C65> <com.apple.AuthKit/1 (com.apple.dt.Xcode/3594.4.19)>";
const USER_AGENT: &str = "akd/1.0 CFNetwork/808.1.4";
/// How long the server waits for the client's next message while provisioning.
const STEP: Duration = Duration::from_secs(60);

struct Shared {
    adi: Adi,
    work: PathBuf,
}

/// A running server. Dropping it stops serving.
pub struct Server {
    pub address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    /// The address isideload takes as its anisette server.
    pub fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Fetches the libraries if they aren't in `data_dir` yet, loads them, and serves on `bind`
/// (use port 0 to take any free one).
pub async fn start(data_dir: &Path, bind: SocketAddr) -> Result<Server, Report> {
    let (services, core) = apk::libraries(&data_dir.join("libraries")).await?;
    let work = data_dir.join("work");
    std::fs::create_dir_all(&work)?;
    let adi = Adi::start(services, core, &work)?;
    let shared = Arc::new(Shared { adi, work });
    let app = Router::new()
        .route("/v3/client_info", get(client_info))
        .route("/v3/get_headers", post(get_headers))
        .route("/v3/provisioning_session", get(provisioning_session))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(bind).await.context(format!("can't listen on {bind}"))?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Server { address, task })
}

/// ADI's identifier for the client's 16 bytes.
fn adi_identifier(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    // The first 16 characters of the hyphenated UUID: 8 digits, a hyphen, 4 digits, a hyphen, 2.
    format!("{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..14])
}

fn identifier_from(value: &str) -> Option<String> {
    let bytes: [u8; 16] = B64.decode(value).ok()?.try_into().ok()?;
    Some(adi_identifier(&bytes))
}

/// A folder only this request uses.
struct Scratch(PathBuf);

impl Scratch {
    fn new(work: &Path) -> Result<Scratch, Report> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name = format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
        let path = work.join(name);
        std::fs::create_dir_all(&path)?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn client_info() -> Json<Value> {
    Json(json!({ "client_info": CLIENT_INFO, "user_agent": USER_AGENT }))
}

#[derive(Deserialize)]
struct HeadersRequest {
    identifier: String,
    adi_pb: String,
}

async fn get_headers(State(shared): State<Arc<Shared>>, Json(request): Json<HeadersRequest>) -> Json<Value> {
    let result = headers_for(&shared, request).await;
    Json(match result {
        Ok(headers) => headers,
        Err(error) => json!({ "result": "GetHeadersError", "message": format!("{error}") }),
    })
}

async fn headers_for(shared: &Arc<Shared>, request: HeadersRequest) -> Result<Value, Report> {
    let identifier = identifier_from(&request.identifier).ok_or_else(|| report!("the identifier isn't 16 bytes of base64"))?;
    let adi_pb = B64.decode(&request.adi_pb).context("adi_pb isn't base64")?;
    let scratch = Scratch::new(&shared.work)?;
    std::fs::write(scratch.0.join("adi.pb"), adi_pb)?;
    let folder = scratch.0.clone();
    let otp = shared
        .adi
        .run(move |adi| -> Result<_, Report> {
            adi.select(&identifier, &folder)?;
            if !adi.is_provisioned() {
                bail!("this device isn't provisioned");
            }
            adi.request_otp()
        })??;
    Ok(json!({
        "result": "Headers",
        "X-Apple-I-MD-M": B64.encode(otp.machine_id),
        "X-Apple-I-MD": B64.encode(otp.one_time_password),
        "X-Apple-I-MD-RINFO": RINFO,
    }))
}

async fn provisioning_session(State(shared): State<Arc<Shared>>, upgrade: WebSocketUpgrade) -> axum::response::Response {
    upgrade.on_upgrade(move |socket| async move {
        let _ = provision(shared, socket).await;
    })
}

/// Sends `{"result": …, …}` to the client.
async fn say(socket: &mut WebSocket, result: &str, extra: Value) -> Result<(), Report> {
    let mut message = json!({ "result": result });
    if let (Some(into), Value::Object(from)) = (message.as_object_mut(), extra) {
        into.extend(from);
    }
    socket.send(Message::text(message.to_string())).await.context("the client went away")?;
    Ok(())
}

/// The client's next message as JSON, or the reason there isn't one.
async fn hear(socket: &mut WebSocket) -> Result<Value, &'static str> {
    loop {
        match tokio::time::timeout(STEP, socket.recv()).await {
            Err(_) => return Err("Timeout"),
            Ok(None | Some(Err(_))) => return Err("Closed"),
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(text.as_str()).map_err(|_| "Closed"),
            Ok(Some(Ok(Message::Close(_)))) => return Err("Closed"),
            // Pings are answered by the library; anything else isn't part of the protocol.
            Ok(Some(Ok(_))) => {}
        }
    }
}

fn field(value: &Value, name: &str) -> Option<Vec<u8>> {
    B64.decode(value.get(name)?.as_str()?).ok()
}

async fn provision(shared: Arc<Shared>, mut socket: WebSocket) -> Result<(), Report> {
    macro_rules! hear {
        () => {
            match hear(&mut socket).await {
                Ok(value) => value,
                Err("Timeout") => return say(&mut socket, "Timeout", json!({})).await,
                Err(_) => return Ok(()),
            }
        };
    }

    say(&mut socket, "GiveIdentifier", json!({})).await?;
    let reply = hear!();
    let Some(identifier) = reply.get("identifier").and_then(Value::as_str).and_then(identifier_from) else {
        return say(&mut socket, "InvalidIdentifier", json!({})).await;
    };
    let scratch = Scratch::new(&shared.work)?;

    say(&mut socket, "GiveStartProvisioningData", json!({})).await?;
    let reply = hear!();
    let Some(spim) = field(&reply, "spim") else {
        return say(&mut socket, "StartProvisioningError", json!({ "message": "no spim" })).await;
    };
    let (select, folder) = (identifier.clone(), scratch.0.clone());
    let started = shared
        .adi
        .run(move |adi| -> Result<_, Report> {
            adi.select(&select, &folder)?;
            adi.start_provisioning(&spim)
        })?;
    let (cpim, session) = match started {
        Ok(started) => started,
        Err(error) => return say(&mut socket, "StartProvisioningError", json!({ "message": format!("{error}") })).await,
    };

    say(&mut socket, "GiveEndProvisioningData", json!({ "cpim": B64.encode(cpim) })).await?;
    let reply = hear!();
    let (Some(ptm), Some(tk)) = (field(&reply, "ptm"), field(&reply, "tk")) else {
        let _ = shared.adi.run(move |adi| adi.abandon(session));
        return say(&mut socket, "EndProvisioningError", json!({ "message": "no ptm and tk" })).await;
    };
    let folder = scratch.0.clone();
    let ended = shared
        .adi
        .run(move |adi| -> Result<_, Report> {
            // Another request may have pointed the library elsewhere since.
            adi.select(&identifier, &folder)?;
            adi.end_provisioning(session, &ptm, &tk)
        })?;
    if let Err(error) = ended {
        return say(&mut socket, "EndProvisioningError", json!({ "message": format!("{error}") })).await;
    }

    let adi_pb = std::fs::read(scratch.0.join("adi.pb")).context("provisioning left no adi.pb")?;
    say(&mut socket, "ProvisioningSuccess", json!({ "adi_pb": B64.encode(adi_pb) })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provisions against Apple and makes headers, through isideload's own client. Reaches the
    /// network, needs no Apple ID: `APPARITION_NET=1`.
    #[tokio::test(flavor = "multi_thread")]
    async fn isideloads_client_provisions_and_gets_headers() {
        use isideload::anisette::AnisetteDataGenerator;
        use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
        use isideload::auth::grandslam::GrandSlam;
        use isideload::util::storage::InMemoryStorage;

        if std::env::var_os("APPARITION_NET").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("apparition-anisette-{}", std::process::id()));
        let server = start(&dir, "127.0.0.1:0".parse().unwrap()).await.unwrap();

        let provider = RemoteV3AnisetteProvider::new(&server.url(), Box::new(InMemoryStorage::new()), "0".to_string()).unwrap();
        let mut generator = AnisetteDataGenerator::new(Arc::new(tokio::sync::RwLock::new(provider)));
        let client_info = generator.get_client_info().await.unwrap();
        let grandslam = Arc::new(GrandSlam::new(client_info, false, None).await.unwrap());
        let data = generator.get_anisette_data(grandslam).await.unwrap();

        // The values themselves are an identity; only check that they're there.
        let headers = data.get_headers();
        for name in ["X-Apple-I-MD", "X-Apple-I-MD-M", "X-Mme-Device-Id"] {
            assert!(headers.get(name).is_some_and(|value| value.len() > 8), "{name} missing or empty");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identifier_is_the_first_sixteen_characters_of_the_uppercase_uuid() {
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe];
        assert_eq!(adi_identifier(&bytes), "01234567-89AB-CD");
        assert_eq!(adi_identifier(&bytes).len(), 16);
    }
}
