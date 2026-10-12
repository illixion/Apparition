# Apparition

An on-device sideloader for Apple Vision Pro: install apps from AltStore-format
sources, sign them with your own (free) Apple ID, keep them refreshed, and
launch them with debugger-prepared JIT, all from the headset.

Early spike stage. `Probe/` is spike 1: can an app pair with, and open a
RemotePairing tunnel to, the device it runs on, with no computer involved?

Spike 5 (sources and refresh) works on the headset. Sources are AltStore/SideStore JSON,
or `github:owner/repo` for apps published only as release assets (UTM's visionOS build).
Installed apps keep their IPA in Documents/apps, so a refresh re-signs and reinstalls
without the network, and their data survives. With "Remember password" on, the password
stays in this device's Keychain, and a refresh can sign in with nobody present (Apple
skips 2FA for a device it already trusts). Background tasks ask to run this about every
12 hours: a short app-refresh task, and a processing task for refreshes that outlast its
~30 s (uploading UTM takes 40 to 150 s). They need the companion or LocalDevVPN, and their
firing is not verified yet (`scheduleBackgroundRefresh` is the test hook).

`Companion/` is Apparition Companion, an optional macOS menu-bar app. It replaces
LocalDevVPN: the headset resets connections it makes to its own RemotePairing service,
so Apparition asks the companion (TCP 49160) to connect back to it. The headset then sees
the connection coming from the Mac. The companion only ever connects back to the address
that asked, and it carries only ciphertext (pair-verify, then TLS-PSK) with no keys. With
the headset on Tailscale, which LocalDevVPN no longer occupies, a companion at home keeps
signing and JIT working from anywhere. Tested 2026-10-11 with LocalDevVPN off: tunnel,
refresh (109 s over the LAN, 150 s over Tailscale) and JIT launch. On the LAN, Apparition
finds the companion over Bonjour ("Find companion"); over Tailscale, type its address.

`Lab/` is a test app for JIT experiments, launched through the probe's `launchJIT`.
Its first experiment (2026-10-11, visionOS 27.2): bytes the debugger writes into the
app's own signed `__TEXT` keep running after it detaches, including code first run
only after the detach and a site rewritten to branch to a trampoline. Apparition serves
this through an experimental JIT26 command, `x16 = 2` (copy x2 bytes from x1 to x0).

## Shortcuts automation

Background refresh is not reliable, so the dependable way to keep apps signed is a
Shortcuts automation. Apparition offers three actions: **Refresh Apps** (apps that expire
within 3 days, or all with "All Apps"; returns how many it refreshed), **Launch with JIT**
(an installed app, through the companion or LocalDevVPN) and **Open Apparition**. Each
opens Apparition and runs there, because a refresh takes 40 to 150 s. Siri knows them too:
"Refresh apps in Apparition", "Launch UTM with JIT in Apparition".

To refresh every day: Shortcuts, Automation, New Automation, Time of Day, pick a time
you are usually wearing the headset, Daily, Run Immediately, then add Apparition's
Refresh Apps action. It needs "Remember password" on and the companion (or LocalDevVPN)
reachable. Apps refreshed are reported as "Refreshed UTM; expires Oct 18"; when nothing
is due it says when the next app expires; and when nothing could be refreshed the action
fails with the reason (not signed in, companion unreachable), so the automation shows it.

## Your own developer certificate

With a paid developer account, Apparition can sign without an Apple ID (Settings > Signing):
import an Apple Development or Distribution certificate as a .p12, plus development, ad hoc or
enterprise profiles that list the headset. Apps then sign on the headset for as long as their
profile lasts, usually a year, with no sign-in, anisette server or free-account slots. A wildcard
profile keeps each app's own bundle ID, and development profiles keep Launch with JIT working.
The .p12 password is only used to open the file; the decrypted key is kept in Apparition's
container, like the free account's key.

## Releases

Releases are listed on [apparition.illixion.com](https://apparition.illixion.com) (repo
`apparition.illixion.com`, whose README has the steps). `scripts/package-release.sh <version>`
builds the unsigned IPA and the installers; the site signs their SHA256SUMS with a key in
`release-signers`. `core/src/release.rs` checks that signature against the keys compiled in,
then every download's hash, so the files can be hosted anywhere. `apparition-installer install`
with no IPA installs the latest release this way.

## Installing Apparition from a computer

`installer/` is Apparition Installer, a command-line program for Windows, Linux and macOS
that puts Apparition (or any IPA) on the headset without Xcode, using the same Rust core:

    apparition-installer pair       # with Settings > General > Remote Devices open; type the PIN
    apparition-installer install --apple-id you@example.com   # macOS; elsewhere add --anisette <url>
    apparition-installer install --p12 development.p12 --profile wildcard.mobileprovision
    apparition-installer install Other.ipa --apple-id you@example.com --anisette http://host:6969

Without an IPA it installs the latest signed release of Apparition (see Releases).

It pairs over the network once (the pairing is kept), signs with your own certificate or an
Apple ID, and installs. Installing Apparition also hands it what signed it, through its Files
inbox: the .p12 and profiles (it asks for the .p12's password once), or the Apple ID's
certificate key as a `.apparitionsigning` file. With the key, Apparition signs with the same
certificate as the computer once signed in to the same Apple ID, so the two use one of a free
account's two certificates instead of one each. An
update of an app that is open on the headset finishes when it's closed. It never revokes a
certificate: an Apple ID with no free certificate slot fails with Apple's error instead.

    cd installer && cargo build --release                                   # this machine
    cargo build --release --target x86_64-pc-windows-gnu                    # Windows, from macOS with mingw-w64

### The desktop app

`desktop/` is the same installer as a guided app (Tauri v2) for macOS, Windows and Linux, so
nobody needs a terminal: it finds the headset over mDNS (or takes its address) and pairs with
the PIN it shows, or pairs an iPhone or iPad over its cable (like `pair --usb`), signs in to an Apple ID (asking for the 2FA code) or opens a .p12 and
profiles, installs the latest signed release (or an IPA you pick) with progress, and hands
Apparition its signing at the end. When a free account's certificate or App ID slots are full
it lists them and frees one only after you type `revoke` or `delete`. It drives the same
`apparition_core::api` as the command line and shares its pairings and Apple ID data, so a
headset paired with one is paired with both. Passwords go from the app's own fields to the
core and nowhere else. On launch it checks the release site for a newer installer and links to
it; it never replaces itself.

    cargo install tauri-cli --version '^2' --locked                         # once
    cd desktop && cargo tauri build --bundles app                           # this Mac
    cargo tauri build --target universal-apple-darwin --bundles app         # arm64 + x86_64
    cargo build --release --features custom-protocol --target x86_64-pc-windows-gnu   # Windows exe

The page is plain HTML, CSS and JavaScript in `desktop/ui/` (no npm), under a strict CSP. A
debug build opens on any step with `APPARITION_DESKTOP_PREVIEW=signing` (or `pair`, `pin`,
`usbpair`, `twofactor`, `install`, `progress`, `slots`, `done`), and can read a scratch release site with
`APPARITION_RELEASE_SITE` and `APPARITION_RELEASE_SIGNERS`; release builds ignore all three.
`scripts/package-release.sh` puts the app in the macOS and Windows installer zips next to the
command line. Linux needs WebKitGTK in the build image and an AppImage or .deb, not built yet.

## Anisette

Signing in to Apple needs "anisette" data that identifies the computer. On macOS the installer
makes its own: it fetches two libraries from Apple's CDN the first time (the ones Apple Music for
Android uses; nothing of Apple's is stored in this repository), loads them in its own process
(`core/src/anisette/`) and talks to Apple through them. Nothing to run or install. Elsewhere, and
on a Mac that prefers it, use a server: Apparition ships none, so enter the URL of
any anisette-v3 server, or self-host one with `docker compose -f selfhost/anisette/compose.yaml
up -d` (Dadoum's anisette-v3-server, pulled from its own registry and pinned by
digest), and enter `http://<host>:6969`. The server sees this device's anisette identity
and IP address, never the Apple ID or password, which go straight to Apple.

The companion can run the same compose file with your Docker (Docker Desktop, OrbStack
or Colima): Start in its window or menu. It then advertises the server, and Apparition
offers "Use its anisette server" next to the companion; nothing switches on its own.

## Build

    scripts/build-core.sh      # builds the Rust core (isideload + idevice) for visionOS and iOS
    scripts/build-core.sh visionos-sim   # adds the visionOS simulator (rustup target add aarch64-apple-visionos-sim)
    xcodegen                   # generates Apparition.xcodeproj from project.yml
    build-and-sign             # or open Apparition.xcodeproj and Run
    SCHEME_NAME=ApparitionLab TARGET_NAME=ApparitionLab BUILD_BUNDLE_ID=com.illixion.apparition.lab \
        build-and-sign --no-mcp    # the lab app
    xcodebuild -scheme ApparitionCompanion -allowProvisioningUpdates build   # the Mac companion

Run the companion from /Applications (or anywhere other users can read). mDNSResponder
reads its Info.plist to allow the Bonjour service, and can't inside a private home or
temp directory: the relay then still works, but Apparition won't find it on its own.

Licensed MIT.
