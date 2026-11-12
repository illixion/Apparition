//! The anisette v3 protocol, served from one of four places (see [`Backend`]).
//!
//! This is the protocol isideload's `RemoteV3AnisetteProvider` speaks (MIT licensed, a dependency
//! of this crate), so what it expects is what's implemented:
//!
//! - `GET /v3/client_info`: the `client_info` and `user_agent` strings to send to Apple;
//! - `GET /v3/provisioning_session` (WebSocket): provisions a client;
//! - `POST /v3/get_headers`: the `X-Apple-I-MD`, `X-Apple-I-MD-M` and `X-Apple-I-MD-RINFO`
//!   headers, and whatever else AuthKit puts on the request.
//!
//! A Mac's own anisette and its VM's answer from macOS (`mac`, macOS only). Apple Music's Android libraries
//! answer from `android` (macOS and Linux, switched by `ANDROID_LIBRARIES`). A third-party server
//! is passed through as it is (`remote`, every platform). `/v3/client_info` of the first two also
//! says what this server serves (`apparition_anisette`: `mac`), with the matching client info,
//! which the sign-in claims (see claimed_platform).

use std::net::SocketAddr;
use std::sync::Arc;
#[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
use std::time::Duration;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rootcause::prelude::*;
use serde_json::Value;

use super::remote::Upstream;

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
    #[cfg(all(feature = "anisette", target_os = "macos"))]
    Native,
    /// This Mac's anisette VM (vm.rs): the same server, run inside the guest, reached through the VM runner's front
    /// door on loopback. Each request makes sure the runner is up (resuming the saved VM); the paired-device check
    /// stays here, on the host.
    #[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
    Vm { bundle: std::path::PathBuf, client: reqwest::Client },
    /// Apple Music's Android libraries, loaded in this process (`android`): where this computer has no Mac's own.
    #[cfg(all(feature = "anisette", unix))]
    Android(super::android::Android),
    /// A third-party server at an address the person gave, on any platform: everything it answers is
    /// passed through, so a device paired with this companion gets that server's anisette.
    Remote(Upstream),
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

/// Serves Apple Music's Android libraries on `bind` (see `android`), fetched into `data_dir` on first use: the
/// address [`crate::ANDROID_ANISETTE`] starts. Fails when [`super::ANDROID_LIBRARIES`] is off.
#[cfg(all(feature = "anisette", unix))]
pub async fn start_android(data_dir: &std::path::Path, bind: SocketAddr, access: Access) -> Result<Server, Report> {
    if !super::ANDROID_LIBRARIES {
        bail!("Apparition's own Android anisette is switched off in this build (anisette::ANDROID_LIBRARIES).");
    }
    let android = super::android::Android::new(data_dir).await?;
    serve(bind, access, Backend::Android(android)).await
}

/// Serves this Mac's anisette on `bind` (use port 0 to take any free one). Several servers can
/// run at once, say one on loopback for this computer's own sign-in and one for the companion's
/// channel. Fails when this Mac doesn't give its anisette (macOS 27 outside a prepared VM).
#[cfg(all(feature = "anisette", target_os = "macos"))]
pub async fn start(bind: SocketAddr, access: Access) -> Result<Server, Report> {
    if !tokio::task::spawn_blocking(|| super::native::source().is_some()).await.unwrap_or(false) {
        bail!("{}", super::mac::UNAVAILABLE);
    }
    serve(bind, access, Backend::Native).await
}

/// Serves the anisette of this Mac's anisette VM in `bundle` (vm.rs) on `bind`, the way [`start`] serves this Mac's
/// own: for a Mac on macOS 27, whose own anisette stays with macOS.
#[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
pub async fn start_vm(bind: SocketAddr, access: Access, bundle: std::path::PathBuf) -> Result<Server, Report> {
    // A request may have to wait for the VM to resume (a second) or, after a crash, to boot (about a minute).
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(150)).build().context("making the VM's client")?;
    serve(bind, access, Backend::Vm { bundle, client }).await
}

/// Serves the third-party anisette server at `address` (see [`super::remote::normalize`]) on `bind`, the way
/// [`start`] serves this Mac's own. Nothing is asked of the server until a device uses it, so a server that is
/// down doesn't stop this from starting.
pub async fn start_remote(bind: SocketAddr, access: Access, address: &str) -> Result<Server, Report> {
    serve(bind, access, Backend::Remote(Upstream::new(address)?)).await
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
    client_info(State(shared)).await
}

async fn token_get_headers(State(shared): State<Arc<Shared>>, UrlPath(token): UrlPath<String>, body: Json<Value>) -> Response {
    if !shared.allows(&token) {
        return unpaired().await.into_response();
    }
    get_headers(State(shared), body).await
}

async fn token_provisioning_session(State(shared): State<Arc<Shared>>, UrlPath(token): UrlPath<String>, upgrade: WebSocketUpgrade) -> Response {
    if !shared.allows(&token) {
        return unpaired().await.into_response();
    }
    provisioning_session(State(shared), upgrade).await
}

async fn client_info(State(shared): State<Arc<Shared>>) -> Response {
    match &shared.backend {
        #[cfg(all(feature = "anisette", target_os = "macos"))]
        Backend::Native => super::mac::client_info().into_response(),
        #[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
        Backend::Vm { bundle, client } => match from_vm(bundle, client, "client_info", None).await {
            Ok(info) => Json(info).into_response(),
            Err(error) => (StatusCode::SERVICE_UNAVAILABLE, format!("{error}\n")).into_response(),
        },
        #[cfg(all(feature = "anisette", unix))]
        Backend::Android(_) => super::android::client_info().into_response(),
        Backend::Remote(upstream) => upstream.client_info().await,
    }
}

/// This Mac's anisette VM's answer to `path` (its own copy of this server, in the guest).
#[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
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

async fn get_headers(State(shared): State<Arc<Shared>>, Json(body): Json<Value>) -> Response {
    match &shared.backend {
        #[cfg(all(feature = "anisette", target_os = "macos"))]
        Backend::Native => Json(super::mac::get_headers(body).await).into_response(),
        #[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
        // The VM's server answers in this same shape, errors included.
        Backend::Vm { bundle, client } => Json(match from_vm(bundle, client, "get_headers", Some(body)).await {
            Ok(reply) => reply,
            // Never "provision" in the message: isideload's client provisions again on that word.
            Err(error) => serde_json::json!({ "result": "GetHeadersError", "message": format!("{error}").replace("rovision", "repar") }),
        })
        .into_response(),
        #[cfg(all(feature = "anisette", unix))]
        Backend::Android(android) => Json(super::android::get_headers(android, body).await).into_response(),
        Backend::Remote(upstream) => upstream.get_headers(body).await,
    }
}

async fn provisioning_session(State(shared): State<Arc<Shared>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| async move {
        let _ = match &shared.backend {
            // A Mac is provisioned by macOS, a VM's too: the same placeholder.
            #[cfg(all(feature = "anisette", target_os = "macos"))]
            Backend::Native => super::mac::provision(socket).await,
            #[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
            Backend::Vm { .. } => super::mac::provision(socket).await,
            #[cfg(all(feature = "anisette", unix))]
            Backend::Android(android) => super::android::provision(android, socket).await,
            Backend::Remote(upstream) => upstream.provision(socket).await,
        };
    })
}

#[cfg(all(test, feature = "anisette", target_os = "macos"))]
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
}
