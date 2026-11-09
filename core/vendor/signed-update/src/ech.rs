//! A client that reaches one host over Encrypted Client Hello: the config comes from the host's
//! HTTPS record, fetched over DNS-over-HTTPS, and the connection goes to the addresses in it. The
//! name is never looked up by the system and never sent in the clear: the network sees DNS-over-HTTPS
//! to an IP, then a TLS handshake naming Cloudflare's shared `cloudflare-ech.com`.
//!
//! Enforced, not best effort: with ECH enabled, rustls fails a handshake the server doesn't accept
//! rather than carrying on in the clear, so a block or a stale key is an error and the caller takes
//! the next source.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::{EchConfig, EchMode};
use rustls::crypto::aws_lc_rs;
use rustls::pki_types::EchConfigListBytes;
use rustls::{ClientConfig, RootCertStore};

use crate::dns;
use crate::error::{Error, Result, fail};

pub(crate) async fn client(host: &str, doh: &[String], user_agent: &str) -> Result<reqwest::Client> {
    let record = dns::lookup(host, doh, user_agent).await?;
    let Some(list) = record.ech else {
        fail!("{host} publishes no ECH config");
    };
    if record.addrs.is_empty() {
        fail!("{host} has no address");
    }
    build(host, list, &record.addrs, user_agent)
}

/// The client for `host` with ECH config `list`, connecting to `addrs`.
fn build(host: &str, list: Vec<u8>, addrs: &[std::net::IpAddr], user_agent: &str) -> Result<reqwest::Client> {
    let ech = EchConfig::new(EchConfigListBytes::from(list), aws_lc_rs::hpke::ALL_SUPPORTED_SUITES)
        .map_err(|e| format!("{host}'s ECH config isn't usable: {e}"))?;
    let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let mut tls = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
        .with_ech(EchMode::Enable(ech))
        .map_err(|e| format!("ECH isn't available: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let addrs: Vec<SocketAddr> = addrs.iter().map(|ip| SocketAddr::new(*ip, 443)).collect();
    reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        // A proxy would be told the name in its CONNECT, and would look it up itself; plain http
        // would put it in the Host header in the clear.
        .no_proxy()
        .https_only(true)
        .resolve_to_addrs(host, &addrs)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::new(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "signed-update-test";

    fn live_doh() -> Vec<String> {
        vec!["https://1.1.1.1/dns-query".into(), "https://1.0.0.1/dns-query".into()]
    }

    #[tokio::test]
    async fn a_blocked_resolver_is_an_error_not_a_fallback() {
        let error = client("apps.illixion.com", &["https://127.0.0.1:1/dns-query".into()], UA).await.unwrap_err().to_string();
        assert!(error.contains("no DNS-over-HTTPS resolver answered"), "{error}");
        let none = client("apps.illixion.com", &[], UA).await.unwrap_err().to_string();
        assert!(none.contains("no DNS-over-HTTPS resolver answered"), "{none}");
    }

    /// Needs the network: `cargo test --features ech -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn live_a_host_without_ech_is_refused() {
        let error = client("github.com", &live_doh(), UA).await.unwrap_err().to_string();
        // github.com publishes no HTTPS record at all, so there is no ECH config to be had.
        assert!(error.contains("no HTTPS record") || error.contains("no ECH config"), "{error}");
    }

    /// A proxy that forwards one connection to `upstream` and keeps the first bytes the client sent.
    async fn capturing_proxy(upstream: SocketAddr) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let first_flight = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = first_flight.clone();
        tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut hello = vec![0u8; 16384];
            let n = client.read(&mut hello).await.unwrap();
            hello.truncate(n);
            *seen.lock().unwrap() = hello.clone();
            let mut server = TcpStream::connect(upstream).await.unwrap();
            server.write_all(&hello).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
        });
        (port, first_flight)
    }

    /// The claim, on the wire: the first flight of the handshake, as a proxy between the client and
    /// Cloudflare sees it, names Cloudflare's shared ECH host and nothing of ours.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn live_the_wire_shows_no_illixion() {
        let record = dns::lookup("apps.illixion.com", &live_doh(), UA).await.unwrap();
        let (port, first_flight) = capturing_proxy(SocketAddr::new(record.addrs[0], 443)).await;
        // The URL's port is the proxy's; the name in it is the one that must not leave the machine.
        let client = build("apps.illixion.com", record.ech.unwrap(), &[std::net::Ipv4Addr::LOCALHOST.into()], UA).unwrap();
        let response = client.get(format!("https://apps.illixion.com:{port}/")).send().await.expect("a handshake through the proxy");
        assert!(response.status().as_u16() < 500, "{}", response.status());
        let wire = first_flight.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&wire).to_ascii_lowercase();
        assert!(wire.len() > 200 && wire[0] == 0x16, "a TLS handshake record, {} bytes", wire.len());
        assert!(!text.contains("illixion"), "the first flight names the host");
        assert!(!text.contains("apps."), "the first flight names the host");
        assert!(text.contains("cloudflare-ech.com"), "the outer name is Cloudflare's shared one");
    }

    /// The control: through the same proxy, an ordinary client does show the name, so the test above
    /// can fail.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn live_control_an_ordinary_client_names_the_host() {
        let record = dns::lookup("apps.illixion.com", &live_doh(), UA).await.unwrap();
        let (port, first_flight) = capturing_proxy(SocketAddr::new(record.addrs[0], 443)).await;
        let client = reqwest::Client::builder()
            .resolve("apps.illixion.com", SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port))
            .build()
            .unwrap();
        let _ = client.get(format!("https://apps.illixion.com:{port}/")).send().await.expect("an ordinary handshake");
        let text = String::from_utf8_lossy(&first_flight.lock().unwrap()).to_ascii_lowercase();
        assert!(text.contains("apps.illixion.com"), "without ECH the SNI is in the clear");
    }

    /// With ECH enabled rustls fails a handshake the server doesn't accept, so any HTTP answer at
    /// all means the connection was made with the name encrypted.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn live_apps_illixion_com_accepts_ech() {
        let client = client("apps.illixion.com", &live_doh(), UA).await.expect("a private client");
        let response = client.get("https://apps.illixion.com/").send().await.expect("a handshake the server accepted");
        assert!(response.status().as_u16() < 500, "{}", response.status());
    }
}
