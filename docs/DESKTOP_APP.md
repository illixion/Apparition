# The desktop app

The first time it opens, the app asks what you want to do. **Install apps** shows the install
steps and Settings. **Develop apps** adds a Develop tab (a checklist of what is ready, your
projects, builds and reports) and turns Install into Devices, where you pair a device and set up
signing. Both modes share the same pairing and signing, and the buttons in the header's corner
switch between them at any time; the choice is remembered. To add a project, choose its folder: the
Apparition Store reads its schemes and bundle IDs and writes a small `build-signing.conf` with no
signing material, kept out of Git. Settings holds the companion (a dot on the tab shows whether it
is running), anisette, opening at login and updates.

`desktop/` is Apparition for macOS, Windows and Linux (Tauri v2): the same installer as a
guided app, so nobody needs a terminal, and the companion, kept running from the menu bar or
tray after the window closes (its Settings tab: the relay and its connections, the anisette
server offered to devices, opening at login). As an installer, it finds the headset over mDNS (or takes its address) and pairs with
the PIN it shows, or pairs an iPhone or iPad over its cable (like `pair --usb`), signs in to an Apple ID (asking for the 2FA code) or opens a .p12 and
profiles, installs the latest signed release (or an IPA or .deb you pick) with progress, and hands
Apparition its signing at the end. When a free account's certificate or App ID slots are full
it lists them and frees one only after you type `revoke` or `delete`. It drives the same
`apparition_core::api` as the command line and shares its pairings and Apple ID data, so a
headset paired with one is paired with both. Passwords go from the app's own fields to the
core and nowhere else.

Desktop update checks are **off by default**, including for existing installations.
Choose automatic checks on step 6, **Done**, or with the **Companion > Updates** toggle.
When enabled, the app contacts apps.illixion.com (which only redirects, to wherever the release is
hosted: GitHub Releases, or a self-hosted copy while GitHub is down; GitHub directly if that domain
is down) on launch and every six hours of
wall-clock time (a laptop that slept past a check catches up on wake), including while
running from the tray, for releases hosted on GitHub. A failed check retries after 15 minutes. When off, **Get started**
offers a one-time check without enabling future checks; declining makes no update request.
The tray/menu-bar menu has an updates section: a greyed-out **No Updates** line, which becomes
**Update to x.y.z...** when one is available, and a **Check Automatically** checkbox mirroring
the setting. A badge on the icon and a banner in every app view also announce an update;
the banner disappears once a check finds nothing newer. **Update & restart** downloads and
installs it only when clicked; the companion briefly stops while restarting. Development
builds are never replaced.

Updates use the existing GitHub Releases assets and release workflow: the pinned release
keys must verify the YubiKey-signed `SHA256SUMS` SSH signature, then the downloaded file must
match that signed hash **before it is extracted or installed**. Version, OS, architecture
and package format are bound to the signed filename; older or equal versions are refused. Where the
files are served from proves nothing, so a mirror or redirect can be added or swapped without a new
release. Finding, verifying and downloading a release is the
[signed-update](https://github.com/illixion/signed-update) crate (vendored in
`core/vendor/signed-update`, refreshed by `scripts/vendor-signed-update.sh`; its
`docs/PROTOCOL.md` has the protocol and the redirects); the status, the schedule and the install
steps below stay here.

The check reads the newest release's manifest (GitHub's `releases/latest`), so an app-only release
(IPAs and no installers) offers the desktop app nothing until the next full release: a copy older
than the last full release is not shown that release in between.

On macOS the whole ad-hoc-signed `.app` (including its CLI and entitled anisette helper) is
staged beside the installed app, its code-signing seal is checked, and the bundles are
atomically exchanged. The Applications folder must be writable by your account; use
`~/Applications` if necessary. Updating does not re-sign the helper or require a new
quarantine exception. On Windows the portable app, `apparition-cli.exe` and
`WebView2Loader.dll` update together: a Windows PowerShell helper waits for the app to
exit, replaces the files, restarts it, and rolls back if installation or launch fails.
The helper is passed to PowerShell as a command, not a script file, so a Group Policy
execution policy doesn't block it. PowerShell locked to Constrained Language mode
(AppLocker or WDAC) does: the banner then says the computer's policy blocks the updater
and links to the Releases page for a manual download.
A copy from the zip has to stay in a user-writable folder, and so does one set up for one person
(`%LOCALAPPDATA%\Programs\Apparition`), which updates with no question asked. A copy set up for
everyone sits in `%ProgramFiles%\Apparition`, where only an administrator may write, and its
background service runs `apparition-cli.exe`: the helper then runs as an administrator, which
Windows asks you to approve at each update, stops the service for the swap and starts it again,
and the app is started again as you, not as the administrator who approved. Either way the helper
brings the version in Windows' list of installed apps up to date. On Linux an AppImage replaces itself atomically and keeps
its executable permissions. A Debian installation instead asks for system authorization
through PolicyKit and installs the verified `.deb` with `dpkg`; it needs `pkexec` and a
desktop authorization agent. If `dpkg` reports missing dependencies, repair them with
your package manager before retrying. Settings, pairings and signing credentials
stay outside these application packages and are not replaced.

Before signing, the desktop installer and native iPhone/iPad, Vision Pro and Apple TV apps
list the IPA's embedded extensions and let you choose which to include. Apple ID installs show the number of new App IDs needed and let
you reuse an existing registration by installing under its bundle identifier. Assignments
and extension choices are saved per team, device and source app in
`app-id-assignments.json` beside the desktop's pairing database, including reservations
made before an install fails. The native apps store the same choices in `library.json`,
with the team and source identifier; Apple TV mirrors that state to its Keychain vault.
An occupied identifier replaces the app already using it, and the previous library entry
stops refreshing over its replacement. Removing a native Library entry deletes the copy and releases its ID reservations; forgetting
one whose device deletion failed keeps them while that copy is installed; a fresh device listing releases reservations
once a forgotten copy is gone. Failed replacements retain old extension reservations until
installation succeeds. These databases are local to each installation;
assignments are not synchronized between computers and devices.

Assignments are per device, so one registered App ID can serve different apps on different
devices (and platforms): the same bundle identifier can be the target of one app on a Vision Pro
and of another on an Apple TV or iPhone, and one app can get a different identifier on each
device. Apparition only looks for conflicts within one team and device. An identifier that is
occupied replaces the app using it on the device being installed to, never on another device.
Apple registers one App ID per team, not per platform, so these share its capabilities (App
Groups, for one) and its free-account limits. Because each computer keeps its own assignments,
and none of them can see what other devices or installers use, Apparition cannot warn that an
identifier is already in use somewhere else; reusing one across devices is your call.

The review asks whether to keep existing app data or uninstall the destination first.
Uninstalling discards its app container before the new install, so a failed installation
leaves the old app removed. When an app moves from a known previous identifier, Keep
copies Documents and Library using the same migration code as changing its signer on the
device. The source is removed only after every file arrives. Incomplete or queued transfers
keep the source and can be retried. Keychain items and shared app group containers cannot
be migrated or cleared by this choice. Registrations may also be used by other devices or
installers; the local assignment database cannot determine that account-wide ownership.
ExtensionKit helpers are currently unavailable with Apple ID signing; imported profiles
can still sign selected helpers when they cover them. Ordinary app extensions each need
a compatible registration; the installer reuses matching child identifiers where present.
Certificate installs preview the actual identifier chosen by the provisioning profile and
disable extensions whose profiles are missing. Both frontends check the App ID allowance
before signing. Native Library > More > **Review Install Options** reopens the picker for
a kept app, including file imports. Foreground updates review the new archive; unattended
refreshes retain the saved identity and extension selection and never add new extensions.
Changing a native app's signer requires reviewing its install options before refreshing.
Existing library records retain their previous refresh behavior until reviewed. Cancelling
a review keeps the previous archive available for refresh.

After Apple ID sign-in, **Manage App IDs** is available on the signing and install
screens. It shows Apple's remaining allowance, registered bundle identifiers and expiry
dates, with search, refresh and deletion after confirmation. Deleting an ID may not
restore a free account's weekly allowance before it expires.

## Building it

    scripts/install-tauri-cli.sh      # once (or cargo install tauri-cli --version '^2' --locked)
    cd desktop && cargo tauri build --bundles app                           # this Mac
    scripts/install-desktop.sh        # build, sign, install to /Applications, link `apparition` onto PATH and relaunch (this Mac; LINK=0 skips the link)
    cargo tauri build --target universal-apple-darwin --bundles app         # arm64 + x86_64
    cargo build --release --features custom-protocol --target x86_64-pc-windows-gnu   # Windows exe
    docker build -t apparition-desktop-linux -f ../scripts/desktop-linux.Dockerfile ../scripts
    docker run --rm -v "$PWD/..:/src" -w /src/desktop -v apparition-desktop-target:/target \
        -e CARGO_TARGET_DIR=/target apparition-desktop-linux sh -c 'mkdir -p /src/build &&
        cargo tauri build --bundles deb,appimage && cd /target/release/bundle &&
        cp appimage/*.AppImage deb/*.deb /src/build/'                       # Linux, into build/

The page is plain HTML, CSS and JavaScript in `desktop/ui/` (no npm), under a strict CSP. A
debug build opens on any step with `APPARITION_DESKTOP_PREVIEW=signing` (or `pair`, `pin`,
`usbpair`, `twofactor`, `install`, `progress`, `slots`, `done`, `companion`), can read a scratch release site with
`APPARITION_RELEASE_SITE` and `APPARITION_RELEASE_SIGNERS`, and can move the companion's ports with
`APPARITION_COMPANION_PORTS=<relay>,<anisette>`; release builds ignore all four.
`scripts/package-release.sh` puts the app in the macOS and Windows installer zips next to the
command line, wraps the Windows files in a setup program as well (NSIS, `scripts/windows/setup.nsi`;
its first lines say what it does and why it starts unelevated), and with `--linux` builds a .deb and an AppImage per architecture (Ubuntu 22.04,
so glibc 2.35 or newer). The Linux release includes direct .deb and AppImage downloads as well
as an archive with both and the headless command line. Opening the app again shows the running
one, which is also the way back to its window on a Linux desktop with no tray (niri or plain
GNOME, say).
