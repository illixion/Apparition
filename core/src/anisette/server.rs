//! The anisette v3 protocol, served from this Mac's own anisette (see `native`).
//!
//! This is the protocol isideload's `RemoteV3AnisetteProvider` speaks (MIT licensed, a dependency
//! of this crate), so what it expects is what's implemented:
//!
//! - `GET /v3/client_info`: the `client_info` and `user_agent` strings to send to Apple;
//! - `GET /v3/provisioning_session` (WebSocket): provisions a client. A Mac is provisioned by
//!   macOS already, so this only takes the client's identifier and answers at once, with a
//!   placeholder `adi.pb` the client stores (and sends back, unused);
//! - `POST /v3/get_headers`: the `X-Apple-I-MD`, `X-Apple-I-MD-M` and `X-Apple-I-MD-RINFO`
//!   headers, and whatever else AuthKit puts on the request.
//!
//! The client's identifier still gives its own `X-Mme-Device-Id` and `X-Apple-I-MD-LU`, which
//! Apple takes alongside a Mac's headers.
//!
//! `/v3/client_info` also says what this server serves (`apparition_anisette`: `mac`), with the
//! matching client info, which the sign-in claims (see claimed_platform).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rootcause::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

const RINFO: &str = "17106176";
/// How long the server waits for the client's next message while provisioning.
const STEP: Duration = Duration::from_secs(60);

/// Who may use a server: `None` serves anyone who reaches it, the way every v3 server does.
/// `Some` serves only `/t/<token>/v3/...`, for the tokens it accepts (the devices paired with
/// this companion, see pairing.rs). A token in the path needs nothing from the client but its
/// base URL, which every v3 client already takes.
pub type Access = Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>;

struct Shared {
    access: Access,
    backend: Backend,
}

/// Where a server's anisette comes from.
enum Backend {
    /// This Mac's own (`native`).
    Native,
    /// This Mac's anisette VM (vm.rs): the same server, run inside the guest, reached through the VM runner's front
    /// door on loopback. Each request makes sure the runner is up (resuming the saved VM); the paired-device check
    /// stays here, on the host.
    #[cfg(feature = "desktop")]
    Vm { bundle: std::path::PathBuf, client: reqwest::Client },
}

/// What a client stores as its `adi.pb`: nothing of this Mac's, which keeps its own.
const PLACEHOLDER_ADI_PB: &[u8] = b"apparition-mac-anisette";

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

/// Serves this Mac's anisette on `bind` (use port 0 to take any free one). Several servers can
/// run at once, say one on loopback for this computer's own sign-in and one for the companion's
/// channel. Fails when this Mac doesn't give its anisette (macOS 27 outside a prepared VM).
pub async fn start(bind: SocketAddr, access: Access) -> Result<Server, Report> {
    if !tokio::task::spawn_blocking(|| super::native::source().is_some()).await.unwrap_or(false) {
        bail!("{}", UNAVAILABLE);
    }
    serve(bind, access, Backend::Native).await
}

/// Serves the anisette of this Mac's anisette VM in `bundle` (vm.rs) on `bind`, the way [`start`] serves this Mac's
/// own: for a Mac on macOS 27, whose own anisette stays with macOS.
#[cfg(feature = "desktop")]
pub async fn start_vm(bind: SocketAddr, access: Access, bundle: std::path::PathBuf) -> Result<Server, Report> {
    // A request may have to wait for the VM to resume (a second) or, after a crash, to boot (about a minute).
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(150)).build().context("making the VM's client")?;
    serve(bind, access, Backend::Vm { bundle, client }).await
}

async fn serve(bind: SocketAddr, access: Access, backend: Backend) -> Result<Server, Report> {
    let open = access.is_none();
    let shared = Arc::new(Shared { access, backend });
    let app = if open {
        Router::new()
            .route("/v3/client_info", get(client_info))
            .route("/v3/get_headers", post(get_headers))
            .route("/v3/provisioning_session", get(provisioning_session))
    } else {
        Router::new()
            .route("/t/{token}/v3/client_info", get(token_client_info))
            .route("/t/{token}/v3/get_headers", post(token_get_headers))
            .route("/t/{token}/v3/provisioning_session", get(token_provisioning_session))
            .fallback(unpaired)
    };
    let app = app.with_state(shared);
    let listener = tokio::net::TcpListener::bind(bind).await.context(format!("can't listen on {bind}"))?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Server { address, task })
}

/// Why a Mac has no anisette to give.
pub const UNAVAILABLE: &str = "This Mac doesn't give its anisette to other programs. Apparition needs a Mac on macOS 26 or earlier, or a macOS virtual machine prepared with selfhost/macos-vm/setup.sh (see docs/ANISETTE_VM.md); on macOS 27, pair this Mac with such a VM instead.";

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

impl Shared {
    fn allows(&self, token: &str) -> bool {
        self.access.as_ref().is_none_or(|accepts| accepts(token))
    }
}

/// Anything but a paired device's path, on a server that wants one.
async fn unpaired() -> (StatusCode, &'static str) {
    (StatusCode::UNAUTHORIZED, "This anisette server serves only devices paired with it. Pair this device in Apparition first.\n")
}

async fn token_client_info(State(shared): State<Arc<Shared>>, UrlPath(token): UrlPath<String>) -> Response {
    if !shared.allows(&token) {
        return unpaired().await.into_response();
    }
    client_info(State(shared)).await.into_response()
}

async fn token_get_headers(State(shared): State<Arc<Shared>>, UrlPath(token): UrlPath<String>, body: Json<Value>) -> Response {
    if !shared.allows(&token) {
        return unpaired().await.into_response();
    }
    get_headers(State(shared), body).await.into_response()
}

async fn token_provisioning_session(State(shared): State<Arc<Shared>>, UrlPath(token): UrlPath<String>, upgrade: WebSocketUpgrade) -> Response {
    if !shared.allows(&token) {
        return unpaired().await.into_response();
    }
    provisioning_session(State(shared), upgrade).await
}

async fn client_info(State(shared): State<Arc<Shared>>) -> Response {
    #[cfg(feature = "desktop")]
    if let Backend::Vm { bundle, client } = &shared.backend {
        return match from_vm(bundle, client, "client_info", None).await {
            Ok(info) => Json(info).into_response(),
            Err(error) => (StatusCode::SERVICE_UNAVAILABLE, format!("{error}\n")).into_response(),
        };
    }
    let _ = &shared;
    native_client_info().into_response()
}

/// This Mac's anisette VM's answer to `path` (its own copy of this server, in the guest).
#[cfg(feature = "desktop")]
async fn from_vm(bundle: &std::path::Path, client: &reqwest::Client, path: &str, body: Option<Value>) -> Result<Value, Report> {
    let base = crate::vm::serve(bundle).await?;
    let url = format!("{base}/v3/{path}");
    let request = match body {
        Some(body) => client.post(url).json(&body),
        None => client.get(url),
    };
    let reply = request.send().await.context("The anisette VM didn't answer")?.error_for_status()?;
    Ok(reply.json().await.context("The anisette VM's answer isn't JSON")?)
}

fn native_client_info() -> Json<Value> {
    use crate::claimed_platform::{Served, USER_AGENT};
    if super::native::source() == Some(super::native::Source::Helper) {
        let (client_info, user_agent) = super::native::client_info();
        return Json(json!({ "client_info": client_info, "user_agent": user_agent, "apparition_anisette": Served::Mac.name() }));
    }
    Json(json!({ "client_info": Served::Mac.client_info(), "user_agent": USER_AGENT, "apparition_anisette": Served::Mac.name() }))
}

#[derive(Deserialize)]
struct HeadersRequest {
    // The client's `identifier` and `adi_pb` come too, unused: this Mac's headers don't depend on them.
    /// Apple's resync data, sent by isideload 0.5.0 and later after GrandSlam answers hsc=434.
    #[serde(default)]
    sim: Option<String>,
}

async fn get_headers(State(shared): State<Arc<Shared>>, Json(body): Json<Value>) -> Json<Value> {
    #[cfg(feature = "desktop")]
    if let Backend::Vm { bundle, client } = &shared.backend {
        // The VM's server answers in this same shape, errors included.
        return Json(match from_vm(bundle, client, "get_headers", Some(body)).await {
            Ok(reply) => reply,
            // Never "provision" in the message: isideload's client provisions again on that word.
            Err(error) => json!({ "result": "GetHeadersError", "message": format!("{error}").replace("rovision", "repar") }),
        });
    }
    let request = match serde_json::from_value::<HeadersRequest>(body) {
        Ok(request) => request,
        Err(error) => return Json(json!({ "result": "GetHeadersError", "message": format!("bad request: {error}") })),
    };
    let result = headers_for(&shared, request).await;
    Json(match result {
        Ok(headers) => headers,
        Err(error) => json!({ "result": "GetHeadersError", "message": format!("{error}") }),
    })
}

async fn headers_for(_shared: &Arc<Shared>, request: HeadersRequest) -> Result<Value, Report> {
    // Resync isn't done here, and ordinary headers in its place would be refused again. The
    // message mustn't say "provision": isideload's client reprovisions on that word.
    if request.sim.is_some() {
        bail!("Apple asked for an anisette resync (hsc 434), which this server can't do");
    }
    // This Mac's own headers, whatever the client's identity: its identifier still gives its
    // X-Mme-Device-Id and X-Apple-I-MD-LU, which Apple takes alongside them.
    let headers = tokio::task::spawn_blocking(super::native::headers).await.context("the native anisette task failed")??;
    let mut reply = json!({
        "result": "Headers",
        "X-Apple-I-MD-M": headers.machine_id,
        "X-Apple-I-MD": headers.one_time_password,
        "X-Apple-I-MD-RINFO": RINFO,
    });
    for (key, value) in headers.extra {
        reply[key] = Value::String(value);
    }
    Ok(reply)
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

/// A Mac is provisioned by macOS: take the client's identifier (it keeps its own device id
/// from it) and answer with the placeholder at once, without asking Apple for anything.
async fn provision(_shared: Arc<Shared>, mut socket: WebSocket) -> Result<(), Report> {
    say(&mut socket, "GiveIdentifier", json!({})).await?;
    let reply = match hear(&mut socket).await {
        Ok(value) => value,
        Err("Timeout") => return say(&mut socket, "Timeout", json!({})).await,
        Err(_) => return Ok(()),
    };
    if reply.get("identifier").and_then(Value::as_str).and_then(identifier_from).is_none() {
        return say(&mut socket, "InvalidIdentifier", json!({})).await;
    }
    say(&mut socket, "ProvisioningSuccess", json!({ "adi_pb": B64.encode(PLACEHOLDER_ADI_PB) })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Provisions and makes headers through isideload's own client, on a Mac that gives its
    /// anisette. Reaches Apple's URL bag, needs no Apple ID: `APPARITION_NET=1`.
    #[tokio::test(flavor = "multi_thread")]
    async fn isideloads_client_provisions_and_gets_headers() {
        use isideload::anisette::AnisetteDataGenerator;
        use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
        use isideload::auth::grandslam::GrandSlam;
        use isideload::util::storage::InMemoryStorage;

        if std::env::var_os("APPARITION_NET").is_none() || super::super::native::source().is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = start("127.0.0.1:0".parse().unwrap(), None).await.unwrap();

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
    }

    /// The anisette VM's anisette (vm.rs) as the companion serves it to a device, through isideload's own client:
    /// provisioning, headers, and a client info that claims the guest. Needs the VM set up (`apparition vm setup`)
    /// and reaches Apple's URL bag: `APPARITION_VM=1`.
    #[cfg(feature = "desktop")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_vm_backend_provisions_and_gets_headers_through_isideloads_client() {
        use isideload::anisette::AnisetteDataGenerator;
        use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
        use isideload::auth::grandslam::GrandSlam;
        use isideload::util::storage::InMemoryStorage;

        if std::env::var_os("APPARITION_VM").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = start_vm("127.0.0.1:0".parse().unwrap(), None, crate::vm::default_bundle()).await.unwrap();
        let info: Value = reqwest::get(format!("{}/v3/client_info", server.url())).await.unwrap().json().await.unwrap();
        assert!(info["client_info"].as_str().unwrap().starts_with("<VirtualMac"), "claims the guest: {info}");
        assert_eq!(info["apparition_anisette"], "mac");

        let provider = RemoteV3AnisetteProvider::new(&server.url(), Box::new(InMemoryStorage::new()), "0".to_string()).unwrap();
        let mut generator = AnisetteDataGenerator::new(Arc::new(tokio::sync::RwLock::new(provider)));
        let client_info = generator.get_client_info().await.unwrap();
        let grandslam = Arc::new(GrandSlam::new(client_info, false, None).await.unwrap());
        let data = generator.get_anisette_data(grandslam).await.unwrap();
        let headers = data.get_headers();
        for name in ["X-Apple-I-MD", "X-Apple-I-MD-M", "X-Mme-Device-Id"] {
            assert!(headers.get(name).is_some_and(|value| value.len() > 8), "{name} missing or empty");
        }
    }

    #[test]
    fn a_headers_request_may_carry_resync_data() {
        let legacy: HeadersRequest = serde_json::from_value(json!({
            "identifier": "AAAAAAAAAAAAAAAAAAAAAA==", "adi_pb": "AA=="
        })).unwrap();
        assert!(legacy.sim.is_none());
        let resync: HeadersRequest = serde_json::from_value(json!({
            "identifier": "AAAAAAAAAAAAAAAAAAAAAA==", "adi_pb": "AA==",
            "sim": "AQID", "device_id": "some-device", "md_lu": "some-local-user"
        })).unwrap();
        assert!(resync.sim.is_some());
    }

    #[test]
    fn identifier_is_the_first_sixteen_characters_of_the_uppercase_uuid() {
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe];
        assert_eq!(adi_identifier(&bytes), "01234567-89AB-CD");
        assert_eq!(adi_identifier(&bytes).len(), 16);
    }
}
