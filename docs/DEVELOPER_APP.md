# The Developer tab

The **Developer** tab, on the Mac and on devices, builds your own Xcode projects through the Apparition Store,
the build service built into the desktop app (`apparition build` is its builder). [DEVELOPER.md](DEVELOPER.md)
explains how the pieces fit and how to set them up, and [STORE.md](STORE.md) is the Store's reference; this
page is the summary of what the app does.

**From Windows, Linux or another Mac**, pair that computer with your Mac for developer access
(`apparition mac pair <address>`): its agents get the Mac's MCP server with `apparition mcp`, and
builds made there (xtool, Xcode) go to your devices with `apparition deploy <app.ipa>`, over the
companion's encrypted channel at home or over Tailscale. See
**[docs/REMOTE_DEVELOPER.md](REMOTE_DEVELOPER.md)**.

**On a Mac, Apparition runs the Store itself.** The Store is Rust, inside the app
(`apparition-service`, hosted by `desktop/src/host.rs`): it starts at launch in the app's own
process, with nothing to install and nothing left running after Quit. It listens on `127.0.0.1`
on the `port` in its `config.json` (8484 by default), and its config and data are in
`~/Library/Application Support/Apparition/Store/`. The app talks to it with a token it makes at
launch and keeps in memory. Its web pages are off until you turn them on with a password
(**Devices & signing › Web UI and other devices**); that password also lets Apparition on another
device connect. **Connect to another Mac** takes the HTTPS address and password of that Mac's
Store. The Store speaks the `apparition-service` version 1 API. Builds run `apparition build`,
which builds with Xcode and installs and launches over Apparition's own pairing with the device,
falling back to Xcode's tunnel (`devicectl`) only when the device can't be reached that way. Without the app, `apparition store serve` runs the same Store, and
`apparition store` does everything the Developer tab does from a terminal, on this computer's Store, a paired Mac's,
or, after `apparition store login <address>`, another computer's by its password ([docs/STORE.md](STORE.md)).

**Debug** is the desktop app's second tab. Each running dev build (this Mac's Store) has **Tune**, **Log**,
**Profile** and **Memory** buttons that open the Store's debug view on that tab in its own window, titled
with the app's name: sliders for `DebugTunables` values and panels an agent built, the live log, a profiler and
a memory view. A one-time link signs it in, web UI on or off;
the window has no access to the app.

The develop tabs are **Apps** (projects and the devices they install to), **Debug** (running debug apps, then crash
logs and traces grouped by app), **Builds** (the build queue and logs), Devices and Settings (the build service's
Web UI, signing hand-off and About sit under Build service and signing). Each project lists its targets, one per device it installs
on, with what the Mac installed there and whether the device is reachable; each target has
**Install on _device_**, **Build only** and **Re-sign…**. Launch, debug access, live patching
and Debug configuration are the project's **Build options**. The device list leaves out
simulators. New builds offer **Build details** and **Download**:
the saved artifact is checked against its manifest's SHA-256 and size before replacing
your chosen destination file. Each project lists its **kept builds**; **Other kept builds**
shows artifacts no project claims, such as Xcode products registered by hand. Manifests record bundle identities, binary UUIDs, symbols,
toolchain and before/after source/dependency fingerprints. Device debug access still asks
for approval on the device; connecting to the service does not grant it.

**Re-sign…** (on a target or a kept build) signs an app with another of the Mac's provisioning
profiles and installs it over the copy on a device. The device can't give an installed app back,
so the Mac signs the build it kept for that install, or, when it kept none, first builds the
target without installing it (the same build **Build only** makes) and signs that. The Mac
(the Store, with `apparition build --ipa --profile`) does the work; the view sends a build ID or a
project and platform, a device name, a profile ID, keep or discard data, and an optional dry run. A profile for the same App ID
updates in place and keeps the app's data. The device refuses an update to a different App ID, so
then the Mac copies Documents and Library to `~/.local/state/apparition/resign-backups`, removes the
app, installs the new signature and copies them back, checking the file count. If the new signature
won't install, it puts the previous one back with its data. Keychain items and app-group data
don't move. The backup is never deleted for you. **Check first** only tests that the data can be
reached.

Only the service address is remembered on disk. The service password field clears when
connecting; its session credential stays in Rust memory, outside the page. Disconnect
revokes that session. After quitting Apparition, connect again. This connection is separate
from the companion relay and does not share its signing credentials.

## Registering projects

In desktop Apparition's Developer view, use **Register a project** to add an existing
checkout on the Mac that runs the Store. Enter its Mac path, device names, platform aliases and
bundle identifiers; its `build-signing.conf` must already be set up. Registration saves
the project without building it or changing signing material. Local branch names are
optional; the current checkout remains available. The Store validates the checkout
and updates its configuration atomically, preserving existing settings.

When the Store is connected, the Library's **Your Projects** group shows the
installed debuggable apps a configured project builds, matched by bundle ID. A card offers the
newest kept build as an update (installed through Apparition's own signing and review), JIT
launch, and **Open Project**, which opens the project's page on the Developer tab with its
builds, re-signing and reports. Apps with no configured project match remain in Other Apps. The
Store connection is shared with Developer.
