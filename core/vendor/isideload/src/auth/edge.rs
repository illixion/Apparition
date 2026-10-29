//! Apple's edge servers, for when one of them turns a network away.
//!
//! Which edge an Apple host resolves to depends on the resolver asking. Checked 2026-10-29 from one
//! home network: the router's resolver and Google's (8.8.8.8) gave edges for `gsd.apple.com` that
//! answer, Cloudflare's (1.1.1.1) always gave 17.171.11.101, which answers every GrandSlam request
//! with an HTML `403 Forbidden` before Apple reads it. Cloudflare passes on no client subnet, so
//! which edge it hands out depends on where its own server is, and any resolver can land someone
//! on a refusing edge.
//!
//! So the system's resolver is still asked first (it is right for nearly everyone, and a VPN or
//! a workplace network expects it), and only an edge that refuses like that changes anything: it
//! is avoided for an hour, so a sign-in doesn't knock on it again and again, and the request goes
//! to another edge, from the system's answer or, when that holds only avoided ones, from
//! DNS-over-HTTPS. After [`EDGES_PER_REQUEST`] refusing edges the request fails saying so.
//! The connection still names the host, so TLS checks the certificate exactly as before.
//!
//! An edge that can't be connected to, or doesn't answer in time, gets one retry on the other
//! edges DNS-over-HTTPS knows, for that request only: nothing is remembered, as an outage of the
//! person's own network looks the same, and a retry later has to be able to work. An HTTP 5xx is
//! no refusal: GrandSlam answers an occasional 503 to a request that a moment later goes through.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest_middleware::{Middleware, Next};
use tracing::{debug, warn};

/// How long an edge that answered 403 isn't used again.
const AVOID_FOR: Duration = Duration::from_secs(60 * 60);
/// How long the DNS-over-HTTPS answers for a host are reused.
const ALTERNATIVES_FOR: Duration = Duration::from_secs(5 * 60);
/// How long a DNS-over-HTTPS lookup may take, so being offline doesn't wait long to say so.
const DOH_TIMEOUT: Duration = Duration::from_secs(3);
/// Edges one request tries before giving up.
const EDGES_PER_REQUEST: usize = 3;

/// DNS-over-HTTPS JSON queries for other edges, each followed by the host. Google answers for the
/// asker's own subnet, and with no subnet for its own location; Cloudflare for its location.
/// Apple rotates its answers, so each is one more edge to try.
const DOH: [&str; 3] = [
    "https://dns.google/resolve?type=A&name=",
    "https://dns.google/resolve?type=A&edns_client_subnet=0.0.0.0/0&name=",
    "https://cloudflare-dns.com/dns-query?type=A&name=",
];

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    #[error(
        "Apple's servers for {host} turned this network away ({tried} answered 403 Forbidden). Try again in an hour, on another network, or with this computer's DNS set to the router's."
    )]
    Refused { host: String, tried: usize },
    #[error(
        "Every Apple server found for {host} turned this network away in the last hour. Try again later, on another network, or with this computer's DNS set to the router's."
    )]
    AllAvoided { host: String },
}

#[derive(Default)]
struct Edges {
    /// Edges that answered 403, and until when they are avoided.
    avoided: HashMap<IpAddr, Instant>,
    /// The addresses handed out per host, so a 403 through a proxy doesn't mark the proxy.
    handed_out: HashMap<String, Vec<IpAddr>>,
    /// The addresses of the latest lookup per host, which a connection failure tried.
    latest: HashMap<String, Vec<IpAddr>>,
    /// DNS-over-HTTPS answers per host, and when they were fetched.
    alternatives: HashMap<String, (Instant, Vec<IpAddr>)>,
    /// Addresses a retry uses for a host in place of the system's, while it runs.
    pinned: HashMap<String, Vec<IpAddr>>,
}

static EDGES: LazyLock<Mutex<Edges>> = LazyLock::new(Default::default);

fn edges() -> MutexGuard<'static, Edges> {
    EDGES.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_apple(host: &str) -> bool {
    host == "apple.com" || host.ends_with(".apple.com")
}

/// The addresses, in their order, without the edges avoided at `now`.
fn usable(addrs: impl IntoIterator<Item = SocketAddr>, now: Instant) -> Vec<SocketAddr> {
    let edges = edges();
    let mut usable: Vec<SocketAddr> = Vec::new();
    for addr in addrs {
        let avoided = edges.avoided.get(&addr.ip()).is_some_and(|until| *until > now);
        if !avoided && !usable.contains(&addr) {
            usable.push(addr);
        }
    }
    usable
}

fn avoid(ip: IpAddr, now: Instant) {
    let mut edges = edges();
    edges.avoided.retain(|_, until| *until > now);
    edges.avoided.insert(ip, now + AVOID_FOR);
}

fn hand_out(host: &str, addrs: &[SocketAddr]) {
    let mut edges = edges();
    edges.latest.insert(host.to_string(), addrs.iter().map(SocketAddr::ip).collect());
    let known = edges.handed_out.entry(host.to_string()).or_default();
    for addr in addrs {
        if !known.contains(&addr.ip()) {
            known.push(addr.ip());
        }
    }
}

fn was_handed_out(host: &str, ip: IpAddr) -> bool {
    edges().handed_out.get(host).is_some_and(|ips| ips.contains(&ip))
}

/// The IPv4 addresses in a DNS-over-HTTPS JSON answer (CNAMEs skipped).
fn parse_doh(answer: &serde_json::Value) -> Vec<IpAddr> {
    answer
        .get("Answer")
        .and_then(|records| records.as_array())
        .into_iter()
        .flatten()
        .filter(|record| record.get("type").and_then(|t| t.as_u64()) == Some(1))
        .filter_map(|record| record.get("data")?.as_str()?.parse().ok())
        .collect()
}

/// A client per lookup: these are rare, and a shared one would be tied to the first lookup's
/// runtime.
async fn ask_doh(endpoint: &str, host: &str) -> Vec<IpAddr> {
    let client = match reqwest::Client::builder().timeout(DOH_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            debug!("No client for DNS-over-HTTPS: {error}");
            return Vec::new();
        }
    };
    let response = client
        .get(format!("{endpoint}{}", urlencoding::encode(host)))
        .header("Accept", "application/dns-json")
        .send()
        .await;
    match response {
        Ok(response) => match response.json::<serde_json::Value>().await {
            Ok(answer) => parse_doh(&answer),
            Err(error) => {
                debug!("DNS-over-HTTPS answer from {endpoint} unreadable: {error}");
                Vec::new()
            }
        },
        Err(error) => {
            debug!("DNS-over-HTTPS request to {endpoint} failed: {error}");
            Vec::new()
        }
    }
}

/// Other edges for `host`, from DNS-over-HTTPS. An answer is reused for [`ALTERNATIVES_FOR`]
/// while it still holds an edge that isn't avoided; an empty one (every lookup failed) isn't kept.
async fn alternatives(host: &str, now: Instant) -> Vec<IpAddr> {
    // Its own statement, so the lock is released before `usable` takes it again.
    let cached = edges().alternatives.get(host).cloned();
    if let Some((fetched, ips)) = cached
        && now.duration_since(fetched) < ALTERNATIVES_FOR
        && !usable(ips.iter().map(|ip| SocketAddr::new(*ip, 0)), now).is_empty()
    {
        return ips;
    }
    let answers = futures::future::join_all(DOH.iter().map(|endpoint| ask_doh(endpoint, host))).await;
    let mut ips: Vec<IpAddr> = Vec::new();
    for ip in answers.into_iter().flatten() {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    if !ips.is_empty() {
        edges().alternatives.insert(host.to_string(), (now, ips.clone()));
    }
    ips
}

/// The system's resolver, as reqwest's default one asks it.
struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Resolves through another resolver, leaving out avoided Apple edges.
pub struct EdgeResolver {
    inner: Arc<dyn Resolve>,
}

impl EdgeResolver {
    pub fn system() -> Self {
        Self::over(Arc::new(SystemResolver))
    }

    pub fn over(inner: Arc<dyn Resolve>) -> Self {
        Self { inner }
    }
}

impl Resolve for EdgeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let pinned = edges().pinned.get(&host).cloned();
        if let Some(ips) = pinned {
            let addrs: Vec<SocketAddr> = ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            hand_out(&host, &addrs);
            return Box::pin(async move { Ok(Box::new(addrs.into_iter()) as Addrs) });
        }
        let resolving = self.inner.resolve(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = resolving.await?.collect();
            if !is_apple(&host) {
                return Ok(Box::new(addrs.into_iter()) as Addrs);
            }
            let now = Instant::now();
            let mut chosen = usable(addrs, now);
            if chosen.is_empty() {
                let others = alternatives(&host, now).await;
                chosen = usable(others.into_iter().map(|ip| SocketAddr::new(ip, 0)), now);
                debug!("{host}: the system's edges are avoided, {} others found", chosen.len());
            }
            if chosen.is_empty() {
                return Err(Box::new(EdgeError::AllAvoided { host }) as BoxError);
            }
            hand_out(&host, &chosen);
            Ok(Box::new(chosen.into_iter()) as Addrs)
        })
    }
}

/// The edge that refused the request with an HTML 403, if one of ours did.
fn refused_by(response: &reqwest::Response, host: &str) -> Option<IpAddr> {
    if response.status() != reqwest::StatusCode::FORBIDDEN {
        return None;
    }
    let html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/html"));
    let ip = response.remote_addr()?.ip();
    (html && was_handed_out(host, ip)).then_some(ip)
}

/// Whether `error` is a connection that couldn't be made or didn't answer in time.
fn is_unreachable(error: &reqwest_middleware::Error) -> bool {
    matches!(error, reqwest_middleware::Error::Reqwest(error) if error.is_connect() || error.is_timeout())
}

/// While alive, `host` resolves to other edges than the system's.
struct Pinned(String);

impl Drop for Pinned {
    fn drop(&mut self) {
        edges().pinned.remove(&self.0);
    }
}

/// Pins `host` to the edges DNS-over-HTTPS knows apart from the ones the last lookup gave, which
/// are the ones that just failed; `None` when there are none (being offline, say).
async fn fall_back(host: &str) -> Option<Pinned> {
    let now = Instant::now();
    let tried = edges().latest.get(host).cloned().unwrap_or_default();
    let others: Vec<IpAddr> = usable(alternatives(host, now).await.into_iter().map(|ip| SocketAddr::new(ip, 0)), now)
        .into_iter()
        .map(|addr| addr.ip())
        .filter(|ip| !tried.contains(ip))
        .collect();
    if others.is_empty() {
        return None;
    }
    warn!("{host}: no connection to its edge, trying {} others", others.len());
    edges().pinned.insert(host.to_string(), others);
    Some(Pinned(host.to_string()))
}

/// Retries a request an edge refused on another edge, avoiding the refusing one; and one that
/// couldn't connect, once, on the other edges there are.
pub struct EdgeAvoidance;

#[async_trait::async_trait]
impl Middleware for EdgeAvoidance {
    async fn handle(
        &self,
        mut req: reqwest::Request,
        extensions: &mut http::Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        let host = req.url().host_str().unwrap_or_default().to_string();
        if !is_apple(&host) {
            return next.run(req, extensions).await;
        }
        let mut tried = 0;
        // Held until this returns, so the other edges are used for the whole retry.
        let mut pin: Option<Pinned> = None;
        loop {
            let retry = req.try_clone();
            let response = match next.clone().run(req, extensions).await {
                Ok(response) => response,
                Err(error) => {
                    let (Some(again), None, true) = (retry, pin.as_ref(), is_unreachable(&error)) else {
                        return Err(error);
                    };
                    let Some(guard) = fall_back(&host).await else {
                        return Err(error);
                    };
                    pin = Some(guard);
                    req = again;
                    continue;
                }
            };
            let Some(ip) = refused_by(&response, &host) else {
                return Ok(response);
            };
            tried += 1;
            avoid(ip, Instant::now());
            warn!("{host}: edge {ip} answered 403 Forbidden, avoiding it for an hour");
            match retry {
                Some(again) if tried < EDGES_PER_REQUEST => req = again,
                Some(_) => {
                    return Err(reqwest_middleware::Error::middleware(EdgeError::Refused { host, tried }));
                }
                None => return Ok(response),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 0)
    }

    #[test]
    fn avoided_edges_are_left_out_in_order() {
        let now = Instant::now();
        avoid("192.0.2.1".parse().unwrap(), now);
        let chosen = usable([at("192.0.2.3"), at("192.0.2.1"), at("192.0.2.2"), at("192.0.2.3")], now);
        assert_eq!(chosen, [at("192.0.2.3"), at("192.0.2.2")]);
    }

    #[test]
    fn avoidance_ends() {
        let now = Instant::now();
        avoid("192.0.2.10".parse().unwrap(), now);
        assert!(usable([at("192.0.2.10")], now).is_empty());
        assert_eq!(usable([at("192.0.2.10")], now + AVOID_FOR + Duration::from_secs(1)), [at("192.0.2.10")]);
    }

    #[test]
    fn cached_alternatives_are_reused() {
        let now = Instant::now();
        let ips: Vec<IpAddr> = vec!["192.0.2.50".parse().unwrap(), "192.0.2.51".parse().unwrap()];
        edges().alternatives.insert("cache-test.apple.com".to_string(), (now, ips.clone()));
        avoid("192.0.2.50".parse().unwrap(), now);
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(runtime.block_on(alternatives("cache-test.apple.com", now)), ips);
    }

    #[test]
    fn only_apple_hosts() {
        assert!(is_apple("gsd.apple.com") && is_apple("developerservices2.apple.com"));
        assert!(!is_apple("notapple.com") && !is_apple("apple.com.example"));
    }

    #[test]
    fn doh_answers_keep_addresses_only() {
        let answer = serde_json::json!({
            "Status": 0,
            "Answer": [
                {"name": "gsd.apple.com.", "type": 5, "data": "gsd.idms-apple.com.akadns.net."},
                {"name": "gsd.idms-apple.com.akadns.net.", "type": 1, "data": "17.179.252.19"},
                {"name": "gsd.idms-apple.com.akadns.net.", "type": 1, "data": "17.23.96.20"}
            ]
        });
        assert_eq!(parse_doh(&answer), ["17.179.252.19".parse::<IpAddr>().unwrap(), "17.23.96.20".parse().unwrap()]);
        assert!(parse_doh(&serde_json::json!({"Status": 3})).is_empty());
    }

    /// Always answers one address, like a resolver handing out a refusing edge.
    struct Fixed(IpAddr);

    impl Resolve for Fixed {
        fn resolve(&self, _: Name) -> Resolving {
            let addr = SocketAddr::new(self.0, 0);
            Box::pin(async move { Ok(Box::new(std::iter::once(addr)) as Addrs) })
        }
    }

    /// An edge that refuses gets one request, and the next request doesn't reach it at all.
    #[test]
    fn a_refusing_edge_is_asked_once() {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/html\r\nContent-Length: 9\r\nConnection: close\r\n\r\nForbidden",
                );
            }
        });
        let client = reqwest::ClientBuilder::new()
            .dns_resolver(EdgeResolver::over(Arc::new(Fixed("127.0.0.1".parse().unwrap()))))
            .build()
            .unwrap();
        let client = reqwest_middleware::ClientBuilder::new(client).with(EdgeAvoidance).build();
        let url = format!("http://edge-test.invalid.apple.com:{port}/");
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        for _ in 0..2 {
            let error = runtime.block_on(client.post(&url).body("x").send()).unwrap_err();
            let text = format!("{error:?}");
            assert!(text.contains("AllAvoided"), "{text}");
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// Where the system's edge can't be connected to, the request goes to another one DNS-over-HTTPS
    /// knows (put in the cache here), and nothing about it is remembered afterwards.
    #[test]
    fn a_dead_edge_falls_back_to_another_and_forgets() {
        use std::io::{Read, Write};
        let _ = rustls::crypto::ring::default_provider().install_default();
        // The server is on ::1; the system's answer, 127.0.0.1, has nothing on that port.
        let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            }
        });
        let host = "fallback-test.apple.com";
        edges().alternatives.insert(host.to_string(), (Instant::now(), vec!["::1".parse().unwrap()]));
        let client = reqwest::ClientBuilder::new()
            .dns_resolver(EdgeResolver::over(Arc::new(Fixed("127.0.0.1".parse().unwrap()))))
            .build()
            .unwrap();
        let client = reqwest_middleware::ClientBuilder::new(client).with(EdgeAvoidance).build();
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let url = format!("http://{host}:{port}/");
        let response = runtime.block_on(client.post(&url).body("x").send()).unwrap();
        assert_eq!(runtime.block_on(response.text()).unwrap(), "ok");
        assert!(!edges().pinned.contains_key(host), "the pin outlived the request");
        // Nothing is remembered about the dead edge: the next request tries it first and falls back again.
        let again = runtime.block_on(client.post(&url).body("x").send()).unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::OK);
    }

    /// Offline: no other edge is known, so the first error comes back unchanged.
    #[test]
    fn with_no_other_edge_the_connection_error_comes_back() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let host = "offline-test.apple.com";
        // A fresh empty answer is not cached, and the lookups here have no network to reach.
        edges().alternatives.insert(host.to_string(), (Instant::now(), vec!["127.0.0.1".parse().unwrap()]));
        let client = reqwest::ClientBuilder::new()
            .dns_resolver(EdgeResolver::over(Arc::new(Fixed("127.0.0.1".parse().unwrap()))))
            .build()
            .unwrap();
        let client = reqwest_middleware::ClientBuilder::new(client).with(EdgeAvoidance).build();
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let error = runtime.block_on(client.get(format!("http://{host}:1/")).send()).unwrap_err();
        assert!(is_unreachable(&error), "{error:?}");
    }

    /// Against Apple: the system's resolver hands out 17.171.11.101, which refused every GrandSlam
    /// request from a home network in Riga on 2026-10-29. The request should land on another edge
    /// and get Apple's own answer. Needs that edge to still refuse from where it runs.
    ///
    ///     cargo test -p isideload --lib refusing_edge -- --ignored
    #[test]
    #[ignore = "talks to Apple"]
    fn refusing_edge_is_avoided_and_another_answers() {
        use crate::auth::grandslam::APPLE_ROOT_G3;
        use crate::sideload::cert_identity::APPLE_ROOT;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let refusing: IpAddr = "17.171.11.101".parse().unwrap();
        let client = reqwest::ClientBuilder::new()
            .add_root_certificate(reqwest::Certificate::from_der(APPLE_ROOT).unwrap())
            .add_root_certificate(reqwest::Certificate::from_der(APPLE_ROOT_G3).unwrap())
            .http1_title_case_headers()
            .dns_resolver(EdgeResolver::over(Arc::new(Fixed(refusing))))
            .build()
            .unwrap();
        let client = reqwest_middleware::ClientBuilder::new(client).with(EdgeAvoidance).build();
        let body = r#"<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd"><plist version="1.0"><dict><key>Header</key><dict><key>Version</key><string>1.0.1</string></dict><key>Request</key><dict><key>cpd</key><dict/><key>o</key><string>init</string><key>u</key><string>probe-nobody-2026@example.com</string></dict></dict></plist>"#;
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let response = runtime
            .block_on(
                client
                    .post("https://gsd.apple.com/grandslam/GsService2")
                    .header("Content-Type", "text/x-xml-plist")
                    .header("Accept", "text/x-xml-plist")
                    .header("X-Xcode-Version", "27.0 (27A5218g)")
                    .header("X-Apple-App-Info", "com.apple.gs.xcode.auth")
                    .header("User-Agent", "akd/1.0 CFNetwork/808.1.4")
                    .header(
                        "X-Mme-Client-Info",
                        "<Pixel 8><Android; 14; UQ1A.240205.004><com.apple.AuthKit/1 (com.apple.android.music/4.9.6)>",
                    )
                    .header("Connection", "close")
                    .body(body)
                    .send(),
            )
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_ne!(response.remote_addr().map(|addr| addr.ip()), Some(refusing));
        assert!(edges().avoided.contains_key(&refusing));
    }
}
