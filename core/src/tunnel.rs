//! The RemotePairing tunnel, opened directly or through an Apparition Companion relay.
//!
//! The device resets any connection it makes to its own RemotePairing service, so Apparition
//! can't reach the headset it runs on directly. Two ways around that:
//! - LocalDevVPN, whose 10.7.0.1 sends the packets out and back in;
//! - a companion on another machine (Mac or PC, on the LAN or over Tailscale) that connects
//!   back to the address the request came from and copies bytes both ways. To the device the
//!   connection then comes from that machine.
//!
//! A tunnel takes two TCP connections: pair-verify on the RemotePairing port, then a TLS-PSK
//! tunnel on a port the device picks. Both go through the relay. Both are end-to-end
//! encrypted with keys only this pairing holds, so the relay sees ciphertext only.
//!
//! Relay protocol: the client sends `APPARITION-RELAY 1 <port>\n`, and the companion connects
//! to that port on the client's own address and answers `OK\n` (or `ERR <reason>\n`). After
//! that it only copies bytes. Because it only ever connects back to the requester, it can't
//! be used as a proxy to anything else.

use std::ffi::{CStr, c_char, c_void};
use std::net::SocketAddr;

use idevice::IdeviceError;
use idevice::remote_pairing::{RemotePairingClient, RpPairingSocket, connect_tls_psk_tunnel_native};
use idevice::rsd::RsdHandshake;
use idevice_ffi::core_device_proxy::AdapterHandle;
use idevice_ffi::rp_pairing_file::RpPairingFileHandle;
use idevice_ffi::rsd::RsdHandshakeHandle;
use rootcause::prelude::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{reply, runtime, text};

pub type PinFn = Option<extern "C" fn(context: *mut c_void) -> *const c_char>;

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub(crate) async fn dial(device: SocketAddr, relay: Option<SocketAddr>) -> Result<TcpStream, Report> {
    let Some(relay) = relay else {
        return Ok(tokio::time::timeout(TIMEOUT, TcpStream::connect(device)).await??);
    };
    let mut stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(relay))
        .await?
        .context("can't reach the companion")?;
    stream.set_nodelay(true)?;
    stream.write_all(format!("APPARITION-RELAY 1 {}\n", device.port()).as_bytes()).await?;
    let mut line = Vec::new();
    loop {
        let byte = tokio::time::timeout(TIMEOUT, stream.read_u8()).await??;
        if byte == b'\n' {
            break;
        }
        line.push(byte);
        if line.len() > 256 {
            bail!("companion sent no reply line");
        }
    }
    let line = String::from_utf8_lossy(&line);
    if line != "OK" {
        bail!("companion refused port {}: {line}", device.port());
    }
    Ok(stream)
}

pub(crate) async fn open(
    device: SocketAddr,
    relay: Option<SocketAddr>,
    hostname: &str,
    pairing: &mut idevice::remote_pairing::RpPairingFile,
    pin: impl Fn() -> String,
) -> Result<(idevice::tcp::handle::AdapterHandle, RsdHandshake), Report> {
    let control = dial(device, relay).await.context("RemotePairing connection")?;
    let mut rpc = RemotePairingClient::new(RpPairingSocket::new(control), hostname);
    rpc.connect(pairing, async || pin()).await?;

    let mut tunnel_addr = device;
    tunnel_addr.set_port(rpc.create_tcp_listener().await?);
    let stream = dial(tunnel_addr, relay).await.context("tunnel connection")?;
    let tunnel = connect_tls_psk_tunnel_native(stream, rpc.encryption_key()).await?;

    let parse = |text: &str| text.parse::<std::net::IpAddr>().map_err(|e| IdeviceError::InternalError(format!("{e}")));
    let client_ip = parse(&tunnel.info.client_address)?;
    let server_ip = parse(&tunnel.info.server_address)?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;
    let mut adapter = idevice::tcp::adapter::Adapter::new(Box::new(tunnel.into_inner()), client_ip, server_ip);
    adapter.set_mss(mtu.saturating_sub(60));
    let mut adapter = adapter.to_async_handle();
    let rsd = adapter.connect(rsd_port).await.map_err(|e| IdeviceError::InternalError(format!("{e}")))?;
    let handshake = RsdHandshake::new(rsd).await?;
    Ok((adapter, handshake))
}

/// Opens a tunnel to `device_host:device_port`, or, with `relay` (`ip:port`), through a
/// companion that connects back to this device (`device_host` is then unused).
/// On success fills the same handle types `tunnel_create_rppairing` produces.
/// Returns `{ok}` or `{ok: false, error}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_tunnel_create(
    device_host: *const c_char,
    device_port: u16,
    relay: *const c_char,
    hostname: *const c_char,
    pairing_file: *mut RpPairingFileHandle,
    pin: PinFn,
    pin_context: *mut c_void,
    out_adapter: *mut *mut AdapterHandle,
    out_handshake: *mut *mut RsdHandshakeHandle,
) -> *mut c_char {
    struct Context(*mut c_void);
    unsafe impl Send for Context {}
    unsafe impl Sync for Context {}
    let context = Context(pin_context);

    reply((|| {
        if pairing_file.is_null() || out_adapter.is_null() || out_handshake.is_null() {
            bail!("missing argument");
        }
        let hostname = unsafe { text(hostname) }?;
        let relay: Option<SocketAddr> = if relay.is_null() { None } else { Some(unsafe { text(relay) }?.parse()?) };
        // With a relay only the port matters: the companion connects back to our address.
        let host = if relay.is_some() { "0.0.0.0".to_string() } else { unsafe { text(device_host) }? };
        let device = SocketAddr::new(host.parse()?, device_port);
        let pairing = unsafe { &mut (*pairing_file).0 };
        let ask_pin = || {
            pin.and_then(|callback| {
                let reply = callback(context.0);
                (!reply.is_null()).then(|| unsafe { CStr::from_ptr(reply) }.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "000000".to_string())
        };
        let (adapter, handshake) = runtime().block_on(open(device, relay, &hostname, pairing, ask_pin))?;
        unsafe {
            *out_adapter = Box::into_raw(Box::new(AdapterHandle(adapter)));
            *out_handshake = Box::into_raw(Box::new(RsdHandshakeHandle(handshake)));
        }
        Ok(json!({}))
    })())
}

/// Pair-setup (or pair-verify, when already paired) with the RemotePairing service at
/// `port`, through the companion at `relay` ("ip:port"), with no tunnel. The device resets
/// pair-setup attempts it makes to itself, so on the headset pairing goes through a relay
/// like everything else. Updates `pairing_file` in place. Returns `{ok}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_pair_relay(
    relay: *const c_char,
    port: u16,
    hostname: *const c_char,
    pairing_file: *mut RpPairingFileHandle,
    pin: PinFn,
    pin_context: *mut c_void,
) -> *mut c_char {
    struct Context(*mut c_void);
    unsafe impl Send for Context {}
    unsafe impl Sync for Context {}
    let context = Context(pin_context);

    reply((|| {
        if pairing_file.is_null() {
            bail!("missing argument");
        }
        let relay: SocketAddr = unsafe { text(relay) }?.parse()?;
        let hostname = unsafe { text(hostname) }?;
        let pairing = unsafe { &mut (*pairing_file).0 };
        let ask_pin = || {
            pin.and_then(|callback| {
                let reply = callback(context.0);
                (!reply.is_null()).then(|| unsafe { CStr::from_ptr(reply) }.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "000000".to_string())
        };
        runtime().block_on(async {
            let device = SocketAddr::new("0.0.0.0".parse().unwrap(), port);
            let control = dial(device, Some(relay)).await.context("RemotePairing connection")?;
            let mut rpc = RemotePairingClient::new(RpPairingSocket::new(control), &hostname);
            rpc.connect(pairing, async || ask_pin()).await?;
            Ok::<_, Report>(())
        })?;
        Ok(json!({}))
    })())
}
