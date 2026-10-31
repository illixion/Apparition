# Building your own apps with Apparition

Apparition can install other people's apps. With a Mac, it can also build yours: start a build of
your Xcode project from the headset, see it install, and read its crash logs on the same screen.
This guide explains how the pieces fit, how to set them up, and what each part of the Developer
screen does.

## The pieces

- **[appstore](https://github.com/illixion/appstore)** is an open-source build service that runs
  on your Mac. It builds your projects from their Git checkouts with its `build-and-sign` script,
  signs them with your developer certificate, and installs them on devices paired with the Mac
  (`xcrun devicectl`). It keeps every signed build with a manifest of exactly what went into it,
  pulls crash logs off your devices and symbolicates them, and receives debug traces from apps
  that link [DebugTrace](https://github.com/illixion/DebugTrace). It has its own web page too, but
  Apparition talks to its versioned service API.
- **Apparition on the Mac** (the desktop app) has a **Developer** tab that connects to appstore on
  the same Mac or another one. It can also register projects and send your signing certificates to
  a device.
- **Apparition on a device** (Apple Vision Pro, iPhone, iPad, Apple TV) has the same **Developer**
  tab, and its **Library** shows the apps your projects installed there, with their updates.

Apparition never builds anything itself. The Mac builds; Apparition asks it to and shows the
result. The device's own approvals still apply: connecting to the service doesn't grant debug
access or code changes.

## What you need

- A Mac with Xcode, Node 18 or newer and Homebrew's bash (`brew install bash`).
- A paid Apple Developer account, a development certificate exported as `.p12`, and a development
  provisioning profile listing your devices. A wildcard profile (App ID `*`) signs every app.
- Your devices paired with the Mac in Xcode (Window > Devices and Simulators).
- For a headset, iPhone or Apple TV away from the Mac: a way to reach the Mac over HTTPS. Tailscale
  Serve is the easy one (see "Away from home, over Tailscale" in the README).

## Setting up

1. **Install appstore.** Clone it and follow its README:

   ```sh
   git clone https://github.com/illixion/appstore
   cd appstore
   mkdir -p ~/.config/build-signing/{certs,profiles}
   cp examples/global.conf.example ~/.config/build-signing/global.conf   # then edit
   cp config.example.json config.json                                    # then edit
   ```

   Put your `.p12` in `~/.config/build-signing/certs` and your profiles in `profiles`. Each app's
   repository gets a gitignored `build-signing.conf` (from `examples/build-signing.conf.example`)
   naming its scheme and bundle ID.

2. **Set a service password.** Apparition needs one even if the web page runs without a login:

   ```sh
   node server.js set-password
   ```

3. **Start it.** `node server.js` serves `http://127.0.0.1:8484`. To keep it running, wrap it in a
   launch agent with `KeepAlive`.

4. **Connect.** In Apparition's Developer tab, enter the address and the password. On the Mac
   itself, `http://localhost:8484` works. A device needs the HTTPS address, for example
   `https://your-mac.your-tailnet.ts.net`. Apparition refuses plain HTTP to another host. The
   device can remember the password in its Keychain; the desktop app forgets the session when it
   quits.

5. **Add your projects.** On the Mac, Developer > **Register a project** takes the checkout's path,
   its branches, and one target per device it installs on: a platform alias (`visionOS`, `iOS`,
   `iPad`, `tvOS`), the device's name as Xcode shows it, and the bundle ID. You can also add them to
   appstore's `config.json` by hand.

## The Developer screen

Before you connect, it shows how to get started. Once connected it has four sections.

**Projects.** One card or row per project, showing its branch and commit and whether the checkout
has uncommitted changes. Each project lists its **targets**, one per device it installs on, with
what the Mac installed there last, whether that is still the checkout's commit, and whether the
device is reachable now. Each target can:

- **Install on _device_**: build and install it there. Under **Build options** you can choose the
  branch, launch it after installing, allow debug access (the device still asks), enable live
  patching, or build the Debug configuration.
- **Build only**: build and sign it without installing it. The Mac keeps the IPA, so you can
  download it, install it over the air, install it on this device through Apparition's own signing
  (on a device: **Install … on this headset**), or re-sign it later.
- **Re-sign…**: see below.

A project also lists its **kept builds** (every signed build the Mac made, exactly as signed) and
how many crash logs and traces its apps have. On a device, open a project to see all of this; the
Library's **Open Project** goes straight there.

**Activity.** The build queue: what is running or queued first, then recent builds, each with its
log and, while it runs, a Cancel. Builds of one project run one at a time.

**Reports.** Crash logs and DebugTrace traces, grouped by app. Crash logs open symbolicated
against the symbols the Mac kept for that exact build. On the Mac, this section also lists running
dev builds with a debug server; inspecting one asks for approval on the device.

**Devices & signing** (on the Mac) or **Service** (on a device). The devices the Mac can install to
(simulators are left out) and the connection. On the Mac, **Signing on devices** sends
build-and-sign's certificates and profiles into Apparition's Signing folder on a paired device, so
Apparition there can sign with your developer certificate instead of an Apple ID. Nothing is
reinstalled; enter each `.p12`'s password on the device once.

## Re-signing an app with another profile

**Re-sign…** signs an app that is already on a device with another of the Mac's provisioning
profiles (any `.mobileprovision` in `~/.config/build-signing/profiles`) and installs it over the
copy on the device. Use it to move an app onto a profile with a capability it now needs, onto
another team, or off a profile that is about to expire.

A device can't hand an installed app back, so the Mac needs the IPA:

- If it **kept the build that is installed**, it re-signs exactly that build.
- If it didn't (the app was installed before builds were kept, or the build was pruned), it first
  **builds the project target** from its current checkout without installing it, the same build
  Build only makes, and re-signs that. Activity shows the two jobs, the build first.

Then:

- A profile for the **same App ID** updates the app in place and keeps its data.
- A profile for a **different App ID** can't be installed over the old copy: the device refuses
  that update. The Mac copies the app's Documents and Library off the device into
  `~/.local/state/apparition/resign-backups/`, removes the app, installs the new signature and
  copies the data back, checking the file count. If the new signature won't install, it puts the
  previous one back with its data. Keychain items and app-group data don't move, and the backup is
  never deleted for you.
- **Check first** only tests that the app's data can be reached. It builds and changes nothing.
- Choosing to discard data skips the backup.

Re-signing needs the device reachable from the Mac. An over-the-air install can't change an
installed app's App ID.

## When something doesn't work

- **"Connect to the build service again."** The password changed or the session was revoked.
  Connect again.
- **A device is missing from a target.** It isn't paired with the Mac, or Xcode names it
  differently. The target's device name must match the name Xcode shows.
- **A build fails.** Read its log in Activity. The same build from a terminal is
  `build-and-sign` in the project's checkout, which prints the same output.
- **An app crashed.** Its crash log usually reaches Reports within a few minutes; the Mac pulls them
  from each device while it is reachable. If nothing arrives, appstore's own Traces page says why a
  device was skipped.

## Security

The service password protects everything, so use a long one and keep the service on localhost or
your tailnet. Apparition keeps the service session in memory (and, on a device, the password in its
Keychain if you allow it), sends only identifiers to the service and never file paths, and refuses
redirects. The service decides what is allowed: configured projects, targets, branches and
profiles only.

### Live patches from Developer or the CLI

Build options in the native and desktop Developer views offer **Patch backend** when
live patching is enabled. Swift dynamic replacement is the default. The experimental
**Apparition entry boundaries** backend leaves the rest of the app optimized and uses
explicit synchronous Swift boundaries; it does not support arbitrary optimized
functions or mutation of existing type layouts. Setup and restrictions are in
[DebugTrace's live-patching guide](../../DebugTrace/Docs/live-patching.md#apparition-native-entry-backend-experimental).

After launching the patchable build, edit its sources on the build Mac and choose
**Patch running app** for that project/device. The authenticated service asks its local
Apparition CLI to attach, compile and sign, request device approval, apply and detach.
The button applies changed files from the captured build, including shared packages;
use the CLI's `--file` option to select files when other edits are present. Patching
never builds, installs or relaunches an app. Build-only also never attaches a debugger.
The service advertises `entry-patching` only in versions with this route; older clients
and services retain the existing build controls.

On the Mac with Xcode, the captured build products and current DebugTrace tools:

```sh
build-and-sign --run --mcp --inject-backend apparition
apparition inject com.example.app --device AVP --file Sources/Update.swift
apparition inject com.example.app --device AVP --run 'return model.count'
```

`DEBUGTRACE_MCP` overrides the CLI's DebugTrace tool path. The Mac service uses its
`debugtraceTool` setting and `apparitionCli` (default `apparition` on PATH). Install a
CLI built from this version before using the button. Service API:
`POST /api/service/v1/inject` with `{repo, device, targetPlatform, files?: string[]}`.
It accepts only configured targets, requires a native service session and permits
one operation per app/device at a time. It returns the ordinary MCP injection result
or an error with a hint. App approval and signed-code checks apply to both backends.

The isolated `ApparitionLab` target exposes `entryProbe`, whose value starts at 11 and
whose counter survives live patches. Its `EntryWorkflowLab.swift` boundary can be
patched through this complete workflow without deploying the main Apparition app.

For the reproducible host performance comparison and tested struct receiver
conventions, see [native entry performance and validation](ENTRY_PERFORMANCE.md).

A private optimized implementation can now be reloaded as one source unit: a
`private enum DebugTraceImplementation…` plus explicit native entry functions in a
dedicated file. A helper edit regenerates every entry in the file, even if its body
is unchanged. Helpers can inline into each other and change private signatures;
entry signatures stay fixed. Both old and new code must pass syntax and raw-SIL
ownership checks. The group is restricted to state-free scalar/String functions;
it cannot access models or retain callbacks. See DebugTrace's live-patching guide,
“Recompiling a private optimized implementation group,” for the full contract.

The lab's `EntryWorkflowImplementation.swift` exercises this path. A leaf-only edit
changed both `entryProbe` group results from `31/131` to `71/171` on Vision Pro,
without editing either entry body or rebuilding the app.

A subsequent private helper signature change produced `73/173` on the same live
Vision Pro process. Restoring the original group returned `31/131`; the lab's
receiver and read counters survived all three two-entry transactions.

The lab also includes an explicit state-generation experiment. Its original
`EntryWorkflowState.swift` provides a main-actor `Any` slot, a V1 value snapshot and
a synchronous scalar entry. The excluded `Lab/Patches/StateV2.swift` and
`StateV3.swift` add separately named types and migrate value data through that slot;
they never change a loaded type's layout. To reproduce on a fresh lab launch:

```sh
SCHEME_NAME=ApparitionLab TARGET_NAME=ApparitionLab \
  BUILD_BUNDLE_ID=com.illixion.apparition.lab \
  build-and-sign --platform visionOS --device AVP --inject \
  --inject-backend apparition --run --mcp
python3 scripts/test-entry-state.py --device AVP
```

The script checks two successive migrations, retained added state, rejection
before publishing a candidate and old-code refusal after migration. It restores
the edited source even on failure. The running app finishes with V3 data and its
original boundary returning `-1` without changing that data; relaunch before
repeating. Migration is an application operation on first use after code has
loaded, separate from the native entry transaction. Read DebugTrace's
`Docs/live-patching.md`, “Explicit state generations,” for ownership, concurrency
and rollback limits before applying the pattern to app state.
