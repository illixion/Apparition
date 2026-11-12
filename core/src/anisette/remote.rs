//! A third-party anisette server, offered through the companion on any platform.
//!
//! The person gives the address of an anisette v3 server (one they run, or someone else's), and the
//! companion passes what devices paired with it ask for through to that server: `client_info`,
//! `get_headers` and the `provisioning_session` WebSocket. Nothing is changed on the way, so a device
//! gets what it would get from the server itself, claims included (see claimed_platform: the claim
//! follows the reply and its `Implementation-Version` header, which are passed on).
//!
//! Only anisette requests go through. The Apple ID and password never reach the companion: the
//! device sends them straight to Apple, or through the companion's opaque, encrypted Apple relay.

use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use rootcause::prelude::*;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;

/// What to tell the person about an address that can't be a server's.
pub const ADDRESS_HELP: &str = "Give the anisette server's address, like https://ani.example.com or http://localhost:6970";

/// How long one request to the server may take, and the whole of a provisioning session.
const REQUEST: Duration = Duration::from_secs(60);
const SESSION: Duration = Duration::from_secs(300);
/// The most a server may answer a request with: its replies are a few hundred bytes.
const LIMIT: usize = 1 << 20;

/// Whether `host` is a name or address on this computer or its own network, which no one outside can reach: where
/// a missing scheme means `http://` (a Docker server on `localhost:6970`, a Mac at `192.168.1.20:6969`) rather
/// than `https://`.
fn is_local_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".localhost") || host.ends_with(".lan") || !host.contains('.') && !host.contains(':') {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local() || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1])),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// `address` as the server's base address (no trailing slash, no doubled slashes), or why it can't be one. The
/// address is kept in the settings and shown on screen, so it carries no username or password. With no scheme
/// (`localhost:6970`, `ani.example.com`) it is `http://` for an address on this computer or its network and
/// `https://` for any other, so nothing identifying leaves in the clear by accident.
pub fn normalize(address: &str) -> Result<String, &'static str> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err(ADDRESS_HELP);
    }
    let full = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else if trimmed.contains("://") {
        return Err(ADDRESS_HELP);
    } else {
        let authority = trimmed.split(['/', '?', '#']).next().unwrap_or("");
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = if host.starts_with('[') { host.split(']').next().unwrap_or(host) } else { host.split(':').next().unwrap_or(host) };
        format!("{}://{trimmed}", if is_local_host(host) { "http" } else { "https" })
    };
    let url = reqwest::Url::parse(&full).map_err(|_| ADDRESS_HELP)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ADDRESS_HELP);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("Leave the username and password out of the anisette server's address.");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("The anisette server's address ends at its host or path: leave out anything after a ? or #.");
    }
    // A doubled slash in the path makes `https://server//v3/client_info`, which servers answer with a 404.
    let mut base = url.as_str().trim_end_matches('/').to_string();
    let after_scheme = base.find("://").map(|at| at + 3).unwrap_or(0);
    while let Some(at) = base[after_scheme..].find("//") {
        base.remove(after_scheme + at);
    }
    Ok(base)
}

/// The third-party server at `base`.
pub struct Upstream {
    base: String,
    client: reqwest::Client,
}

impl Upstream {
    /// For the server at `address` (see [`normalize`]).
    pub fn new(address: &str) -> Result<Upstream, Report> {
        let base = normalize(address).map_err(|why| report!("{why}"))?;
        let client = reqwest::Client::builder().timeout(REQUEST).build().context("making the anisette server's client")?;
        Ok(Upstream { base, client })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/v3/{path}", self.base)
    }

    /// The server's own `client_info`, with the header the claim reads.
    pub async fn client_info(&self) -> Response {
        match self.pass(self.client.get(self.url("client_info"))).await {
            Ok(reply) => reply,
            Err(error) => (StatusCode::BAD_GATEWAY, format!("{error}\n")).into_response(),
        }
    }

    /// The server's answer to a `get_headers` request, as it gave it.
    pub async fn get_headers(&self, body: Value) -> Response {
        match self.pass(self.client.post(self.url("get_headers")).json(&body)).await {
            Ok(reply) => reply,
            // In the shape a v3 server answers errors in. Never "provision" in the message:
            // isideload's client provisions again on that word.
            Err(error) => Json(json!({ "result": "GetHeadersError", "message": format!("{error}").replace("rovision", "repar") })).into_response(),
        }
    }

    /// Sends `request` and returns the server's reply as it came: status, type, body, and the
    /// `Implementation-Version` header some servers are told apart by.
    async fn pass(&self, request: reqwest::RequestBuilder) -> Result<Response, Report> {
        let mut reply = request.send().await.context(format!("Couldn't reach the anisette server at {}", self.base))?;
        let mut body = Vec::new();
        while let Some(chunk) = reply.chunk().await.context("The anisette server's answer broke off")? {
            body.extend_from_slice(&chunk);
            if body.len() > LIMIT {
                bail!("The anisette server's answer is far larger than anisette is");
            }
        }
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = StatusCode::from_u16(reply.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        for name in ["content-type", "implementation-version"] {
            if let Some(value) = reply.headers().get(name).and_then(|value| HeaderValue::from_bytes(value.as_bytes()).ok()) {
                response.headers_mut().insert(HeaderName::from_static(name), value);
            }
        }
        Ok(response)
    }

    /// Bridges the client's provisioning session to the server's: every text frame goes across as it is,
    /// both ways, until either side closes. The server provisions the client for real here, so
    /// what it hands back (the `adi.pb`) is the client's own, kept by the client.
    pub async fn provision(&self, mut client: WebSocket) -> Result<(), Report> {
        let url = match self.base.strip_prefix("https://") {
            Some(rest) => format!("wss://{rest}/v3/provisioning_session"),
            None => format!("ws://{}/v3/provisioning_session", self.base.strip_prefix("http://").unwrap_or(&self.base)),
        };
        let (mut server, _) = tokio::time::timeout(Duration::from_secs(30), tokio_tungstenite::connect_async(url))
            .await
            .context("The anisette server's provisioning socket didn't answer")?
            .context("Couldn't open the anisette server's provisioning socket")?;
        let bridged = tokio::time::timeout(SESSION, async {
            loop {
                tokio::select! {
                    heard = client.recv() => match heard {
                        Some(Ok(Message::Text(text))) => server.send(UpstreamMessage::Text(text.as_str().into())).await.context("the server went away")?,
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        // Pings are answered by the libraries; nothing else is part of the protocol.
                        Some(Ok(_)) => {}
                    },
                    heard = server.next() => match heard {
                        Some(Ok(UpstreamMessage::Text(text))) => client.send(Message::text(text.as_str())).await.context("the client went away")?,
                        Some(Ok(UpstreamMessage::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                }
            }
            Ok::<(), Report>(())
        })
        .await;
        let _ = server.close(None).await;
        let _ = client.send(Message::Close(None)).await;
        bridged.unwrap_or(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::WebSocketUpgrade;
    use axum::extract::ws::Message as ServedMessage;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    use super::*;
    use crate::anisette::start_remote;

    #[test]
    fn addresses_are_normalised_or_refused() {
        assert_eq!(normalize(" https://ani.example.com/ ").unwrap(), "https://ani.example.com");
        assert_eq!(normalize("http://192.168.1.20:6969").unwrap(), "http://192.168.1.20:6969");
        assert_eq!(normalize("https://example.com/anisette/").unwrap(), "https://example.com/anisette");
        // No scheme: http for this computer's or its network's addresses, https for any other.
        assert_eq!(normalize("localhost:6970").unwrap(), "http://localhost:6970");
        assert_eq!(normalize("192.168.1.20:6969/").unwrap(), "http://192.168.1.20:6969");
        assert_eq!(normalize("100.69.196.5:6969").unwrap(), "http://100.69.196.5:6969");
        assert_eq!(normalize("my-mac.local:6969").unwrap(), "http://my-mac.local:6969");
        assert_eq!(normalize("ani.example.com").unwrap(), "https://ani.example.com");
        assert_eq!(normalize("http://192.0.2.10:6969//proxy//").unwrap(), "http://192.0.2.10:6969/proxy");
        for refused in ["", "ftp://example.com", "https://", "https://user:secret@example.com", "https://example.com/?key=1", "https://example.com/#x"] {
            assert!(normalize(refused).is_err(), "{refused:?} should be refused");
        }
    }

    /// A stand-in for someone else's server: Dadoum's `Implementation-Version`, an Android client info,
    /// headers that echo the identifier the client sent, and a provisioning socket that asks for it.
    async fn third_party() -> std::net::SocketAddr {
        async fn client_info() -> impl IntoResponse {
            (
                [("implementation-version", "anisette-v3-server v2.2.2")],
                Json(json!({ "client_info": "<Pixel 8><Android; 14; UQ1A><com.apple.AuthKit/1 (com.apple.android.music/4.9.6)>", "user_agent": "akd/1.0" })),
            )
        }
        async fn headers(Json(body): Json<Value>) -> impl IntoResponse {
            if body["identifier"] == "refuse" {
                return (StatusCode::UNAUTHORIZED, Json(json!({ "result": "GetHeadersError", "message": "no" })));
            }
            (StatusCode::OK, Json(json!({ "result": "Headers", "X-Apple-I-MD": "otp", "X-Apple-I-MD-M": "machine", "echo": body["identifier"] })))
        }
        async fn provisioning(upgrade: WebSocketUpgrade) -> Response {
            upgrade.on_upgrade(|mut socket| async move {
                let _ = socket.send(ServedMessage::text(r#"{"result":"GiveIdentifier"}"#)).await;
                if let Some(Ok(ServedMessage::Text(text))) = socket.recv().await {
                    let identifier = serde_json::from_str::<Value>(text.as_str()).unwrap()["identifier"].clone();
                    let _ = socket.send(ServedMessage::text(json!({ "result": "ProvisioningSuccess", "adi_pb": identifier }).to_string())).await;
                }
            })
        }
        let app = Router::new()
            .route("/v3/client_info", get(client_info))
            .route("/v3/get_headers", post(headers))
            .route("/v3/provisioning_session", get(provisioning));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        address
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_paired_device_gets_the_third_party_servers_answers_unchanged() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let upstream = third_party().await;
        let access: crate::anisette::Access = Some(Arc::new(|token: &str| token == "paired"));
        let proxy = start_remote("127.0.0.1:0".parse().unwrap(), access, &format!("http://{upstream}/")).await.unwrap();
        let base = proxy.url();
        let client = reqwest::Client::new();

        // Only a paired device's path is served.
        let refused = client.get(format!("{base}/t/stranger/v3/client_info")).send().await.unwrap();
        assert_eq!(refused.status(), 401);
        assert_eq!(client.get(format!("{base}/v3/client_info")).send().await.unwrap().status(), 401);

        // The claim reads this header and the client info.
        let info = client.get(format!("{base}/t/paired/v3/client_info")).send().await.unwrap();
        assert_eq!(info.status(), 200);
        assert_eq!(info.headers()["implementation-version"], "anisette-v3-server v2.2.2");
        let info: Value = info.json().await.unwrap();
        assert!(info["client_info"].as_str().unwrap().contains("<Android"));

        let headers: Value = client.post(format!("{base}/t/paired/v3/get_headers")).json(&json!({ "identifier": "abc" })).send().await.unwrap().json().await.unwrap();
        assert_eq!(headers["X-Apple-I-MD"], "otp");
        assert_eq!(headers["echo"], "abc");

        // An error status and body from the server arrive as they were.
        let refused = client.post(format!("{base}/t/paired/v3/get_headers")).json(&json!({ "identifier": "refuse" })).send().await.unwrap();
        assert_eq!(refused.status(), 401);
        assert_eq!(refused.json::<Value>().await.unwrap()["message"], "no");

        // Provisioning: the client's frames reach the server and its frames come back.
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{}/t/paired/v3/provisioning_session", proxy.address)).await.unwrap();
        let text = |message: Option<Result<UpstreamMessage, _>>| match message {
            Some(Ok(UpstreamMessage::Text(text))) => serde_json::from_str::<Value>(text.as_str()).unwrap(),
            other => panic!("expected a text frame, got {:?}", other.map(|result| result.is_ok())),
        };
        assert_eq!(text(socket.next().await)["result"], "GiveIdentifier");
        socket.send(UpstreamMessage::Text(r#"{"identifier":"AAAA"}"#.into())).await.unwrap();
        let done = text(socket.next().await);
        assert_eq!(done["result"], "ProvisioningSuccess");
        assert_eq!(done["adi_pb"], "AAAA");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_that_is_down_is_reported_not_hidden() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Nothing listens on the address a closed listener leaves behind.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = closed.local_addr().unwrap();
        drop(closed);
        let proxy = start_remote("127.0.0.1:0".parse().unwrap(), None, &format!("http://{address}")).await.unwrap();
        let client = reqwest::Client::new();

        let info = client.get(format!("{}/v3/client_info", proxy.url())).send().await.unwrap();
        assert_eq!(info.status(), 502);
        assert!(info.text().await.unwrap().contains("Couldn't reach the anisette server"));

        let headers: Value = client.post(format!("{}/v3/get_headers", proxy.url())).json(&json!({})).send().await.unwrap().json().await.unwrap();
        assert_eq!(headers["result"], "GetHeadersError");
    }
}
