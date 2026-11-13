# Apparition Store

The Store is Apparition's build service for your own apps. It builds Xcode projects from local Git
checkouts, signs and installs them on your devices, keeps every build with a manifest of what went
into it, and tells you what each device is running compared with your sources. It also receives
signed debug traces from apps that link [DebugTrace](https://github.com/illixion/DebugTrace),
reaches those apps' debug servers, serves an AltStore feed and Mac app downloads, and re-signs an
installed app with another profile.

It is part of the Apparition desktop app. There is nothing to install or run besides the app: it
starts with Apparition, runs in the app's own process and stops when you quit. This page is the
reference. **[DEVELOPER.md](DEVELOPER.md)** is the guide to setting it up and using it from the
Developer tab, **[REMOTE_DEVELOPER.md](REMOTE_DEVELOPER.md)** covers driving it from another
computer, and **[RUST_STORE.md](RUST_STORE.md)** describes how it is built.

## Where it runs

- **In the desktop app.** The Store listens on `127.0.0.1` on the `port` in `config.json` (8484 by
  default). If that port is taken it listens on another free one and says so in the app's log; apps
  that upload traces to 8484 and a `tailscale serve` pointed there then miss it, so keep the port
  free (the app's Developer tab shows it when this happens). Nothing outside the computer reaches the
  port directly: other computers come through the companion's encrypted channel, and devices through
  an HTTPS address you put in front of it (see "Security").
- **Without the app: `apparition store serve`.** The same Store, folder and port, in a terminal,
  under launchd or systemd, until Ctrl-C or SIGTERM. One Store runs per folder: `serve` refuses
  while the app's runs, and the reverse. `apparition relay` beside it offers it to computers paired
  for developer access, as the app's companion does. `APPARITION_STORE_DIR` runs a Store over
  another folder (a second Store, a test). Stopping a Store (quitting the app, Stop in the Developer
  tab, or ending `serve`) stops its running builds too and notes them as failed.
- **Config and data** are in `~/Library/Application Support/Apparition/Store/` on a Mac
  (`%APPDATA%\Apparition\Store` on Windows, `~/.local/share/Apparition/Store` on Linux):
  `config.json`, and `data/` with `state.json` (what is installed where), `queue.json` (build
  jobs), `logs/` (one log per build), `clones/` (build clones), `traces/`, `crashlogs/`, `ota/`,
  `altsource/`, `macapps/`, `sessions.json` and `secret`. `developer.json` appears beside them while
  a Store runs.
- **Builds need a Mac with Xcode.** On Windows and Linux the Store runs too, with kept builds,
  deploys from other computers, traces and the apps' debug servers, but it doesn't build.

## From the command line

Everything the Store does works from a terminal through `apparition store`, against the Store this
computer reaches: the one signed in to with its password (`store login`), else the Mac paired for
developer access (`apparition mac pair`), else the app's (or `serve`'s) here. `--local` insists on
this computer's, and `--json` prints the Store's answer as it is. `store status` says which it is.
`apparition store help` has the details.

| | |
|---|---|
| `store serve [--port N]` | run the Store without the app |
| `store login <address> [--password-stdin]` · `store logout` | sign in to another computer's Store with its password, and out again |
| `store status` · `store web-ui on\|off [--password]` · `store open [debug [<app>]]` | the Store itself, its web pages and password, its page or Debug console in the browser here |
| `store projects` · `store add <folder> [--app S] [--device D]` | the projects; add an Xcode project (as the Developer tab's Add project) |
| `store devices [--refresh [--force]]` | paired devices and what they have installed |
| `store build <project> [--device D]... [--install M] [--run] [--jit] ...` | queue a deploy and follow its log (exit 1 if it fails) |
| `store builds` · `store log <id> [--follow]` · `store cancel <id>` | the queue |
| `store kept [download <id>]` | kept builds, downloaded and checked against their manifest |
| `store resign ...` · `store resign --profiles` | re-sign on a device with another profile |
| `store ota [<bundle id>]` · `store altsource` · `store mac <slug>` | OTA, AltStore and Mac download links |
| `store traces [delete <id>]` | uploaded traces (`apparition crash-reports` reads them) |

Alongside: `apparition updates` (the update queue), `apparition deploy` (send an IPA built
elsewhere), `apparition deploy-signing` (the certificate the Store signs sent builds with),
`apparition crash-reports` and `apparition mcp` (the apps' debug servers, for agents).

**Signing in with the password.** For a Store on another computer that this one isn't paired with
(a server running `store serve`, a Mac whose web pages you turned on), `apparition store login
https://mac.your-tailnet.ts.net` asks for the Store's password (or reads one line of it from stdin
with `--password-stdin`) and keeps the session it gets back, not the password, in a file only this
user reads, next to the pairing (`anisette/store-login.json`). `store`, `deploy`, `updates` and
`mcp` then drive that Store. The address is HTTPS (what `tailscale serve` gives you), or `http://`
for this computer; the password and the session are never sent over plain HTTP, and a redirect is
never followed. The session is the web page's: builds, deploys, updates and debugging, not the
Store's own settings (its password, its web pages), which stay with its Mac (`store status` and
`store web-ui` work there). It lasts 30 idle days and ends when the password changes; sign in
again then. `store logout` ends it on the Store and here. Pairing (`apparition mac pair`) needs no
password and the Mac's person approves each computer once; prefer it for a Mac you can reach.

Links for devices made from the command line (OTA, AltStore, Mac) would otherwise name
`127.0.0.1`, which a phone can't reach: set `"publicUrl": "https://your-mac.your-tailnet.ts.net"` in
`config.json`, the address `tailscale serve` puts in front of the Store, and they start with it.
Requests through a proxy keep using the proxy's own address.

## What you need

- A Mac with Xcode (`xcodebuild`). Nothing else: no Node.js, no Homebrew bash, no `devicectl`. A
  repo's `build-signing.conf` is read by the system's own `/bin/bash`.
- Your devices paired with Apparition on this Mac (`apparition pair`, or pairing in the app;
  `apparition pair --usb` for an iPhone or iPad the first time). Everything the Store does to a
  device, reading its apps, installing, launching, moving an app's data, goes over Apparition's own
  pairing and tunnel, never Xcode's. That is also why a sleeping Apple TV stays asleep.
- For installs signed by the Mac: a paid Apple Developer account, a development certificate
  exported as `.p12` and a development provisioning profile that lists your devices. For installs
  signed on the device, nothing: the Mac keeps an unsigned IPA and the device's Apparition signs it
  with its own Apple ID or certificate.
- For live patching: DebugTrace's `debugtrace-inject` tool on the Mac.

## Signing material and each project's conf

Signing material lives outside every repo, in `~/.config/build-signing/` (`BUILD_SIGNING_DIR`
overrides the root): `certs/` for `.p12` files and `profiles/` for `.mobileprovision` files.
`global.conf` there holds what every project shares:

```sh
# ~/.config/build-signing/global.conf
TEAM_ID="ABCDE12345"
DEV_P12_PATH="$HOME/.config/build-signing/certs/development.p12"
P12_PASSWORD="change-me"
# A wildcard development profile (App ID "*") that lists your devices signs every app.
DEV_PROFILE_PATH="$HOME/.config/build-signing/profiles/wildcard-dev.mobileprovision"
# Where DebugTrace apps upload traces: this Store, over an address the app can reach.
# DEBUGTRACE_UPLOAD_URL="https://your-mac.your-tailnet.ts.net/api/traces"
```

Each project has a gitignored `build-signing.conf` (or `scripts/build-signing.conf`), read after
`global.conf`, so it wins:

```sh
PROJECT_PATH="MyApp.xcodeproj"      # or a .xcworkspace
SCHEME_NAME="MyApp"
TARGET_NAME="MyApp"
PLATFORM="${PLATFORM:-iOS}"         # respect the caller's PLATFORM, default otherwise
BUILD_BUNDLE_ID="com.example.myapp"
DEVICE_NAME="My iPhone"             # as `apparition devices` lists it
# PRE_BUILD_HOOK="scripts/fetch-deps.sh"
# EXTRA_BUILD_SETTINGS=('SWIFT_ACTIVE_COMPILATION_CONDITIONS=$(inherited) MY_FLAG')
```

The Developer tab's **Add a project** writes a minimal conf (project, scheme, platform, device; no
certificates or passwords) when the checkout has none, and never replaces an existing one.

- **Wildcard profiles** (App ID `*`) are the intended setup: one profile signs every app on your
  team, and each app's bundle ID comes from `BUILD_BUNDLE_ID`. A profile for one explicit App ID
  works for that app: point `DEV_PROFILE_PATH` at it in that repo's conf. Capabilities such as push
  or associated domains come from the profile. Extensions and watch apps need their own profiles
  when the profile isn't a wildcard: `DEV_APPEX_PROFILE_PATH`, `DEV_WATCH_PROFILE_PATH`. Apple's
  rules decide what installs; the Store doesn't second-guess them.
- Signing uses the conf's `.p12` and profiles through Apparition's own signer, not the login
  keychain.
- **Platform aliases.** `PLATFORM` is a build alias, not always an Xcode platform: a conf can map
  `iPad` back to the iOS build with a different `DEVICE_NAME`, and `visionOS` runs the iPad binary
  as "Designed for iPad". When a platform name in `config.json` isn't one Xcode knows, look in that
  repo's conf.
- Repo-specific behaviour (`PRE_BUILD_HOOK`, `EXTRA_BUILD_SETTINGS`, aliases) belongs in the conf,
  never in the Store.

A project is added from the Developer tab (**Add a project**, or **Advanced: register a project by
hand**) or by editing `config.json`. Adding one validates the checkout, writes `config.json`
atomically and keeps every other key.

## `config.json`

The file is read again whenever it changes; the exceptions are marked. Keys the Store doesn't know
are kept untouched, never rejected. A fresh Store starts with
`{"port": 8484, "bundlePrefix": [], "repos": []}`.

| Key | What it does |
|---|---|
| `port` | The loopback port (default 8484). Needs the Store restarted (quit and reopen the app). |
| `repos` | The projects, below. |
| `bundlePrefix` | A string or a list. Only apps whose bundle ID starts with one of these are tracked from a device's app inventory. Empty tracks all of them. |
| `deployDefaults` | Which deploy-menu checkboxes start ticked: `launch`, `mcp` (default on), `jit`, `buildDebug`, `inject`. Per repo too: a repo's keys win. Non-boolean values are ignored. |
| `ledgerOnlyDevices` | Device names the Store doesn't ask for their app list on its own (startup, the periodic look, a plain refresh). The row is judged from the install ledger. The refresh button with Alt held, or `POST /api/refresh-devices {"force": true}`, reads them anyway, and an install there refreshes them since the device is awake then. |
| `autoUpdate` | `{enabled, settleMinutes}`: build what is behind by itself, see "Keeping apps up to date". `settleMinutes` defaults to 5 (at most 1440). |
| `maxConcurrentBuilds` | Builds at once across projects, 1 to 16, default 2. Read at start: restart to change it. |
| `cloneMaxAgeDays` | A build clone unused for this long is cleared (default 7). |
| `apparitionCli` | The `apparition` command line the Store runs for builds, re-signs and patches. Unset, it uses `APPARITION_CLI`, the one beside the app, `~/.local/bin/apparition`, then `PATH`. |
| `buildManifestDir` | Where kept builds live, relative to `config.json` or absolute. Default `~/.local/state/apparition/builds` (`APPARITION_BUILD_DIR` overrides it). |
| `debugtraceTool` | Handed to `apparition inject` as `DEBUGTRACE_MCP`. The Store finds running apps with its own Bonjour browse and doesn't need it for that. |
| `debugtraceInjectTool` | The `debugtrace-inject` binary, handed to builds and to `apparition inject` as `DEBUGTRACE_INJECT_TOOL`. The MCP `inject` tool looks for it in `DEBUGTRACE_INJECT_TOOL`, then `~/Projects/DebugTrace/Tools/debugtrace-inject/debugtrace-inject`, then `PATH`. |
| `altSource` | The AltStore feed, see "AltStore source". |
| `webUi`, `passwordHash` | Written by the app when you turn the web pages on (Developer › Devices & signing › Web UI and other devices). Don't edit them by hand. |

Old configs may still carry `quietAppsDevices`, `crashLogs` and `builder`. They are no longer needed:
every device is read over Apparition's pairing, crash-log pulling is off, and every build runs
`apparition build`. (A device named in both `ledgerOnlyDevices` and `quietAppsDevices` is still read
on every refresh.)

### Repos

Each entry of `repos`:

- `name`, `path` (the Git checkout), `branches` (the ones offered in the UI; the checked-out branch
  is always available as `current`).
- `platforms[]`: one row per device an app installs on, `{platform, bundleId, device}`. `device` is
  the name Apparition lists for the paired device (`apparition devices`). `platform` reaches the
  build as `PLATFORM`, see "Platform aliases" above. A repo with only Mac apps sets `"platforms": []`
  and gets no Deploy button.
- `deviceBranches`: `{"<device>": "<branch>"}`, the branch a device follows. It is what Updates
  compares a device against, and what a batch deploy builds for that device.
- `cloneExclude`: paths relative to the repo root that a build clone leaves out (`["build",
  "target", "clients/android"]`). Cloning makes an inode per file even when the data blocks are
  shared, so a repo with hundreds of thousands of files clones slowly. List only what the build
  regenerates or never reads, and keep the caches a `PRE_BUILD_HOOK` reuses. Tracked files under an
  excluded path come back from Git, so exclude directories, not tracked sources. It applies only
  to builds in a clone.
- `buildSettings`: extra `--set KEY=VALUE` arguments for `apparition build`. An array applies to
  every branch, or an object `{"<branch>": [...], "*": [...]}` sets them per branch, for a flag that
  only compiles on some branches. Either form's value may itself be an object
  `{"<platform>": [...], "*": [...]}`, for a repo whose platforms are separate targets that link
  different dependencies. These land after the conf's settings on the xcodebuild line and win, so a
  platform-blind setting here can't be undone downstream.
- `macApps`, `altSource`, `deployDefaults`: see their sections.

## Builds

A build is `apparition build` run in the project's checkout or in a build clone, with `PLATFORM`
set. It is the same command you can run in a terminal, in the repo, with the same conf, flags,
`build/` folder and report (`apparition build --help`):

- It builds with `xcodebuild`, stamps the bundle ID and build number, gives a DebugTrace app its
  trace key and debug-server token, signs with the conf's certificate and profile, keeps the signed
  build and its dSYMs, and installs it over the device's pairing. With `--run` it launches the app
  the same way: `--log` streams the console to `build/device-console.log`, `--mcp` makes the launch
  a debug session for the `apps` tools, `--jit` serves the app's JIT requests, `--inject` builds for
  live patching. If a launch through Apparition gets no answer, it opens Apparition on the device
  through Xcode's tunnel (`devicectl`) and retries, keeping console, MCP and JIT features available.
  `--via auto|apparition|devicectl` picks how the signed app reaches the device (`auto`, the default, uses Apparition's own pairing, which also closes a running app so its update applies, and falls back to Xcode's tunnel, `devicectl`, when the device can't be reached that way; `devicectl` forces the tunnel; `APPARITION_INSTALL_VIA` in a conf sets it), `--no-deploy` builds and keeps without installing, `--unsigned` leaves signing to
  the device.
- DerivedData is per checkout, in `<checkout>/build/DerivedData` (`DerivedData-inject` for
  `--inject`). A per-checkout lock (`build/.build-and-sign.lock`) makes two builds in one checkout
  queue instead of colliding; a dead holder's lock is cleared.
- Mac apps (`PLATFORM=macOS`) are signed with their target's entitlements and no profile, kept as
  `build/<Name>-macOS.app`, and never installed or launched.

### The queue

Every build goes through one queue: deploys from the page, updates, AltStore and Mac downloads,
and the build a re-sign waits for. Jobs and their logs are in `data/queue.json` and
`data/logs/build-<id>.log`; job IDs start at 1000000.

- At most `maxConcurrentBuilds` builds run at once, and one at a time per checkout or build clone,
  since each holds one `build/` folder.
- A deploy of the branch that is checked out (or `current`) builds in the checkout itself, so its
  uncommitted state is part of the build. Any other branch, an update, an AltStore download and a
  Mac download build in a **build clone** at an exact commit, so your working tree is never touched.
- **Build clones** (`data/clones/<repo>/<branch>/`) are laid out like `~/Projects`: the app, and
  each local Swift package it builds against beside it, found from its Xcode projects and
  `Package.swift` files transitively, so relative references such as `../../RAVESDK` resolve. The app
  is at the commit asked for and each package at its checkout's HEAD, committed state only: nobody's
  uncommitted edits ship. Gitignored files the build needs (`build-signing.conf`, and anything
  listed in the repo's `.worktree-include`) are copied from the checkout. A clone is kept between
  builds, so a build fetches only the new commits and Xcode builds incrementally. One unused for
  `cloneMaxAgeDays` is cleared (checked hourly), never while a build uses it.
- A batch deploy to several devices queues together and runs through the same rules. A request
  names its target: a target that isn't configured is refused, never replaced by the first one.
- Cancelling stops the build's process group. A job still queued or running when the Store stops is
  marked failed on the next start.
- `data/logs/` holds each build's log; the page tails the open one.

### Version freshness

Dev builds keep a static `CFBundleShortVersionString`, so the version string can never say whether
a device is stale. The **build number** (`CFBundleVersion`) does:

```
<sha>[-<6hex>][-dirty]
```

- `<sha>` is the app's commit.
- `<6hex>` is six hex digits over the commits of the local packages the app built against (RAVESDK,
  vergence...), named by folder so a build clone and the checkout stamp alike. A rebuild for a
  package change therefore has a number of its own and counts as an update to Apparition, AltStore
  and the page alike. An app with no local packages has no tag.
- `-dirty` means the project or a package had uncommitted changes.
- A project outside Git has no stamp.

The Apps view compares the SHA the device reports (the leading part of its build number, falling
back to the install ledger's `commit`) with the repo's HEAD and shows up to date, out of date,
dirty, or on another branch. When touching version logic, reason about the SHA, not the version
string.

## Install modes

A build request has an `install` mode (`POST /api/service/v1/build`, or the deploy menu). Without
one it is `auto`, except that `deploy: false` means `none`.

- `mac`: `apparition build` installs the signed build over the Mac's own pairing with the device, in
  the same run. The quickest route, and the only one that can launch the app (`run`, `mcp`, `jit`).
  If the install fails, the signed build has been kept already and the failed job carries `buildId`
  and `keptBuildId`, so the device's own Apparition can still install it.
- `device`: `--no-deploy --unsigned`. The build is kept and the Apparition app on the device
  downloads the IPA over the Store's HTTPS address (`/artifacts/<id>/download`), signs it with its
  own identity and installs it. It needs nothing from the Mac's network, so it works over Tailscale
  and with a free Apple ID. The device reports the install with `POST /api/service/v1/installs
  {device, bundleId, buildId}`, and the Store fills in repo, branch, commit and version from its own
  records.
- `none`: build and keep only. The ledger records an unconfirmed guess, which Updates ignores.
- `auto`: `device` while that device's Apparition is open, else `mac`; always `mac` when the request
  asks to launch the app, since only the Mac's tunnel can start a process. Apparition names its
  device in an `X-Apparition-Device` header (percent-encoded) on every Store request, and a device
  not heard from for 20 seconds counts as closed.

### The install ledger

`data/state.json` keeps `installs[device][bundleId]`, the Store's record of what it put where:
`{repo, branch, commit, buildId, platform, version, buildNo, at, confirmed}`. `confirmed` is true
when the Mac installed it itself or the device reported it, false for a `none` build (an offer
nobody confirmed). The ledger is the source of truth for the install history and for devices in
`ledgerOnlyDevices`; the device's own app list (`deviceApps`, refreshed at startup, every 15 minutes
while the automatic mode is on, and after installs) feeds the freshness column. Whichever is newer
wins, and a build ID from the ledger is kept when the device reports the same build number. A
`state.json` written by an older Store is read as it is, and keys the Store doesn't know survive a
write.

## Keeping apps up to date

The **Updates** view (and `apparition updates` from a terminal, on the Mac or on a computer paired
with it) lists the apps on your devices that are behind, and why: the app's own branch, or a local
package it builds against.

- The kept build installed on a device records every source it was built from, each with its
  commit. A source that moved counts only when its commits change what the build compiles: for a
  package, its manifest and its targets' folders (less tests and docs); for the app, anything but
  tests and docs. So a commit to vergence that only touches tests leaves an app up to date.
- **Update all** (`apparition updates --run [--only <device or bundle id>]`) builds each app once
  in a clean clone and installs it on every device that runs it, as signed, never re-signed. Rows
  whose conf resolves to the same build (`apparition build --plan`; the `iPad` alias is the iOS
  build) share one build, installed twice. A device the Mac can't reach keeps the build for its
  Apparition to install. A `watchOS` row goes with its iPhone app and isn't listed.
- **Update automatically** (`autoUpdate: {"enabled": true, "settleMinutes": 5}`, or `apparition
  updates --auto on|off`) does that by itself once an app's sources have been still for
  `settleMinutes`, so a burst of commits across a package and an app is one build. It checks every
  minute, asks the devices what they have every 15 minutes, and won't retry a commit whose build
  failed for six hours.
- `apparition updates --check` asks the devices first. `GET /api/updates` returns the plan;
  `POST /api/updates/{check,run,auto}` and the same under `/api/service/v1` drive it. The queue's
  jobs and the install ledger are in `data/queue.json` and `data/state.json`.

## Re-signing an installed app

**Re-sign** signs an app that is already on a device with another of the Mac's provisioning
profiles (any `.mobileprovision` in `~/.config/build-signing/profiles`) and installs it over the
old copy. A device can't hand an installed app back, so the Store signs a kept build.

`GET /api/service/v1/resign/options` lists the profiles, without paths or device lists.
`POST /api/service/v1/resign` (capabilities `resign` and `resign-build`) takes either of:

- `{buildId, device, profile, dataMode?, dryRun?}`: re-sign that kept IPA.
- `{repo, device, platform?, branch?, profile, dataMode?, dryRun?}`: no kept build is needed. The
  Store first queues a signed `--no-deploy` build of that project target (the same build **Build
  only** makes), then a re-sign of the IPA it kept. The reply lists both jobs, the build first. A
  dry run builds nothing.

A profile for the same App ID updates the app in place and keeps its data. A different App ID is
refused by installd as an update, so the Store copies the app's Documents and Library off the
device into `~/.local/state/apparition/resign-backups/` (`APPARITION_RESIGN_BACKUPS` overrides it),
removes the app, installs the new signature and puts the data back, checking the file count. If the
new signature won't install, it restores the previous one with its data. `dataMode: "discard"`
skips the backup, and the backup is never deleted for you. Keychain items and app-group containers
don't move. A dry run, **Check first**, only tests that the data can be reached. This goes through
Apparition's pairing (`apparition container pull|push|count`, `apparition uninstall`), so the
device must be reachable from the Mac; an OTA install can't change an installed app's App ID.

## Kept builds and provenance

Every build is kept in the build store, `~/.local/state/apparition/builds/<buildId>/` (see
`buildManifestDir`), as the exact signed IPA or Mac app zip plus an `apparition-build/1`
`manifest.json`. The build ID is the public name of that build: it is also the build's DebugTrace
key ID when a credential is embedded, and it joins jobs, ledger entries, traces and symbols,
separately from the Git SHA in the build number.

- The manifest records the artifact's SHA-256 and size (the artifact's file name is its hash), binary
  UUIDs, original and installed bundle IDs, the app's version and build number, and the retained
  symbol set.
- Source builds capture state before and after compiling: Git commits, dirty diffs, untracked
  file contents, local Swift package and submodule dependencies, resolved-package file hashes,
  toolchain versions and a digest of the build arguments. A project outside Git uses a directory
  fingerprint. Cached build directories are excluded. This is scoped provenance of Git, directory
  and package state, not of every input an arbitrary build hook might read. The version stamp uses
  the commit captured at the start even if HEAD advances during compilation. A source that changed
  while compiling marks the build inconsistent, and AltStore and Mac-download builds refuse to
  publish it. Signing-only inputs record unknown source provenance instead of inheriting the
  checkout's.
- The Store uses the manifest's exact artifact whenever there is one. It reads a kept build only
  if the artifact still matches its manifest's SHA-256 and size. The desktop app checks both before
  replacing a destination file you chose in **Download**.
- Development source builds also keep immutable symbols under
  `~/.local/state/debugtrace/dsyms/<bundleId>@<buildId>/` (`DEBUGTRACE_DSYM_DIR`), including
  `--no-deploy` and macOS builds. UUIDs from the app and its embedded binaries select the exact DWARF
  files, each with a SHA-256 and size. Updating an app doesn't rotate its older symbols away.
  `--sign-only` builds are excluded.
- Nothing is pruned automatically. Archives hold signed apps and credentials and are private to
  your user.
- `GET /api/artifacts`, `/api/artifacts/<buildId>/manifest` and `/download` (also under
  `/api/service/v1`) list, describe and stream them.

An IPA made elsewhere (xtool, another Mac) joins the store through `apparition deploy`, see
[REMOTE_DEVELOPER.md](REMOTE_DEVELOPER.md). It is kept with origin `upload`, and the device's
Apparition lists it under **Sent from your computers**.

## Debug traces and the key ledger

Apps that link DebugTrace upload a trace zip to `POST /api/traces` from their Debug Trace sheet; the
web page's Traces view lists them and also takes a dropped `.zip` (one shared by AirDrop, say), or
an encrypted problem report (`.trace`, [below](#problem-reports-from-public-copies)).

- **Keys.** Every dev build of a DebugTrace app gets a fresh Ed25519 key pair from `apparition
  build`. The private half goes into `DebugTraceCredential.plist` in the app (sealed by the code
  signature), with `DEBUGTRACE_UPLOAD_URL` from `global.conf`. The public half goes into the **key
  ledger**, `~/.local/state/debugtrace/keys/<keyId>.json` (`DEBUGTRACE_KEY_DIR` overrides it), with
  the bundle ID, build, platform, configuration, branch and device. A build run from a terminal is
  verified the same as one the Store ran.
- **Verification.** The Store parses the zip itself (stored or deflate entries only, no zip64, every
  inflate capped at the manifest's declared size, 64 MB upload, at most 512 files, 256 MB
  inflated in all). A trace is kept only when `manifest.sig` verifies against the ledger key, the
  manifest's bundle ID and build equal the ledger's, and the zip holds exactly `manifest.json`,
  `manifest.sig` and the listed files, each with a matching size and SHA-256. Unsigned traces
  (Xcode builds) are refused with 403; share those as files.
- **What a signature proves.** The private key ships in the IPA, so a valid signature means "a build
  this Mac produced, and which one", not "a genuine device".
- **Storage.** `data/traces/<traceId>/` holds `trace.zip` as uploaded, `files/` extracted (agents can
  read `files/logs.txt` directly) and `meta.json`. The newest 300 are kept. Uploading a trace ID
  again returns the existing record with `duplicate: true`. Behind `tailscale serve` the uploader's
  `Tailscale-User-Login` is recorded.
- **API.** `POST /api/traces` (raw zip body; `X-DebugTrace-Source: web` marks page drops) answers `201
  {ok, trace}` or `{error}`. `GET /api/traces`, `GET /api/traces/<id>.zip`, `GET
  /api/traces/<id>/files/<path>` (text served as `text/plain` with `nosniff` and a `default-src
  'none'` CSP, never HTML, since the content came from a device) and `POST /api/traces/<id>/delete`.
  `POST /api/traces/report` takes an encrypted report, and needs a session.
- MetricKit reports a trace carries (`crashes/*.json`) are what link it to a crash log.

### Problem reports from public copies

A copy from the release site has no build key, so its Report a Problem makes an **encrypted
report**: a `.trace` file, the trace zip sealed to the maintainer's public key (HPKE, RFC 9180:
X25519, HKDF-SHA256, ChaCha20-Poly1305; the layout is in DebugTrace's `Docs/traces.md`). The
person attaches it to a public issue, and only the Mac holding the private key can open it.
GitHub takes no `.trace` file on an issue, so the app shares it wrapped: `<name>.trace.zip`, a zip
of exactly one entry, the `.trace`, unchanged. The Store takes either form, and tells them apart
by content, not by name: a zip whose only entry is a plain name ending `.trace` is a wrapped
report, and any other zip is a trace.

- **The key.** `debugtrace-decrypt keygen` (in DebugTrace's `Tools/debugtrace-decrypt`) makes the
  pair. The private half is `~/.local/state/debugtrace/report-keys/<key id>.key`, mode 600
  (`DEBUGTRACE_REPORT_KEYS` names another folder); the public half is built into the app. A file
  names the key it needs, so old keys keep opening old reports. Back the folder up: nothing else
  opens a report. **Or keep the key in 1Password only:** put `<key id>.op` in the folder instead,
  holding no secret, just `ref: op://<vault>/<item>/password` and optionally `account: <account>`.
  The Store and `debugtrace-decrypt` then run `op read` (Touch ID) when a report needs that key and
  keep it in memory for that report alone.
- **Adding one.** Drop the `.trace.zip` (or `.trace`) on the **Crashes & traces** page (or **Upload trace…**), or
  run `apparition crash-reports add <file.trace>...`, which needs no running Store. Either way it
  is decrypted, checked and unpacked into `data/traces/<traceId>/` like any trace, so
  `crash-reports show`, the files view and symbolication read it the same way.
- **The intake rule.** A report is accepted **unsigned**, and that is the one exception to the
  ledger rule above. Everything else is checked as for a signed trace: the zip's form, every file
  listed with its size and SHA-256, safe paths, nothing unlisted, the size limits. The record
  carries `via: "report"` and `report: {keyId, signed}`: the report key it was sealed to, and
  whether a ledger key also signed it. The page and `crash-reports list` mark it `report`, and
  `unverified` or `unsigned` when no key vouches for it, and its `build` fields are empty: the app,
  version, build and device of an unsigned report are **what the sender's copy said**, nothing
  more.
  - A report whose manifest names a key **in this Mac's ledger** (one of your own dev builds) must
    verify against it, bundle ID and build included, or it is refused. It is then stored with the
    ledger's build details and `signed: true`.
  - One that names a key this Mac has never seen (another Mac's build) can't be checked, and
    counts as unsigned.
- **Who may add one.** The public key ships in a public app, so anyone can seal a file to it:
  decrypting proves the file was meant for this Mac, not who made it. So a report goes in only
  through a **signed-in page** (`POST /api/traces/report`: a session, never the service API's
  bearer or a paired computer) or the command line on this Mac. `POST /api/traces`, which needs no
  session because a ledger signature is its whole gate, refuses a `.trace` and says where to take
  it, and still refuses an unsigned zip.
- **Errors say what to do.** A report sealed to a key this Mac lacks names that key's id and the
  folder searched; a changed or truncated file says to ask for the original; a file that isn't a
  report says so.

### Symbols for public releases

`apparition build` keeps each dev build's dSYMs by build ID. A release has no build ID, so its
symbols are imported once, from the dSYM zips the release attaches
(`Apparition-<version>-<platform>-dSYMs.zip`, also in `build/dsyms/<version>/` after a release):

```sh
apparition crash-reports import-dsyms build/dsyms/1.0.0/Apparition-1.0.0-*-dSYMs.zip
```

Each zip becomes a set of its own, `~/.local/state/debugtrace/dsyms/<bundle id>@release-<version>-<platform>/`,
indexed by binary UUID like a build's. Signing a copy on a device changes its signature, not the
binary's UUID, so one import covers every copy installed from that release, and
`crash-reports show <id>` then resolves a report's app frames to function and line. Importing the
same zip again changes nothing; a set is never replaced or removed. The command runs DebugTrace's
`debugtrace-symbolicate import-dsyms`, which works on its own too.

## Crash logs

**Pulling crash logs off devices is disabled for every device.** The old periodic pull ran
`devicectl` against each device every five minutes, and doing that to a sleeping Apple TV woke it
again within seconds of being switched off. There is no timer, `POST /api/crashlogs/refresh`
answers `{ok: false, disabled: true}`, and no new `.ips` files arrive: apps push their traces to
the Store themselves, which is how a new crash shows up (as MetricKit reports in a trace, plus the
breadcrumbs from the run that died). Crash logs already stored stay readable, with their
symbolicated text and the trace linked by bundle ID, `pid` and time.

- **`apparition crash-reports`** reads the Store's data folder directly (`STORE_DATA` names another),
  so it needs no login and works while the Store is stopped:
  - `list [--app TEXT] [--device NAME]`: crash logs, then traces, newest first. A problem report
    shows `report` where a trace shows its device, and `unsigned` when no key vouches for it.
  - `show <id>`: a crash log's symbolicated stack, or a trace's MetricKit reports. An id is any
    unambiguous prefix of what `list` prints.
  - `done <id>... [--dry-run]`: investigated and fixed. It moves the crash log's `.ips`, its text
    and the linked trace to the Trash, never deletes them. The Store forgets a crash log whose files
    are gone.
  - `add <file.trace>...`: open an encrypted problem report and keep its trace
    ([Problem reports from public copies](#problem-reports-from-public-copies)).
  - `import-dsyms <dSYMs.zip>...`: keep a release's symbols
    ([Symbols for public releases](#symbols-for-public-releases)).
  Clean up only what you investigated and fixed.
- **API.** `GET /api/crashlogs` returns `{crashLogs, pull}`; `GET /api/crashlogs/<id>/ips` and
  `/symbolicated` serve text as `text/plain`, never HTML. Nothing here skips the session.

## Live debugging

Dev builds of apps that link DebugTrace's `DebugTraceServer` start a debug server on every launch
and advertise it over Bonjour (`_debugtrace._tcp`). The Store browses for them itself, from the
process that holds the Mac's Local Network permission. The **Debug** view, and the desktop app's
**Debug** tab (Running now: **Tune**, **Log**, **Profile** and **Memory** buttons open that view in a window titled
with the app's name), list the ones running on your network. For each one:
a live tail of its log (filter by level, pause), the **Tune** tab, **Profile** and
**Memory** (views of the app's `_profile` and `_memory`), **Crashes & traces** for that app, the generic **Endpoints** browser and
caller, **Info**, and **Capture trace…**, which asks for an optional note, captures a trace and uploads it here. The search box
in the header (⌘F or `/`) searches whichever tab is open: it narrows tunables (by key, label or group), endpoints (by name or
description), profile rows, memory regions and reports to the matches, asks the app for matching log lines only, and
highlights matches in Info and endpoint results. A device chip in **Apps** shows *live* while its app runs.

- **Finding and trusting apps.** An app is addressed by its build's key ID. The address comes from
  discovery (or from the launch record `apparition build --run --mcp` writes under
  `~/.local/state/debugtrace/sessions/`) and the bearer token from the key ledger, so a page can't
  point the proxy at another host. A Bonjour address never receives the token: the Store signs each
  request with it (`GET /_nonce`, then `Authorization: DebugTrace-HMAC`). A build from before that
  protocol (`txtvers` 1 or 2) is listed as not attachable until it is rebuilt. Bonjour covers the
  local network only.
- **Prompts.** The first call from a client waits on the device's "Allow debug access?" prompt, and
  `_profile` samples for up to 10 seconds, so the proxy waits up to 90 seconds; a timeout is a 504
  whose hint points at the prompt. Connecting to the Store does not grant debug access.
- **Tune** turns values an app reads through DebugTrace's `DebugTunables`
  (`DebugTunables.number("hdr.peakNits", 600, range: 100...2000)`) into sliders, toggles and menus
  that change the running app as they move. Agents save their own panels for what is being tested
  with the `apps` MCP server's `panel` tool, kept in `~/.local/state/debugtrace/panels/<bundleId>/`.
  **Copy values** gives the tuned numbers as text to paste back to an agent. On Apple Vision Pro,
  tune from this page on a phone, or on the Mac Virtual Display (Settings › Developer › Allow Mac
  Virtual Display), which breaks `.progressive` immersion styles, so turn it off to test those.
  Details are in DebugTrace's `Docs/tuning.md`.
- **MCP.** `POST /mcp` is an MCP server (`apps`, `help`, `call`, `trace`, `panel`, `inject`) behind
  the same bearer as the service API. Agents on the Mac run `apparition mcp`, which reads
  `developer.json`; agents on another computer reach it through the companion's channel, see
  [REMOTE_DEVELOPER.md](REMOTE_DEVELOPER.md).
- **Routes.** `GET /api/debug/apps`, `GET /api/debug/<keyId>/help`, `POST /api/debug/<keyId>/call
  {endpoint, arguments}` (binary replies pass through with a locked-down CSP), `GET
  /api/debug/<keyId>/panels`, and `GET /api/debug/<keyId>/logs?level=&contains=`, an SSE stream that
  survives the app going quiet or relaunching.

### Live patching

`inject` applies edited Swift sources to a running app. The Store runs `debugtrace-inject` from the
app's project root, one patch at a time per app and device, so the project must be the Mac's own
checkout, edited in place. It needs a build launched with `--inject` that is still running: the route
runs `apparition inject --no-launch`, so it never builds or relaunches (the CLI and the MCP `inject`
tools do, when the app isn't running such a build; the Store's build route is how it builds). Through
the service API it is `POST /api/service/v1/inject {repo, device, targetPlatform, files?}` (capability
`entry-patching`): the Store runs `apparition inject` for a configured target, which verifies the
live session, compiles and signs the patches, asks the device to approve the code change and
detaches. No build or install happens. Build requests accept `injectBackend: "dynamic" |
"apparition"` alongside `inject`; `dynamic` is the default and the Apparition backend uses separate
`DerivedData-inject-apparition` products. Build requests with the Apparition backend also save
compiler optimization remarks under `build/inject/optimization-records/`, which DebugTrace's
`debugtrace-inject callers` can query; they are incomplete observations and authorize nothing. See
`DebugTrace/Docs/live-patching.md` for what can be patched and [DEVELOPER.md](DEVELOPER.md) for the
workflow.

## JIT apps

Apps that generate code at run time (emulators, Wine, JIT-compiling VMs) need a debugger attached
to be allowed to. Tick **JIT** next to *Launch app* in the deploy menu (`"jit": true` in a build
request, `apparition build --jit` in a terminal). The build then launches the app suspended with
`apparition launch --jit`, which attaches, resumes it and serves its JIT requests from a background
process for as long as the app runs. It needs the `mac` install mode, since only the Mac's own
pairing can launch, and combines with the other options:

| Deploy options | `apparition build` | What runs |
|---|---|---|
| Launch + JIT | `--no-mcp --jit` | the app, JIT served; events in `build/device-console.log` |
| Launch + MCP + JIT | `--run --mcp --jit` | also its console in that file and its debug server registered for agents |
| + Inject | `--inject` as well | patches go live while JIT is served |

The session's pid is in `build/device-console.pid`: `kill $(cat build/device-console.pid)` detaches
the debugger cleanly and leaves the app running, and the next launch stops the previous session
first. Stop any Xcode debugging session of the app before launching it with JIT. A launch counts
only once the app reports `Launched application` (within 20 seconds, `APPARITION_LAUNCH_WAIT`):
a device whose tunnel is unreachable, such as a headset with the on-device Apparition closed,
otherwise sits connecting. A headset that isn't worn holds launches. By hand, `apparition launch
<bundle ID> --device <name> --jit [--console]` does the same, and `apparition jit <bundle ID>
--attach` serves an app that is already running. The device must be paired with Apparition on this
Mac once.

## AltStore source

With `"altSource": {"enabled": true}` in `config.json`, the Store publishes
[AltStore-format sources](https://faq.altstore.io/developers/make-a-source) of the configured apps,
which AltStore, SideStore, or any client that reads the format can subscribe to. The `/ota` page
shows each source URL with "Add to AltStore / SideStore" links; `GET /api/altsource` returns the
same.

- **One source per platform**, generated on request: `/altsource/<platform>.json` for every platform
  name in `config.json` (`iOS`, `iPad`, `visionOS`..., case doesn't matter), because AltStore can't
  filter a source by device. Each lists the apps that have a row for that platform, built with that
  row's settings, so an `iPad` or `visionOS` alias gets its own build. Add the one that matches the
  device. `/altsource/source.json` combines them, one entry per bundle ID (the first row wins), for
  clients that filter on the per-app `platform` key themselves.
- Each app is listed at the head of its source branch (a repo's `altSource.branch`, else `main` or
  `master` when offered, else its first branch). `buildVersion` is that commit's short SHA, the same
  stamp `apparition build` writes into `CFBundleVersion`, so a client sees an update whenever the
  branch moves.
- Nothing is built ahead of time. Downloading a version builds exactly that commit, from a clean
  build clone (uncommitted edits never ship), signs it without deploying, and answers with the IPA
  once the build finishes. It goes through the same queue as everything else, and simultaneous
  downloads of one commit share one build. A clean OTA build already stamped with that exact commit
  is reused. A failed build is answered with an error for two minutes instead of being retried.
  Two IPAs are kept per app.
- The wait is a long poll. A request is held for `holdSeconds` (default 50, under URLSession's
  60-second idle timeout), then redirected (307) back to itself to wait again; the build carries on
  whether or not anyone is waiting. A client that controls its own timeout can pass `?hold=<seconds>`
  (up to 3600). URLSession follows a limited number of redirects, so a very long build can outlast a
  stock client's patience; asking again picks up the running build or its finished IPA.
- AltStore refuses an IPA whose entitlements or privacy strings differ from the source's
  `appPermissions`, so those are read from the built IPA. Before a commit is built, the listing
  carries the previous build's values (and `1.0` for an app never built here).
- Each app also gets a non-standard `platform` key.

Settings, all optional: `name`, `identifier`, `subtitle`, `iconURL`, `website`, `developerName`,
`branch`, `token`, `holdSeconds`, `linkTtlDays` (7). Per repo, `"altSource": false` leaves it out, or
`{branch, platforms: ["iOS"], description, subtitle, iconURL, developerName, category}`. `platforms`
limits which of the repo's rows reach the sources. Within one source a bundle ID is listed once.

## Mac apps (.app.zip downloads)

A repo can list Mac apps next to its device platforms. Each gets a row on its card with a
**.app.zip** button that downloads a signed `.app` of the selected branch, built on demand:

```json
"macApps": [
  { "name": "MyApp for Mac", "bundleId": "com.example.myapp.mac" },
  { "name": "MyApp Helper", "platform": "macOS-helper", "bundleId": "com.example.myapp.helper" }
]
```

- `platform` (default `macOS`) reaches the build as `PLATFORM`, like a device row's. A repo with
  several Mac apps gives each its own alias and maps it in its `build-signing.conf` to a scheme,
  target and bundle ID (`macOS-helper` maps to `PLATFORM=macOS`, `SCHEME_NAME=MyAppHelper`), the same
  way an `iPad` row maps back to the iOS build. `branch` picks the default branch (else `main` or
  `master`, else the first offered); the card's branch menu picks another.
- A download builds the branch's committed head, never uncommitted edits, from a clean build clone
  in the queue, signed with the development certificate with the target's entitlements and no
  profile, so apps whose entitlements need one aren't built here. The zip is kept per commit (the
  newest three per app), so the next download of the same commit starts at once. The build is
  refused if the app it produced has another bundle ID than the row's, which catches a conf alias
  that fell through to the wrong scheme.
- `/mac/<slug>/<commit>.app.zip` serves or builds one commit, long-polling like the AltStore IPA
  (`?hold=` up to 3600 s); `/mac/<slug>/latest.app.zip?branch=` redirects to the branch head's. Both
  need the session or a signed link. The **curl** button copies a command with a link signed for a
  week, for a Mac that isn't signed in.
- Downloaded from a browser, the app is quarantined and Gatekeeper blocks its first open, since it
  is not notarized: allow it in System Settings › Privacy & Security › Open Anyway, or run `xattr -dr
  com.apple.quarantine` on it. curl doesn't quarantine what it downloads.

## OTA installs

Every IPA the Store builds is also copied to `data/ota/`, and the `/ota` page lists them with
`itms-services` install links. installd can't send cookies, so every OTA link the Store
hands out (the page, `GET /api/ota/<slug>/link`, the IPA URL inside a manifest) carries an HMAC
signature that expires after an hour, and only the signed-in page hands them out. Don't build OTA
URLs in a browser. An OTA install can't change an installed app's App ID.

## The web pages

The Store's pages (Apps, Debug, Builds, Updates, Crashes & traces) are compiled into the app. They are off
until you turn them on with a password: Settings › Build service and signing › **Web UI and other devices**
(8 to 256 characters). With them off, every page and cookie route answers 404 saying so, while the
machine routes still work: signed trace uploads, `/mcp`, and the signed `/ota/`, `/altsource/` and
`/mac/` downloads. The `/ota` page itself is one of the pages.

The same password lets Apparition on another device connect (`POST /api/service/connect`). The
Developer tab inside the app doesn't need it: the app talks to the Store with a token only it holds.
The Debug window is the same page signed in by a one-time link, with the pages off too.

## Security

- **Password.** `passwordHash` is scrypt (N=2^15, r=8, p=1). Sessions are a random cookie
  (`HttpOnly`, `SameSite=Strict`, `Secure` behind HTTPS); only their SHA-256 is stored, in
  `data/sessions.json` (mode 600), tied to the current password, so changing the password signs
  everyone out. They last 30 idle days. Five wrong passwords lock sign-in for a minute for everyone
  (behind `tailscale serve` every client is 127.0.0.1). A non-GET with a foreign `Origin` is refused.
  A Store with no password and the pages off has no browser sessions at all.
- **What works without a session**, each with its own check:
  - `POST /api/traces`: only traces signed by a key in this Mac's ledger are kept. (An encrypted
    problem report is unsigned, and so needs a session: `POST /api/traces/report`.)
  - OTA manifests and IPAs: signed links, valid for an hour.
  - The AltStore source, when enabled: its URL carries a lasting token (`altSource.token`, else one
    derived from `data/secret`; delete that file to rotate it, which also signs every other link
    out), and the download and icon links inside carry their own signatures (`linkTtlDays`).
    Anyone holding the source URL can start builds of the listed commits.
  - Mac downloads, through a link the signed-in page hands out (the curl button). The signature
    covers the app and branch and lasts a week; anyone holding it can start builds of that branch's
    head.
- **Reaching it.** The Store listens on `127.0.0.1`. Devices reach it through an HTTPS address,
  `tailscale serve` over a private tailnet being the intended way, for example
  `https://your-mac.your-tailnet.ts.net`. Apparition refuses plain HTTP credentials to another
  host and doesn't follow redirects. Don't expose the port to the internet, password or not.
- The trace key ships inside each IPA, so a valid signature proves "a build this Mac made", not "a
  genuine device". Installation still requires each device's UDID to be in the app's provisioning
  profile.

## The service API and the desktop app

The desktop app is the Store's client on the Mac: it starts the Store in its own process and
talks to it with a 43-character token it makes at launch and keeps in memory, as a bearer on
`/api/service/v1/*`. The same API serves Apparition on devices and other Macs, which sign in with
the web password (`POST /api/service/connect {password}` returns a session token; password changes
end it; `POST /api/service/v1/disconnect` revokes it). Native clients refuse HTTP credentials to
remote hosts and don't follow redirects. Only the service address is remembered on disk.

- `GET /api/service` advertises protocol `apparition-service`, version 1, and its capabilities
  (`projects`, `devices`, `builds`, `build-logs`, `cancel-builds`, `traces`, `crash-logs`,
  `debug-endpoints`, `artifact-manifests`, `artifact-downloads`, `project-registration`,
  `project-discovery`, `resign`, `resign-build`, `entry-patching`, `install-modes`, `report-install`).
  Clients enable features from that list.
- Authenticated operations under `/api/service/v1/`: `state`, `build`, `builds/<id>/log`,
  `builds/<id>/cancel`, `traces`, trace files, `crashlogs`, crash files, `debug/apps` and an app's
  `help`/`call`, `artifacts` (recent kept builds) with `artifacts/<buildId>/manifest` and
  `/download`, `updates` with `updates/{check,run,auto}`, `inject`, `installs`, `projects` and
  `projects/discover`, `resign` and `resign/options`, `devices`, `devices/<device>/apps`,
  `builds/<id>/install?device=`, and `deploy`.
- **A build request** is `{repo, device, targetPlatform, branch, run, mcp, jit, inject,
  injectBackend, buildDebug, deploy, install}`. The booleans are optional; `jit` applies only with
  `run`. The project, platform, device and branch (`current` or a configured one) must match the
  configuration, and the platform is kept even when one device has several platform aliases.
- The rest of what the page does is under `/api/service/v1/` too, by the page's own paths without
  `/api`: `refresh-devices`, `altsource`, `ota/<slug>/link`, `mac/<slug>/link` and `/build`,
  `debug/<id>/panels` and `/logs`, `traces/<id>.zip` and `/delete`, `crashlogs/refresh`.
  `apparition store` uses them.
- **Only the app's token** (which `apparition store` on the same computer reads from `developer.json`) reaches `GET /api/service/v1/host` (status) and `POST
  /api/service/v1/host/web-ui {enabled, password?}`, and `POST /api/service/v1/host/web-link`, which
  returns `/host-login?code=...`: a code good once for 60 seconds that becomes an in-memory session
  cookie (gone when the app quits), which passes the web gate even with the pages off. The Debug window
  window uses it. Computers paired for developer access come in with the app's token too, but the
  channel names them (`X-Apparition-Computer`), and the Store gives them what a password session
  gets: builds, deploys, debugging, not the password, the pages or sign-in links. The channel also
  drops `X-Forwarded-*` and `Tailscale-*` headers they send.
- **Agents on the Mac** use the app's token through `developer.json` in the Store's folder (the
  loopback URL, token and pid, mode 0600, removed when the app quits), which `apparition mcp`,
  `apparition updates`, `apparition deploy` and `apparition store` read. Other computers come through the companion's
  encrypted channel with a pairing of their own, see
  [REMOTE_DEVELOPER.md](REMOTE_DEVELOPER.md).
- **Project registration**: `POST /api/service/v1/projects` (`name`, Mac `path`, optional `branches`,
  and `platforms: [{platform, device, bundleId}]`) adds a checkout that already has a
  `build-signing.conf` (registration never executes it) to `config.json` atomically, preserving
  other settings, and makes it available at once. `projects/discover` reads a checkout's schemes
  and platforms without building or sourcing anything. Duplicate names, duplicate paths and invalid
  targets or branches are refused.

## What isn't in 1.0

- **`--distribution` builds.** `apparition build` refuses them.
- The conf keys `EXTRA_ENTITLEMENTS_PLIST` and `ASSOCIATED_DOMAINS`.
- The **`apparition xcode` handoff** (registering an Xcode product, or a generated scheme that
  registers it) has no Rust implementation yet.
- Automatic **pruning** of kept builds and symbols.
- Pulling **crash logs** off devices, see "Crash logs".
