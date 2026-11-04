# Building your own apps with Apparition

Apparition can install other people's apps. With a Mac, it can also build and deploy your Xcode
projects, launch them with JIT, apply live Swift patches, and keep build artifacts available for
refresh and diagnosis. Start a build from the headset and follow it in Activity.
This guide explains how the pieces fit, how to set them up, and what each part of the Developer
screen does.

## The pieces

- **The Apparition Store** is the build service built into the desktop app, so it runs on your Mac
  while Apparition is open ([reference](STORE.md)). It builds your projects from their Git checkouts
  with `apparition build`, the same command you can run in a terminal. For a device-side install it
  keeps an unsigned IPA for Apparition to sign on the device; a direct Mac install is signed with
  your development certificate and installed over Apparition's own pairing with the device. It
  keeps build artifacts with a manifest of exactly what went into them and receives debug traces
  from apps that link [DebugTrace](https://github.com/illixion/DebugTrace). It has its own web page
  too, but Apparition talks to its versioned service API.
- **Apparition on the Mac** (the desktop app) has a **Develop** tab (choose **Develop apps** the first
  time it opens, or switch with the buttons in the header) that connects to the Apparition Store on
  the same Mac or another one. It can also register projects and send your signing certificates to
  a device.
- **Apparition on a device** (Apple Vision Pro, iPhone, iPad, Apple TV) has the same **Developer**
  tab, and its **Library** shows the apps your projects installed there, with their updates.

Builds happen on a Mac with Xcode, in the Store; Apparition on a device asks for them and shows the
result. The device's own approvals still apply: connecting to the Store doesn't grant debug access
or code changes.

## What you need

- A Mac with Xcode. Nothing else to install: the Store is part of Apparition.
- For **Mac-side signed installs**, a paid Apple Developer account, a development certificate
  exported as `.p12`, and a development provisioning profile listing your devices. A wildcard
  profile (App ID `*`) signs every app. For **device-side installs**, the Mac retains an unsigned
  build and Apparition signs it on the device with the Apple ID or certificate configured there;
  no Mac-side signing material is needed for that route.
- Your devices paired with Apparition on the Mac (`apparition pair`, or pairing in the app).
  The Store reads, installs to and launches on a device over that pairing, not Xcode's.
- For a headset, iPhone or Apple TV away from the Mac: a way to reach the Mac over HTTPS. Tailscale
  Serve is the easy one (see "Away from home, over Tailscale" in the README).

## Setting up

1. **Set up the Store.** It comes with the desktop app and runs while Apparition is open
   (Developer shows it running); it needs Xcode on the Mac. Add signing material only if you will
   install directly from the Mac:

   ```sh
   mkdir -p ~/.config/build-signing/{certs,profiles}
   # then write ~/.config/build-signing/global.conf (see STORE.md, "Signing material")
   ```

   Put your `.p12` in `~/.config/build-signing/certs` and your profiles in `profiles`. Each app's
   repository gets a gitignored `build-signing.conf` naming its scheme and bundle ID; **Add a
   project** writes a minimal one for you. [STORE.md](STORE.md) shows both files.

2. **Let other devices in.** On the Mac, Developer › Devices & signing › **Web UI and other
   devices**: set a password. Apparition on a headset, iPhone or Apple TV connects with it (and so
   does a browser, once the web UI is on). The service listens on 127.0.0.1 only, so a device
   reaches it through an HTTPS address such as `tailscale serve`, for example
   `https://your-mac.your-tailnet.ts.net`; Apparition refuses plain HTTP to another host. The
   device can remember the password in its Keychain.

3. **Add your projects.** On the Mac, Develop > **Add a project**: choose the checkout's folder and
   **Look**. Apparition reads its schemes, finds each app's platforms and bundle ID, and you tick the
   apps to add and pick the device each installs on. If the checkout has no `build-signing.conf`, it
   writes a small one (project, scheme, platform, device; no certificates or passwords) and adds it to
   the clone's local Git exclude. An existing conf is never replaced. For anything unusual,
   **Advanced: register a project by hand** takes the path, branches and one target per device (a
   platform alias such as `visionOS`, `iOS`, `iPad` or `tvOS`, the device's name as Apparition lists
   it (`apparition devices`), and the bundle ID), or edit the Store's `config.json`
   (`~/Library/Application Support/Apparition/Store/`, keys in [STORE.md](STORE.md)).

## The Develop screen

Before you connect, it shows how to get started. Once connected it has four sections.

**Projects.** One row per project, showing its branch and commit and whether the checkout has
uncommitted changes. Each project lists its **targets**, one per device it installs on, with what
was installed there last and whether that is still the checkout's commit. Open a project for its
page, which has a header (the target, a status badge, where it stands) and tabs:

- **Overview**: **Build & Install** for the target, by the best route available (see "How a build
  reaches a device"), with the other route as a secondary button. Also what is installed, and a
  newer kept build to install when there is one.
- **Builds**: the build options (branch, launch after installing, debug access, live patching and
  its backend), **Build only** in its own section so it isn't hit by accident, recent builds with
  their logs, and retained build artifacts. A device-side install signs the retained IPA on the
  device; a direct Mac install retains the IPA signed by the Mac.
- **Reports**: this project's crash logs and traces.
- **Signing**: how the copy on this device is signed and when it expires, with a Refresh; and, from
  the Mac, **Re-sign…** (below).

The Library's project cards link into the same page, and to its Reports tab when they show crashes.

**Activity.** The build queue: what is running or queued first, then recent builds, each with its
log and, while it runs, a Cancel. Builds of one project run one at a time.

**Reports.** Crash logs and DebugTrace traces, grouped by app. Crash logs open symbolicated
against the symbols the Mac kept for that exact build. On the Mac, this section also lists running
dev builds with a debug server; inspecting one asks for approval on the device.

**Devices & signing** (on the Mac) or **Service** (on a device). The devices the Mac can install to
(simulators are left out) and the connection. On the Mac, **Signing on devices** sends
the build-signing folder's certificates and profiles into Apparition's Signing folder on a paired device, so
Apparition there can sign with your developer certificate instead of an Apple ID. Nothing is
reinstalled; enter each `.p12`'s password on the device once.

## How a build reaches a device

There are two routes, and Apparition picks the first one that works.

1. **On this device (the default).** The Mac builds the project and keeps an unsigned IPA. Apparition
   downloads it from the build service over HTTPS, signs it with this device's own identity (Apple
   ID or an imported certificate) and installs it through its usual route. The Mac never touches the
   device, so this works away from home over Tailscale, and with a free Apple ID. The app joins the
   Library like any other: it is refreshed before its profile expires, and a newer kept build shows
   as an update (optionally installed automatically). The first install shows the usual review of
   which App IDs it takes; later ones re-sign with that plan and ask nothing. With a free Apple ID the
   app gets the team's suffix on its bundle ID and counts against the App ID limit, like any app
   Apparition signs.
2. **From the Mac over its own pairing (secondary).** `apparition build` installs it using the
   Mac's own certificate and profile, through the Mac's pairing with the device (not `devicectl` or
   Xcode's tunnel). It is the quick way to update a build, the only route that can launch the app
   from the build, and it keeps the Mac's profile, but the Mac has to reach the device on the
   network, and the device's Apparition isn't involved. It is also what happens when Apparition is
   closed: the Store lets a device install its own build only while that device's Apparition is open
   (Apparition names its device on every request and goes quiet when closed), and installs from the
   Mac otherwise, for builds started from the web page or elsewhere too.

A target on another device, or a device Apparition can't sign for yet, is installed from the Mac, and
the page says what would make the first route work. A failed Mac install keeps its build, so the
page offers to install that build here instead. After an install here, Apparition tells the service,
so the install history on the Mac and in other clients is right.

## Keeping every app up to date

The Store's **Updates** view (and `apparition updates` from a terminal, here or on a computer
paired with the Mac) lists the apps on your devices that are behind, and why: their own branch, or
a local package they build against. Kept builds record every package's commit, so a commit to
vergence makes Convolution behind, while a commit that only touches tests or docs doesn't.

**Update all** builds each one once, from committed sources in a build clone (nobody's uncommitted
edits ship), and installs it on every device that runs it: the iPhone and iPad builds of an app
are one build. At most two build at once (`maxConcurrentBuilds` in `config.json`), and a clone
is kept between builds, so the next one is incremental. A device the Mac can't reach keeps the
build for its Apparition to install. Build numbers carry a short tag for the packages
(`38ad878-06d7aa`), so a package update shows as an update in Apparition and AltStore too.

**Update automatically** does the same by itself once the sources have been still for a few
minutes (`autoUpdate: {"enabled": true, "settleMinutes": 5}`), so a burst of commits across a
package and an app is one build. Devices in `ledgerOnlyDevices` aren't asked on their own what they
have; what the Store installed there stands for them.

## Linking apps to projects

An app on the device belongs to a project when its bundle identifier matches a target's, exactly or
with the team suffix a free Apple ID adds, or a `.debug` suffix. Anything else, an Xcode build under
another identifier say, shows under **Other Apps** with **Link to Project…**: choose the target and
the app is shown with that project's builds, updates and reports. A link survives refreshes, can be
changed or removed from the card's menu, and an app you unlink isn't matched again. Without a build
service at all, apps installed by Xcode still list there (debuggable ones) with JIT launch.

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

Re-signing replaces the app in place over the Mac's pairing with the device and moves its data, so
it needs the device reachable from the Mac; the page says so when it isn't. To change which identity signs the
copy on this device, change the app's signer in the Library instead.

## When something doesn't work

- **"Connect to the build service again."** The password changed or the session was revoked.
  Connect again.
- **A device is missing from a target.** It isn't paired with Apparition on the Mac, or Apparition
  names it differently. The target's device name must match the name `apparition devices` shows.
- **A build fails.** Read its log in Activity. The same build from a terminal is
  `apparition build` in the project's checkout, which prints the same output.
- **An app crashed.** Reports can show older crash logs already present in the Store and traces
  uploaded by apps that link DebugTrace. Automatic device crash-log pulling is currently disabled,
  so a new crash may not appear unless the app uploads a trace.

## Security

The Store's password protects everything, so use a long one and keep the Store on localhost or
your tailnet. Apparition keeps the service session in memory (and, on a device, the password in its
Keychain if you allow it), sends only identifiers to the service and never file paths, and refuses
redirects. The service decides what is allowed: configured projects, targets, branches and
profiles only.

### Live patches

Live patching is a **build setting** (`--inject`): the Developer views only toggle it for a build
(**Live patching** in the project's Builds tab), and building with it works with either install
route. Patching itself happens from the Mac, through the `inject` MCP tool or the CLI, so the
mobile app has no Live tab. The Mac compiles the edited sources and sends them to the app's debug
server. It needs a patchable build that is running and an app the Mac can find: discovery uses
Bonjour on the local network, or the session record `apparition build --run --mcp` writes when it
launches the app. An app launched some other way leaves neither, so patching one away from the
network the Mac is on is not yet verified.

Build options in the native and desktop Developer views offer **Patch backend** when
live patching is enabled. Swift dynamic replacement is the default. The experimental
**Apparition entry boundaries** backend leaves the rest of the app optimized and uses
explicit synchronous Swift boundaries; it does not support arbitrary optimized
functions or mutation of existing type layouts. Setup and restrictions are in
[DebugTrace's live-patching guide](../../DebugTrace/Docs/live-patching.md#apparition-native-entry-backend-experimental).

After launching the patchable build, edit its sources on the build Mac and patch that
project/device. The Store's `inject` route asks its local
Apparition CLI to attach, compile and sign, request device approval, apply and detach.
It applies changed files from the captured build, including shared packages;
use the CLI's `--file` option to select files when other edits are present. Patching
never builds, installs or relaunches an app. Build-only also never attaches a debugger.
The service advertises `entry-patching` only in versions with this route; the Patch backend
choice appears only for those.

On the Mac with Xcode, the captured build products and current DebugTrace tools:

```sh
apparition build --run --mcp --inject-backend apparition
apparition inject com.example.app --device AVP --file Sources/Update.swift
apparition inject com.example.app --device AVP --run 'return model.count'
```

`DEBUGTRACE_MCP` overrides the CLI's DebugTrace tool path. The Store uses its
`debugtraceTool` setting and `apparitionCli` (default the CLI beside the app). Install a
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
`EntryWorkflowState.swift` provides a main-actor `DebugPatchStateSlot`, a V1 value snapshot and
a synchronous scalar entry. The excluded `Lab/Patches/StateV2.swift` and
`StateV3.swift` add separately named types and migrate value data through that slot;
they never change a loaded type's layout. To reproduce on a fresh lab launch:

```sh
SCHEME_NAME=ApparitionLab TARGET_NAME=ApparitionLab \
  BUILD_BUNDLE_ID=com.illixion.apparition.lab \
  apparition build --platform visionOS --device AVP --inject \
  --inject-backend apparition --run --mcp
python3 scripts/test-entry-state.py --device AVP
```

The script checks two successive migrations, retained added state, validation
failure after mutating an array candidate, unchanged revisions on failure and
old-code refusal after migration. The slot owns publication and checks generation
names and exact payload types. It restores
the edited source even on failure. The running app finishes with V3 data and its
original boundary returning `-1` without changing that data; relaunch before
repeating. Migration is an application operation on first use after code has
loaded, separate from the native entry transaction. Read DebugTrace's
`Docs/live-patching.md`, “Explicit state generations,” for ownership, concurrency
and rollback limits before applying the pattern to app state.

For cross-file optimized callers, the lab's
`Lab/DebugTraceImplementationDomain/` contains separate leaf, caller and entry
sources. Launch the lab with the same build command above, then run:

```sh
python3 scripts/test-entry-domain.py --device AVP
```

The script changes only the leaf at first and verifies that both entry results
change through the untouched caller file. It then checks refusal of an outside
caller and a snippet that directly accesses helpers, changes a helper signature
alongside its caller, and restores the original entries in the same process.
Source is restored in `finally`; if the experiment fails after applying a patch,
relaunch the baseline lab before repeating it.

These directories opt into complete domain recompilation, including peer-file
edits when the CLI selects only one file. Helpers use internal implementation
enums and remain inside that domain; callers use the explicit public boundaries.
The driver checks build/live/candidate whole-module SIL and reports
`domainCoverage` in the ordinary CLI/MCP result. This supports app-module domains
with the existing scalar/String restrictions. Read DebugTrace's live-patching
guide, “Covering optimized callers across source files,” for the exact coverage,
source-capture and compilation-cost limits.

Whole-module coverage currently refuses macro expansions and requires the captured
compiler driver/frontend fingerprints. The lab stores its SwiftUI `State` dynamic
properties explicitly so its module can be audited without replaying attached
`@State` macros. Apps that use macros can still use private single-file groups;
extending whole-module coverage to them requires actual compiler build inputs.
