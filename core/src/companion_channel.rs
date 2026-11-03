//! Paired-device TCP channels for companion capabilities.
//!
//! This is the Rust host implementation of `RAVECompanionChannel` for the desktop app and the
//! CLI. TLS 1.2-PSK authenticates the per-device credential; the first length-prefixed JSON
//! message proves the device ID against the TLS exporter, matching RAVESDK's channel protocol.
//! Once identified, HTTP requests reach the local anisette v3 service; CONNECT requests
//! forward opaque Apple TLS. Apple certificates and credentials are handled on the device.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use hkdf::Hkdf;
use openssl::error::ErrorStack;
use openssl::ssl::{SslAcceptor, SslMethod, SslVersion};
use rootcause::prelude::*;
use serde::Deserialize;
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio_openssl::SslStream as AsyncSslStream;

use crate::companion::PairedDevices;

pub(crate) const CHANNEL_IDENTITY: &str = "Apparition/Anisette";
pub(crate) const CHANNEL_SALT: &[u8] = b"Apparition-Anisette-v1";
pub(crate) const PROOF_SALT: &[u8] = b"Apparition-Device-Proof-v1";
pub(crate) const EXPORTER_LABEL: &str = "EXPORTER-Apparition-Anisette-Device-v1";
pub(crate) const CIPHER: &str = "PSK-AES128-GCM-SHA256";
const MAX_IDENTIFY: usize = 16 * 1024;
const MAX_HTTP_HEADER: usize = 32 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identify {
    #[serde(rename = "deviceID")]
    device_id: String,
    #[serde(with = "base64_bytes")]
    proof: Vec<u8>,
}

mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let value = String::deserialize(deserializer)?;
        STANDARD.decode(value).map_err(serde::de::Error::custom)
    }
}

/// Starts the encrypted anisette tunnel listener. The advertised port is supplied by the caller.
pub async fn start(
    bind: SocketAddr,
    anisette: SocketAddr,
    devices: Arc<PairedDevices>,
    enabled: Arc<AtomicBool>,
    paired_path: Arc<AtomicBool>,
) -> Result<(u16, tokio::task::JoinHandle<()>), Report> {
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server())
        .context("couldn't configure the companion TLS listener")?;
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_max_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_cipher_list(CIPHER)?;
    let keys = devices.clone();
    builder.set_psk_server_callback(move |_ssl, identity, output| {
        let identity = identity.and_then(|bytes| std::str::from_utf8(bytes).ok());
        let Some(device_id) =
            identity.and_then(|value| value.strip_prefix(&format!("{CHANNEL_IDENTITY}/")))
        else {
            return Err(ErrorStack::get());
        };
        let Some(token) = keys.channel_token(device_id) else {
            return Err(ErrorStack::get());
        };
        let key = derive(token.as_bytes(), CHANNEL_SALT, b"psk", 32);
        if output.len() < key.len() {
            return Err(ErrorStack::get());
        }
        output[..key.len()].copy_from_slice(&key);
        Ok(key.len())
    });
    let acceptor = Arc::new(builder.build());
    let listener = TcpListener::bind(bind)
        .await
        .context("couldn't listen for secure companion channels")?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, peer)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let devices = devices.clone();
            let enabled = enabled.clone();
            let paired_path = paired_path.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    handle(stream, acceptor, anisette, devices, enabled, paired_path).await
                {
                    tracing::debug!(peer = %peer, error = %error, "companion channel closed");
                }
            });
        }
    });
    Ok((port, task))
}

async fn handle(
    stream: TcpStream,
    acceptor: Arc<SslAcceptor>,
    anisette: SocketAddr,
    devices: Arc<PairedDevices>,
    enabled: Arc<AtomicBool>,
    paired_path: Arc<AtomicBool>,
) -> Result<(), Report> {
    let ssl = openssl::ssl::Ssl::new(acceptor.context())?;
    let mut tls = AsyncSslStream::new(ssl, stream)?;
    tokio::time::timeout(Duration::from_secs(10), Pin::new(&mut tls).accept())
        .await
        .context("TLS-PSK handshake timed out")?
        .context("TLS-PSK handshake failed")?;
    if !enabled.load(Ordering::Acquire) {
        bail!("anisette provider is not enabled");
    }
    let identity = tls
        .ssl()
        .psk_identity()
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.strip_prefix(&format!("{CHANNEL_IDENTITY}/")))
        .ok_or_else(|| report!("TLS identity is missing"))?
        .to_owned();
    let bytes = tokio::time::timeout(Duration::from_secs(10), async {
        let mut length = [0u8; 4];
        tls.read_exact(&mut length)
            .await
            .context("device identification is missing")?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > MAX_IDENTIFY {
            bail!("invalid identify message length");
        }
        let mut bytes = vec![0; length];
        tls.read_exact(&mut bytes)
            .await
            .context("device identification is incomplete")?;
        Ok::<_, Report>(bytes)
    })
    .await
    .context("device identification timed out")??;
    let identify: Identify =
        serde_json::from_slice(&bytes).context("invalid device identification")?;
    if identify.device_id != identity {
        bail!("TLS identity and device ID disagree");
    }
    let token = devices
        .channel_token(&identity)
        .ok_or_else(|| report!("device is no longer paired"))?;
    if !devices.channel_allows_anisette(&identity) {
        bail!("device has no anisette grant");
    }
    let mut binding = [0u8; 32];
    tls.ssl()
        .export_keying_material(&mut binding, EXPORTER_LABEL, Some(&[]))?;
    let proof_key = derive(token.as_bytes(), PROOF_SALT, b"", 32);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &proof_key);
    if ring::hmac::verify(&key, &binding, &identify.proof).is_err() {
        bail!("device proof failed");
    }

    let mut header = tokio::time::timeout(Duration::from_secs(15), async {
        let mut header = Vec::with_capacity(1024);
        loop {
            if header.len() >= MAX_HTTP_HEADER {
                bail!("anisette request header is too large");
            }
            let mut byte = [0u8; 1];
            tls.read_exact(&mut byte)
                .await
                .context("empty anisette request")?;
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        Ok::<_, Report>(header)
    })
    .await
    .context("anisette request header timed out")??;
    if header.starts_with(b"CONNECT ") {
        if !devices.channel_allows(&identity, "appleRelay") {
            tls.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await?;
            bail!("pair this device again to grant Apple TLS relay access");
        }
        let host = apple_connect_host(&header)?;
        let mut target = tokio::time::timeout(
            Duration::from_secs(10), TcpStream::connect((host, 443)),
        ).await.context("Apple relay connection timed out")?
            .context("Apple relay connection failed")?;
        tls.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        // The inner TLS is owned by the device. Never inspect or terminate it here.
        copy_bidirectional(&mut tls, &mut target).await.context("Apple TLS relay closed")?;
        return Ok(());
    }
    let mut target = TcpStream::connect(anisette)
        .await.context("anisette provider is unavailable")?;
    prepare_http_request(
        &mut header,
        paired_path
            .load(Ordering::Acquire)
            .then_some(token.as_str()),
    )?;
    target.write_all(&header).await?;
    copy_bidirectional(&mut tls, &mut target)
        .await
        .context("anisette tunnel failed")?;
    Ok(())
}

/// Only the Apple services used by GrandSlam and developer signing, on standard HTTPS.
/// No IP literals, alternate ports, URL userinfo, or host suffix matching.
fn apple_connect_host(header: &[u8]) -> Result<&str, Report> {
    let line = std::str::from_utf8(header)?.split("\r\n").next().unwrap_or_default();
    let parts: Vec<_> = line.split(' ').collect();
    if parts.len() != 3 || parts[0] != "CONNECT" || parts[2] != "HTTP/1.1" {
        bail!("invalid Apple CONNECT request");
    }
    let host = parts[1].strip_suffix(":443").ok_or_else(|| report!("Apple relay requires port 443"))?;
    if !matches!(host, "gsa.apple.com" | "gsd.apple.com" | "idmsa.apple.com" | "developerservices2.apple.com") {
        bail!("host is not allowed by the Apple TLS relay");
    }
    Ok(host)
}

fn prepare_http_request(header: &mut Vec<u8>, token: Option<&str>) -> Result<(), Report> {
    let line_end = header
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| report!("invalid anisette HTTP request line"))?;
    let line = std::str::from_utf8(&header[..line_end]).context("invalid anisette request line")?;
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty() || !path.starts_with('/') || !version.starts_with("HTTP/") {
        bail!("invalid anisette HTTP request line");
    }
    let path = match token {
        Some(token) if !path.starts_with("/t/") => format!("/t/{token}{path}"),
        _ => path.to_string(),
    };
    let request_line = format!("{method} {path} {version}\r\n");
    let rest = &header[line_end + 2..];
    let upgrade = rest.split(|byte| *byte == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let lower = line
            .iter()
            .map(|byte| byte.to_ascii_lowercase())
            .collect::<Vec<_>>();
        lower.starts_with(b"upgrade:")
            || lower.starts_with(b"connection:")
                && lower.windows(7).any(|window| window == b"upgrade")
    });
    let mut rewritten = request_line.into_bytes();
    for line in rest.split(|byte| *byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let lower = line
            .iter()
            .map(|byte| byte.to_ascii_lowercase())
            .collect::<Vec<_>>();
        if !upgrade && lower.starts_with(b"connection:") {
            continue;
        }
        rewritten.extend_from_slice(line);
        rewritten.extend_from_slice(b"\r\n");
    }
    if !upgrade {
        rewritten.extend_from_slice(b"Connection: close\r\n");
    }
    rewritten.extend_from_slice(b"\r\n");
    *header = rewritten;
    Ok(())
}

pub(crate) fn derive(input: &[u8], salt: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let mut output = vec![0; length];
    Hkdf::<Sha256>::new(Some(salt), input)
        .expand(info, &mut output)
        .expect("valid HKDF length");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::ssl::{SslConnector, SslVerifyMode};

    #[test]
    fn connect_restricts_hosts_ports_and_syntax() {
        for host in ["gsa.apple.com", "gsd.apple.com", "idmsa.apple.com", "developerservices2.apple.com"] {
            let request = format!("CONNECT {host}:443 HTTP/1.1\r\n\r\n");
            assert_eq!(apple_connect_host(request.as_bytes()).unwrap(), host);
        }
        for target in ["127.0.0.1:443", "[::1]:443", "gsa.apple.com:80", "gsa.apple.com.evil:443",
                       "evil@gsa.apple.com:443", "gsa.apple.com.:443", "apple.com:443", "gsa.apple.com:443/", "GSA.APPLE.COM:443"] {
            assert!(apple_connect_host(format!("CONNECT {target} HTTP/1.1\r\n\r\n").as_bytes()).is_err());
        }
        assert!(apple_connect_host(b"GET gsa.apple.com:443 HTTP/1.1\r\n\r\n").is_err());
    }

    /// Uses the actual SDK encoder and Network.framework TLS exporter, rather than a Rust
    /// client repeating the server's own assumptions. Run explicitly on the development Mac.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires Swift toolchain and sibling RAVESDK"]
    async fn sdk_swift_client_interoperates() {
        let root = tempfile::tempdir().unwrap();
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let sources = repo.parent().unwrap().join("RAVESDK/Sources/RAVECompanion");
        let executable = root.path().join("client");
        let output = tokio::process::Command::new("swiftc")
            .arg(sources.join("RAVECompanionNamespace.swift"))
            .arg(sources.join("RAVECompanionCredential.swift"))
            .arg(repo.join("scripts/tests/companion-channel-client.swift"))
            .arg("-o").arg(&executable).output().await.unwrap();
        assert!(output.status.success(), "Swift compile: {}", String::from_utf8_lossy(&output.stderr));
        std::fs::write(root.path().join("paired-devices.json"), serde_json::to_vec(&serde_json::json!({
            "devices": [{"id": "test-device", "name": "Test", "token": "test-only-pairing-token", "paired": 0, "grants": ["anisette"]}]
        })).unwrap()).unwrap();
        let devices = Arc::new(PairedDevices::load(root.path()));
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_addr = provider.local_addr().unwrap();
        let (port, server) = start("127.0.0.1:0".parse().unwrap(), provider_addr, devices,
            Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true))).await.unwrap();
        let provider_task = tokio::spawn(async move {
            let (mut stream, _) = provider.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
                assert!(header.len() < 4096);
            }
            assert!(header.starts_with(b"GET /t/test-only-pairing-token/v3/client_info HTTP/1.1"));
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        let output = tokio::process::Command::new(executable).arg(port.to_string()).output().await.unwrap();
        server.abort();
        if !output.status.success() { provider_task.abort(); }
        assert!(output.status.success(), "Swift channel: {} {}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        provider_task.await.unwrap();
    }

    #[tokio::test]
    async fn encrypted_channel_checks_proof_grants_and_forwards_http() {
        let root = tempfile::tempdir().unwrap();
        let token = "test-only-pairing-token";
        std::fs::write(root.path().join("paired-devices.json"), serde_json::to_vec(&serde_json::json!({
            "devices": [{"id": "test-device", "name": "Test", "token": token, "paired": 0, "grants": ["anisette"]}]
        })).unwrap()).unwrap();
        let devices = Arc::new(PairedDevices::load(root.path()));
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_addr = provider.local_addr().unwrap();
        let (port, server) = start("127.0.0.1:0".parse().unwrap(), provider_addr, devices,
            Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true))).await.unwrap();
        let client = |valid_proof: bool| async move {
            let mut builder = SslConnector::builder(SslMethod::tls_client()).unwrap();
            builder.set_verify(SslVerifyMode::NONE);
            builder.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            builder.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
            builder.set_cipher_list(CIPHER).unwrap();
            builder.set_psk_client_callback(move |_, _, identity, output| {
                let id = b"Apparition/Anisette/test-device\0";
                identity[..id.len()].copy_from_slice(id);
                let key = derive(token.as_bytes(), CHANNEL_SALT, b"psk", 32);
                output[..key.len()].copy_from_slice(&key);
                Ok(key.len())
            });
            let ssl = builder.build().configure().unwrap().into_ssl("localhost").unwrap();
            let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let mut tls = AsyncSslStream::new(ssl, stream).unwrap();
            Pin::new(&mut tls).connect().await.unwrap();
            let mut binding = [0u8; 32];
            tls.ssl().export_keying_material(&mut binding, EXPORTER_LABEL, Some(&[])).unwrap();
            let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &derive(token.as_bytes(), PROOF_SALT, b"", 32));
            let mut proof = ring::hmac::sign(&key, &binding).as_ref().to_vec();
            if !valid_proof { proof[0] ^= 1; }
            use base64::Engine;
            let body = serde_json::to_vec(&serde_json::json!({"deviceID": "test-device", "proof": base64::engine::general_purpose::STANDARD.encode(proof)})).unwrap();
            tls.write_all(&(body.len() as u32).to_le_bytes()).await.unwrap();
            tls.write_all(&body).await.unwrap();
            tls
        };
        let mut bad = client(false).await;
        let mut bytes = [0; 256];
        let result = tokio::time::timeout(Duration::from_secs(2), bad.read(&mut bytes)).await.unwrap();
        assert!(result.is_err() || result.unwrap() == 0, "invalid proof was accepted");
        let mut tls = client(true).await;
        tls.write_all(b"CONNECT gsa.apple.com:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let length = tls.read(&mut bytes).await.unwrap();
        assert!(bytes[..length].starts_with(b"HTTP/1.1 403"), "missing grant accepted");
        assert!(tokio::time::timeout(Duration::from_millis(100), provider.accept()).await.is_err());
        let mut tls = client(true).await;
        tls.write_all(b"GET /v3/client_info HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
        let (mut upstream, _) = tokio::time::timeout(Duration::from_secs(2), provider.accept()).await.unwrap().unwrap();
        let mut request = vec![0; 1024];
        let length = upstream.read(&mut request).await.unwrap();
        assert!(request[..length].starts_with(b"GET /t/test-only-pairing-token/v3/client_info HTTP/1.1"));
        upstream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await.unwrap();
        let length = tls.read(&mut bytes).await.unwrap();
        assert!(bytes[..length].ends_with(b"ok"));
        server.abort();
    }
}

#[cfg(test)]
mod apple_tls_tests {
    use super::*;

    /// A paired PC can terminate the outer channel, but it must not be able to impersonate
    /// Apple on the inner connection, even with a certificate naming the correct host.
    #[tokio::test]
    async fn apple_client_rejects_companion_certificate_before_sending_credentials() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = rcgen::generate_simple_self_signed(vec!["gsa.apple.com".into()]).unwrap();
        let mut server = SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).unwrap();
        server.set_certificate(&openssl::x509::X509::from_der(cert.cert.der()).unwrap()).unwrap();
        server.set_private_key(&openssl::pkey::PKey::private_key_from_der(&cert.signing_key.serialize_der()).unwrap()).unwrap();
        let server = server.build();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
                assert!(header.len() < 4096);
            }
            assert!(header.starts_with(b"CONNECT gsa.apple.com:443 HTTP/1.1\r\n"));
            assert!(!String::from_utf8_lossy(&header).contains("test-credential"));
            stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.unwrap();
            let mut tls = AsyncSslStream::new(openssl::ssl::Ssl::new(server.context()).unwrap(), stream).unwrap();
            assert!(Pin::new(&mut tls).accept().await.is_err(), "untrusted Apple certificate was accepted");
        });
        let client = isideload::auth::grandslam::GrandSlam::build_reqwest_client(false, Some(format!("http://{address}"))).unwrap();
        let error = client.get("https://gsa.apple.com/grandslam/GsService2/lookup")
            .header("X-Test-Credential", "test-credential").send().await.unwrap_err();
        assert!(format!("{error:?}").to_lowercase().contains("cert"), "unexpected failure: {error:?}");
        tokio::time::timeout(Duration::from_secs(3), task).await.unwrap().unwrap();
        assert!(client.get("http://gsa.apple.com/").send().await.is_err(), "plaintext downgrade allowed");
        assert!(isideload::auth::grandslam::GrandSlam::build_reqwest_client(true, Some(format!("http://{address}"))).is_err());
    }
}
