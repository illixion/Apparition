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
/// The loopback port a Mac's companion serves its own anisette on, for its encrypted channel.
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
    channel: Mutex<Option<u16>>,
    /// The port devices pair on, while anisette is offered only to paired devices.
    pairing: Mutex<Option<u16>>,
    /// A stable id for this companion (its paired-devices host id), so a device that finds an
    /// address of it on another network can tell it's the same computer.
    id: Mutex<Option<String>>,
    mdns: Mutex<Option<Mdns>>,
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
            id: Mutex::new(None),
            channel: Mutex::new(None),
            pairing: Mutex::new(None),
            mdns: Mutex::new(mdns),
            log,
            next: Default::default(),
        });
        if let Some(mdns) = &*inner.mdns.lock().unwrap() {
            mdns.publish(None, None, None, &inner.log);
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
        if let Some(mdns) = &*self.inner.mdns.lock().unwrap() {
            mdns.publish(port, *self.inner.pairing.lock().unwrap(), *self.inner.channel.lock().unwrap(), &self.inner.log);
        }
    }

    /// The id INFO reports (see `Inner::id`).
    pub fn set_id(&self, id: &str) {
        *self.inner.id.lock().unwrap() = Some(id.to_string());
    }

    pub fn set_channel(&self, port: Option<u16>) {
        let mut current = self.inner.channel.lock().unwrap();
        if *current == port { return; }
        *current = port;
        if let Some(mdns) = &*self.inner.mdns.lock().unwrap() {
            mdns.publish(*self.inner.anisette.lock().unwrap(), *self.inner.pairing.lock().unwrap(), port, &self.inner.log);
        }
    }

    /// Whether the companion announces itself over mDNS. Off, devices that know its address
    /// keep connecting as before; only Find Companion stops listing it.
    pub fn set_advertise(&self, on: bool) {
        let mut mdns = self.inner.mdns.lock().unwrap();
        match (on, mdns.is_some()) {
            (true, false) => {
                *mdns = Mdns::start(self.inner.port, &self.inner.log);
                if let Some(started) = &*mdns {
                    started.publish(*self.inner.anisette.lock().unwrap(), *self.inner.pairing.lock().unwrap(), *self.inner.channel.lock().unwrap(), &self.inner.log);
                }
            }
            (false, true) => {
                if let Some(stopped) = mdns.take() {
                    stopped.stop();
                }
                (self.inner.log)("stopped advertising over mDNS");
            }
            _ => {}
        }
    }

    pub fn advertising(&self) -> bool {
        self.inner.mdns.lock().unwrap().is_some()
    }

    /// The port a device pairs on before it may use the anisette server, or `None` when the
    /// anisette server (if any) serves anyone.
    pub fn set_pairing(&self, port: Option<u16>) {
        *self.inner.pairing.lock().unwrap() = port;
        if let Some(mdns) = &*self.inner.mdns.lock().unwrap() {
            mdns.publish(*self.inner.anisette.lock().unwrap(), port, *self.inner.channel.lock().unwrap(), &self.inner.log);
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
        if let Some(mdns) = self.mdns.lock().unwrap().take() {
            mdns.stop();
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
    // The Mac stays awake while a device uses it (see stay_awake), INFO included: it's how a
    // device checks that the Mac it just woke answers.
    let _busy = crate::stay_awake::busy();
    if request == "APPARITION-INFO 1" {
        let mut info = serde_json::json!({ "version": 1 });
        // Where to knock to wake this computer, for a device that saved another address of it
        // (a Tailscale one): a sleeping Mac wakes on a TCP SYN at its LAN address only.
        // Every way to reach it, so a device picks the one that works where it is: a LAN address
        // at home (where a knock wakes a sleeping Mac), its Tailscale address away.
        let addresses = lan_addresses();
        if !addresses.is_empty() {
            info["addresses"] = serde_json::json!(addresses.iter().map(|ip| ip.to_string()).collect::<Vec<_>>());
        }
        let tailscale = tailscale_addresses();
        if !tailscale.is_empty() {
            info["tailscale"] = serde_json::json!(tailscale.iter().map(|ip| ip.to_string()).collect::<Vec<_>>());
        }
        if let Some(id) = &*inner.id.lock().unwrap() {
            info["id"] = id.clone().into();
        }
        if let Some(port) = *inner.anisette.lock().unwrap() {
            info["anisette"] = port.into();
            if let Some(pairing) = *inner.pairing.lock().unwrap() {
                info["anisettePairing"] = pairing.into();
            }
        }
        if let Some(port) = *inner.channel.lock().unwrap() {
            info["anisetteChannel"] = port.into();
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

struct Mdns {
    daemon: mdns_sd::ServiceDaemon,
    name: String,
    port: u16,
}

impl Mdns {
    fn start(port: u16, log: &Log) -> Option<Self> {
        match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => {
                // Only addresses a device on the network can use: no loopback, no IPv6 (the
                // phone connects over IPv4), no self-assigned 169.254 ones (an interface with
                // no DHCP lease, which the phone would save and never reach again).
                let skipped = [
                    mdns_sd::IfKind::LoopbackV4,
                    mdns_sd::IfKind::IPv6,
                    mdns_sd::IfKind::Predicate(mdns_sd::IfPredicate::new(|intf| match intf.ip() {
                        IpAddr::V4(ip) => ip.is_link_local(),
                        IpAddr::V6(_) => false,
                    })),
                ];
                if let Err(error) = daemon.disable_interface(skipped.to_vec()) {
                    log(&format!("mDNS: couldn't leave out loopback and link-local interfaces: {error}"));
                }
                Some(Self { daemon, name: machine_name(), port })
            }
            Err(error) => {
                log(&format!("mDNS unavailable, Apparition won't find this companion on its own: {error}"));
                None
            }
        }
    }

    /// Withdraws the announcement (a goodbye, so browsers drop it at once) and stops.
    fn stop(self) {
        if let Ok(goodbye) = self.daemon.unregister(&format!("{}.{SERVICE_TYPE}", self.name)) {
            let _ = goodbye.recv_timeout(Duration::from_secs(1));
        }
        let _ = self.daemon.shutdown();
    }

    /// Registers (or re-registers, which updates the TXT record) the service.
    fn publish(&self, anisette: Option<u16>, pairing: Option<u16>, channel: Option<u16>, log: &Log) {
        let mut txt = vec![("v".to_string(), "1".to_string())];
        if let Some(port) = anisette {
            txt.push(("anisette".to_string(), port.to_string()));
        }
        if let Some(port) = pairing { txt.push(("anisettePairing".to_string(), port.to_string())); }
        if let Some(port) = channel { txt.push(("anisetteChannel".to_string(), port.to_string())); }
        let host = mdns_host(&self.name);
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

/// The host name the service's addresses are published under. Not the computer's own name:
/// the OS already answers for `<name>.local` (Bonjour, Windows, Avahi) with every address it
/// has, and a phone resolving it would mix those in.
fn mdns_host(name: &str) -> String {
    let label: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let label = label.trim_matches('-');
    let label = if label.is_empty() { "companion" } else { label };
    format!("apparition-{}.local.", &label[..label.len().min(40)])
}

/// The address this computer reaches the network from (the default route's interface), for
/// telling the person what to type on a device.
pub fn primary_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|address| address.ip()).filter(|ip| !ip.is_unspecified())
}

/// This computer's private IPv4 addresses on a LAN (10/8, 172.16/12, 192.168/16): where a device
/// knocks to wake it. Not Tailscale's (100.64/10), whose UDP never wakes a Mac.
pub fn lan_addresses() -> Vec<std::net::Ipv4Addr> {
    let mut addresses: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|interface| !interface.is_loopback())
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(v4) if v4.is_private() => Some(v4),
            _ => None,
        })
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
}

/// This computer's Tailscale IPv4 addresses (100.64.0.0/10, the CGNAT range Tailscale assigns
/// from), for a device away from home.
pub fn tailscale_addresses() -> Vec<std::net::Ipv4Addr> {
    let mut addresses: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            IpAddr::V4(v4) if v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) => Some(v4),
            _ => None,
        })
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
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
    #[test]
    fn mdns_host_is_apart_from_the_os_name() {
        assert_eq!(super::mdns_host("Pegasus"), "apparition-pegasus.local.");
        assert_eq!(super::mdns_host("Ixion's MacBook Pro"), "apparition-ixion-s-macbook-pro.local.");
        assert_eq!(super::mdns_host("Пегас"), "apparition-companion.local.");
    }

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
        let info: serde_json::Value = serde_json::from_str(reply.strip_prefix("INFO ").unwrap().trim()).unwrap();
        assert_eq!(info["anisette"], 6969);
        assert_eq!(info["version"], 1);
        // Where a device knocks to wake it: this machine's LAN address, when it has one.
        let addresses: Vec<String> = lan_addresses().iter().map(|ip| ip.to_string()).collect();
        if !addresses.is_empty() {
            assert_eq!(info["addresses"], serde_json::json!(addresses));
        }

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
