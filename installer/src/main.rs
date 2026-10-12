//! Apparition Installer: puts Apparition (or any IPA) on an Apple Vision Pro from a computer
//! running Windows, Linux or macOS, with no Xcode. The same core the app uses does the work:
//!
//! 1. `pair`: with Settings > General > Remote Devices open on the headset, pair-setup with
//!    the PIN it shows. The pairing is kept, so this happens once per computer.
//! 2. `install`: open a tunnel, sign the app for the headset (an Apple ID, or your own .p12
//!    and profiles) and install it. When the app is Apparition, the signing material is put
//!    in its Files inbox (Documents/Signing) too, so it can refresh itself and its apps.
//!
//! The Apple ID password and the .p12 password are typed here and go no further than this
//! process: the password to Apple's sign-in, the .p12's to opening the file.

use std::{
    io::Write,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use apparition_core::api::{self, RpPairingFile, TwoFactorCallbackParams, TwoFactorCallbackResponse};
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};

const MANUAL_PAIRING: &str = "_remotepairing-manual-pairing._tcp.local.";
const PAIRED: &str = "_remotepairing._tcp.local.";
/// The RemotePairing port on visionOS; mDNS says when it differs.
const TUNNEL_PORT: u16 = 49152;
const APPARITION_ID: &str = "com.illixion.apparition";

const USAGE: &str = "\
Apparition Installer

  apparition-installer devices
      Devices on this network that are paired or waiting to pair.
  apparition-installer pair [<address>[:<port>]]
      Pair with a headset showing Settings > General > Remote Devices. Without an address,
      finds it over mDNS. <port> is its pairing port (mDNS shows it).
  apparition-installer install <app.ipa> [--device <name or UDID>]
          (--apple-id <email> --anisette <url> | --p12 <file> --profile <file>...)
          [--keep-extensions] [--no-handoff]
      Sign and install. With an Apple ID, a free account's apps last 7 days; with your own
      certificate, as long as its profile. Installing Apparition also puts the certificate in
      its Files inbox (--no-handoff skips that).
  apparition-installer forget <name or UDID>
      Forget a paired device (remove the computer from its Remote Devices list too).
";

#[derive(Serialize, Deserialize, Clone)]
struct Device {
    udid: String,
    name: String,
    address: IpAddr,
    port: u16,
    /// File name under pairings/.
    pairing: String,
}

fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Apparition Installer")
}

fn devices_path() -> PathBuf {
    data_dir().join("devices.json")
}

fn load_devices() -> Vec<Device> {
    std::fs::read(devices_path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

fn save_devices(devices: &[Device]) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(devices_path(), serde_json::to_vec_pretty(devices)?)
}

/// The name this computer pairs under, shown in the headset's Remote Devices list.
fn hostname() -> String {
    let machine = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .or_else(|| {
            std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty());
    match machine {
        Some(machine) => format!("Apparition Installer on {machine}"),
        None => "Apparition Installer".to_string(),
    }
}

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim().to_string()
}

fn ask_secret(prompt: &str) -> String {
    rpassword::prompt_password(prompt).unwrap_or_default()
}

#[tokio::main]
async fn main() -> ExitCode {
    api::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("devices") => devices().await,
        Some("pair") => pair(args.get(1).map(String::as_str)).await,
        Some("install") => install(&args[1..]).await,
        Some("forget") => forget(args.get(1).map(String::as_str)),
        _ => {
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

type Error = Report;

fn fail(message: impl Into<String>) -> Error {
    report!("{}", message.into())
}

// MARK: Discovery

struct Found {
    name: String,
    service: &'static str,
    addresses: Vec<IpAddr>,
    port: u16,
}

/// Browses both RemotePairing services for `seconds`. On macOS a command-line program can't
/// use multicast (Local Network privacy), so pass an address there instead.
async fn browse(seconds: u64) -> Result<Vec<Found>, Error> {
    let daemon = mdns_sd::ServiceDaemon::new()?;
    let mut found = Vec::new();
    for service in [MANUAL_PAIRING, PAIRED] {
        let events = daemon.browse(service)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
            if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                let name = info.get_property_val_str("name").unwrap_or(info.get_hostname()).trim_end_matches(".local.").to_string();
                let addresses: Vec<IpAddr> = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                // One device resolves once per interface: merge them.
                match found.iter_mut().find(|f: &&mut Found| f.service == service && f.name == name && f.port == info.get_port()) {
                    Some(existing) => {
                        for address in addresses {
                            if !existing.addresses.contains(&address) {
                                existing.addresses.push(address);
                            }
                        }
                    }
                    None => found.push(Found { name, service, addresses, port: info.get_port() }),
                }
            }
        }
        let _ = daemon.stop_browse(service);
    }
    let _ = daemon.shutdown();
    Ok(found)
}

async fn devices() -> Result<(), Error> {
    let paired = load_devices();
    if !paired.is_empty() {
        println!("Paired with this computer:");
        for device in &paired {
            println!("  {} ({}) at {}", device.name, device.udid, device.address);
        }
    }
    println!("Looking on this network for 3 seconds…");
    for found in browse(3).await? {
        let state = if found.service == MANUAL_PAIRING { "ready to pair" } else { "RemotePairing" };
        println!("  {} [{state}] {:?} port {}", found.name, found.addresses, found.port);
    }
    Ok(())
}

// MARK: Pairing

/// Prefers an IPv4 address: link-local IPv6 ones need a scope this program doesn't track.
fn pick(addresses: &[IpAddr]) -> Option<IpAddr> {
    addresses.iter().find(|a| a.is_ipv4()).or_else(|| addresses.first()).copied()
}

async fn pair(target: Option<&str>) -> Result<(), Error> {
    let (address, pairing_port) = match target {
        Some(text) => match text.parse::<SocketAddr>() {
            Ok(socket) => (socket.ip(), Some(socket.port())),
            Err(_) => (text.parse::<IpAddr>().map_err(|_| fail(format!("{text} isn't an IP address")))?, None),
        },
        None => {
            println!("Open Settings > General > Remote Devices on the headset. Looking for it…");
            let found: Vec<Found> = browse(5).await?.into_iter().filter(|f| f.service == MANUAL_PAIRING).collect();
            let found = match found.as_slice() {
                [] => return Err(fail("no headset is waiting to pair. Keep Remote Devices open, or pass its address")),
                [one] => one,
                many => {
                    for (index, found) in many.iter().enumerate() {
                        println!("  {}. {} {:?}", index + 1, found.name, found.addresses);
                    }
                    let choice: usize = ask("Which one? ").parse().unwrap_or(0);
                    many.get(choice.wrapping_sub(1)).ok_or_else(|| fail("no such device"))?
                }
            };
            (pick(&found.addresses).ok_or_else(|| fail("the device sent no address"))?, Some(found.port))
        }
    };
    let pairing_port = match pairing_port {
        Some(port) => port,
        None => ask("Pairing port (from `devices`, or mDNS on another machine): ").parse()
            .map_err(|_| fail("not a port number"))?,
    };

    let hostname = hostname();
    let mut pairing = RpPairingFile::generate(&hostname);
    println!("Pairing as \"{hostname}\". The headset shows a PIN.");
    let pin = || ask("PIN: ").chars().filter(char::is_ascii_digit).collect::<String>();
    api::pair(SocketAddr::new(address, pairing_port), &hostname, &mut pairing, pin).await?;

    // The paired service says who the device is.
    let mut tunnel = api::connect(SocketAddr::new(address, TUNNEL_PORT), &hostname, &mut pairing, pin).await?;
    let info = tunnel.device_info().await?;
    let file = format!("{}.plist", info.udid);
    std::fs::create_dir_all(data_dir().join("pairings"))?;
    std::fs::write(data_dir().join("pairings").join(&file), pairing.to_bytes())?;
    let mut devices = load_devices();
    devices.retain(|d| d.udid != info.udid);
    devices.push(Device { udid: info.udid.clone(), name: info.name.clone(), address, port: TUNNEL_PORT, pairing: file });
    save_devices(&devices)?;
    println!("Paired with {} ({}, {} {}).", info.name, info.udid, info.product_type, info.os_version);
    Ok(())
}

fn find_device(wanted: Option<&str>) -> Result<Device, Error> {
    let devices = load_devices();
    match (wanted, devices.as_slice()) {
        (None, [one]) => Ok(one.clone()),
        (None, []) => Err(fail("no paired device; run `apparition-installer pair` first")),
        (None, _) => Err(fail("several paired devices; choose one with --device")),
        (Some(wanted), _) => devices
            .iter()
            .find(|d| d.udid.eq_ignore_ascii_case(wanted) || d.name.eq_ignore_ascii_case(wanted))
            .cloned()
            .ok_or_else(|| fail(format!("no paired device called {wanted}"))),
    }
}

fn forget(wanted: Option<&str>) -> Result<(), Error> {
    let device = find_device(wanted)?;
    let _ = std::fs::remove_file(data_dir().join("pairings").join(&device.pairing));
    let mut devices = load_devices();
    devices.retain(|d| d.udid != device.udid);
    save_devices(&devices)?;
    println!("Forgot {}. Remove this computer from the headset's Remote Devices list too.", device.name);
    Ok(())
}

// MARK: Install

struct Options {
    ipa: PathBuf,
    device: Option<String>,
    apple_id: Option<String>,
    anisette: Option<String>,
    p12: Option<PathBuf>,
    profiles: Vec<PathBuf>,
    keep_extensions: bool,
    handoff: bool,
}

fn parse_install(args: &[String]) -> Result<Options, Error> {
    let mut options = Options {
        ipa: PathBuf::new(),
        device: None,
        apple_id: None,
        anisette: None,
        p12: None,
        profiles: Vec::new(),
        keep_extensions: false,
        handoff: true,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or_else(|| fail(format!("{arg} needs a value")));
        match arg.as_str() {
            "--device" => options.device = Some(value()?),
            "--apple-id" => options.apple_id = Some(value()?),
            "--anisette" => options.anisette = Some(value()?),
            "--p12" => options.p12 = Some(value()?.into()),
            "--profile" => options.profiles.push(value()?.into()),
            "--keep-extensions" => options.keep_extensions = true,
            "--no-handoff" => options.handoff = false,
            other if other.starts_with("--") => return Err(fail(format!("unknown option {other}"))),
            path if options.ipa.as_os_str().is_empty() => options.ipa = path.into(),
            extra => return Err(fail(format!("unexpected {extra}"))),
        }
    }
    if options.ipa.as_os_str().is_empty() {
        return Err(fail(format!("which app?\n\n{USAGE}")));
    }
    match (&options.apple_id, &options.p12) {
        (Some(_), None) if options.anisette.is_none() => Err(fail("--apple-id needs --anisette <url> (an anisette v3 server)")),
        (Some(_), None) => Ok(options),
        (None, Some(_)) if options.profiles.is_empty() => Err(fail("--p12 needs at least one --profile")),
        (None, Some(_)) => Ok(options),
        _ => Err(fail("sign with either --apple-id or --p12")),
    }
}

/// Asked during Apple ID sign-in when 2FA is needed.
fn two_factor(params: &TwoFactorCallbackParams) -> TwoFactorCallbackResponse {
    if let Some(error) = &params.last_error {
        println!("{error}");
    }
    if !params.numbers.is_empty() {
        for number in &params.numbers {
            println!("  sms {}: {}", number.id, number.number_with_dial_code);
        }
    }
    let answer = ask("Two-factor code (or `sms <id>` for a text, empty to cancel): ");
    match answer.split_once(' ') {
        Some(("sms", id)) => id.trim().parse().map(TwoFactorCallbackResponse::SendSms).unwrap_or(TwoFactorCallbackResponse::Abort),
        _ if answer.is_empty() => TwoFactorCallbackResponse::Abort,
        _ => TwoFactorCallbackResponse::SubmitCode(answer.chars().filter(char::is_ascii_digit).collect()),
    }
}

fn bundle_id(app: &Path) -> Option<String> {
    let info: plist::Dictionary = plist::from_file(app.join("Info.plist")).ok()?;
    info.get("CFBundleIdentifier")?.as_string().map(str::to_string)
}

async fn install(args: &[String]) -> Result<(), Error> {
    let options = parse_install(args)?;
    let device = find_device(options.device.as_deref())?;
    let hostname = hostname();
    let mut pairing = RpPairingFile::from_bytes(&std::fs::read(data_dir().join("pairings").join(&device.pairing))?)?;

    println!("Connecting to {}…", device.name);
    let pin = || ask("The headset wants to pair again. PIN: ");
    let mut tunnel = api::connect(SocketAddr::new(device.address, device.port), &hostname, &mut pairing, pin).await?;
    let info = tunnel.device_info().await?;
    let strip = !options.keep_extensions;

    // Signed into a scratch folder, removed afterwards with what it holds (a .p12's key).
    let scratch = std::env::temp_dir().join(format!("apparition-installer-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let result = sign_and_install(&options, &info, &mut tunnel, &scratch, strip).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

async fn sign_and_install(
    options: &Options,
    info: &api::DeviceInfo,
    tunnel: &mut api::Tunnel,
    scratch: &Path,
    strip: bool,
) -> Result<(), Error> {
    let signed = if let Some(p12) = &options.p12 {
        let password = ask_secret(&format!("Password for {}: ", p12.display()));
        let identity = api::import_identity(scratch, &std::fs::read(p12)?, password)?;
        let id = identity["identity"]["id"].as_str().ok_or_else(|| fail("no certificate id"))?.to_string();
        for profile in &options.profiles {
            api::import_profile(scratch, std::fs::read(profile)?, Some(&info.udid))?;
        }
        println!("Signing with {}…", identity["identity"]["name"].as_str().unwrap_or("your certificate"));
        api::sign_offline(scratch, options.ipa.clone(), &id, None, &info.udid, strip)?
    } else {
        let email = options.apple_id.as_deref().unwrap();
        let password = ask_secret(&format!("Apple ID password for {email}: "));
        println!("Signing in…");
        let apple_dir = data_dir().join("apple-id");
        let (mut session, _) =
            api::sign_in(email, password, options.anisette.as_deref().unwrap(), &apple_dir, two_factor).await?;
        println!("Signing for team {}…", session.team_id());
        api::sign_with_apple_id(&mut session, options.ipa.clone(), &info.udid, &info.name, strip).await?
    };
    let signed_path = PathBuf::from(signed["signedPath"].as_str().ok_or_else(|| fail("signing returned no app"))?);
    if let Some(stripped) = signed["strippedExtensions"].as_array().filter(|s| !s.is_empty()) {
        println!("Removed extensions no profile covers: {stripped:?}");
    }
    let installed_id = bundle_id(&signed_path).ok_or_else(|| fail("the signed app has no bundle ID"))?;

    // visionOS holds an update of an open app until it closes.
    if let Some(name) = tunnel.installed_app(&installed_id).await? {
        println!("{name} is already on {}. If it's open there, close it so the update can finish.", info.name);
    }
    println!("Installing {installed_id} on {}…", info.name);
    let source = options.ipa.is_file().then_some(options.ipa.as_path());
    // An update of an app that's running waits in installd until the app closes.
    let reply = tunnel.install(&signed_path, source, 20_000).await?;
    let seconds = reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0;
    if reply["queued"].as_bool() == Some(true) {
        println!("Sent in {seconds:.1} s. It's an update of an app that is open on the headset: close it there to finish.");
    } else {
        println!("Installed in {seconds:.1} s.");
    }

    if options.handoff && installed_id.starts_with(APPARITION_ID) {
        handoff(options, tunnel, &installed_id).await?;
    }
    Ok(())
}

/// Puts the .p12 and profiles in Apparition's Files inbox, where it asks for the .p12's
/// password and imports them: then it can refresh itself and its apps on its own.
async fn handoff(options: &Options, tunnel: &mut api::Tunnel, installed_id: &str) -> Result<(), Error> {
    let Some(p12) = &options.p12 else {
        println!("Open Apparition and sign in with the same Apple ID to keep it refreshed.");
        return Ok(());
    };
    let mut files = vec![(file_name(p12)?, std::fs::read(p12)?)];
    for profile in &options.profiles {
        files.push((file_name(profile)?, std::fs::read(profile)?));
    }
    tunnel.put_documents(installed_id, "Signing", &files).await?;
    println!("Put your certificate and profiles in Apparition's inbox: open Apparition and enter the .p12's password once.");
    Ok(())
}

fn file_name(path: &Path) -> Result<String, Error> {
    Ok(path.file_name().ok_or_else(|| fail("bad file name"))?.to_string_lossy().to_string())
}
