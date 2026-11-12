<p align="center"><img src="docs/images/icon.png" width="128" alt="Apparition's icon: a ghost holding an app tile"></p>

# Apparition

Apparition is an on-device developer tool for Apple Vision Pro, iPhone, iPad and Apple TV. It keeps
the apps you sideload with your own free Apple ID signed and running past Apple's seven-day limit
without a cable or a trip back to your desk, and it launches them with JIT for emulators such as
UTM. A Mac gives the most complete and most stable setup: it can provide the sign-in data Apple
wants ("anisette") from macOS itself, over the network, and needn't be on the same Wi-Fi
(Tailscale works). **A Mac is not required, though: Windows and Linux computers are fully
supported** and run the same app, as the [computer-by-computer guide below](#start-here-pick-your-computer)
says. Apps
install from AltStore-format sources, from a GitHub project's releases, or from an IPA you have,
signed with a free Apple ID or your own certificate. A computer installs Apparition on the device
the first time; after that, refreshing and JIT run on the device, reaching your Mac only when the
Apple ID work needs it.

<p align="center">
  <img src="docs/images/desktop.png" height="420" alt="The desktop app's welcome screen">
</p>


## Start here: pick your computer

Apparition's desktop app runs on **macOS, Windows and Linux**, and all three install Apparition on
the device, refresh it, launch apps with JIT and act as the companion that keeps those working away
from home. **None of that needs a Mac.** What differs is how you sign in with a free Apple ID,
because Apple wants an "anisette" source for that, and a Mac is the most reliable one.

| | **Mac** (most complete, most stable) | **Windows** | **Linux** |
|---|---|---|---|
| Get the app | Download the zip from [apparition.illixion.com](https://apparition.illixion.com), move **Apparition** to Applications, then run `xattr -dr com.apple.quarantine /Applications/Apparition.app` (it isn't notarized) | Download the zip (x64 or ARM64), unzip it to a folder you can write to and open **Apparition.exe**. SmartScreen may warn, since it isn't signed by a publisher: choose More info, then Run anyway. Needs Microsoft Edge WebView2, which Windows 10 and 11 normally have | Download the **.deb** (Debian, Ubuntu) or the **AppImage** (anything else; `chmod +x` it first) for your CPU, x64 or ARM64. Needs glibc 2.35 or newer |
| Pair a Vision Pro or Apple TV | ✅ Over Wi-Fi with the code it shows | ✅ | ✅ |
| Pair an iPhone or iPad | ✅ Cable once, then Wi-Fi | 🔧 Cable once, then Wi-Fi. Install **Apple Devices** (or iTunes) first: it provides the USB connection | 🔧 Cable once, then Wi-Fi. Install and start `usbmuxd` first |
| Install, refresh, JIT, companion | ✅ | ✅ | ✅ |
| Sign in with a free Apple ID | ✅ The Mac's own anisette, or a macOS 26 VM on macOS 27. Nothing else needed | 🔧 Pair with a Mac running Apparition (best), **or** run your own anisette server in Docker (Docker Desktop or WSL, [one command](docs/ANISETTE.md#run-your-own-anisette-server-with-docker)), **or** give a third-party server's address. No Mac needed for the last two | ✅ Apparition's **own Android anisette** (nothing to set up), or pair with a Mac, or [your own server in Docker](docs/ANISETTE.md#run-your-own-anisette-server-with-docker), or a third-party server's address |
| Sign with a paid account's certificate | ✅ no sign-in and no anisette | ✅ | ✅ |
| Build your own apps | ✅ Xcode, live patching, the whole Develop workflow | 🔧 `apparition deploy` from a PC that pairs with a Mac, or [xtool](https://github.com/xtool-org/xtool) through WSL ([how](docs/REMOTE_DEVELOPER.md#without-a-mac-xtool-on-windows-or-linux)) | 🔧 Same as Windows, with xtool running natively |

**What "no Mac" means.** On Windows and Linux the app installs and refreshes everything without one.
Only the Apple ID sign-in needs an anisette source, and you have four: a Mac of your own, Apparition's
own Android anisette on Linux (and macOS), an anisette server you run yourself in Docker, or a third-party
server's address. The last two work on any computer, Windows included. Apple
decides what it accepts and has changed that several times since October, so a Mac you own is the
setup least likely to break; the others work today, and Apparition keeps them for as long as Apple lets them. If you have a Mac at all, even an old Intel MacBook left on a shelf, pair it as a companion.

The device itself needs Developer Mode on (Settings > Privacy & Security), and a free Apple ID's
limits apply wherever you sign in ([About Apple IDs](docs/USING.md#about-apple-ids)). The full
comparison of anisette sources is in [docs/ANISETTE.md](docs/ANISETTE.md).

## What it does

- **Install apps** from AltStore and SideStore sources, from a GitHub project's releases
  (`github:owner/repo`), from an IPA you have, or from a `.deb` package. On the device, share an
  `.ipa` or `.deb` from Safari or Files to Apparition and it asks before installing.
- **Sign** with a free Apple ID (apps last 7 days, two certificates) or with a paid account's
  certificate and profile (a year, no sign-in). Apps that ask for the increased memory limit get it.
- **Refresh** apps before they expire, on the device: from the app, a Shortcuts automation or in
  the background, through the companion or LocalDevVPN.
- **JIT that works on current chips.** iOS 26 on A15 and M2 and later, and visionOS 27 on Vision Pro,
  no longer let an app run generated code just because a debugger attached once. **Launch with JIT**
  keeps the debugger attached and prepares the memory the app asks for, on the device itself. It speaks
  the JIT26 requests StikDebug defined, so apps built for them work as they are. See [docs/JIT.md](docs/JIT.md).
- **Pair once, then work over Wi-Fi.** Vision Pro and Apple TV pair with the code they show; an iPhone or
  iPad pairs over USB once.
- **Share one certificate** between your devices and computers, so a free account's two certificates
  never run out, and export it as a `.p12` for apps such as [LiveContainer](docs/SUPPORTED_APPS.md#livecontainer)
  ([how](docs/PAIRING_AND_SIGNING.md)).
- **Away from home**, a companion on your computer keeps signing, refreshing and JIT working over
  [Tailscale](docs/REMOTE_ACCESS.md).
- **Build and debug your own apps** with the Apparition Store, built into the Mac app: builds, crash reports,
  live patching and an MCP server for agents ([docs/DEVELOPER.md](docs/DEVELOPER.md)).

<p align="center">
  <img src="docs/images/gta-sa-vision-pro.png" width="420" alt="GTA San Andreas running in ARMSX2 on Apple Vision Pro, launched with JIT from Apparition">
</p>

## Getting started

1. Download the desktop app for macOS, Windows or Linux from
   [apparition.illixion.com](https://apparition.illixion.com) and open it. It isn't notarized,
   so on macOS move it to Applications and clear the download quarantine first:
   `xattr -dr com.apple.quarantine /Applications/Apparition.app`.
2. Pair the device, on the same Wi-Fi as the computer:
   - **Vision Pro:** open Settings > General > Remote Devices. The headset shows up in the
     desktop app; pick it and type the code the headset shows.
   - **Apple TV:** open Settings > Remotes and Devices > Remote App and Devices, and pair the
     same way. The TV asks for a code twice: once for the computer, once for Apparition's own
     pairing, which the desktop app hands over at install.
   - **iPhone or iPad:** plug it into the computer with a USB cable and unlock it, then tap
     Trust and enter its passcode if it asks. There's no code to type. You only need the cable
     until the device trusts the computer: after that, pairing it again works over Wi-Fi.
3. Sign in with your Apple ID, or choose your own certificate and profile, and install. The sign-in's
   **Anisette data** choice depends on your computer:
   - **Mac on macOS 26 or earlier:** nothing to choose, it uses the Mac's own.
   - **Mac on macOS 27:** the anisette VM (Companion > Set up the anisette VM).
   - **Windows:** From my paired Mac, or From a third-party anisette server and its address, which can be
     one you run yourself in [Docker](docs/ANISETTE.md#run-your-own-anisette-server-with-docker).
     Apparition's own isn't available on Windows.
   - **Linux:** Apparition's own (Android libraries) with nothing to set up, or either of the Windows choices.

   Apparition opens on the device ready to sign: the desktop app hands it the pairing and the signing it used.
4. If the device won't open Apparition, it needs one of two settings. Turn on Developer Mode
   under Settings > Privacy & Security, which restarts the device. Or, if it says the developer
   isn't trusted, trust it under Settings > General > VPN & Device Management.
5. Leave the desktop app running if you like: as the companion, it keeps refreshing and JIT
   working while the device is away from home ([how](docs/HOW_IT_WORKS.md)).

The command line, `apparition`, does the same from a terminal: see [docs/COMMAND_LINE.md](docs/COMMAND_LINE.md),
which also says how to put it on your `PATH`.

**About Apple IDs.** Use a spare Apple ID for sideloading if you can. Apparition never stores the password
unless you turn on Remember password on the device, where it stays in the Keychain, and it sends it only to
Apple. See [docs/USING.md](docs/USING.md#about-apple-ids) for the certificate limits.

## Documentation

| | |
|---|---|
| [Anisette and signing in](docs/ANISETTE.md) | Every anisette source, running your own in Docker, how the built-in one works, troubleshooting |
| [Using Apparition day to day](docs/USING.md) | What refreshes when, permission prompts, Shortcuts, your own certificate |
| [JIT](docs/JIT.md) | How Launch with JIT works and what it needs |
| [Pairing and sharing signing](docs/PAIRING_AND_SIGNING.md) | When Apparition says it isn't paired; one certificate across devices; exporting a `.p12` |
| [Away from home](docs/REMOTE_ACCESS.md) | The companion over Tailscale, waking a sleeping Mac |
| [How it fits together](docs/HOW_IT_WORKS.md) | Refreshing, the companion, the firewall and mDNS |
| [The desktop app](docs/DESKTOP_APP.md) | Install and Develop modes, updates, install options and App IDs |
| [The command line](docs/COMMAND_LINE.md) | Every `apparition` command |
| [Building your own apps](docs/DEVELOPER.md) | The Develop workflow, the [Developer tab](docs/DEVELOPER_APP.md), the [Store](docs/STORE.md), [remote development](docs/REMOTE_DEVELOPER.md) |
| [Supported apps](docs/SUPPORTED_APPS.md) | What has been tried, app by app |
| [Releases](docs/RELEASES.md) | How a release is built, attested and signed |
| [Roadmap](docs/ROADMAP.md) and [the lab](docs/LAB.md) | Where the developer workflow stands, and the live-patching experiments |
| [The anisette VM](selfhost/macos-vm/README.md) | A small macOS VM for a Mac on macOS 27 |

## Build

The Apple app builds against two sibling packages by path, so clone them next to this repo:
[DebugTrace](https://github.com/illixion/DebugTrace) and [RAVESDK](https://github.com/illixion/RAVESDK)
(`../DebugTrace`, `../RAVESDK`). The desktop app and command line need only this repo and Rust.

    cp build-signing.conf.example build-signing.conf   # then set your device names
    scripts/build-core.sh      # builds the Rust core (isideload + idevice) for visionOS and iOS
    scripts/build-core.sh visionos-sim   # adds the visionOS simulator (rustup target add aarch64-apple-visionos-sim)
    xcodegen                   # generates Apparition.xcodeproj from project.yml
    apparition build           # or open Apparition.xcodeproj and Run
    PLATFORM=iPad apparition build --no-mcp    # the lab app on iPad (PLATFORM=iOS for iPhone)

The desktop app and its packaging are in [docs/DESKTOP_APP.md](docs/DESKTOP_APP.md#building-it).

Contributions are welcome ([CONTRIBUTING.md](CONTRIBUTING.md)); security problems go through [SECURITY.md](SECURITY.md).

Licensed MIT. Signing (`core/signer`) builds on [apple-codesign](https://github.com/indygreg/apple-platform-rs)
(MPL-2.0), used unmodified from crates.io.

## Support

I'm Ixion. I make Apparition, and everything else I publish, as free and open-source software, and I don't expect anything in return. If it's useful to you and you'd like to help me keep building, you can [sponsor me on GitHub](https://github.com/sponsors/illixion). If not, I hope you enjoy it all the same. 💖
