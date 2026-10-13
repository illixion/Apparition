//! `apparition`, the command line: puts Apparition (or any IPA) on an Apple Vision Pro, iPhone,
//! iPad or Apple TV from a computer running Windows, Linux or macOS, with no Xcode, and is the
//! companion those devices' Apparition uses (`relay`, `anisette`). The same core the app uses
//! does the work:
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
    companion::{self, Companion},
    relay,
    api::{self, APPARITION_ID, DeviceStore, Found, Handoff, PairedDevice as Device, Slot, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    release,
};
use rootcause::prelude::*;

const USAGE: &str = "\
Apparition: puts Apparition (or any IPA) on your devices, and is their companion.

  apparition devices
      Devices on this network that are paired or waiting to pair.
  apparition pair [<address>[:<port>]]
      Pair with a headset showing Settings > General > Remote Devices. Without an address,
      finds it over mDNS. <port> is its pairing port (mDNS shows it).
  apparition pair --usb [<UDID>]
      Pair with an iPhone or iPad over its cable (it has no PIN pairing screen). If it doesn't
      trust this computer yet, it asks: unlock it, tap Trust and enter its passcode. Afterwards
      it's reached over the network like a headset.
  apparition install [<app.ipa>] [--device <name or UDID>]
          (--apple-id <email> [--anisette <url>] | --p12 <file> --profile <file>...)
          [--keep-extensions] [--no-handoff] [--release-site <url>]
      Sign and install. Without an IPA, installs the latest Apparition from
      apparition.illixion.com, checked against its release signature first. With an Apple
      ID, a free account's apps last 7 days; with your own certificate, as long as its
      profile. Installing Apparition also puts the certificate in its Files inbox
      (--no-handoff skips that).
      Signing in to Apple needs anisette data. Where this build has it (macOS, Linux), it makes
      its own from Apple's libraries; --anisette <url> uses an anisette v3 server instead,
      and is required elsewhere.
  apparition certificates --apple-id <email> [--anisette <url>]
  apparition app-ids --apple-id <email> [--anisette <url>]
      What uses up a free account's two certificates and ten App IDs a week, and freeing
      one you choose (after you confirm). Revoking a certificate stops the apps signed with
      it until they are signed again.
  apparition signing share --apple-id <email>
      Seal this computer's certificate key for the Apple ID into a .apparitionkey file
      here, and print the five words that open it. AirDrop or send the file to a device and
      open it in Apparition (or `signing import` it on another computer), so both sign with
      one certificate instead of using another of the account's two.
  apparition signing import <file>
      Use the certificate key in a .apparitionkey from another device (asks its five words) or
      a .apparitionsigning file, for that Apple ID's signing here.
  apparition forget <name or UDID>
      Forget a paired device (remove the computer from its Remote Devices list too).
  apparition relay [--port <port>] [--no-mdns] [--anisette builtin|local|none]
      Be the companion for Apparition on your devices: relay its connection back to the
      device (port 49160) and offer it an anisette server. builtin (the default where this
      build has it) serves this computer's own on port 6969; local offers a server already
      answering on port 6969 here (the Docker one); none offers nothing. Runs until Ctrl-C.
  apparition anisette serve [--port <port>]
      Serve the built-in anisette (v3) to this computer's network (port 6969 by default).
  apparition anisette up | down | compose
      Start or stop the self-hosted anisette server with your Docker; compose prints its file.
  apparition browse
      List the companions mDNS finds from here, for 10 seconds: a check that multicast works.
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
    // APPARITION_LOG=<file>: isideload's progress (info and up, no secrets), for diagnosing sign-in.
    if let Some(path) = std::env::var_os("APPARITION_LOG").and_then(|path| path.into_string().ok()).and_then(|path| std::ffi::CString::new(path).ok()) {
        unsafe { apparition_core::apparition_init_logging(path.as_ptr()) };
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("devices") => devices().await,
        Some("pair") if args.get(1).map(String::as_str) == Some("--usb") => pair_usb(args.get(2).map(String::as_str)).await,
        Some("pair") => pair(args.get(1).map(String::as_str)).await,
        Some("install") => install(&args[1..]).await,
        Some("forget") => forget(args.get(1).map(String::as_str)),
        Some("signing") => signing(&args[1..]),
        Some("certificates") => account_command(&args[1..], Slot::Certificates).await,
        Some("app-ids") => account_command(&args[1..], Slot::AppIds).await,
        Some("relay") => relay(&args[1..]).await,
        Some("anisette") => anisette(&args[1..]).await,
        Some("browse") => {
            relay::browse(10, |found| println!("found {found}")).map_err(|e| fail(format!("{e}")))
        }
        _ => {
            print!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", api::describe(&error));
            ExitCode::FAILURE
        }
    }
}

type Error = Report;

fn fail(message: impl Into<String>) -> Error {
    report!("{}", message.into())
}

// MARK: Sharing the Apple ID's certificate key

fn signing(args: &[String]) -> Result<(), Error> {
    let data_dir = store().apple_id_dir();
    match args.first().map(String::as_str) {
        Some("share") => {
            let email = match (args.get(1).map(String::as_str), args.get(2)) {
                (Some("--apple-id"), Some(email)) => email,
                _ => return Err(fail("usage: apparition signing share --apple-id <email>")),
            };
            let (file, words) = api::export_apple_id_key_sealed(&data_dir, email, None, &api::hostname())?;
            let path = PathBuf::from(format!("Apple ID signing.{}", api::SEALED_EXTENSION));
            std::fs::write(&path, file).context(format!("Can't write {}", path.display()))?;
            println!("Wrote {}. Open it in Apparition on the other device and type these words:", path.display());
            println!();
            println!("    {words}");
            println!();
            println!("Anyone with the file and the words can sign as this Apple ID: delete the file once it's imported.");
            Ok(())
        }
        Some("import") => {
            let path = args.get(1).ok_or_else(|| fail("usage: apparition signing import <file>"))?;
            let data = std::fs::read(path).context(format!("Can't read {path}"))?;
            let words = match api::sealed_from(&data) {
                Some(from) => Some(ask(&format!("The five words {from} showed: "))),
                None => None,
            };
            let imported = api::import_any_signing_file(&data_dir, &data, words.as_deref())?;
            println!(
                "Signing with {} here now uses the same certificate as {}.",
                imported["appleId"].as_str().unwrap_or("that Apple ID"),
                imported["from"].as_str().unwrap_or("the other device")
            );
            if imported["replaced"] == true {
                println!("This computer's own certificate for it stays on the account until revoked (`apparition certificates`).");
            }
            Ok(())
        }
        _ => Err(fail("usage: apparition signing share --apple-id <email> | signing import <file>")),
    }
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
            println!("Open Settings > General > Remote Devices on the headset (Apple TV: Remotes and Devices > Remote App and Devices). Looking for it…");
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

    println!("Pairing as \"{}\". The device shows a PIN (an Apple TV asks twice: the second is for Apparition).", api::hostname());
    let pin = || ask("PIN: ").chars().filter(char::is_ascii_digit).collect::<String>();
    let (_, info) = store().pair(address, pairing_port, pin).await?;
    println!("Paired with {} ({}, {} {}).", info.name, info.udid, info.product_type, info.os_version);
    Ok(())
}

/// iPhone and iPad: the RemotePairing record is made over usbmuxd (USB, or the network once
/// trusted), then the device's own `_remotepairing._tcp` service is found over mDNS by trying
/// each until one answers to the new record. Its port isn't fixed as on visionOS.
async fn pair_usb(wanted: Option<&str>) -> Result<(), Error> {
    let devices = api::usb_devices().await?;
    let udid = match (wanted, devices.as_slice()) {
        (Some(udid), _) => udid.to_string(),
        (None, []) => return Err(fail("usbmuxd sees no device. Plug it in, unlock it and tap Trust")),
        (None, [one]) => one.udid.clone(),
        (None, many) => {
            for (index, device) in many.iter().enumerate() {
                println!("  {}. {}{}", index + 1, device.udid, if device.usb { " (USB)" } else { " (network)" });
            }
            let choice: usize = ask("Which one? ").parse().unwrap_or(0);
            many.get(choice.wrapping_sub(1)).ok_or_else(|| fail("no such device"))?.udid.clone()
        }
    };
    println!("Pairing with {udid} as \"{}\"…", api::hostname());
    let (device, info) = store().pair_usb(&udid, &|what| println!("{what}")).await?;
    println!("Paired with {} ({} {}), reached at {}:{}.", info.name, info.product_type, info.os_version, device.address, device.port);
    Ok(())
}

fn find_device(wanted: Option<&str>) -> Result<Device, Error> {
    let devices = load_devices();
    match (wanted, devices.as_slice()) {
        (None, [one]) => Ok(one.clone()),
        (None, []) => Err(fail("no paired device; run `apparition pair` first")),
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
        (Some(_), None) if options.anisette.is_none() && !api::builtin_anisette_available() => {
            Err(fail("--apple-id needs --anisette <url> (an anisette v3 server) on this platform"))
        }
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

/// The latest official Apparition for the device's platform, downloaded into `dir` once its release
/// signature and hash check out.
async fn latest_apparition(site: &str, dir: &Path, platform: api::Platform) -> Result<PathBuf, Error> {
    println!("Checking {site} for the latest Apparition…");
    let verified = release::latest(site).await?;
    println!("Downloading Apparition {} (signed by {})…", verified.release.version, verified.signer);
    let shown = std::sync::atomic::AtomicU64::new(0);
    let path = api::download_apparition_for(&verified, platform, dir, |received, total| {
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
        options.ipa = latest_apparition(&options.release_site, &scratch.join("download"), device.platform()).await?;
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
        api::sign_offline(scratch, options.ipa.clone(), &id, None, &info.udid, strip, None, api::Platform::of_product(&info.product_type))?
    } else {
        let mut session = sign_in(options.apple_id.as_deref().unwrap(), options.anisette.as_deref().unwrap_or(api::BUILTIN_ANISETTE)).await?;
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
        if store().hand_over_pairing(tunnel, &installed_id, &info.udid).await? {
            println!("Gave Apparition its pairing with this device.");
        }
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
    let Some(email) = email else {
        return Err(fail("needs --apple-id <email>"));
    };
    let anisette = match anisette {
        Some(url) => url,
        None if api::builtin_anisette_available() => api::BUILTIN_ANISETTE.to_string(),
        None => return Err(fail("needs --anisette <url> (an anisette v3 server) on this platform")),
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

// MARK: Companion

fn anisette_dir() -> PathBuf {
    store().dir.join("anisette")
}

async fn relay(args: &[String]) -> Result<(), Error> {
    let (mut port, mut advertise) = (relay::RELAY_PORT, true);
    let mut anisette = if api::builtin_anisette_available() { "builtin" } else { "local" }.to_string();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or_else(|| fail(format!("{arg} needs a value")));
        match arg.as_str() {
            "--port" => port = value()?.parse().map_err(|_| fail("--port needs a number"))?,
            "--no-mdns" => advertise = false,
            "--anisette" => anisette = value()?,
            other => return Err(fail(format!("unexpected {other}"))),
        }
    }
    let log: relay::Log = std::sync::Arc::new(|line: &str| println!("{line}"));
    let companion = Companion::start(port, relay::ANISETTE_PORT, advertise, &anisette_dir(), log).await?;
    println!("Apparition companion relay listening on port {}.", companion.relay.port());
    println!("In Apparition, use \"Find companion\" on this network, or set the relay to this computer's address, port {}.", companion.relay.port());
    match anisette.as_str() {
        "builtin" => {
            println!("Starting the built-in anisette (the first time, this fetches Apple's libraries)…");
            companion.share_builtin_anisette().await?;
            println!("Offering {} on port {}.", anisette_kind(), relay::ANISETTE_PORT);
        }
        "local" => println!("Offering an anisette server on port {} whenever one answers here.", relay::ANISETTE_PORT),
        "none" => {}
        other => return Err(fail(format!("--anisette is builtin, local or none, not {other}"))),
    }
    loop {
        if anisette == "local" {
            companion.refresh().await;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {}
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

/// What the built-in anisette serves, said plainly.
fn anisette_kind() -> &'static str {
    if api::native_anisette() { "this Mac's own anisette" } else { "anisette from Apple's Android libraries" }
}

async fn anisette(args: &[String]) -> Result<(), Error> {
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["serve"] | ["serve", "--port", _] => {
            let port = match args.get(2) {
                Some(port) => port.parse().map_err(|_| fail("--port needs a number"))?,
                None => relay::ANISETTE_PORT,
            };
            if !api::builtin_anisette_available() {
                return Err(fail("this build has no built-in anisette; use `apparition anisette up` (Docker)"));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            {
                println!("Starting the built-in anisette (the first time, this fetches Apple's libraries)…");
                let server = apparition_core::anisette::start(&anisette_dir(), SocketAddr::from(([0, 0, 0, 0], port))).await?;
                println!("Serving {} (v3) on port {}. Ctrl-C stops it.", anisette_kind(), server.address.port());
                let _ = tokio::signal::ctrl_c().await;
            }
            let _ = port;
            Ok(())
        }
        ["up"] | ["down"] => {
            let up = args[0] == "up";
            companion::docker_anisette(up).await?;
            if up {
                println!("Anisette starting on port {}. `apparition relay --anisette local` offers it once it answers.", relay::ANISETTE_PORT);
            }
            Ok(())
        }
        ["compose"] => {
            print!("{}", companion::COMPOSE);
            Ok(())
        }
        _ => Err(fail("usage: apparition anisette serve [--port <port>] | up | down | compose")),
    }
}
