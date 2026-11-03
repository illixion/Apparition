//! Paired-device TCP channels for companion capabilities.
//!
//! This is the Rust host implementation of `RAVECompanionChannel` for the desktop app and the
//! CLI. TLS 1.2-PSK authenticates the per-device credential; the first length-prefixed JSON
//! message proves the device ID against the TLS exporter, matching RAVESDK's channel protocol.
//! Once identified, the connection is a byte-for-byte tunnel to the local anisette v3 service.

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

const CHANNEL_IDENTITY: &str = "Apparition/Anisette";
const CHANNEL_SALT: &[u8] = b"Apparition-Anisette-v1";
const PROOF_SALT: &[u8] = b"Apparition-Device-Proof-v1";
const EXPORTER_LABEL: &str = "EXPORTER-Apparition-Anisette-Device-v1";
const CIPHER: &str = "PSK-AES128-GCM-SHA256";
const MAX_IDENTIFY: usize = 16 * 1024;
const MAX_HTTP_HEADER: usize = 32 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identify {
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
        .export_keying_material(&mut binding, EXPORTER_LABEL, None)?;
    let proof_key = derive(token.as_bytes(), PROOF_SALT, b"", 32);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &proof_key);
    if ring::hmac::verify(&key, &binding, &identify.proof).is_err() {
        bail!("device proof failed");
    }

    let mut target = TcpStream::connect(anisette)
        .await
        .context("anisette provider is unavailable")?;
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

fn derive(input: &[u8], salt: &[u8], info: &[u8], length: usize) -> Vec<u8> {
    let mut output = vec![0; length];
    Hkdf::<Sha256>::new(Some(salt), input)
        .expand(info, &mut output)
        .expect("valid HKDF length");
    output
}
