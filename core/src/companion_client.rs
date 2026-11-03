//! This computer's anisette from a Mac it pairs with: the client half of the companion's pairing
//! (pairing.rs) and encrypted channel (companion_channel.rs), as Apparition on a device runs them.
//!
//! Only a Mac makes the anisette Apple gives the Xcode token to, so a computer that isn't one (or
//! a Mac on macOS 27, whose anisette macOS keeps to itself) pairs with a Mac running Apparition:
//! the person's own Mac on macOS 26 or earlier, or a macOS VM prepared for it
//! (selfhost/macos-vm/README.md). Pairing is the devices' numeric comparison: both screens show a
//! six-digit code and the person allows this computer on the Mac. The Mac then mints a token for
//! this computer alone, kept in `paired-mac.json` (readable only by its owner).
//!
//! Signing in, [`adapter_url`] serves the v3 protocol on a loopback port and carries each
//! connection to the Mac over TLS 1.2-PSK, proving this computer's pairing as a device does. The
//! Mac sees only anisette requests; Apple credentials never go through it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio_openssl::SslStream;

use crate::companion_channel::{CHANNEL_IDENTITY, CHANNEL_SALT, CIPHER, EXPORTER_LABEL, PROOF_SALT, derive};
use crate::pairing::{self, Exchange, Message, Namespace};
use crate::relay::RELAY_PORT;

const FILE: &str = "paired-mac.json";
const CONNECT: Duration = Duration::from_secs(5);
/// How long the person has to compare the codes and allow this computer on the Mac.
const DECISION: Duration = Duration::from_secs(150);

/// The paired Mac couldn't be reached, or didn't accept this computer: what `failure_kind`
/// calls `"pairedMac"`.
#[derive(Debug)]
pub struct Unreachable;

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Couldn't get anisette from the paired Mac")
    }
}

impl std::error::Error for Unreachable {}

/// The Mac this computer is paired with.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairedMac {
    mac_id: String,
    mac_name: String,
    /// What the person typed, tried first.
    host: String,
    relay_port: u16,
    /// The addresses the Mac gave when pairing, tried after `host`.
    #[serde(default)]
    addresses: Vec<String>,
    /// This computer's id on the Mac (its paired-devices list).
    device_id: String,
    token: String,
    paired: u64,
}

/// What can be shown of the pairing: no token.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub mac_name: String,
    /// `host` or `host:port`, as the person typed it.
    pub address: String,
    /// Seconds since 1970.
    pub paired: u64,
}

impl PairedMac {
    fn summary(&self) -> Summary {
        let address = if self.relay_port == RELAY_PORT { self.host.clone() } else { format!("{}:{}", self.host, self.relay_port) };
        Summary { mac_name: self.mac_name.clone(), address, paired: self.paired }
    }

    fn hosts(&self) -> Vec<String> {
        let mut hosts = vec![self.host.clone()];
        for address in &self.addresses {
            if !hosts.contains(address) {
                hosts.push(address.clone());
            }
        }
        hosts
    }
}

fn load(dir: &Path) -> Option<PairedMac> {
    serde_json::from_slice(&std::fs::read(dir.join(FILE)).ok()?).ok()
}

fn save(dir: &Path, paired: &PairedMac) -> Result<(), Report> {
    std::fs::create_dir_all(dir)?;
    let temporary = dir.join(format!("{FILE}.new"));
    std::fs::write(&temporary, serde_json::to_vec_pretty(paired)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temporary, dir.join(FILE))?;
    Ok(())
}

/// The Mac this computer is paired with, if it is, for the anisette folder `dir`
/// (`api::anisette_dir`).
pub fn status(dir: &Path) -> Option<Summary> {
    load(dir).map(|paired| paired.summary())
}

/// Forgets the pairing here (unpair this computer on the Mac too, to revoke its token there).
pub fn forget(dir: &Path) -> bool {
    let path: PathBuf = dir.join(FILE);
    path.is_file() && std::fs::remove_file(path).is_ok()
}

/// `host`, `host:port` or `[v6]:port` into the host and the relay port.
fn split_address(address: &str) -> Result<(String, u16), Report> {
    let address = address.trim().trim_start_matches("http://").trim_start_matches("https://").trim_end_matches('/');
    if address.is_empty() {
        bail!("Give the Mac's address: its name or IP address, as its Apparition shows it");
    }
    if let Some(rest) = address.strip_prefix('[') {
        let (host, port) = rest.split_once(']').ok_or_else(|| report!("Can't read the address {address}"))?;
        let port = match port.strip_prefix(':') {
            Some(port) => port.parse().map_err(|_| report!("Can't read the port in {address}"))?,
            None => RELAY_PORT,
        };
        return Ok((host.to_string(), port));
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host.to_string(), port.parse().map_err(|_| report!("Can't read the port in {address}"))?)),
        _ => Ok((address.to_string(), RELAY_PORT)),
    }
}

async fn connect(host: &str, port: u16) -> Result<TcpStream, Report> {
    let stream = tokio::time::timeout(CONNECT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| report!("{host}:{port} didn't answer"))?
        .context(format!("Can't reach {host}:{port}"))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// The ports the companion at `host` pairs and serves its channel on, from its relay's INFO.
/// `None` for one it doesn't offer (a companion that isn't a Mac giving its anisette).
async fn ports(host: &str, relay_port: u16) -> Result<(Option<u16>, Option<u16>), Report> {
    let mut stream = connect(host, relay_port).await?;
    stream.write_all(b"APPARITION-INFO 1\n").await?;
    let mut reply = Vec::new();
    tokio::time::timeout(CONNECT, async {
        let mut byte = [0u8; 1];
        while reply.len() < 4096 && stream.read(&mut byte).await? == 1 && byte[0] != b'\n' {
            reply.push(byte[0]);
        }
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|_| report!("{host}:{relay_port} didn't answer as Apparition's companion"))??;
    let text = String::from_utf8_lossy(&reply);
    let info: serde_json::Value = text
        .strip_prefix("INFO ")
        .and_then(|json| serde_json::from_str(json).ok())
        .ok_or_else(|| report!("{host}:{relay_port} isn't Apparition's companion"))?;
    let port = |key: &str| info.get(key).and_then(|value| value.as_u64()).and_then(|port| u16::try_from(port).ok());
    Ok((port("anisettePairing"), port("anisetteChannel")))
}

/// What a companion that doesn't offer anisette is.
const NOT_A_MAC: &str = "That computer's Apparition doesn't offer anisette. Pair with a Mac on macOS 26 or earlier, or a macOS VM set up for it (selfhost/macos-vm/README.md), with Apparition's companion running and offering this Mac's anisette.";

/// Pairs this computer with the Mac at `address` (`host` or `host:port`, the companion's relay
/// port, 49160 by default). `show_code` gets the six-digit code ("123 456") to show while the
/// person compares it with the Mac's and allows this computer there. Replaces an earlier pairing.
pub async fn pair(dir: &Path, address: &str, show_code: impl FnOnce(&str)) -> Result<Summary, Report> {
    let (host, relay_port) = split_address(address)?;
    let (pairing_port, _) = ports(&host, relay_port).await.context(Unreachable)?;
    let Some(pairing_port) = pairing_port else { bail!("{NOT_A_MAC}") };
    let device_id = load(dir).map(|paired| paired.device_id).unwrap_or_else(pairing::new_token);
    let mut stream = connect(&host, pairing_port).await.context(Unreachable)?;

    let mine = Exchange::new();
    let committed = pairing::commitment(&mine.public_key(), &mine.nonce).to_vec();
    let commit = Message::Commit {
        headset_id: device_id.clone(),
        headset_name: crate::relay::machine_name(),
        commitment: committed.clone(),
        requested: Some(vec!["anisette".into()]),
        ssh_public_key: None,
    };
    stream.write_all(&commit.framed()).await?;
    let mut buffer = Vec::new();
    let Message::MacHello { mac_id, mac_name, public_key: host_key, nonce: host_nonce } = next(&mut stream, &mut buffer, CONNECT * 2).await? else {
        bail!("The Mac answered the pairing out of turn");
    };
    stream.write_all(&Message::Reveal { public_key: mine.public_key().to_vec(), nonce: mine.nonce.to_vec() }.framed()).await?;
    let agreement = pairing::agree(
        Namespace::APPARITION, &mine, &host_key, &committed, &mine.public_key(), &mine.nonce, &host_key, &host_nonce,
    )
    .ok_or_else(|| report!("The Mac's pairing keys aren't valid"))?;
    show_code(&agreement.display_code());

    let grant = match next(&mut stream, &mut buffer, DECISION).await {
        Ok(Message::Accept { sealed_grant }) => {
            pairing::open(&sealed_grant, &agreement.key).ok_or_else(|| report!("The Mac's answer couldn't be opened: the codes didn't match"))?
        }
        Ok(Message::Deny {}) => bail!("The Mac declined the pairing"),
        Ok(_) => bail!("The Mac answered the pairing out of turn"),
        Err(error) => return Err(error.context("The Mac didn't answer: allow this computer there within two minutes").into_dynamic()),
    };
    if grant.mac_id != mac_id {
        bail!("The Mac's answer names another computer");
    }
    if !grant.grants.as_ref().is_some_and(|grants| grants.iter().any(|grant| grant == "anisette")) {
        bail!("The Mac paired this computer without anisette access");
    }
    let paired = PairedMac {
        mac_id,
        mac_name: if grant.mac_name.is_empty() { mac_name } else { grant.mac_name },
        host,
        relay_port,
        addresses: grant.addresses,
        device_id,
        token: grant.token,
        paired: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    save(dir, &paired)?;
    Ok(paired.summary())
}

async fn next(stream: &mut TcpStream, buffer: &mut Vec<u8>, wait: Duration) -> Result<Message, Report> {
    tokio::time::timeout(wait, async {
        loop {
            if let Some(message) = Message::take(buffer)? {
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await?;
            if read == 0 {
                bail!("the Mac closed the connection");
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    })
    .await
    .map_err(|_| report!("the Mac didn't answer in time"))?
}

/// The TLS-PSK settings for `paired`, as the device's `RAVECompanionChannel` makes them.
fn connector(paired: &PairedMac) -> Result<Arc<SslConnector>, Report> {
    let mut builder = SslConnector::builder(SslMethod::tls_client())?;
    // PSK authenticates both ends; there's no certificate to check.
    builder.set_verify(SslVerifyMode::NONE);
    builder.set_min_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_max_proto_version(Some(SslVersion::TLS1_2))?;
    builder.set_cipher_list(CIPHER)?;
    let identity = format!("{CHANNEL_IDENTITY}/{}\0", paired.device_id).into_bytes();
    let key = derive(paired.token.as_bytes(), CHANNEL_SALT, b"psk", 32);
    builder.set_psk_client_callback(move |_, _, identity_out, key_out| {
        if identity_out.len() < identity.len() || key_out.len() < key.len() {
            return Err(openssl::error::ErrorStack::get());
        }
        identity_out[..identity.len()].copy_from_slice(&identity);
        key_out[..key.len()].copy_from_slice(&key);
        Ok(key.len())
    });
    Ok(Arc::new(builder.build()))
}

/// Why a channel to the Mac didn't open.
#[derive(Debug, PartialEq)]
enum Refusal {
    /// No connection: the Mac is off, asleep, or on another network.
    Unreachable(String),
    /// The handshake failed: the Mac doesn't know this computer's pairing any more.
    NotPaired,
}

/// An identified channel to the Mac, ready for HTTP.
async fn open_channel(paired: &PairedMac, channel_port: u16, connector: &SslConnector) -> Result<SslStream<TcpStream>, Refusal> {
    let mut last = String::from("no address to try");
    for host in paired.hosts() {
        let stream = match connect(&host, channel_port).await {
            Ok(stream) => stream,
            Err(error) => {
                last = error.to_string();
                continue;
            }
        };
        let ssl = connector.configure().and_then(|config| config.verify_hostname(false).into_ssl("apparition-companion"));
        let Ok(ssl) = ssl else { return Err(Refusal::Unreachable("TLS setup failed".into())) };
        let Ok(mut tls) = SslStream::new(ssl, stream) else { return Err(Refusal::Unreachable("TLS setup failed".into())) };
        match tokio::time::timeout(CONNECT * 2, Pin::new(&mut tls).connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(Refusal::NotPaired),
            Err(_) => {
                last = format!("{host}:{channel_port} didn't finish the handshake");
                continue;
            }
        }
        let mut binding = [0u8; 32];
        if tls.ssl().export_keying_material(&mut binding, EXPORTER_LABEL, Some(&[])).is_err() {
            return Err(Refusal::Unreachable("TLS exporter failed".into()));
        }
        let proof_key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &derive(paired.token.as_bytes(), PROOF_SALT, b"", 32));
        let proof = ring::hmac::sign(&proof_key, &binding);
        let body = serde_json::json!({ "deviceID": paired.device_id, "proof": B64.encode(proof.as_ref()) }).to_string();
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend(body.as_bytes());
        if tls.write_all(&frame).await.is_err() {
            return Err(Refusal::NotPaired);
        }
        return Ok(tls);
    }
    Err(Refusal::Unreachable(last))
}

/// Asks the Mac for its client info over a fresh channel: whether it answers, and accepts this
/// computer, before a sign-in depends on it.
async fn check(paired: &PairedMac, channel_port: u16, connector: &SslConnector) -> Result<(), Report> {
    let refused = |refusal: Refusal| match refusal {
        Refusal::Unreachable(why) => report!(
            "Can't reach {} ({why}). Check that it's on, awake, and running Apparition's companion with its anisette offered, on this network or your tailnet.",
            paired.mac_name
        ),
        Refusal::NotPaired => report!("{} no longer accepts this computer. Pair with it again.", paired.mac_name),
    };
    let mut tls = open_channel(paired, channel_port, connector).await.map_err(refused)?;
    tls.write_all(b"GET /v3/client_info HTTP/1.1\r\nHost: apparition-companion\r\nConnection: close\r\n\r\n").await?;
    let mut reply = Vec::new();
    let _ = tokio::time::timeout(CONNECT * 2, tls.read_to_end(&mut reply)).await;
    let reply = String::from_utf8_lossy(&reply);
    if reply.starts_with("HTTP/1.1 200") && reply.contains("client_info") {
        return Ok(());
    }
    if reply.is_empty() {
        bail!("{} accepted this computer but isn't offering its anisette right now (in its Apparition, turn on offering this Mac's anisette).", paired.mac_name);
    }
    bail!("{} answered the anisette request with an error", paired.mac_name)
}

/// A loopback v3 server, carrying each connection to the Mac.
struct Adapter {
    mac_id: String,
    token: String,
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Adapter {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_adapter(paired: PairedMac, channel_port: u16, connector: Arc<SslConnector>) -> Result<Adapter, Report> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let (mac_id, token) = (paired.mac_id.clone(), paired.token.clone());
    let paired = Arc::new(paired);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut local, _)) = listener.accept().await else { continue };
            let (paired, connector) = (paired.clone(), connector.clone());
            tokio::spawn(async move {
                match open_channel(&paired, channel_port, &connector).await {
                    Ok(mut tls) => {
                        let _ = copy_bidirectional(&mut local, &mut tls).await;
                    }
                    Err(refusal) => tracing::debug!(?refusal, "paired Mac channel refused"),
                }
            });
        }
    });
    Ok(Adapter { mac_id, token, address, task })
}

/// The address of a loopback v3 server for the paired Mac's anisette, for the anisette folder
/// `dir`, checking first that the Mac answers and accepts this computer. Started on first use,
/// restarted when the pairing changes.
pub async fn adapter_url(dir: &Path) -> Result<String, Report> {
    static ADAPTER: tokio::sync::Mutex<Option<Adapter>> = tokio::sync::Mutex::const_new(None);
    let Some(paired) = load(dir) else {
        bail!("This computer isn't paired with a Mac. Pair it with your Mac (or a macOS VM) running Apparition, or give the address of an anisette server you run.");
    };
    let (_, channel_port) = ports(&paired.host, paired.relay_port).await.context(Unreachable).or_else(|error| {
        // The Mac's own address didn't answer the relay: try the others it gave when pairing.
        if paired.addresses.is_empty() { Err(error) } else { Ok((None, None)) }
    })?;
    let channel_port = channel_port.unwrap_or(paired.relay_port.wrapping_add(2));
    let connector = connector(&paired)?;
    check(&paired, channel_port, &connector).await.context(Unreachable)?;
    let mut adapter = ADAPTER.lock().await;
    if adapter.as_ref().is_none_or(|running| running.mac_id != paired.mac_id || running.token != paired.token) {
        *adapter = Some(start_adapter(paired, channel_port, connector).await?);
    }
    Ok(format!("http://{}", adapter.as_ref().expect("started").address))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn addresses_split_into_host_and_relay_port() {
        assert_eq!(split_address("mac.local").unwrap(), ("mac.local".to_string(), RELAY_PORT));
        assert_eq!(split_address(" 100.64.0.2:50000 ").unwrap(), ("100.64.0.2".to_string(), 50000));
        assert_eq!(split_address("[fd7a::1]:49160").unwrap(), ("fd7a::1".to_string(), 49160));
        assert_eq!(split_address("fd7a::1").unwrap(), ("fd7a::1".to_string(), RELAY_PORT));
        assert_eq!(split_address("http://mac.local/").unwrap(), ("mac.local".to_string(), RELAY_PORT));
        assert!(split_address("mac.local:http").is_err());
        assert!(split_address("  ").is_err());
    }

    /// The host side the device uses, against this client: pairing with the Mac's approval,
    /// then a v3 request through the loopback adapter, with the token path the Mac adds.
    #[tokio::test(flavor = "multi_thread")]
    async fn pairs_with_a_companion_and_carries_anisette_requests() {
        let host = tempfile::tempdir().unwrap();
        let here = tempfile::tempdir().unwrap();
        let devices = Arc::new(crate::companion::PairedDevices::open(host.path()));

        // The Mac's pairing listener, approving whatever asks, and its channel to a fake v3
        // provider on loopback.
        let pairing_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pairing_port = pairing_listener.local_addr().unwrap().port();
        let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider.local_addr().unwrap();
        let (channel_port, channel) = crate::companion_channel::start(
            "127.0.0.1:0".parse().unwrap(), provider_address, devices.clone(),
            Arc::new(AtomicBool::new(true)), Arc::new(AtomicBool::new(true)),
        ).await.unwrap();
        let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_port = relay.local_addr().unwrap().port();
        let info = format!("INFO {{\"version\":1,\"anisette\":6969,\"anisettePairing\":{pairing_port},\"anisetteChannel\":{channel_port}}}\n");
        let relay_task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = relay.accept().await.unwrap();
                let mut line = [0u8; 18];
                stream.read_exact(&mut line).await.unwrap();
                stream.write_all(info.as_bytes()).await.unwrap();
            }
        });
        let codes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let host_codes = codes.clone();
        let host_devices = devices.clone();
        let pairing_task = tokio::spawn(async move {
            let (mut stream, _) = pairing_listener.accept().await.unwrap();
            crate::pairing::serve(
                Namespace::APPARITION, &mut stream, "test".into(), &host_devices.host_id(), "Test Mac",
                vec!["127.0.0.1".into()], vec!["anisette".into(), "appleRelay".into()],
                |request| async move {
                    host_codes.lock().unwrap().push(request.code.clone());
                    Some(host_devices.add(&request))
                },
            ).await.unwrap()
        });

        let summary = pair(here.path(), &format!("127.0.0.1:{relay_port}"), |code| codes.lock().unwrap().push(code.to_string())).await.unwrap();
        assert!(pairing_task.await.unwrap());
        assert_eq!(summary.mac_name, "Test Mac");
        let codes = codes.lock().unwrap().clone();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0], codes[1], "both ends show the same code");
        assert_eq!(status(here.path()).unwrap().address, format!("127.0.0.1:{relay_port}"));
        let token = load(here.path()).unwrap().token;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(here.path().join(FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the pairing file is private");
        }

        let provider_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = provider.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.unwrap());
                }
                let expected = format!("GET /t/{token}/v3/client_info HTTP/1.1");
                assert!(header.starts_with(expected.as_bytes()), "{}", String::from_utf8_lossy(&header));
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"client_info\":1}").await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let url = adapter_url(here.path()).await.unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        let reply = reqwest::get(format!("{url}/v3/client_info")).await.unwrap().text().await.unwrap();
        assert!(reply.contains("client_info"), "{reply}");
        provider_task.await.unwrap();

        // Unpaired on the Mac: the next sign-in is told to pair again.
        let device_id = load(here.path()).unwrap().device_id;
        assert!(devices.forget(&device_id));
        let error = adapter_url(here.path()).await.unwrap_err();
        assert!(error.to_string().contains("Pair with it again") || format!("{error:?}").contains("Pair with it again"), "{error:?}");
        assert_eq!(crate::failure_kind(&error), Some("pairedMac"));
        assert!(forget(here.path()));
        assert!(status(here.path()).is_none());
        channel.abort();
        relay_task.abort();
    }
}
