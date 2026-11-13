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
mod inject;
mod developer;
mod builder;
mod crash_reports;
mod login;
mod scratch;
mod store;
mod vm;

const USAGE: &str = "\
Apparition: puts Apparition (or any IPA) on your devices, and is their companion.

  apparition devices
      Devices on this network that are paired or waiting to pair.
  apparition apps [--device <name or UDID>] [--all] [--json]
      What a paired device has installed (user apps; --all for every app), with each app's
      version and build. Over this pairing's own tunnel, so it never asks a PIN and Xcode's
      tools never notice: unlike `devicectl device info apps`, it leaves a sleeping Apple TV
      asleep even while Xcode's DeviceHub is running. --json for programs.
  apparition push <signed.ipa> [--device <name or UDID>] [--json]
      Install an IPA that is already signed for the device (a build tool's output) as it is,
      over the same tunnel as `apps`: no re-signing, and a sleeping Apple TV stays asleep.
  apparition uninstall <bundle ID> [--device <name or UDID>]
      Remove an app and its data container, over the same tunnel as `apps`.
  apparition container pull|push|count <bundle ID> [--device <name or UDID>] [<folder>] [--json]
      An installed, development-signed app's data, over the same tunnel: `pull` copies its
      Documents and Library into an empty <folder> (caches and saved state stay behind), `push`
      copies such a folder back into the app, `count` says how many folders and files it holds.
      For backing data up across a reinstall without waking a sleeping Apple TV.
  apparition launch <bundle ID> [--device <name or UDID>] [--env KEY=VALUE]... [--console]
          [--jit [--wait <seconds>]] [-- <app arguments>...]
      Launch an app (ending a running copy) over the same tunnel as `apps`, as `devicectl device
      process launch` would, without waking a sleeping Apple TV. --console streams its stdout
      and stderr here until it exits. --jit starts it suspended and serves its JIT requests
      like `jit`, so a JIT app gets its console and environment in the same launch; Ctrl-C or
      SIGTERM detaches and leaves it running. Every APPARITION_CHILD_<KEY> variable is passed
      as <KEY>, which keeps values out of the command line.
  apparition pair [<address>[:<port>]]
      Pair with a headset showing Settings > General > Remote Devices. Without an address,
      finds it over mDNS. <port> is its pairing port (mDNS shows it).
  apparition pair --usb [<UDID>]
      Pair with an iPhone or iPad (it has no PIN pairing screen). Despite the name, a cable
      is only needed until it trusts this computer: after that this works over Wi-Fi too,
      whenever this computer sees it on the network (listed as \"network\" here). If it doesn't
      trust this computer yet, it asks: unlock it, tap Trust and enter its passcode.
      Afterwards it's reached over the network like a headset. It also makes the pairing
      Apparition on the device uses, which `install` or `pairing hand-off` gives it.
  apparition pairing hand-off [--device <name or UDID>] [--bundle-id <id>]
      Give Apparition, already installed on an iPhone, iPad or Apple TV, its pairing with
      that device, as installing Apparition does. For when Apparition says it isn't paired
      (it was reinstalled some other way) or that the device no longer trusts its pairing
      (run `pair --usb` first then, or `pair` for an Apple TV). Works over Wi-Fi.
  apparition install [<app.ipa or .deb>] [--device <name or UDID>]
          (--apple-id <email> [--anisette <url>] | --p12 <file> --profile <file>...)
          [--keep-extensions] [--no-watch] [--no-handoff] [--release-site <url>]
      Sign and install. Without an IPA, installs the latest Apparition from
      apparition.illixion.com, checked against its release signature first. A .deb package
      (how Kodi is published) is unpacked first: the app inside is what gets installed. With an Apple
      ID, a free account's apps last 7 days; with your own certificate, as long as its
      profile. Installing Apparition also puts the certificate in its Files inbox
      (--no-handoff skips that). --no-watch leaves the app's watch app out: nothing goes to the
      Apple Watch and its App ID isn't spent.
      Signing in to Apple needs anisette data from a Mac you own: this Mac's own (macOS 26 or
      earlier, or a macOS VM set up for it), else the Mac this computer is paired with
      (`apparition anisette pair`). --anisette <url> uses a third-party anisette v3 server instead.
  apparition sign <app.ipa or .deb> [--device <name or UDID>] [--keep-extensions] [--no-watch]
          (--apple-id <email> [--anisette <url>] | --p12 <file> --profile <file>...)
      Sign for a paired device without reaching it and install nothing. With an Apple ID,
      says which capabilities (increased memory limit, extended virtual addressing) the app
      got; with a .p12, which profile signed it.
  apparition jit <bundle ID> [--attach [<pid>]] [--device <name or UDID>] [--wait <seconds>]
      Be the app's debugger from here, so it can use JIT: launch it (or with --attach, take
      its running process) and serve its JIT requests until it closes or says it's done. The
      app has to be development-signed (installed by Apparition is). --wait is how long to
      wait for its first request before leaving it (default: no limit).
  apparition inject [<app>] [--device <name or UDID>] [--file PATH]... [--run SWIFT]
          [--allow-view-type-change] [--no-launch]
      Patch a running app with the Swift sources edited since its build: compile and sign on
      this Mac, ask the device to allow it, apply (attaching as its debugger for the Apparition
      backend). <app> is a bundle ID, a name or name@device from its launch record; without it,
      the latest app launched for live patching. If the app isn't running a patchable build,
      it is first rebuilt with --inject as its last launch was and relaunched, which builds the
      edits in (--no-launch refuses instead).
  apparition certificates --apple-id <email> [--anisette <url>]
  apparition app-ids --apple-id <email> [--anisette <url>]
      What uses up a free account's two certificates and ten App IDs a week, and freeing
      one you choose (after you confirm). Revoking a certificate stops the apps signed with
      it until they are signed again: share Apparition's instead (`signing`, below).
  apparition build [--run, --log, --mcp, --jit, --inject, --device, --set K=V, ...]
      On a Mac, in a repo with a build-signing.conf: build with Xcode, sign with the conf's
      certificate, keep the build and its dSYMs, install it over this pairing and, with --run,
      launch it the same way. No devicectl, Node or Homebrew bash. `apparition build --help`.

  Sharing one certificate between your devices and computers. A free account holds two, so
  each Apparition signs with the same one rather than making its own: once one has signed,
  hand its key to the others. Signing stops and asks before it would make a second.
  apparition signing receive
      Wait for the key from Apparition on a nearby device or computer: there, choose Share
      Signing in Settings and pick this computer under Nearby. Both screens show a six-digit
      code; check that they match on each.
  apparition signing send --apple-id <email> [--to <address>:<port>]
      Hand this computer's key to Apparition on a nearby device (Settings > Get Signing from
      Another Device there) or computer (`signing receive`), found over mDNS or at the
      address it shows.
  apparition signing hand-off --apple-id <email> [--device <name>] [--bundle-id <id>]
  apparition signing hand-off --build-signing [<dir>] [--device <name>] [--bundle-id <id>]
  apparition signing hand-off --p12 <file>... --profile <file>... [--device <name>] [--bundle-id <id>]
      Put signing in Apparition's Files inbox (Documents/Signing) on a paired device, as
      installing Apparition does, for an Apparition installed another way. --apple-id gives this
      computer's key; --build-signing gives every .p12 and .mobileprovision of build-and-sign
      (~/.config/build-signing, or <dir>), so the headset signs what the Apparition Store builds;
      --p12 and --profile give the files you name. Apparition there asks for each .p12's
      password once. Nothing is reinstalled.
  apparition signing share --apple-id <email>
      For devices that aren't on this network: seal the key into a .apparitionkey file here
      and print the five words that open it. Send the file any way you like and open it in
      Apparition (or `signing import` it on another computer).
  apparition signing import <file>
      Use the key in a .apparitionkey from another device (asks its five words) or a
      .apparitionsigning file, for that Apple ID's signing here.
  apparition forget <name or UDID>
      Forget a paired device (remove the computer from its Remote Devices list too).
  apparition relay [--port <port>] [--no-mdns] [--anisette builtin|android|none] [--no-prompt]
      Be the companion for Apparition on your devices: relay its connection back to the
      device (port 49160), and on a Mac that gives its own anisette (macOS 26 or earlier, or a
      macOS VM set up for it), offer that anisette (builtin, the default there; android offers Apparition's
      own Android anisette, on any computer; none offers nothing). Runs until Ctrl-C. Anisette goes only to devices and computers paired with this
      Mac: one asks to pair (port 49161), both show a six-digit code, and you answer in this
      terminal. Without a terminal, or with --no-prompt, requests are declined: pair in a
      terminal once (paired devices are remembered). All anisette requests use the encrypted
      channel on port 49162. --no-mdns doesn't announce this computer: devices then need its
      address typed in. While an Apparition Store runs here (the app's or `apparition store
      serve`), computers can also pair for developer access to it, as with the app.
  apparition relay devices [forget <id>]
      List the devices and computers paired to use this Mac's anisette, or unpair one.
  apparition anisette pair <address>[:<port>]
      Pair this computer with a Mac running `apparition relay` or the Apparition app with its
      anisette offered: your own Mac on macOS 26 or earlier, or a macOS VM on your Mac
      (selfhost/macos-vm/README.md). Both show a six-digit code; allow this computer on the Mac if
      they match. Sign-ins here then use that Mac's anisette, over the encrypted channel.
  apparition mac pair <address>[:<port>] [--developer-only | --anisette-only]
      Pair this computer with the Mac you build and deploy on: one approval there gives it
      developer access to that Mac's Apparition Store and, from a Mac on macOS 26 or earlier,
      its anisette. Requests go over the companion's encrypted channel, at home over the LAN
      and away over your tailnet. `apparition mac status | forget`.
  apparition deploy <app.ipa> [--device <name>] [--keep] [--local]
      Send an app built here (xtool, Xcode, any build) to the paired Mac's Store. With a device
      named, the Mac installs it itself when it is paired with that device and has a certificate
      for it (`apparition deploy-signing import`, run on the Mac); otherwise it waits there and the
      device's Apparition installs it with its own signing once someone opens it. --keep only
      stores it. --local uses the Store of the Apparition app running on this Mac.
  apparition deploy-signing import <cert.p12> --profile <file>... [--device <name or UDID>] | list
      On the Mac that builds for you: give its Apparition Store a certificate and provisioning
      profiles, once, so it can sign what other computers send and install it without the
      device's Apparition. Kept in the Apparition folder, next to the device pairings.
  apparition updates [--check] [--run [--only <device>|<bundle id>]...] [--auto on|off] [--json] [--local]
      Which apps on your devices are behind their branch, or a local package they build against
      (vergence, RAVESDK...), and why: from the Mac's Store. --check asks the devices first; --run
      builds each once in a clean clone and installs it on every device that runs it; --auto on
      lets the Store do that by itself once sources have been still for a few minutes.
  apparition store serve | status | web-ui | open | projects | add | devices | build | builds | log
          | cancel | kept | resign | ota | altsource | mac | traces
      Everything the Apparition Store does, from here: run it without the app (serve), add
      projects, queue builds and follow their logs, re-sign, and the OTA, AltStore and Mac download
      links. `apparition store help` for each.
  apparition crash-reports list [--app <text>] [--device <name>] | show <id> | done <id>... [--dry-run]
          | add <file.trace>... | import-dsyms <dSYMs.zip>...
      What the Store holds about crashes: crash logs pulled off devices and uploaded debug traces,
      newest first. show prints a crash log symbolicated (or a trace's MetricKit reports); done
      moves what you investigated to the Trash. An id is any unambiguous prefix of what list prints.
      add opens an encrypted problem report (a .trace file from a public copy) with this Mac's
      report key and keeps its trace; import-dsyms keeps a release's symbols so its crashes resolve.
      Reads the Store's data folder directly (STORE_DATA to use another), so it needs no login.
  apparition mcp [--local]
      An MCP server on stdin/stdout for agents here: the apps' debug servers the Mac can see
      (apps, help, call, trace, panel), plus deploy for builds made on this computer. Add it
      to an agent as a command: `claude mcp add apps -- apparition mcp`.
  apparition anisette status | check [--anisette <url>] | forget
      Which anisette sign-ins here use; ask it for its client info and, from a Mac, its
      headers, without an Apple ID (only yes or no is shown); forget the pairing with a Mac
      (unpair this computer on the Mac too, with `apparition relay devices forget`).
  apparition anisette serve [--port <port>]      (macOS)
      Serve this Mac's own anisette (v3) on loopback (port 6969 by default), for programs on
      this Mac.
  apparition browse
      List the companions mDNS finds from here, for 10 seconds: a check that multicast works.
  apparition vm setup [--ipsw <file>] | update | status | serve | quit | check | remove   (Mac with Apple silicon)
      The anisette VM, for a Mac on macOS 27, which keeps its own anisette to itself: a macOS 26
      VM that exists only for that, the one the app's Companion tab sets up. setup downloads
      macOS 26.6.2 from Apple (or uses a copy already on this Mac, checked against Apple's
      SHA-256), installs it and prepares it with no questions: about 11 minutes with the image
      on this Mac, 45 with the download. It carries on where it stopped if interrupted. Then
      sign-ins here use it, and it is saved to disk whenever it is idle or stopped (about 450
      MB), resuming in a second. It is never made again: update puts a newer app's programs
      into it from its own recoveryOS (a few minutes, no download; setup does that too when
      they are older). quit saves and stops it; check asks Apple whether it accepts the VM's
      anisette, without an Apple ID. `apparition vm help` lists the rest.
  apparition vm-setup                           (macOS)
      Set up a macOS VM on this Mac as an anisette companion, with no internet: serves this
      copy and its helper to the VM's recoveryOS and prints the line to type there. Stops by
      itself once the VM has them (selfhost/macos-vm/README.md).
  apparition link [--dir <dir>] [--remove]      (macOS and Linux)
      Make `apparition` available in any terminal: a symlink to this copy in <dir> (default
      ~/.local/bin, made if missing), and what to add to PATH if <dir> isn't on it already. From
      the Mac app, run it as /Applications/Apparition.app/Contents/MacOS/apparition-cli link.
      The link follows the app when it updates. --remove takes the link away again.
  apparition --verbose <command>      (or APPARITION_VERBOSE=1 in the environment)
      Also writes what a sign-in sends to Apple and what comes back, as structure and sizes (no
      passwords, tokens or anisette values), to apparition-verbose.log beside the log, and says
      where. For finding out what Apple changed; read it before posting it, since Apple's error
      text can name a team or an app ID.
  apparition --version | --help
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

/// A thread's stack here, the main one's included: what macOS and Linux give the main thread.
const STACK: usize = 8 << 20;

/// Runs the command line on a thread with [`STACK`] bytes of stack, and gives the runtime's
/// threads the same. Windows gives a program's main thread 1 MiB and takes no more at run
/// time; an unoptimised build's futures (the installer's and the signer's) are larger than
/// that, and it overflowed before the first command ran.
fn main() -> ExitCode {
    let run = || {
        tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(STACK)
            .enable_all()
            .build()
            .expect("couldn't start the async runtime")
            .block_on(run())
    };
    let thread = std::thread::Builder::new().name("main".into()).stack_size(STACK).spawn(run).expect("couldn't start the main thread");
    // A panic there has already printed itself; 101 is what Rust exits with for one.
    thread.join().unwrap_or(ExitCode::from(101))
}

async fn run() -> ExitCode {
    api::init();
    // The log sits beside the desktop app's, so one file tells what either did. APPARITION_LOG=<file>
    // picks another and adds idevice's debug output; the content is safe to send (no secrets).
    match std::env::var_os("APPARITION_LOG") {
        Some(path) => apparition_core::init_logging(std::path::Path::new(&path), true),
        None => apparition_core::init_logging(&api::DeviceStore::default().dir.join("apparition-core.log"), false),
    }
    // What a cancelled or killed run left in the temp folder (a .p12's key among it).
    scratch::sweep();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--verbose") {
        args.remove(0);
        apparition_core::verbose::set(true);
    }
    if apparition_core::verbose::is_on()
        && let Some(path) = apparition_core::verbose::path()
    {
        eprintln!("Verbose record: {}", path.display());
    }
    let result = match args.first().map(String::as_str) {
        Some("devices") => devices().await,
        Some("apps") => apps(&args[1..]).await,
        Some("push") => push(&args[1..]).await,
        Some("launch") => launch(&args[1..]).await,
        Some("uninstall") => uninstall(&args[1..]).await,
        Some("container") => container(&args[1..]).await,
        Some("pair") if args.get(1).map(String::as_str) == Some("--usb") => pair_usb(args.get(2).map(String::as_str)).await,
        Some("pair") => pair(args.get(1).map(String::as_str)).await,
        Some("install") => install(&args[1..]).await,
        Some("sign") => sign(&args[1..]).await,
        Some("jit") => jit(&args[1..]).await,
        Some("watches") => watches(&args[1..]).await,
        Some("inject") => inject::run(&args[1..]).await,
        Some("build") => builder::run(&args[1..]).await,
        Some("forget") => forget(args.get(1).map(String::as_str)),
        Some("signing") => signing(&args[1..]).await,
        Some("pairing") => pairing(&args[1..]).await,
        Some("certificates") => account_command(&args[1..], Slot::Certificates).await,
        Some("app-ids") => account_command(&args[1..], Slot::AppIds).await,
        Some("relay") => relay(&args[1..]).await,
        Some("anisette") => anisette(&args[1..]).await,
        Some("mac") => developer::mac(&args[1..]).await,
        Some("deploy") => developer::deploy(&args[1..]).await,
        Some("deploy-signing") => developer::deploy_signing(&args[1..]),
        Some("mcp") => developer::mcp(&args[1..]).await,
        Some("updates") => developer::updates(&args[1..]).await,
        Some("store") => store::run(&args[1..]).await,
        Some("crash-reports") => crash_reports::run(&args[1..]).await,
        Some("browse") => {
            relay::browse(10, |found| println!("found {found}")).map_err(|e| fail(format!("{e}")))
        }
        Some("link") => link(&args[1..]),
        Some("vm-setup") => vm_setup().await,
        Some("vm") => vm::run(&args[1..]).await,
        Some("--version" | "-V" | "version") => {
            println!("apparition {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
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
            match api::failure_kind(&error) {
                Some("network") => eprintln!("Run the command again to retry, or try another network."),
                Some("pairedMac") => eprintln!("`apparition anisette status` shows the Mac this computer is paired with; `apparition anisette pair <address>` pairs again."),
                _ => {}
            }
            builder::exit_code().map_or(ExitCode::FAILURE, ExitCode::from)
        }
    }
}

type Error = Report;

fn fail(message: impl Into<String>) -> Error {
    report!("{}", message.into())
}

// MARK: Putting the command on PATH

/// `link`: a symlink named `apparition` to this executable in a directory on PATH. A symlink, not a
/// copy, so it follows the app when it updates, and the anisette helper next to the real file
/// is still found (`current_exe` resolves it).
#[cfg(unix)]
fn link(args: &[String]) -> Result<(), Error> {
    let mut dir: Option<PathBuf> = None;
    let mut remove = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = Some(PathBuf::from(args.next().ok_or_else(|| fail("--dir needs a directory"))?)),
            "--remove" => remove = true,
            other => return Err(fail(format!("unknown argument {other}"))),
        }
    }
    let dir = match dir {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| fail("HOME isn't set; pass --dir"))?).join(".local/bin"),
    };
    let target = std::env::current_exe().and_then(std::fs::canonicalize).map_err(|e| fail(format!("can't find this program: {e}")))?;
    let link = dir.join("apparition");
    let existing = std::fs::symlink_metadata(&link).ok();
    if remove {
        match existing {
            Some(meta) if meta.file_type().is_symlink() => {
                std::fs::remove_file(&link).map_err(|e| fail(format!("can't remove {}: {e}", link.display())))?;
                println!("removed {}", link.display());
            }
            Some(_) => return Err(fail(format!("{} isn't a link made by `apparition link`; left alone", link.display()))),
            None => println!("{} doesn't exist", link.display()),
        }
        return Ok(());
    }
    if let Some(meta) = existing {
        // Only ever replace a link: a real file there is someone's own install.
        if !meta.file_type().is_symlink() {
            return Err(fail(format!("{} is a file, not a link; remove it first or pick another --dir", link.display())));
        }
        if std::fs::canonicalize(&link).ok().as_deref() == Some(target.as_path()) {
            println!("{} already points here", link.display());
            return path_hint(&dir);
        }
        std::fs::remove_file(&link).map_err(|e| fail(format!("can't replace {}: {e}", link.display())))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| fail(format!("can't make {}: {e}", dir.display())))?;
    std::os::unix::fs::symlink(&target, &link).map_err(|e| fail(format!("can't link {}: {e}", link.display())))?;
    println!("{} -> {}", link.display(), target.display());
    path_hint(&dir)
}

#[cfg(unix)]
fn path_hint(dir: &Path) -> Result<(), Error> {
    let on_path = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == dir));
    if !on_path {
        println!("{} isn't on your PATH yet. Add it to your shell's startup file:", dir.display());
        println!("    export PATH=\"{}:$PATH\"", dir.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn link(_args: &[String]) -> Result<(), Error> {
    Err(fail("link is for macOS and Linux; on Windows, put the folder holding apparition-cli.exe on PATH"))
}

// MARK: Setting up a macOS VM

#[cfg(target_os = "macos")]
async fn vm_setup() -> Result<(), Error> {
    use apparition_core::api::vm_setup;
    let exe = std::env::current_exe().map_err(|e| fail(format!("{e}")))?;
    let helper = exe.parent().map(|d| d.join("apparition-anisette-helper")).filter(|h| h.is_file());
    let helper = helper.ok_or_else(|| fail("apparition-anisette-helper isn't next to this program"))?;
    let mut session = vm_setup::serve(exe, helper).await.map_err(|e| fail(format!("{e:?}")))?;
    println!("In the VM's recoveryOS (Utilities > Terminal), type the line with the address the VM can reach this Mac at:\n");
    for ip in &session.addresses {
        println!("  curl -s {ip}:{}|bash", session.port);
    }
    println!("\nWhich address? The one on the VM's network: UTM's shared network, or this Mac's LAN address if the VM is bridged.");
    println!("Waiting (Ctrl-C to stop; it stops by itself after 15 minutes)...");
    let mut shown = vm_setup::Progress::default();
    loop {
        if session.progress.changed().await.is_err() {
            break;
        }
        let now = *session.progress.borrow();
        if now.script && !shown.script { println!("The VM fetched the script."); }
        if now.program && !shown.program { println!("The VM fetched apparition."); }
        if now.helper && !shown.helper { println!("The VM fetched the helper."); }
        shown = now;
        if now.done {
            println!("Done: the VM confirmed installation. Let it restart, then pair (selfhost/macos-vm/README.md, step 6).");
            return Ok(());
        }
        if now.timed_out {
            return Err(fail("nobody finished in 15 minutes; run it again"));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn vm_setup() -> Result<(), Error> {
    Err(fail("vm-setup is for a Mac: macOS VMs run only on Apple hardware"))
}

// MARK: Sharing the Apple ID's certificate key

async fn signing(args: &[String]) -> Result<(), Error> {
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
        Some("receive") => {
            if args.len() > 1 {
                return Err(fail("usage: apparition signing receive"));
            }
            receive_signing(None).await
        }
        Some("send") => {
            let usage = || fail("usage: apparition signing send --apple-id <email> [--to <address>:<port>]");
            let (mut email, mut to) = (None, None);
            let mut rest = args[1..].iter();
            while let Some(arg) = rest.next() {
                let value = rest.next().ok_or_else(usage)?;
                match arg.as_str() {
                    "--apple-id" => email = Some(value),
                    "--to" => to = Some(value.parse::<SocketAddr>().map_err(|_| fail(format!("{value} isn't an address and port, like 192.168.1.20:52144")))?),
                    _ => return Err(usage()),
                }
            }
            send_signing(&data_dir, email.ok_or_else(usage)?, to).await
        }
        Some("hand-off") => {
            let usage = || fail("usage: apparition signing hand-off (--apple-id <email> | --build-signing [<dir>] | --p12 <file>... --profile <file>...) [--device <name>] [--bundle-id <id>]");
            let (mut email, mut wanted, mut bundle_id) = (None, None, None);
            let (mut build_signing, mut p12s, mut profiles) = (None::<Option<PathBuf>>, Vec::new(), Vec::new());
            let mut rest = args[1..].iter().peekable();
            while let Some(arg) = rest.next() {
                if arg == "--build-signing" {
                    // An optional directory: the next word, unless it's another option.
                    build_signing = Some(rest.next_if(|next| !next.starts_with("--")).map(PathBuf::from));
                    continue;
                }
                let value = rest.next().ok_or_else(usage)?;
                match arg.as_str() {
                    "--apple-id" => email = Some(value),
                    "--p12" => p12s.push(PathBuf::from(value)),
                    "--profile" => profiles.push(PathBuf::from(value)),
                    "--device" => wanted = Some(value),
                    "--bundle-id" => bundle_id = Some(value.clone()),
                    _ => return Err(usage()),
                }
            }
            let custom = !p12s.is_empty() || !profiles.is_empty();
            if [email.is_some(), build_signing.is_some(), custom].iter().filter(|chosen| **chosen).count() != 1 {
                return Err(usage());
            }
            if custom && p12s.is_empty() {
                return Err(fail("--profile needs at least one --p12"));
            }
            let found = match &build_signing {
                Some(dir) => Some(api::BuildSigning::discover(dir.as_deref())?),
                None => None,
            };
            if let Some(found) = &found {
                println!("From {}: {} certificate(s), {} profile(s).", found.dir.display(), found.p12s.len(), found.profiles.len());
            }
            let device = find_device(wanted.map(String::as_str))?;
            println!("Connecting to {}…", device.name);
            let pin = || ask("The device wants to pair again. PIN: ");
            let mut tunnel = store().connect(&device, pin).await?;
            let bundle_id = match bundle_id {
                Some(id) => id,
                None => match tunnel.installed_like(APPARITION_ID).await?.as_slice() {
                    [one] => one.clone(),
                    [] => return Err(fail(format!("Apparition isn't installed on {}", device.name))),
                    several => return Err(fail(format!("several Apparitions on {}: choose one with --bundle-id ({})", device.name, several.join(", ")))),
                },
            };
            if let Some(email) = email {
                tunnel.hand_off_signing(&bundle_id, Handoff::AppleId { data_dir: &data_dir, email, team_id: None }).await?;
                println!("Put {email}'s certificate key in {bundle_id}'s inbox on {}: Apparition there now signs with this computer's certificate.", device.name);
            } else {
                let what = match &found {
                    Some(found) => found.handoff(),
                    None => Handoff::Certificate { p12s: &p12s, profiles: &profiles },
                };
                tunnel.hand_off_signing(&bundle_id, what).await?;
                println!("Put the certificates and profiles in {bundle_id}'s inbox on {}: open Apparition there and enter each .p12's password once.", device.name);
            }
            Ok(())
        }
        _ => Err(fail("usage: apparition signing receive | send --apple-id <email> | hand-off --apple-id <email> | --build-signing | --p12 … | share --apple-id <email> | import <file>")),
    }
}

/// Asks whether the other device shows the code too (both screens show it; the person checks).
fn compare_codes() -> api::nearby::Compare {
    std::sync::Arc::new(|code: &str, peer: &str| {
        println!();
        println!("    {} {}", &code[..3.min(code.len())], &code[3.min(code.len())..]);
        println!();
        let answer = ask(&format!("Does {peer} show this code too? Type `yes` if it does: "));
        answer.eq_ignore_ascii_case("yes") || answer.eq_ignore_ascii_case("y")
    })
}

/// Before listening for devices on Windows: lets their connections through Windows Defender Firewall, which drops
/// them until a rule allows this program. Windows asks about that only on the desktop and only once, so a run over
/// SSH, or a question left unanswered, is blocked without a word and the other side finds nobody here. Carries on
/// either way: the person may have other arrangements (another firewall, a rule by port).
fn open_firewall() {
    if api::firewall::inbound().is_none_or(|inbound| inbound == api::firewall::Inbound::Allowed) {
        return;
    }
    println!("Windows Defender Firewall keeps your devices from reaching this program. Allowing it (Windows may ask you to approve)…");
    match tokio::task::block_in_place(api::firewall::allow) {
        Ok(()) => println!("Allowed."),
        Err(error) => println!("{error}"),
    }
}

/// Waits for Apparition on another device or computer to hand over its certificate key, and
/// imports it. `email`: the Apple ID it's wanted for, for the instructions.
async fn receive_signing(email: Option<&str>) -> Result<(), Error> {
    open_firewall();
    let name = api::hostname();
    let email_flag = email.unwrap_or("<your Apple ID>");
    let imported = api::nearby::receive(store().apple_id_dir(), &name, |listening| {
        println!("Waiting for the certificate key as \"{name}\". On the device or computer that already signs with this Apple ID:");
        println!("  Apparition on a device: Settings > Share Signing with Another Device, then choose \"{name}\" under Nearby.");
        let address = listening.address.map(|ip| format!("{ip}:{}", listening.port));
        if let Some(address) = &address {
            // On macOS a command line multicasts only with its terminal's Local Network access.
            println!("    Not listed there? Type {address} in the address field under Nearby.");
        }
        let to = address.map(|address| format!(" --to {address}")).unwrap_or_default();
        println!("  Another computer: apparition signing send --apple-id {email_flag}{to}");
        if !listening.advertised {
            println!("  (mDNS isn't working here, so this computer won't be listed: type the address there.)");
        }
        println!("Both screens then show a six-digit code. Ctrl-C stops waiting.");
    }, compare_codes(), std::future::pending())
    .await?
    .ok_or_else(|| fail("Stopped waiting."))?;
    println!(
        "Signing with {} here now uses the same certificate as {}.",
        imported["appleId"].as_str().unwrap_or("that Apple ID"),
        imported["from"].as_str().unwrap_or("the other device")
    );
    Ok(())
}

/// Hands this computer's certificate key for `email` to a nearby Apparition that's waiting for it.
async fn send_signing(data_dir: &Path, email: &str, to: Option<SocketAddr>) -> Result<(), Error> {
    let name = api::hostname();
    let payload = api::export_apple_id_key(data_dir, email, None, &name)?;
    let to = match to {
        Some(to) => to,
        None => {
            println!("Looking for Apparition waiting for a key on this network…");
            let found: Vec<api::nearby::Found> = api::nearby::browse(5).await?.into_iter().filter(|found| found.name != name).collect();
            let chosen = match found.as_slice() {
                [] => {
                    let mut message = "No Apparition is waiting for a key on this network. On the device that needs it, open Settings > Get Signing from Another Device (or Get It from Another Device where signing stopped), then try again, or pass the address it shows with --to.".to_string();
                    if cfg!(target_os = "macos") {
                        message += " On macOS a command line can only browse the network when its terminal has Local Network access (System Settings > Privacy & Security > Local Network); --to works either way.";
                    }
                    return Err(fail(message));
                }
                [one] => one,
                several => {
                    for (index, found) in several.iter().enumerate() {
                        println!("  {}. {}", index + 1, found.name);
                    }
                    let index: usize = ask("Send to which one (number)? ").parse().map_err(|_| fail("Not sent."))?;
                    several.get(index.wrapping_sub(1)).ok_or_else(|| fail("Not sent."))?
                }
            };
            println!("Sending to {}…", chosen.name);
            chosen.address().ok_or_else(|| fail(format!("{} has no address", chosen.name)))?
        }
    };
    let peer = api::nearby::send(to, &name, payload, compare_codes()).await?;
    println!("{peer} now signs with this computer's certificate for {email}.");
    Ok(())
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

/// `apps`: what a paired device has installed, over this pairing's own tunnel. Nothing of
/// Apple's is involved (no CoreDevice, no remoted), so Xcode's DeviceHub never learns of the
/// tunnel and never opens the device's HID service, whose dropped connection wakes a sleeping
/// Apple TV. That is what makes it usable from a build tool that polls. Never asks a PIN.
async fn apps(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition apps [--device <name or UDID>] [--all] [--json]");
    let (mut wanted, mut all, mut json) = (None, false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--all" => all = true,
            "--json" => json = true,
            _ => return Err(usage()),
        }
    }
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_verified(&device).await?;
    let apps = tunnel.installed_apps(all).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "device": device.udid, "name": device.name, "apps": apps }))?);
        return Ok(());
    }
    println!("{} apps on {}:", apps.len(), device.name);
    for app in &apps {
        println!("  {}  {}  {} ({})", app.bundle_id, app.name, app.version.as_deref().unwrap_or("?"), app.build.as_deref().unwrap_or("?"));
    }
    Ok(())
}

/// `push`: installs an IPA that is already signed for the device (a build tool's output), as it
/// is, over this pairing's own tunnel, like `apps`: no CoreDevice, so Xcode's DeviceHub never
/// opens the device's HID service and a sleeping Apple TV stays asleep. Never asks a PIN.
async fn push(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition push <signed.ipa> [--device <name or UDID>] [--json]");
    let (mut ipa, mut wanted, mut json) = (None, None, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--json" => json = true,
            other if !other.starts_with("--") && ipa.is_none() => ipa = Some(PathBuf::from(other)),
            _ => return Err(usage()),
        }
    }
    let ipa = ipa.ok_or_else(usage)?;
    if !ipa.is_file() {
        return Err(fail(format!("{} isn't a file", ipa.display())));
    }
    let bundle_id = apparition_core::install_plan::inspect(&ipa).map_err(|e| fail(format!("{}: {e}", ipa.display())))?.id;
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_verified(&device).await?;
    // As in `install`: only an update can be held for an app that won't close, so only an
    // update stops waiting (after 20 s); a new install is waited for.
    let detach_after_ms = if tunnel.installed_app(&bundle_id).await?.is_some() { 20_000 } else { 0 };
    let reply = tunnel.install_signed_ipa(&ipa, Some(&bundle_id), detach_after_ms).await?;
    if json {
        let mut reply = reply;
        reply["device"] = device.udid.clone().into();
        reply["bundleId"] = bundle_id.into();
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(());
    }
    let seconds = reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0;
    if reply["queued"].as_bool() == Some(true) {
        println!("Sent {bundle_id} to {} in {seconds:.1} s; it finishes installing when the app is closed.", device.name);
    } else {
        println!("Installed {bundle_id} on {} in {seconds:.1} s.", device.name);
    }
    Ok(())
}

/// `uninstall`: removes an app over this pairing's own tunnel, like `push`, so a re-sign that
/// has to reinstall doesn't wake a sleeping Apple TV. Never asks a PIN.
async fn uninstall(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition uninstall <bundle ID> [--device <name or UDID>]");
    let (mut bundle_id, mut wanted) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            other if !other.starts_with('-') && bundle_id.is_none() => bundle_id = Some(other.to_string()),
            _ => return Err(usage()),
        }
    }
    let bundle_id = bundle_id.ok_or_else(usage)?;
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_verified(&device).await?;
    if tunnel.installed_app(&bundle_id).await?.is_none() {
        return Err(fail(format!("{bundle_id} isn't installed on {}", device.name)));
    }
    tunnel.uninstall(&bundle_id).await?;
    println!("Removed {bundle_id} from {}.", device.name);
    Ok(())
}

/// `container pull|push|count`: an app's data off the device and back, over this pairing's own
/// tunnel (house_arrest), for a re-sign that has to reinstall. Exits non-zero when any file
/// failed, after printing the reply, so a caller never takes a partial copy for a whole one.
async fn container(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition container pull|push|count <bundle ID> [--device <name or UDID>] [<folder>] [--json]");
    let mut args = args.iter();
    let action = args.next().map(String::as_str).filter(|a| ["pull", "push", "count"].contains(a)).ok_or_else(usage)?.to_string();
    let (mut bundle_id, mut folder, mut wanted, mut json) = (None, None, None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--json" => json = true,
            other if !other.starts_with('-') && bundle_id.is_none() => bundle_id = Some(other.to_string()),
            other if !other.starts_with('-') && folder.is_none() => folder = Some(PathBuf::from(other)),
            _ => return Err(usage()),
        }
    }
    let bundle_id = bundle_id.ok_or_else(usage)?;
    if (action == "count") != folder.is_none() {
        return Err(usage());
    }
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_verified(&device).await?;
    let reply = match (action.as_str(), &folder) {
        ("pull", Some(dir)) => tunnel.pull_container(&bundle_id, dir).await?,
        ("push", Some(dir)) => {
            if !dir.is_dir() {
                return Err(fail(format!("{} isn't a folder", dir.display())));
            }
            tunnel.push_container(&bundle_id, dir).await?
        }
        _ => tunnel.count_container(&bundle_id).await?,
    };
    let failed = reply["failed"].as_array().map_or(0, Vec::len);
    if json {
        println!("{}", serde_json::to_string(&reply)?);
    } else {
        let (folders, files) = (reply["folders"].as_u64().unwrap_or(0), reply["files"].as_u64().unwrap_or(0));
        let verb = match action.as_str() { "pull" => "Copied off", "push" => "Copied back", _ => "Holds" };
        println!("{verb}: {folders} folders, {files} files, {} bytes ({bundle_id} on {})", reply["bytes"].as_u64().unwrap_or(0), device.name);
        for line in reply["failed"].as_array().into_iter().flatten() {
            println!("  failed: {}", line.as_str().unwrap_or(""));
        }
    }
    if failed > 0 {
        return Err(fail(format!("{failed} item(s) failed")));
    }
    Ok(())
}

/// `launch`: starts an app over this pairing's own tunnel, like `devicectl device process
/// launch --terminate-existing [--console]` but without CoreDevice on this Mac, so a sleeping
/// Apple TV isn't woken by Xcode's DeviceHub. `--jit` starts it suspended and serves its JIT
/// requests as `jit` does, so a JIT app can be launched with its console and environment in
/// one go. Environment comes from `--env K=V` and from every `APPARITION_CHILD_<K>` variable
/// (as devicectl takes `DEVICECTL_CHILD_<K>`), which keeps secrets out of argv. Prints
/// `Launched application with pid N` once it runs. Never asks a PIN.
async fn launch(args: &[String]) -> Result<(), Error> {
    let usage = || {
        fail("usage: apparition launch <bundle ID> [--device <name or UDID>] [--env KEY=VALUE]... [--console] [--jit [--wait <seconds>]] [-- <app arguments>...]")
    };
    let (mut bundle_id, mut wanted, mut console, mut jit, mut wait) = (None, None, false, false, 0u32);
    let mut environment: Vec<(String, String)> = std::env::vars()
        .filter_map(|(k, v)| k.strip_prefix("APPARITION_CHILD_").filter(|k| !k.is_empty()).map(|k| (k.to_string(), v)))
        .collect();
    let mut app_args: Vec<String> = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => wanted = Some(args.next().ok_or_else(usage)?.as_str()),
            "--env" => {
                let pair = args.next().ok_or_else(usage)?;
                let (k, v) = pair.split_once('=').ok_or_else(|| fail(format!("--env {pair}: expected KEY=VALUE")))?;
                environment.retain(|(key, _)| key != k);
                environment.push((k.to_string(), v.to_string()));
            }
            "--console" => console = true,
            "--jit" => jit = true,
            "--wait" => wait = args.next().and_then(|a| a.parse().ok()).ok_or_else(usage)?,
            "--" => app_args.extend(args.by_ref().cloned()),
            other if !other.starts_with('-') && bundle_id.is_none() => bundle_id = Some(other.to_string()),
            _ => return Err(usage()),
        }
    }
    let bundle_id = bundle_id.ok_or_else(usage)?;
    let device = find_device(wanted)?;
    let mut tunnel = store().connect_verified(&device).await?;
    let arguments: Vec<&str> = app_args.iter().map(String::as_str).collect();
    let launched = tunnel.launch(&bundle_id, &arguments, &environment, console, jit).await?;
    let pid = launched.pid;
    println!("Launched application with pid {pid} ({bundle_id} on {}{})", device.name, if jit { ", suspended for JIT" } else { "" });
    let _ = std::io::stdout().flush();

    // The console copies to stdout until the app closes it (it exits) or this process is stopped.
    let console_task = launched.console.map(|mut stream| {
        tokio::spawn(async move {
            let mut out = tokio::io::stdout();
            let _ = tokio::io::copy(&mut stream, &mut out).await;
            use tokio::io::AsyncWriteExt;
            let _ = out.flush().await;
        })
    });

    // SIGINT and SIGTERM (how build-and-sign stops a console it started) detach a JIT session
    // cleanly, leaving the app running, rather than dropping the debugger mid-request.
    // Once the JIT session has detached, the console stops too: the signal ends this run.
    let key = pid.to_string();
    let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_flag = stopping.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        stop_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        if !api::stop_jit(&key) {
            std::process::exit(0);
        }
    });

    if jit {
        // Attaching to the suspended process resumes it under the debugger, as `jit` does.
        let reply = tunnel.serve_jit(api::JitTarget::Attach { pid, bring_back: None }, wait, &mut |line| eprintln!("  jit: {line}")).await?;
        eprintln!(
            "JIT session done ({}): {} MiB prepared over {} requests.",
            reply["ended"].as_str().unwrap_or("?"),
            reply["preparedBytes"].as_u64().unwrap_or(0) >> 20,
            reply["prepares"].as_u64().unwrap_or(0),
        );
    }
    if stopping.load(std::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }
    if let Some(task) = console_task {
        let _ = task.await;
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
        (None, []) => return Err(fail("usbmuxd sees no device. Plug it in with a cable, unlock it and tap Trust")),
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

/// `pairing hand-off`: puts the record `pair --usb` (or an Apple TV's `pair`) made for Apparition
/// in its Documents on the device, as `install` does, without reinstalling it.
async fn pairing(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition pairing hand-off [--device <name or UDID>] [--bundle-id <id>]");
    if args.first().map(String::as_str) != Some("hand-off") {
        return Err(usage());
    }
    let (mut wanted, mut bundle_id) = (None, None);
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let value = rest.next().ok_or_else(usage)?;
        match arg.as_str() {
            "--device" => wanted = Some(value.as_str()),
            "--bundle-id" => bundle_id = Some(value.clone()),
            _ => return Err(usage()),
        }
    }
    let device = find_device(wanted)?;
    if !store().has_app_pairing(&device.udid) {
        return Err(fail(match device.platform() {
            api::Platform::VisionOS => format!(
                "there's no pairing for Apparition on {} to hand over: on Apple Vision Pro, Apparition pairs itself (Setup > Pair This Headset)",
                device.name
            ),
            api::Platform::TvOS => format!(
                "there's no pairing for Apparition on {}: run `apparition pair` with Remote App and Devices open on it (it shows a second code, for Apparition), then try again",
                device.name
            ),
            api::Platform::IOS => format!(
                "there's no pairing for Apparition on {}: run `apparition pair --usb` first (over Wi-Fi works once it trusts this computer), then try again",
                device.name
            ),
        }));
    }
    println!("Connecting to {}…", device.name);
    let pin = || ask("The device wants to pair again. PIN: ");
    let mut tunnel = store().connect(&device, pin).await?;
    let bundle_id = match bundle_id {
        Some(id) => id,
        None => match tunnel.installed_like(APPARITION_ID).await?.as_slice() {
            [one] => one.clone(),
            [] => return Err(fail(format!("Apparition isn't installed on {}; `apparition install` installs it and gives it the pairing", device.name))),
            several => return Err(fail(format!("several Apparitions on {}: choose one with --bundle-id ({})", device.name, several.join(", ")))),
        },
    };
    store().hand_over_pairing(&mut tunnel, &bundle_id, &device.udid).await?;
    println!("Gave {bundle_id} on {} its pairing. Open Apparition there (close it first if it's open) and it can reach the device again.", device.name);
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
    /// Leave the app's watch app out, so the Watch (and its App ID) stays free of it.
    no_watch: bool,
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
        no_watch: false,
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
            "--no-watch" => options.no_watch = true,
            "--no-handoff" => options.handoff = false,
            "--release-site" => options.release_site = value()?,
            other if other.starts_with("--") => return Err(fail(format!("unknown option {other}"))),
            path if options.ipa.as_os_str().is_empty() => options.ipa = path.into(),
            extra => return Err(fail(format!("unexpected {extra}"))),
        }
    }
    match (&options.apple_id, &options.p12) {
        (Some(_), None) => Ok(options),
        (None, Some(_)) if options.profiles.is_empty() => Err(fail("--p12 needs at least one --profile")),
        (None, Some(_)) => Ok(options),
        _ => Err(fail("sign with either --apple-id or --p12")),
    }
}

/// A `.deb` package (how Kodi and many other apps are published) becomes an IPA in `scratch`;
/// an IPA stays as it is.
fn unpack_package(options: &mut Options, scratch: &Path) -> Result<(), Error> {
    if options.ipa.as_os_str().is_empty() {
        return Ok(());
    }
    if apparition_core::package::format(&options.ipa).map_err(|e| fail(format!("{}: {e}", options.ipa.display())))? != apparition_core::package::Format::Deb {
        if options.no_watch && options.ipa.is_file() {
            let out = scratch.join("no-watch").join(options.ipa.file_name().unwrap_or_default());
            std::fs::create_dir_all(out.parent().unwrap())?;
            if apparition_core::install_plan::without_watch(&options.ipa, &out).map_err(|e| fail(e.to_string()))? {
                println!("Leaving the watch app out.");
                options.ipa = out;
            }
        }
        return Ok(());
    }
    println!("Unpacking the app from {}…", options.ipa.display());
    let out = scratch.join("package").join(format!("{}.ipa", options.ipa.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "app".into())));
    std::fs::create_dir_all(out.parent().unwrap())?;
    let converted = apparition_core::package::convert(&options.ipa, &out)?;
    println!("  {} ({} files).", converted.app, converted.files);
    if !converted.others.is_empty() {
        println!("  The package also holds {}, left out.", converted.others.join(", "));
    }
    options.ipa = out;
    if options.no_watch {
        let bare = out_without_watch(&options.ipa)?;
        options.ipa = bare;
    }
    Ok(())
}

/// A copy of the IPA beside it without its watch app, or the IPA itself when it has none.
fn out_without_watch(ipa: &Path) -> Result<PathBuf, Error> {
    let bare = ipa.with_extension("no-watch.ipa");
    Ok(if apparition_core::install_plan::without_watch(ipa, &bare).map_err(|e| fail(e.to_string()))? {
        println!("Leaving the watch app out.");
        bare
    } else {
        ipa.to_path_buf()
    })
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

/// Signs `options.ipa` with the Apple ID for the device `udid` (registering it), freeing a full
/// certificate or App ID slot if the person agrees. Returns the signing reply and the (email, team).
async fn sign_with_apple_id(options: &Options, udid: &str, name: &str, strip: bool, platform: api::Platform) -> Result<(serde_json::Value, (String, String)), Error> {
    let mut session = sign_in(options.apple_id.as_deref().unwrap(), options.anisette.as_deref()).await?;
    println!("Signing for team {}…", session.team_id());
    let signer = (session.email().to_string(), session.team_id().to_string());
    let signed = match api::sign_with_apple_id(&mut session, options.ipa.clone(), udid, name, strip, platform).await {
        Ok(signed) => signed,
        // Another Apparition's certificate is there: get its key, or make one here if they say so.
        Err(error) if api::is_shared_certificate(&error) => {
            if !share_or_make(&mut session).await? {
                return Err(error);
            }
            println!("Signing again…");
            api::sign_with_apple_id(&mut session, options.ipa.clone(), udid, name, strip, platform).await?
        }
        // A full slot: offer to free one, then try once more.
        Err(error) => match Slot::of_error(&error.to_string()) {
            Some(slot) => {
                println!("{}", slot.full_message());
                if !free_slot(&mut session, slot).await? {
                    return Err(error);
                }
                println!("Signing again…");
                api::sign_with_apple_id(&mut session, options.ipa.clone(), udid, name, strip, platform).await?
            }
            None => return Err(error),
        },
    };
    let mut signed = signed;
    if let (Some(report), Some(path)) = (signed.get("capabilities").cloned(), signed["signedPath"].as_str().map(PathBuf::from)) {
        signed["capabilities"]["refused"] = api::capability_refusals(&mut session, &path, &report).await.into();
    }
    Ok((signed, signer))
}

/// Prints what signing changed (extensions removed, capabilities) and returns the signed app.
fn report_signed(signed: &serde_json::Value) -> Result<PathBuf, Error> {
    let signed_path = PathBuf::from(signed["signedPath"].as_str().ok_or_else(|| fail("signing returned no app"))?);
    if let Some(stripped) = signed["strippedExtensions"].as_array().filter(|s| !s.is_empty()) {
        println!("Removed extensions no profile covers: {stripped:?}. A watch app needs a profile that covers it and lists the Watch.");
    }
    if let Some(capabilities) = signed.get("capabilities") {
        println!("Capabilities asked for: {}", capabilities["requested"]);
        if let Some(refused) = capabilities["refused"].as_object().filter(|r| !r.is_empty()) {
            for (capability, reason) in refused {
                println!("  refused {capability}: {}", reason.as_str().unwrap_or_default());
            }
        }
        println!("  signed with: {}", capabilities["signed"]);
    }
    if let Some(watches) = signed["watches"].as_array() {
        match watches.as_slice() {
            [] => println!("The app has a watch app, but no Apple Watch paired with this device is known, so it can't install on one."),
            watches => {
                let names: Vec<&str> = watches.iter().filter_map(|w| w["name"].as_str()).collect();
                println!("Registered the Apple Watch {} for its watch app.", names.join(", "));
            }
        }
    }
    Ok(signed_path)
}

/// `sign`: signs for a paired device without reaching it, with an Apple ID or a .p12 and
/// profiles, and leaves the app signed on this computer. Shows what an app gets from a free
/// account or from the profile chosen for it, from anywhere.
async fn sign(args: &[String]) -> Result<(), Error> {
    let mut options = parse_install(args)?;
    if options.ipa.as_os_str().is_empty() {
        return Err(fail("sign needs an IPA"));
    }
    // A package is unpacked into a scratch folder. The signed app can end up beside what it was
    // signed from, and is what this command leaves behind, so the folder stays unless it failed.
    let unpacked = std::env::temp_dir().join(format!("apparition-unpacked-{}", std::process::id()));
    let result = match unpack_package(&mut options, &unpacked) {
        Ok(()) => sign_unpacked(&options).await,
        Err(error) => Err(error),
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&unpacked);
    }
    result
}

async fn sign_unpacked(options: &Options) -> Result<(), Error> {
    let device = find_device(options.device.as_deref())?;
    let strip = !options.keep_extensions;
    let signed = if let Some(p12) = &options.p12 {
        // The key is decrypted into a scratch folder, removed again once signed.
        let scratch = scratch::folder("sign");
        std::fs::create_dir_all(&scratch)?;
        let result = (|| {
            let password = ask_secret(&format!("Password for {}: ", p12.display()));
            let identity = api::import_identity(&scratch, &std::fs::read(p12)?, password)?;
            let id = identity["identity"]["id"].as_str().ok_or_else(|| fail("no certificate id"))?.to_string();
            for profile in &options.profiles {
                api::import_profile(&scratch, std::fs::read(profile)?, Some(&device.udid))?;
            }
            api::sign_offline(&scratch, options.ipa.clone(), &id, None, &device.udid, strip, None, device.platform())
        })();
        let _ = std::fs::remove_dir_all(&scratch);
        let signed = result?;
        println!("Signed with profile {} as {}.", signed["profileName"].as_str().unwrap_or("?"), signed["bundleId"].as_str().unwrap_or("?"));
        signed
    } else {
        sign_with_apple_id(&options, &device.udid, &device.name, strip, device.platform()).await?.0
    };
    let signed_path = report_signed(&signed)?;
    println!("Signed for {}, nothing installed: {}", device.name, signed_path.display());
    Ok(())
}

/// `watches`: the Apple Watches paired with an iPhone, which signing an app with a watch app
/// registers on the team alongside the phone.
async fn watches(args: &[String]) -> Result<(), Error> {
    let (device, install) = match args {
        [] => (None, None),
        [flag, name] if flag == "--device" => (Some(name.as_str()), None),
        [flag, app] if flag == "--install" => (None, Some(PathBuf::from(app))),
        [flag, name, flag2, app] if flag == "--device" && flag2 == "--install" => (Some(name.as_str()), Some(PathBuf::from(app))),
        _ => return Err(fail("usage: apparition watches [--device <name or UDID>] [--install <signed watch .app>]")),
    };
    let device = find_device(device)?;
    println!("Connecting to {}…", device.name);
    let mut tunnel = store().connect(&device, || ask("The device wants to pair again. PIN: ")).await?;
    let watches = tunnel.paired_watches().await?;
    if watches.is_empty() {
        println!("No Apple Watch is paired with {}.", device.name);
    }
    for watch in &watches {
        println!("  {} ({}) {} watchOS {}", watch.name, watch.udid, watch.product_type, watch.os_version);
    }
    if let Some(app) = install {
        let dir = store().dir.clone();
        for watch in &watches {
            println!("Installing {} on {}…", app.display(), watch.name);
            let waiting = |what: &str| println!("{what}");
            let mut companion = tunnel.watch(&watch.udid, &dir, &waiting).await?;
            let reply = companion.install(&app, &|_| {}).await?;
            println!("Installed on {} in {:.1} s.", watch.name, reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0);
        }
    }
    Ok(())
}

async fn jit(args: &[String]) -> Result<(), Error> {
    let usage = || fail("usage: apparition jit <bundle ID> [--attach [<pid>]] [--device <name or UDID>] [--wait <seconds>]");
    let (mut bundle_id, mut attach, mut pid, mut device, mut wait) = (None, false, None, None, 0u32);
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--attach" => {
                attach = true;
                if let Some(number) = args.peek().and_then(|a| a.parse::<u64>().ok()) {
                    pid = Some(number);
                    args.next();
                }
            }
            "--device" => device = Some(args.next().ok_or_else(usage)?.clone()),
            "--wait" => wait = args.next().and_then(|a| a.parse().ok()).ok_or_else(usage)?,
            other if !other.starts_with('-') && bundle_id.is_none() => bundle_id = Some(other.to_string()),
            _ => return Err(usage()),
        }
    }
    let bundle_id = bundle_id.ok_or_else(usage)?;
    let device = find_device(device.as_deref())?;
    println!("Connecting to {}…", device.name);
    let pin = || ask("The device wants to pair again. PIN: ");
    let mut tunnel = store().connect(&device, pin).await?;
    // What a session is stopped by: its pid when attached (no bundle ID is passed then).
    let mut key = bundle_id.clone();
    let target = if attach {
        let pid = match pid {
            Some(pid) => pid,
            None => tunnel.running_pid(&bundle_id).await?.ok_or_else(|| fail(format!("{bundle_id} isn't running; open it first, or leave out --attach")))?,
        };
        key = pid.to_string();
        println!("Attaching to {bundle_id} (pid {pid}). Leave this running while you use the app; Ctrl-C detaches.");
        api::JitTarget::Attach { pid, bring_back: None }
    } else {
        println!("Launching {bundle_id}. Leave this running while you use the app; Ctrl-C detaches.");
        api::JitTarget::Launch(&bundle_id)
    };
    // Ctrl-C detaches cleanly, leaving the app running, rather than dropping the tunnel.
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            api::stop_jit(&key);
        }
    });
    let reply = tunnel.serve_jit(target, wait, &mut |line| println!("  {line}")).await?;
    println!(
        "Done ({}): {} MiB prepared over {} requests; {} faults and {} signals passed to the app.",
        reply["ended"].as_str().unwrap_or("?"),
        reply["preparedBytes"].as_u64().unwrap_or(0) >> 20,
        reply["prepares"].as_u64().unwrap_or(0),
        reply["faults"].as_u64().unwrap_or(0),
        reply["signals"].as_u64().unwrap_or(0),
    );
    Ok(())
}

async fn install(args: &[String]) -> Result<(), Error> {
    let mut options = parse_install(args)?;
    let device = find_device(options.device.as_deref())?;
    // Signed into a scratch folder, removed afterwards with what it holds (a .p12's key).
    let scratch = scratch::folder("installer");
    std::fs::create_dir_all(&scratch)?;
    let result = download_and_install(&mut options, &device, &scratch).await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

/// The latest official Apparition for the device's platform, downloaded into `dir` once its release
/// signature and hash check out.
async fn latest_apparition(site: &str, dir: &Path, platform: api::Platform) -> Result<PathBuf, Error> {
    println!("Checking {site} for the latest Apparition…");
    let verified = release::latest_app(site, api::release_platform(platform)).await?;
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
    unpack_package(options, scratch)?;
    println!("Connecting to {}…", device.name);
    let pin = || ask("The headset wants to pair again. PIN: ");
    let mut tunnel = store().connect(device, pin).await?;
    let info = tunnel.device_info().await?;
    // A watch app needs the paired Watch registered with it; only the iPhone can say which.
    if api::Platform::of_product(&info.product_type) == api::Platform::IOS
        && (apparition_core::install_plan::is_watch_only(&options.ipa)
            || apparition_core::install_plan::inspect(&options.ipa).is_ok_and(|inventory| inventory.has_watch_app()))
    {
        for watch in tunnel.paired_watches().await? {
            println!("Paired Apple Watch: {} ({}, watchOS {}).", watch.name, watch.product_type, watch.os_version);
        }
    }
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
        let (signed, signer) = sign_with_apple_id(options, &info.udid, &info.name, strip, api::Platform::of_product(&info.product_type)).await?;
        apple_id = Some(signer);
        signed
    };
    let signed_path = report_signed(&signed)?;
    let installed_id = api::bundle_id(&signed_path).ok_or_else(|| fail("the signed app has no bundle ID"))?;
    if signed["watchOnly"].as_bool() == Some(true) {
        // A watchOS app on its own: it goes to the Watch through the iPhone, and nothing to the phone.
        let watches = signed["watches"].as_array().cloned().unwrap_or_default();
        if install_on_watches(&watches, &signed_path, tunnel).await? == 0 {
            return Err(fail("the watch app didn't install on any Apple Watch"));
        }
        return Ok(());
    }

    // installd holds an update of an open app until it closes, so the install closes it first.
    let installed = tunnel.installed_app(&installed_id).await?;
    if let Some(name) = &installed {
        println!("{name} is already on {}; if it's open there, it closes for the update.", info.name);
    }
    println!("Installing {installed_id} on {}…", info.name);
    let source = options.ipa.is_file().then_some(options.ipa.as_path());
    // Only an update can be held for an open app (one that couldn't be closed), so only an
    // update stops waiting after 20 s; a new install, however large, is waited for.
    let detach_after_ms = if installed.is_some() { 20_000 } else { 0 };
    let reply = tunnel.install(&signed_path, source, detach_after_ms).await?;
    let seconds = reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0;
    if reply["closed"].as_bool() == Some(true) {
        println!("Closed {installed_id} on {} so the update could install.", info.name);
    }
    if reply["queued"].as_bool() == Some(true) {
        println!(
            "Sent in {seconds:.1} s, and {} is still installing it. If the app is open there, the update finishes when it's closed.",
            info.name
        );
    } else {
        println!("Installed in {seconds:.1} s.");
    }
    install_watch_apps(&signed, &signed_path, tunnel).await?;

    if options.handoff && installed_id.starts_with(APPARITION_ID) {
        if store().hand_over_pairing(tunnel, &installed_id, &info.udid).await? {
            println!("Gave Apparition its pairing with this device.");
        }
        handoff(options, tunnel, &installed_id, apple_id.as_ref()).await?;
    }
    Ok(())
}

/// Installs the signed watch app on each Apple Watch the signing registered, straight through
/// the iPhone: iOS only copies a watch app to the Watch itself for App Store and paid-team
/// profiles, so a free account's has to go this way.
async fn install_watch_apps(signed: &serde_json::Value, signed_path: &Path, tunnel: &mut api::Tunnel) -> Result<(), Error> {
    let Some(watches) = signed["watches"].as_array().filter(|w| !w.is_empty()) else { return Ok(()) };
    let watch_dir = signed_path.join("Watch");
    let Some(app) = std::fs::read_dir(&watch_dir)
        .ok()
        .and_then(|entries| entries.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|e| e == "app")))
    else {
        println!("The signed app has no watch app left to install.");
        return Ok(());
    };
    install_on_watches(watches, &app, tunnel).await?;
    Ok(())
}

/// Puts the signed watch app `app` on each of `watches` through the iPhone; how many took it.
async fn install_on_watches(watches: &[serde_json::Value], app: &Path, tunnel: &mut api::Tunnel) -> Result<usize, Error> {
    let dir = store().dir.clone();
    let mut installed = 0;
    for watch in watches {
        let (Some(udid), Some(name)) = (watch["udid"].as_str(), watch["name"].as_str()) else { continue };
        println!("Installing the watch app on {name}…");
        let waiting = |what: &str| println!("{what}");
        let result = async {
            let mut companion = tunnel.watch(udid, &dir, &waiting).await?;
            companion.install(app, &|_| {}).await
        }
        .await;
        match result {
            Ok(reply) => {
                installed += 1;
                println!("Installed on {name} in {:.1} s.", reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0)
            }
            Err(error) => println!("Couldn't install on {name}: {error}\n  If the app was signed with your own certificate, its profile has to list this Watch's UDID ({udid}) as well as the iPhone's."),
        }
    }
    Ok(installed)
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
        tunnel.hand_off_signing(installed_id, Handoff::AppleId { data_dir: &data_dir, email, team_id: Some(team) }).await?;
        println!("Shared this computer's certificate with Apparition: sign in there with {email} and it signs with the same one.");
        return Ok(());
    };
    tunnel.hand_off_signing(installed_id, Handoff::Certificate { p12s: std::slice::from_ref(p12), profiles: &options.profiles }).await?;
    println!("Put your certificate and profiles in Apparition's inbox: open Apparition and enter the .p12's password once.");
    Ok(())
}

// MARK: Apple ID and its slots

/// The anisette a sign-in uses: `requested` (`builtin`, `paired` or a v3 server's address), or
/// the default (`api::default_anisette`).
fn anisette_for(requested: Option<&str>) -> Result<String, Error> {
    match requested {
        Some(requested) => Ok(requested.to_string()),
        None => api::default_anisette(&store().apple_id_dir()).map(str::to_string).ok_or_else(|| fail(api::NO_ANISETTE)),
    }
}

async fn sign_in(email: &str, anisette: Option<&str>) -> Result<api::Session, Error> {
    let anisette = anisette_for(anisette)?;
    let password = ask_secret(&format!("Apple ID password for {email}: "));
    println!("Signing in…");
    let (session, _) = api::sign_in(email, password, &anisette, &store().apple_id_dir(), two_factor).await?;
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
    let mut session = sign_in(&email, anisette.as_deref()).await?;
    manage(&mut session, slot).await?;
    Ok(())
}

/// Signing stopped before making a second Apparition certificate: get the first one's key over
/// the network, or make one here. True when signing should go again.
async fn share_or_make(session: &mut api::Session) -> Result<bool, Error> {
    let email = session.email().to_string();
    println!("Apparition on another device or computer already has a certificate for {email}, which this computer could sign with too.");
    println!("A free account holds only two, so rather than make another here, get that one's key:");
    println!("  1. Receive it now over this network (on the other device: Settings > Share Signing with Another Device > Nearby).");
    println!("  2. Make a new certificate here instead. It takes the account's other slot, and the other device keeps its own.");
    println!("  (Not on this network? There: Share Signing > Key File, then `apparition signing import <file>` here.)");
    match ask("1, 2, or Return to stop: ").as_str() {
        "1" => {
            receive_signing(Some(&email)).await?;
            Ok(true)
        }
        "2" => {
            session.allow_new_certificate();
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// A full slot: for certificates, get another Apparition's key (which needs no slot) or revoke
/// one; for App IDs, delete one. True when signing should go again.
async fn free_slot(session: &mut api::Session, slot: Slot) -> Result<bool, Error> {
    if slot == Slot::Certificates {
        println!("Revoking a certificate stops every app signed with it. If one of them is Apparition's on another device, sign with that one instead:");
        println!("  1. Receive its key now over this network (on that device: Settings > Share Signing with Another Device > Nearby).");
        println!("  2. Choose a certificate to revoke.");
        match ask("1, 2, or Return to stop: ").as_str() {
            "1" => {
                let email = session.email().to_string();
                receive_signing(Some(&email)).await?;
                return Ok(true);
            }
            "2" => {}
            _ => return Ok(false),
        }
    }
    manage(session, slot).await
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
    if certificates.iter().any(|cert| cert["madeByApparition"].as_bool() == Some(true) && cert["inUseHere"].as_bool() != Some(true)) {
        println!("Rather than revoke Apparition's, share it: run `apparition signing receive` here, and on that device choose Settings > Share Signing with Another Device > Nearby.");
    }
    let Some(cert) = choose(&certificates, "Revoke which one (number, or Return to keep them all)? ") else {
        return Ok(false);
    };
    let warning = if cert["inUseHere"].as_bool() == Some(true) {
        "This computer signs with it: apps it installed stop opening until installed again."
    } else if cert["madeByApparition"].as_bool() == Some(true) {
        "Apps that device signed with it stop opening until it signs them again, with a new certificate that needs a free slot. Sharing its key (`apparition signing receive`) keeps them working."
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
    if args.first().map(String::as_str) == Some("devices") {
        return relay_devices(&args[1..]);
    }
    let (mut port, mut advertise, mut prompt) = (relay::RELAY_PORT, true, true);
    let mut anisette = if api::native_anisette() { "builtin" } else { "none" }.to_string();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().cloned().ok_or_else(|| fail(format!("{arg} needs a value")));
        match arg.as_str() {
            "--port" => port = value()?.parse().map_err(|_| fail("--port needs a number"))?,
            "--no-mdns" => advertise = false,
            "--no-prompt" => prompt = false,
            "--anisette" => anisette = value()?,
            other => return Err(fail(format!("unexpected {other}"))),
        }
    }
    let log: relay::Log = std::sync::Arc::new(|line: &str| println!("{line}"));
    open_firewall();
    let companion = std::sync::Arc::new(Companion::start(port, relay::ANISETTE_PORT, advertise, &anisette_dir(), log).await?);
    println!("Apparition companion relay listening on port {}.", companion.relay.port());
    let address = relay::primary_address().map(|ip| ip.to_string()).unwrap_or_else(|| "this computer's address".into());
    if advertise {
        println!("In Apparition, use \"Find companion\" on this network, or enter {address}:{}.", companion.relay.port());
    } else {
        println!("Not announced on the network: in Apparition, enter {address}:{} as the companion.", companion.relay.port());
    }
    // A device asking to pair: show its code and take the answer from the terminal. With no
    // terminal (launchd, systemd, nohup) nobody can answer, so it's declined and the log says
    // how to pair instead; devices paired before keep working.
    use std::io::IsTerminal;
    let interactive = prompt && std::io::stdin().is_terminal();
    let (asked, mut questions) = tokio::sync::mpsc::unbounded_channel::<u64>();
    let headless = companion.devices.clone();
    companion.devices.on_pairing(std::sync::Arc::new(move |id, request| {
        if !interactive {
            println!(
                "{} ({}) asked to pair; declined, as nobody can answer here. Pair it by running `apparition relay` in a terminal once, or with the desktop app.",
                request.device_name, request.peer
            );
            headless.decide(id, false);
            return;
        }
        println!(
            "\n{} ({}) asks for {}. Its code: {}\nAllow it if the same code shows on the device or computer. Allow? [y/N]",
            request.device_name, request.peer, describe_grants(&request.grants), request.code
        );
        let _ = asked.send(id);
    }));
    let devices = companion.devices.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Some(id) = questions.recv().await {
            let allow = matches!(lines.next_line().await, Ok(Some(line)) if line.trim().eq_ignore_ascii_case("y"));
            if devices.decide(id, allow) {
                println!("{}", if allow { "Paired." } else { "Declined." });
            }
        }
    });
    match anisette.as_str() {
        "builtin" => {
            companion.share_builtin_anisette().await?;
            println!("Offering this Mac's own anisette to paired devices and computers, over the encrypted channel; Apple TLS stays on the device.");
        }
        "android" => {
            companion.share_android_anisette(anisette_dir()).await?;
            println!("Offering Apparition's own Android anisette to paired devices and computers, over the encrypted channel; Apple TLS stays on the device.");
        }
        "none" if api::builtin_anisette_available() => println!("Not offering anisette: this Mac doesn't give its own (macOS 27 outside a VM set up for it)."),
        "none" => println!("Not offering anisette: only a Mac does. Pair your devices with your Mac's Apparition for it."),
        other => return Err(fail(format!("--anisette is builtin, android or none, not {other}"))),
    }
    // The Store running here (the app's or `apparition store serve`), for the computers paired
    // with developer access: looked for again every few seconds, as it starts, stops and moves.
    let developer = companion.clone();
    tokio::spawn(async move {
        let mut offered: Option<std::net::SocketAddr> = None;
        loop {
            let route = apparition_service::hosted::running(&apparition_service::hosted::store_dir()).map(|(route, _)| route);
            let address = route.as_ref().map(|r| r.address);
            if address != offered {
                match &route {
                    Some(r) => println!("Offering the Apparition Store at http://{} to computers paired for developer access.", r.address),
                    None if offered.is_some() => println!("The Apparition Store here stopped: paired computers can't reach it now."),
                    None => {}
                }
                developer.set_developer(route);
                offered = address;
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
    let _ = tokio::signal::ctrl_c().await;
    Ok(())
}

/// What a pairing asks for, in words.
fn describe_grants(grants: &[String]) -> String {
    let words: Vec<&str> = grants.iter().map(|g| match g.as_str() {
        "anisette" => "this Mac's anisette",
        "appleRelay" => "relaying encrypted Apple connections",
        "developer" => "developer access to the Apparition Store here (builds, deploys, its apps' debug servers)",
        other => other,
    }).collect();
    if words.is_empty() { "pairing".into() } else { words.join(", ") }
}

/// `relay devices [forget <id>]`.
fn relay_devices(args: &[String]) -> Result<(), Error> {
    let devices = companion::PairedDevices::open(&anisette_dir());
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            let list = devices.list();
            if list.is_empty() {
                println!("No devices are paired with this computer's anisette.");
            }
            for device in list {
                println!("{}  {}", device.id, device.name);
            }
            Ok(())
        }
        ["forget", id] => {
            if devices.forget(id) {
                println!("Unpaired {id}; its anisette access stops now (restart a running relay for it to notice).");
                Ok(())
            } else {
                Err(fail(format!("no paired device {id}")))
            }
        }
        _ => Err(fail("usage: apparition relay devices [forget <id>]")),
    }
}

async fn anisette(args: &[String]) -> Result<(), Error> {
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["pair", address] => {
            println!("Asking {address} to pair…");
            let paired = api::paired_mac::pair(&anisette_dir(), address, |code| {
                println!("Code: {code}\nOn the Mac, allow this computer if it shows the same code.");
            })
            .await?;
            println!("Paired with {}. Sign-ins here use its anisette.", paired.mac_name);
            Ok(())
        }
        ["status"] => {
            match api::default_anisette(&store().apple_id_dir()) {
                Some(api::BUILTIN_ANISETTE) => println!("Sign-ins use this Mac's own anisette."),
                Some(api::VM_ANISETTE) => println!("Sign-ins use this Mac's anisette VM (`apparition vm status`)."),
                Some(api::ANDROID_ANISETTE) => println!("Sign-ins use Apparition's own Android anisette (Apple Music's libraries, run in this program). Apple accepts it for sign-in and the Xcode token as of 12 Nov 2026."),
                Some(_) => {
                    let paired = api::paired_mac::status(&anisette_dir()).expect("paired");
                    println!("Sign-ins use the anisette of {}, at {}, over the encrypted channel.", paired.mac_name, paired.address);
                }
                None => println!("{}", api::NO_ANISETTE),
            }
            Ok(())
        }
        ["check"] | ["check", "--anisette", _] => {
            let requested = anisette_for(args.get(2).map(String::as_str))?;
            let check = api::check_anisette(&requested, &store().apple_id_dir()).await?;
            println!("Anisette answers: serves {}, claims {}.", check.serves.as_deref().unwrap_or("(doesn't say)"), check.claims);
            match check.headers {
                Some(true) => println!("A Mac's headers: yes. Sign-in can use it."),
                Some(false) => return Err(fail("the Mac's anisette gave no headers")),
                None => println!("Not a Mac's anisette. A sign-in claims what it is (Android) and asks for the Xcode token as a Mac, which Apple accepted on 12 Nov 2026; Apple may stop."),
            }
            Ok(())
        }
        ["forget"] => {
            if api::paired_mac::forget(&anisette_dir()) {
                println!("Forgot the paired Mac. Unpair this computer there too (`apparition relay devices forget <id>`, or the app's list).");
            } else {
                println!("This computer isn't paired with a Mac.");
            }
            Ok(())
        }
        ["serve"] | ["serve", "--port", _] => {
            let port: u16 = match args.get(2) {
                Some(port) => port.parse().map_err(|_| fail("--port needs a number"))?,
                None => relay::ANISETTE_PORT,
            };
            #[cfg(target_os = "macos")]
            {
                let server = apparition_core::anisette::start(SocketAddr::from(([127, 0, 0, 1], port)), None).await?;
                println!("Serving this Mac's own anisette (v3) on 127.0.0.1:{}. Ctrl-C stops it.", server.address.port());
                let _ = tokio::signal::ctrl_c().await;
                Ok(())
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = port;
                Err(fail("only a Mac serves its own anisette; pair this computer with one: apparition anisette pair <address>"))
            }
        }
        _ => Err(fail("usage: apparition anisette pair <address> | status | check [--anisette <url>] | forget | serve [--port <port>]")),
    }
}
