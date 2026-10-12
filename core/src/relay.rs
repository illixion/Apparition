//! The companion relay: what lets Apparition on a device reach that same device's RemotePairing
//! service, from the computer's side. Apparition's own end is in `tunnel.rs`.
//!
//! The device resets connections it makes to its own RemotePairing service, so Apparition asks
//! a companion to connect back to it:
//!
//! ```text
//! Apparition -> companion:49160   "APPARITION-RELAY 1 <port>\n"
//! companion -> <Apparition's own address>:<port>, then "OK\n" and bytes both ways
//! ```
//!
//! It only ever connects back to the address a request came from, so it can't be used to reach
//! anything else, and what it carries is end-to-end encrypted between Apparition and the device.
//! `APPARITION-INFO 1\n` gets `INFO {json}\n` back: what else this companion offers (the port
//! of an anisette server on this machine). The mDNS TXT record carries the same, so Apparition
//! finds companions on the LAN.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rootcause::prelude::*;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const RELAY_PORT: u16 = 49160;
/// The port the self-hosted anisette server (Docker or built in) listens on.
pub const ANISETTE_PORT: u16 = 6969;
const SERVICE_TYPE: &str = "_apparition._tcp.local.";
/// How many finished connections are remembered for display.
const HISTORY: usize = 100;

/// One relayed connection, for display.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: u64,
    pub peer: String,
    pub port: u16,
    /// Seconds since 1970.
    pub started: u64,
    pub to_device: u64,
    pub from_device: u64,
    /// `None` while open.
    pub outcome: Option<String>,
}

pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

struct Inner {
    port: u16,
    connections: Mutex<VecDeque<Connection>>,
    anisette: Mutex<Option<u16>>,
    mdns: Option<Mdns>,
    log: Log,
    next: std::sync::atomic::AtomicU64,
}

/// A running relay. Dropping the last clone stops it and withdraws the mDNS advertisement.
#[derive(Clone)]
pub struct Relay {
    inner: Arc<Inner>,
    _task: Arc<AbortOnDrop>,
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Relay {
    /// Listens on `port` (all interfaces; 0 takes any free port) and, with `advertise`, announces itself over mDNS.
    /// Call from within a tokio runtime; the relay runs on it.
    pub async fn start(port: u16, advertise: bool, log: Log) -> Result<Relay, Report> {
        let listener = TcpListener::bind(("0.0.0.0", port)).await.context(format!("Can't listen on port {port}"))?;
        let port = listener.local_addr()?.port();
        let mdns = if advertise { Mdns::start(port, &log) } else { None };
        let inner = Arc::new(Inner {
            port,
            connections: Mutex::new(VecDeque::new()),
            anisette: Mutex::new(None),
            mdns,
            log,
            next: Default::default(),
        });
        if let Some(mdns) = &inner.mdns {
            mdns.publish(None, &inner.log);
        }
        let accepting = Arc::downgrade(&inner);
        let task = tokio::spawn(async move {
            loop {
                let Ok((client, peer)) = listener.accept().await else { continue };
                let Some(inner) = accepting.upgrade() else { return };
                tokio::spawn(serve(inner, client, peer));
            }
        });
        Ok(Relay { inner, _task: Arc::new(AbortOnDrop(task)) })
    }

    pub fn port(&self) -> u16 {
        self.inner.port
    }

    /// Recent connections, newest first.
    pub fn connections(&self) -> Vec<Connection> {
        self.inner.connections.lock().unwrap().iter().rev().cloned().collect()
    }

    /// The anisette server this companion offers, if any. Updates mDNS when it changes.
    pub fn set_anisette(&self, port: Option<u16>) {
        let mut current = self.inner.anisette.lock().unwrap();
        if *current == port {
            return;
        }
        *current = port;
        if let Some(mdns) = &self.inner.mdns {
            mdns.publish(port, &self.inner.log);
        }
    }

    pub fn anisette(&self) -> Option<u16> {
        *self.inner.anisette.lock().unwrap()
    }
}

impl Inner {
    fn update(&self, id: u64, change: impl FnOnce(&mut Connection)) {
        if let Some(connection) = self.connections.lock().unwrap().iter_mut().find(|c| c.id == id) {
            change(connection);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(mdns) = &self.mdns {
            let _ = mdns.daemon.shutdown();
        }
    }
}

async fn serve(inner: Arc<Inner>, mut client: TcpStream, peer: SocketAddr) {
    let _ = client.set_nodelay(true);
    // IPv4 peers on a dual-stack socket arrive as ::ffff:a.b.c.d.
    let peer_ip = match peer.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        ip => ip,
    };
    let Some(request) = read_line(&mut client).await else {
        return;
    };
    if request == "APPARITION-INFO 1" {
        let mut info = serde_json::json!({ "version": 1 });
        if let Some(port) = *inner.anisette.lock().unwrap() {
            info["anisette"] = port.into();
        }
        let _ = client.write_all(format!("INFO {info}\n").as_bytes()).await;
        return;
    }
    let parts: Vec<&str> = request.split(' ').collect();
    let port = match parts.as_slice() {
        ["APPARITION-RELAY", "1", port] => port.parse::<u16>().ok().filter(|port| *port >= 1024),
        _ => None,
    };
    let Some(port) = port else {
        let _ = client.write_all(b"ERR bad request\n").await;
        return;
    };
    let target = SocketAddr::new(peer_ip, port);
    let device = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(target)).await {
        Ok(Ok(device)) => device,
        Ok(Err(error)) => return refuse(&inner, &mut client, port, &error.to_string()).await,
        Err(_) => return refuse(&inner, &mut client, port, "timed out").await,
    };
    let _ = device.set_nodelay(true);
    if client.write_all(b"OK\n").await.is_err() {
        return;
    }
    let id = inner.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    {
        let mut connections = inner.connections.lock().unwrap();
        connections.push_back(Connection {
            id,
            peer: peer_ip.to_string(),
            port,
            started: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            to_device: 0,
            from_device: 0,
            outcome: None,
        });
        while connections.len() > HISTORY {
            connections.pop_front();
        }
    }
    (inner.log)(&format!("relay {peer_ip} :{port} open"));
    let mut device = device;
    match tokio::io::copy_bidirectional(&mut client, &mut device).await {
        Ok((to, from)) => {
            inner.update(id, |c| (c.to_device, c.from_device, c.outcome) = (to, from, Some("closed".into())));
            (inner.log)(&format!("relay {peer_ip} :{port} closed ({to} B to, {from} B from)"));
        }
        Err(error) => {
            inner.update(id, |c| c.outcome = Some(error.to_string()));
            (inner.log)(&format!("relay {peer_ip} :{port} closed: {error}"));
        }
    }
}

async fn refuse(inner: &Inner, client: &mut TcpStream, port: u16, reason: &str) {
    (inner.log)(&format!("relay to :{port} failed: {reason}"));
    let _ = client.write_all(format!("ERR can't connect to :{port}: {reason}\n").as_bytes()).await;
}

/// Reads up to the first newline, at most 256 bytes, within 10 seconds.
async fn read_line(stream: &mut TcpStream) -> Option<String> {
    let mut line = Vec::new();
    let read = async {
        loop {
            let byte = stream.read_u8().await.ok()?;
            if byte == b'\n' {
                return Some(());
            }
            line.push(byte);
            if line.len() > 256 {
                return None;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read).await.ok()??;
    String::from_utf8(line).ok()
}

/// Whether an anisette v3 server answers on this machine's `port` (the Docker one, say).
pub async fn anisette_answers(port: u16) -> bool {
    let probe = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        stream
            .write_all(b"GET /v3/client_info HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.ok()?;
        String::from_utf8_lossy(&reply).contains("client_info").then_some(())
    };
    tokio::time::timeout(Duration::from_secs(3), probe).await.ok().flatten().is_some()
}

struct Mdns {
    daemon: mdns_sd::ServiceDaemon,
    name: String,
    port: u16,
}

impl Mdns {
    fn start(port: u16, log: &Log) -> Option<Self> {
        match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => Some(Self { daemon, name: machine_name(), port }),
            Err(error) => {
                log(&format!("mDNS unavailable, Apparition won't find this companion on its own: {error}"));
                None
            }
        }
    }

    /// Registers (or re-registers, which updates the TXT record) the service.
    fn publish(&self, anisette: Option<u16>, log: &Log) {
        let mut txt = vec![("v".to_string(), "1".to_string())];
        if let Some(port) = anisette {
            txt.push(("anisette".to_string(), port.to_string()));
        }
        let host = format!("{}.local.", self.name.replace(' ', "-"));
        let info = mdns_sd::ServiceInfo::new(SERVICE_TYPE, &self.name, &host, "", self.port, &txt[..])
            .map(|info| info.enable_addr_auto());
        match info.and_then(|info| self.daemon.register(info)) {
            Ok(()) => log(&format!(
                "advertising {} over mDNS{}",
                self.name,
                anisette.map(|port| format!(", with anisette on port {port}")).unwrap_or_default()
            )),
            Err(error) => log(&format!("mDNS register failed: {error}")),
        }
    }
}

/// This computer's name, as the companion is advertised.
pub fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| {
            let output = std::process::Command::new("hostname").arg("-s").output().ok()?;
            String::from_utf8(output.stdout).ok()
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "apparition-companion".to_string())
}

/// Lists companions mDNS finds from here for `seconds`: a check that multicast works.
pub fn browse(seconds: u64, mut found: impl FnMut(String)) -> Result<(), Report> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let events = daemon.browse(SERVICE_TYPE)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match events.recv_timeout(left) {
            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                found(format!("{} at {:?} port {}", info.get_fullname(), info.get_addresses(), info.get_port()))
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relays_back_to_the_requester_and_answers_info() {
        let relay = Relay::start(0, false, Arc::new(|_| {})).await.unwrap();
        let port = relay.port();
        relay.set_anisette(Some(ANISETTE_PORT));

        let mut info = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        info.write_all(b"APPARITION-INFO 1\n").await.unwrap();
        let mut reply = String::new();
        info.read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, "INFO {\"anisette\":6969,\"version\":1}\n");

        // A "device" listening on this machine, which the relay connects back to.
        let device = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let device_port = device.local_addr().unwrap().port();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = device.accept().await.unwrap();
            let mut buffer = [0u8; 5];
            socket.read_exact(&mut buffer).await.unwrap();
            socket.write_all(&buffer).await.unwrap();
        });
        let mut client = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        client.write_all(format!("APPARITION-RELAY 1 {device_port}\n").as_bytes()).await.unwrap();
        let mut ok = [0u8; 3];
        client.read_exact(&mut ok).await.unwrap();
        assert_eq!(&ok, b"OK\n");
        client.write_all(b"hello").await.unwrap();
        let mut back = [0u8; 5];
        client.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"hello");
        echo.await.unwrap();
        drop(client);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let connections = relay.connections();
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].port, device_port);
        assert_eq!(connections[0].to_device, 5);
    }
}
