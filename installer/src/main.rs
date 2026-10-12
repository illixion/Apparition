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
};

use apparition_core::{
    api::{self, APPARITION_ID, DeviceStore, Found, Handoff, PairedDevice as Device, Slot, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    release,
};
use rootcause::prelude::*;

const USAGE: &str = "\
Apparition Installer

  apparition-installer devices
      Devices on this network that are paired or waiting to pair.
  apparition-installer pair [<address>[:<port>]]
      Pair with a headset showing Settings > General > Remote Devices. Without an address,
      finds it over mDNS. <port> is its pairing port (mDNS shows it).
  apparition-installer install [<app.ipa>] [--device <name or UDID>]
          (--apple-id <email> --anisette <url> | --p12 <file> --profile <file>...)
          [--keep-extensions] [--no-handoff] [--release-site <url>]
      Sign and install. Without an IPA, installs the latest Apparition from
      apparition.illixion.com, checked against its release signature first. With an Apple
      ID, a free account's apps last 7 days; with your own certificate, as long as its
      profile. Installing Apparition also puts the certificate in its Files inbox
      (--no-handoff skips that).
  apparition-installer certificates --apple-id <email> --anisette <url>
  apparition-installer app-ids --apple-id <email> --anisette <url>
      What uses up a free account's two certificates and ten App IDs a week, and freeing
      one you choose (after you confirm). Revoking a certificate stops the apps signed with
      it until they are signed again.
  apparition-installer forget <name or UDID>
      Forget a paired device (remove the computer from its Remote Devices list too).
";

fn store() -> DeviceStore {
    DeviceStore::default()
}

fn load_devices() -> Vec<Device> {
    store().devices()
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
        Some("certificates") => account_command(&args[1..], Slot::Certificates).await,
        Some("app-ids") => account_command(&args[1..], Slot::AppIds).await,
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

/// On macOS a command-line program can't use multicast (Local Network privacy), so pass an
/// address there instead.
async fn browse(seconds: u64) -> Result<Vec<Found>, Error> {
    api::browse(seconds).await
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
        let state = if found.ready_to_pair { "ready to pair" } else { "RemotePairing" };
        println!("  {} [{state}] {:?} port {}", found.name, found.addresses, found.port);
    }
    Ok(())
}

// MARK: Pairing

async fn pair(target: Option<&str>) -> Result<(), Error> {
    let (address, pairing_port) = match target {
        Some(text) => match text.parse::<SocketAddr>() {
            Ok(socket) => (socket.ip(), Some(socket.port())),
            Err(_) => (text.parse::<IpAddr>().map_err(|_| fail(format!("{text} isn't an IP address")))?, None),
        },
        None => {
            println!("Open Settings > General > Remote Devices on the headset. Looking for it…");
            let found: Vec<Found> = browse(5).await?.into_iter().filter(|f| f.ready_to_pair).collect();
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
            (found.address().ok_or_else(|| fail("the device sent no address"))?, Some(found.port))
        }
    };
    let pairing_port = match pairing_port {
        Some(port) => port,
        None => ask("Pairing port (from `devices`, or mDNS on another machine): ").parse()
            .map_err(|_| fail("not a port number"))?,
    };

    println!("Pairing as \"{}\". The headset shows a PIN.", api::hostname());
    let pin = || ask("PIN: ").chars().filter(char::is_ascii_digit).collect::<String>();
    let (_, info) = store().pair(address, pairing_port, pin).await?;
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
    store().forget(&device.udid)?;
    println!("Forgot {}. Remove this computer from the headset's Remote Devices list too.", device.name);
    Ok(())
}

// MARK: Install

struct Options {
    /// Empty: the latest official Apparition.
    ipa: PathBuf,
    release_site: String,
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
        release_site: release::SITE.to_string(),
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
            "--release-site" => options.release_site = value()?,
            other if other.starts_with("--") => return Err(fail(format!("unknown option {other}"))),
            path if options.ipa.as_os_str().is_empty() => options.ipa = path.into(),
            extra => return Err(fail(format!("unexpected {extra}"))),
        }
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

async fn install(args: &[String]) -> Result<(), Error> {
    let mut options = parse_install(args)?;
    let device = find_device(options.device.as_deref())?;
    // Signed into a scratch folder, removed afterwards with what it holds (a .p12's key).
    let scratch = std::env::temp_dir().join(format!("apparition-installer-{}", std::process::id()));
    std::fs::create_dir_all(&scratch)?;
    let result = download_and_install(&mut options, &device, &scratch).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The latest official Apparition for visionOS, downloaded into `dir` once its release
/// signature and hash check out.
async fn latest_apparition(site: &str, dir: &Path) -> Result<PathBuf, Error> {
    println!("Checking {site} for the latest Apparition…");
    let verified = release::latest(site).await?;
    println!("Downloading Apparition {} (signed by {})…", verified.release.version, verified.signer);
    let shown = std::sync::atomic::AtomicU64::new(0);
    let path = api::download_apparition(&verified, dir, |received, total| {
        let step = received * 10 / total.max(1);
        if step > shown.swap(step, std::sync::atomic::Ordering::Relaxed) {
            print!("\r  {}%", step * 10);
            let _ = std::io::stdout().flush();
        }
    })
    .await?;
    println!("\r  verified against the release signature.");
    Ok(path)
}

async fn download_and_install(options: &mut Options, device: &Device, scratch: &Path) -> Result<(), Error> {
    if options.ipa.as_os_str().is_empty() {
        options.ipa = latest_apparition(&options.release_site, &scratch.join("download")).await?;
    }
    println!("Connecting to {}…", device.name);
    let pin = || ask("The headset wants to pair again. PIN: ");
    let mut tunnel = store().connect(device, pin).await?;
    let info = tunnel.device_info().await?;
    let strip = !options.keep_extensions;
    sign_and_install(options, &info, &mut tunnel, scratch, strip).await
}

async fn sign_and_install(
    options: &Options,
    info: &api::DeviceInfo,
    tunnel: &mut api::Tunnel,
    scratch: &Path,
    strip: bool,
) -> Result<(), Error> {
    // The Apple ID and team that signed, for handing the certificate to Apparition.
    let mut apple_id: Option<(String, String)> = None;
    let signed = if let Some(p12) = &options.p12 {
        let password = ask_secret(&format!("Password for {}: ", p12.display()));
        let identity = api::import_identity(scratch, &std::fs::read(p12)?, password)?;
        let id = identity["identity"]["id"].as_str().ok_or_else(|| fail("no certificate id"))?.to_string();
        for profile in &options.profiles {
            api::import_profile(scratch, std::fs::read(profile)?, Some(&info.udid))?;
        }
        println!("Signing with {}…", identity["identity"]["name"].as_str().unwrap_or("your certificate"));
        api::sign_offline(scratch, options.ipa.clone(), &id, None, &info.udid, strip, None)?
    } else {
        let mut session = sign_in(options.apple_id.as_deref().unwrap(), options.anisette.as_deref().unwrap()).await?;
        println!("Signing for team {}…", session.team_id());
        apple_id = Some((session.email().to_string(), session.team_id().to_string()));
        match api::sign_with_apple_id(&mut session, options.ipa.clone(), &info.udid, &info.name, strip).await {
            Ok(signed) => signed,
            // A full slot: offer to free one, then try once more.
            Err(error) => match Slot::of_error(&error.to_string()) {
                Some(slot) => {
                    println!("{}", slot.full_message());
                    if !manage(&mut session, slot).await? {
                        return Err(error);
                    }
                    println!("Signing again…");
                    api::sign_with_apple_id(&mut session, options.ipa.clone(), &info.udid, &info.name, strip).await?
                }
                None => return Err(error),
            },
        }
    };
    let signed_path = PathBuf::from(signed["signedPath"].as_str().ok_or_else(|| fail("signing returned no app"))?);
    if let Some(stripped) = signed["strippedExtensions"].as_array().filter(|s| !s.is_empty()) {
        println!("Removed extensions no profile covers: {stripped:?}");
    }
    let installed_id = api::bundle_id(&signed_path).ok_or_else(|| fail("the signed app has no bundle ID"))?;

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
        handoff(options, tunnel, &installed_id, apple_id.as_ref()).await?;
    }
    Ok(())
}

/// Puts what signed Apparition in its Files inbox, so it can refresh itself and its apps:
/// the Apple ID's certificate key (then both use one of the account's two certificates), or
/// the .p12 and profiles (it asks for the .p12's password once).
async fn handoff(
    options: &Options,
    tunnel: &mut api::Tunnel,
    installed_id: &str,
    apple_id: Option<&(String, String)>,
) -> Result<(), Error> {
    let Some(p12) = &options.p12 else {
        let Some((email, team)) = apple_id else { return Ok(()) };
        let data_dir = store().apple_id_dir();
        tunnel.hand_off_signing(installed_id, Handoff::AppleId { data_dir: &data_dir, email, team_id: team }).await?;
        println!("Shared this computer's certificate with Apparition: sign in there with {email} and it signs with the same one.");
        return Ok(());
    };
    tunnel.hand_off_signing(installed_id, Handoff::Certificate { p12, profiles: &options.profiles }).await?;
    println!("Put your certificate and profiles in Apparition's inbox: open Apparition and enter the .p12's password once.");
    Ok(())
}

// MARK: Apple ID and its slots

async fn sign_in(email: &str, anisette: &str) -> Result<api::Session, Error> {
    let password = ask_secret(&format!("Apple ID password for {email}: "));
    println!("Signing in…");
    let (session, _) = api::sign_in(email, password, anisette, &store().apple_id_dir(), two_factor).await?;
    Ok(session)
}

async fn account_command(args: &[String], slot: Slot) -> Result<(), Error> {
    let (mut email, mut anisette) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--apple-id" => email = args.next().cloned(),
            "--anisette" => anisette = args.next().cloned(),
            other => return Err(fail(format!("unexpected {other}"))),
        }
    }
    let (Some(email), Some(anisette)) = (email, anisette) else {
        return Err(fail("needs --apple-id <email> and --anisette <url>"));
    };
    let mut session = sign_in(&email, &anisette).await?;
    manage(&mut session, slot).await?;
    Ok(())
}

/// Lists the slot's items and frees one the user picks and confirms. True when one was freed.
async fn manage(session: &mut api::Session, slot: Slot) -> Result<bool, Error> {
    match slot {
        Slot::Certificates => revoke_one(session).await,
        Slot::AppIds => delete_one(session).await,
    }
}

async fn revoke_one(session: &mut api::Session) -> Result<bool, Error> {
    let list = api::list_certificates(session).await?;
    let certificates = list["certificates"].as_array().cloned().unwrap_or_default();
    if certificates.is_empty() {
        println!("The account has no development certificates.");
        return Ok(false);
    }
    println!("Development certificates on team {}:", session.team_id());
    for (index, cert) in certificates.iter().enumerate() {
        let text = |key: &str| cert[key].as_str().unwrap_or("?").to_string();
        let mut notes = Vec::new();
        if cert["inUseHere"].as_bool() == Some(true) {
            notes.push("this computer signs with it");
        } else if cert["madeByApparition"].as_bool() == Some(true) {
            notes.push("Apparition on another device, probably a headset");
        }
        println!(
            "  {}. {} ({}), made on \"{}\", expires {}{}",
            index + 1,
            text("name"),
            cert["platform"].as_str().unwrap_or("any platform"),
            text("machineName"),
            text("expires").split('T').next().unwrap_or("?"),
            if notes.is_empty() { String::new() } else { format!(" [{}]", notes.join(", ")) },
        );
    }
    let Some(cert) = choose(&certificates, "Revoke which one (number, or Return to keep them all)? ") else {
        return Ok(false);
    };
    let warning = if cert["inUseHere"].as_bool() == Some(true) {
        "This computer signs with it: apps it installed stop opening until installed again."
    } else if cert["madeByApparition"].as_bool() == Some(true) {
        "Apps that device signed with it stop opening until it signs them again (it makes a new certificate then, which needs a free slot)."
    } else {
        "Apps signed with it (by Xcode or another tool) stop opening until signed again; that tool makes a new one when it next signs."
    };
    println!("{warning}");
    if ask("Type `revoke` to revoke it: ") != "revoke" {
        println!("Kept it.");
        return Ok(false);
    }
    let serial = cert["serialNumber"].as_str().ok_or_else(|| fail("that certificate has no serial number"))?;
    api::revoke_certificate(session, serial).await?;
    println!("Revoked.");
    Ok(true)
}

async fn delete_one(session: &mut api::Session) -> Result<bool, Error> {
    let list = api::list_app_ids(session).await?;
    let ids = list["appIds"].as_array().cloned().unwrap_or_default();
    println!(
        "App IDs on team {} ({} more allowed now):",
        session.team_id(),
        list["availableQuantity"].as_i64().map(|n| n.to_string()).unwrap_or_else(|| "?".into())
    );
    if ids.is_empty() {
        println!("  none");
        return Ok(false);
    }
    for (index, id) in ids.iter().enumerate() {
        println!(
            "  {}. {} ({}), expires {}",
            index + 1,
            id["identifier"].as_str().unwrap_or("?"),
            id["name"].as_str().unwrap_or("?"),
            id["expires"].as_str().and_then(|e| e.split('T').next()).unwrap_or("?"),
        );
    }
    let Some(id) = choose(&ids, "Delete which one (number, or Return to keep them all)? ") else {
        return Ok(false);
    };
    println!("Apps using it keep working until their profile expires. A free account may refuse until the ID expires, and a deleted ID can still count against this week's ten.");
    if ask("Type `delete` to delete it: ") != "delete" {
        println!("Kept it.");
        return Ok(false);
    }
    let app_id_id = id["appIdId"].as_str().ok_or_else(|| fail("that App ID has no id"))?;
    api::delete_app_id(session, app_id_id).await?;
    println!("Deleted.");
    Ok(true)
}

fn choose<'a>(items: &'a [serde_json::Value], prompt: &str) -> Option<&'a serde_json::Value> {
    let answer = ask(prompt);
    let index: usize = answer.parse().ok()?;
    items.get(index.checked_sub(1)?)
}
