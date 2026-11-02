# appstore

A personal, LAN/tailnet-only "app store" for your own Apple devices. It builds apps from local git
checkouts, signs and installs them with `xcrun devicectl`, and shows what each device is running
against each repo's git HEAD. It also receives signed debug traces from apps that link
[DebugTrace](https://github.com/illixion/DebugTrace).

Two parts, which ship together:

- **`server.js`** (plus `lib/`): a Node HTTP server with a web UI (no dependencies, no build step).
- **`bin/build-and-sign`**: a bash script that builds, signs and installs one app from the git repo
  it is run in. The server calls it, and you can run it directly.

## Requirements

- macOS with Xcode (`xcodebuild`, `xcrun devicectl`) and Node 18 or newer (also required
  by the terminal build-and-sign command for artifact manifests).
- Bash 4+ (`brew install bash`); the script's shebang is `/opt/homebrew/bin/bash`.
- A paid Apple Developer account, a development certificate exported as `.p12`, and a **wildcard
  development provisioning profile** listing your devices (see below).
- Devices paired with this Mac in Xcode.

## Provisioning profiles

`build-and-sign` is designed for **wildcard profiles** (App ID `*`). One wildcard development
profile signs every app on your team, and the script sets each app's bundle id from
`BUILD_BUNDLE_ID` and fills in the concrete `application-identifier` at sign time. Generate the
profile in the Apple Developer portal with every device you install to.

A profile for a single explicit App ID also works for the app that App ID belongs to: point
`DEV_PROFILE_PATH` in that repo's conf at it. Capabilities (push, associated domains, and so on)
come from the profile, so apps that need them use a profile that grants them. Apple's rules decide
what installs; the script doesn't second-guess them.

Extensions and watch apps need their own profiles when the profile isn't a wildcard. See
`DEV_APPEX_PROFILE_PATH` and `DEV_WATCH_PROFILE_PATH` in the script header comments.

## Setup

1. Put the signing material outside any repo:

   ```sh
   mkdir -p ~/.config/build-signing/{certs,profiles}
   cp examples/global.conf.example ~/.config/build-signing/global.conf   # then edit
   ```

2. In each app repo, add a `build-signing.conf` (or `scripts/build-signing.conf`) from
   `examples/build-signing.conf.example` and **gitignore it**.
3. Optionally put `bin/build-and-sign` on your PATH, for example
   `ln -s "$PWD/bin/build-and-sign" ~/bin/build-and-sign`, then run it from inside an app repo:

   ```sh
   build-and-sign              # Release build, sign, install on the conf's device
   build-and-sign --help       # every flag
   ```

   Keep the complete tools checkout: build-and-sign calls `bin/build-manifest` and
   `lib/` beside its real path. A symlink on PATH works; copying only the bash file does not.

4. Create the server's config and start it:

   ```sh
   cp config.example.json config.json   # then edit; it is gitignored
   node server.js                       # http://127.0.0.1:8484
   ```

   To keep it running, wrap `node server.js` in a launchd LaunchAgent with `KeepAlive`.
   `config.json` changes need a restart. Don't start a second copy by hand while the agent
   is running: the port is taken.

## How it works

### Xcode products

Apparition's `apparition xcode register --app PRODUCT.app --project REPO --symbols PRODUCT.app.dSYM`
uses `bin/xcode-handoff` to retain an already-built product, its signature and exact matching
symbols in the build registry. It never rebuilds, re-signs, installs or attaches a debugger.
Unsigned products remain unsigned. A separate optional XcodeGen spec generated with
`apparition xcode configure --spec project.yml --target TARGET` adds pre/post build actions
to capture source context and register the result; the original spec is unchanged.
Without that context, source observed at registration is explicitly separate from compile
provenance. Stop Xcode debugging before choosing a separate Apparition JIT session.

### Service builds

Authenticated Apparition clients can register an existing Git checkout with
`POST /api/service/v1/projects` (`name`, Mac `path`, optional `branches`, and
`platforms: [{platform, device, bundleId}]`). The checkout must already have
`build-signing.conf`; registration never executes it. Duplicate names, duplicate
checkout paths and invalid targets/branches are refused. Registration atomically adds
the project to `config.json`, preserves other settings, and makes it available immediately.

- Builds are serialized per repo and parallel across repos. A branch other than the one checked
  out is built from a disposable copy-on-write clone, so your working tree is never touched.
- Dev builds stamp the git short SHA into `CFBundleVersion`. The UI compares the SHA on the
  device with the repo's HEAD to show up to date, out of date, dirty, or on another branch.
- Per-repo behaviour (pre-build hooks, extra build settings, platform aliases) belongs in that
  repo's `build-signing.conf`, never in `server.js`. See `CLAUDE.md` for the architecture.

## Branding

Optional `branding` in `config.json` (applied on reload, no restart needed):

- `org`: shows as "&lt;org&gt;'s App Store" in the header, login page and tab title.
- `title`: the full name instead, e.g. `"Acme Apps"`. Wins over `org`.
- `icon`: an emoji or any short text, or a path to an image (`.svg .png .jpg .gif .webp .ico`, relative to `config.json`, `~/` allowed). It is shown at 2rem and also becomes the favicon. Unset keeps the triangle.

## Live debugging

Dev builds of apps that link DebugTrace's `DebugTraceServer` start a debug server on every launch
and advertise it over Bonjour. The **Debug** view lists the ones running on your network, and for
each one shows a live tail of its log (filter by level or text, pause), every debug endpoint with
a form to run it, its `_info`, and a button that captures a trace and uploads it here. A device
chip in **Deploy** shows *live* while its app is running.

Discovery uses DebugTrace's `Tools/debugtrace-mcp discover`, so set `debugtraceTool` in
`config.json` to that file's path. Bonjour covers the local network only.

The page never sees an app's address or token: it names the app by its build's key id, and the
server looks up the address from discovery and the token from the key ledger.

**Panels** turns values an app reads through DebugTrace's `DebugTunables`
(`DebugTunables.number("hdr.peakNits", 600, range: 100...2000)`) into sliders, toggles and menus
that change the running app as they move. Agents can save their own panels for what is being
tested with the `apps` MCP server's `panel` tool: tunables, endpoint controls, buttons and live
readouts. **Copy values** gives the tuned numbers as text to paste back to an agent. On Apple Vision
Pro, tune from this page on a phone, or on the Mac Virtual Display: turn on **Settings ›
Developer › Allow Mac Virtual Display** to use it in immersive environments. That setting breaks
`.progressive` immersion styles, so turn it off to test those. Details are in DebugTrace's
`Docs/tuning.md`.

## JIT apps and devices that wake on contact

Both go through [Apparition](https://github.com/illixion/Apparition)'s command line
(`apparition`, found at `apparitionCli` in `config.json`, else `~/.local/bin/apparition`, else
PATH), which reaches a device over its own pairing and tunnel instead of Xcode's CoreDevice.
The device must be paired with Apparition on this Mac once: `apparition pair` for an Apple
TV or headset (with its Remote Devices / Remote App and Devices screen open), `apparition
pair --usb` for an iPhone or iPad.

**JIT.** Apps that generate code at run time (emulators, Wine, JIT-compiling VMs) need a
debugger attached to be allowed to. Tick **JIT** next to *Launch app* in the deploy menu
(`"jit": true` in a build request, `build-and-sign --jit` in a terminal). build-and-sign then
launches the app suspended with `apparition launch --jit`, which attaches, resumes it and
serves its JIT requests from a background process for as long as the app runs. It combines
with the other options:

| Deploy options | build-and-sign | What runs |
|---|---|---|
| Launch + JIT | `--no-mcp --jit` | the app, JIT served; events in `build/device-console.log` |
| Launch + MCP + JIT | `--run --mcp --jit` | also its console in that file and its debug server registered for agents |
| + Inject | `--inject` as well | patches go live while JIT is served |

The session's pid is in `build/device-console.pid`: `kill $(cat build/device-console.pid)`
detaches the debugger (SIGTERM is handled) and leaves the app running; the next launch stops
the previous session first. Stop any Xcode debugging session of the app before launching it
with JIT. By hand, `apparition launch <bundle ID> --device <name> --jit [--console]` does the
same, and `apparition jit <bundle ID> --attach` serves an app that is already running.

**Devices that wake on contact.** While Xcode's DeviceHub runs, any CoreDevice tunnel to an
Apple TV wakes it (see `CLAUDE.md`). Devices named in `config.json`'s `quietAppsDevices` are
read (`apparition apps`), installed to (`apparition push`, the IPA as signed) and launched
(`apparition launch`, console and JIT included) over Apparition's tunnel instead, so a deploy or
refresh leaves a sleeping one asleep. The store passes the list to build-and-sign as
`QUIET_DEVICES`; set that in `global.conf` for terminal runs. Re-signing in place and copying
files still use devicectl.

## Apparition desktop service

Apparition's Developer view uses the versioned native API alongside the existing web UI.
Set a password with `node server.js set-password`; native connections require one even if
the web UI is otherwise running in open mode. Use localhost on this Mac, or expose it
through the existing HTTPS/tailnet setup. Native clients refuse HTTP credentials to remote
hosts and do not follow redirects. Restart the service after updating the server code;
check that no build is active before restarting.

- `GET /api/service` advertises protocol `apparition-service`, version 1, and capabilities.
- `POST /api/service/connect {password}` returns a bearer session without a web cookie.
  Session tokens are hashed in the existing session store; password changes invalidate them.
- Authenticated operations use `/api/service/v1/`: `state`, `build`, `builds/<id>/log`,
  `builds/<id>/cancel`, `traces`, trace files, `crashlogs`, crash files, `debug/apps`, and
  an app's `help`/`call`, plus `artifacts` (recent registered builds) and
  `artifacts/<buildId>/manifest` and `/download`.
  Other methods/paths, report deletion and web authentication
  operations are excluded. Native bearer credentials are not accepted at the old API paths.
- `POST /api/service/v1/disconnect` revokes that session. The desktop app keeps it only in
  Rust memory and remembers only the service URL between launches.

A native build request is `{repo, device, targetPlatform, branch, run, mcp, jit, inject,
buildDebug, deploy}`. The last six fields are optional booleans; `jit` (launch with JIT
served by Apparition, see above) applies only with `run`. The project, platform,
device and branch (`current` or a configured branch) must match the configuration. The
platform is retained even when a single device has several platform aliases. All builds,
logs, cancellation, DebugTrace approvals and report verification use the existing backend.

### Re-signing an installed app with another profile

`GET /api/service/v1/resign/options` lists the `.mobileprovision` files in
`~/.config/build-signing/profiles` (`BUILD_SIGNING_DIR` overrides the root), without paths or
device lists. `POST /api/service/v1/resign` re-signs an app that is already on a device and
installs it over the old copy (capabilities `resign` and `resign-build`). It takes either of:

- `{buildId, device, profile, dataMode?, dryRun?}`: re-sign that kept IPA.
- `{repo, device, platform?, branch?, profile, dataMode?, dryRun?}`: no kept build is needed.
  The service queues a signed `--no-deploy` build of that project target first (the same build
  an OTA link or an Apparition download asks for), then a re-sign of the IPA it kept. The
  reply lists both jobs, the build first. A dry run builds nothing.

Either way, a profile for the same App ID updates the app in place. A different App ID is
refused by installd as an update, so the service backs up the app's Documents and Library
with `devicectl` (kept under `~/.local/state/apparition/resign-backups/`), removes the app,
installs the new signature and restores the data. If the new signature won't install, it puts
the previous one back with its data. `dataMode: "discard"` skips the backup. Keychain items
and app-group containers do not move. This needs the device reachable by `devicectl`; an OTA
install can't change an installed app's App ID.

The device list leaves out simulators: Xcode 27's `devicectl list devices` reports every
simulator too, and nothing here installs to one.

`test/service.test.js` exercises the real HTTP authentication/route boundary and target
validation. The frontend/controller and Rust transport tests live in Apparition.

Build jobs, trace metadata and retained symbols share a public `buildId` (the per-build
credential ID), independent of the Git SHA build number. Development source builds keep
immutable symbols under `~/.local/state/debugtrace/dsyms/<bundleId>@<buildId>/`, including
`--no-deploy` builds served to testers. UUIDs from the app and its embedded binaries select
the exact DWARF files; stale symbols from sibling targets are excluded. Each archived
DWARF file has a SHA-256 and byte count. Updating
an app does not rotate away its older symbols. Distribution and sign-only builds remain
excluded; no automatic symbol pruning is enabled yet.

### Immutable artifacts and source provenance

Every signing invocation now gets a public build ID, also used as its DebugTrace key ID
when a credential is embedded. Before installing, build-and-sign retains the exact signed
IPA or Mac app zip under `~/.local/state/apparition/builds/<buildId>/` with an
`apparition-build/1` manifest. Artifact and binary hashes, byte counts, UUIDs, original and
installed bundle IDs, and the retained symbol set are recorded together. The build report
prints the ID and manifest path. Signing-only inputs explicitly have unknown source
provenance rather than inheriting the current checkout's revision.

Source builds capture state before and after compilation: Git commits, dirty diffs,
untracked file contents, local Swift package and submodule dependencies, resolved-package
file hashes, toolchain versions and a digest of build arguments. Unversioned projects use
a directory fingerprint; an unborn Git repo is supported too. Cached build directories
are excluded. This records Git/directory and package state, not every possible input from
an arbitrary build hook or ignored generated file. The version stamp uses the captured
commit even if HEAD advances during compilation. A changed source fingerprint marks the
build inconsistent, and source-feed/Mac-download jobs refuse to publish it.

The service uses these retained artifacts instead of choosing a mutable build output
when a manifest is available. Historical builds without manifests remain readable without
inventing provenance. `APPARITION_BUILD_DIR` overrides the archive root; the service also
accepts `buildManifestDir` in config and passes it to its build jobs. Archives contain
signed apps and credentials and are private to this user. Relative `buildManifestDir`
paths resolve beside the service configuration file. Automatic pruning is not yet
enabled. Apparition desktop offers build details and verified downloads; it checks SHA-256
and size before replacing a chosen destination file.

## Debug traces

Apps that link DebugTrace can upload a trace zip to `POST /api/traces`. `build-and-sign` generates
a fresh Ed25519 key for each build and records its public half in
`~/.local/state/debugtrace/keys/`. The server only keeps traces whose signature verifies against
that ledger. Set `DEBUGTRACE_UPLOAD_URL` in `global.conf` to the server's URL.

## AltStore source (auto-update)

With `"altSource": { "enabled": true }` in `config.json` (restart after changing it), the server
publishes [AltStore-format sources](https://faq.altstore.io/developers/make-a-source) of the
configured apps, which AltStore, SideStore, or any client that reads the format can subscribe to.
The `/ota` page shows each source URL with "Add to AltStore / SideStore" links; `GET /api/altsource`
returns the same.

- **One source per platform**, generated on request: `/altsource/<platform>.json` for every platform
  name in `config.json` (`iOS`, `iPad`, `visionOS`, …; case doesn't matter), because AltStore
  can't filter a source by device. Each lists the apps that have a row for that platform, built
  with that row's settings, so an `iPad` or `visionOS` alias gets its own build. Add the one that
  matches the device.
- `/altsource/source.json` combines them, one entry per bundle id (the first row wins), for
  clients that filter on the per-app `platform` key themselves.

- Each app is listed at the head of its source branch (a repo's `altSource.branch`, else `main` or
  `master` when offered, else its first branch). `buildVersion` is that commit's short SHA, the
  same stamp build-and-sign writes into `CFBundleVersion`, so a client sees an update whenever the
  branch moves.
- Nothing is built ahead of time. Downloading a version builds exactly that commit, from a clean
  build clone (uncommitted edits in the checkout never ship), signs it without deploying, and
  answers with the IPA once the build finishes. Builds go through the same per-repo queue as the
  web UI's, and simultaneous downloads of one commit share one build.
- The wait is a long poll. A request is held for `holdSeconds` (default 50, under URLSession's
  60-second idle timeout), then redirected (307) back to itself to wait again; the build carries
  on whether or not anyone is waiting. A client that controls its own timeout can pass
  `?hold=<seconds>` (up to 3600) to wait in a single request. URLSession follows a limited number
  of redirects, so a very long build can outlast a stock client's patience; trying again picks up
  the running build or its finished IPA.
- AltStore refuses an IPA whose entitlements or privacy strings differ from the source's
  `appPermissions`, so those are read from the built IPA. Before a commit is built, the listing
  carries the previous build's values (and `1.0` for an app never built here).
- Each app also gets a non-standard `platform` key, so a client can pick the apps for its device.

Settings, all optional: `name`, `identifier`, `subtitle`, `iconURL`, `website`, `developerName`,
`branch`, `token`, `holdSeconds`, `linkTtlDays` (7). Per repo, `"altSource": false` leaves it out,
or `{ branch, platforms: ["iOS"], description, subtitle, iconURL, developerName, category }`.
`platforms` limits which of the repo's rows reach the sources. Within one source a bundle id is
listed once, so of two rows with the same platform and bundle id (two iPhones), the first wins.

## Mac apps (.app.zip downloads)

A repo can list Mac apps next to its device platforms. Each gets a row on its card with a
**⤓ .app.zip** button that downloads a signed `.app` of the selected branch, built on demand:

```json
"macApps": [
  { "name": "MyApp for Mac", "bundleId": "com.example.myapp.mac" },
  { "name": "MyApp Helper", "platform": "macOS-helper", "bundleId": "com.example.myapp.helper" }
]
```

- `platform` (default `macOS`) reaches build-and-sign as `PLATFORM`, like a device row's. A repo
  with several Mac apps gives each its own alias and maps it in its `build-signing.conf` to a
  scheme, target and bundle id (`macOS-helper` → `PLATFORM=macOS`, `SCHEME_NAME=MyAppHelper`), the
  same way an `iPad` row maps back to the iOS build. `branch` picks the default branch (else
  `main`/`master`, else the first offered); the card's branch menu picks another.
- A repo with only Mac apps sets `"platforms": []` and gets no Deploy button.
- A download builds the branch's committed head, never uncommitted edits, from a clean build
  clone in the repo's queue, signed with the development certificate (macOS needs no profile).
  The zip is kept per commit (the newest three per app), so the next download of the same commit
  starts at once. The build is refused if the app it produced has another bundle id than the
  row's, which catches a conf alias that fell through to the wrong scheme.
- `/mac/<slug>/<commit>.app.zip` serves or builds one commit, long-polling like the AltSource IPA
  (`?hold=` up to 3600 s); `/mac/<slug>/latest.app.zip?branch=` redirects to the branch head's.
  Both need the session or a signed link. The **curl** button copies a command with a link signed
  for a week, for a Mac that isn't signed in.
- Downloaded from a browser, the app is quarantined and Gatekeeper blocks its first open, since it
  is not notarized: allow it in System Settings › Privacy & Security › Open Anyway, or run
  `xattr -dr com.apple.quarantine` on it. curl doesn't quarantine what it downloads.

## Tests

```sh
node --test 'test/*.test.js'
```

Node's built-in runner, no dependencies. The suite covers trace verification (signatures,
the key ledger, malformed and hostile zips), config handling, the freshness logic in
`public/freshness.js`, and `bin/build-and-sign`'s syntax and argument handling. A real
build-and-sign run needs a device and certificates, so it isn't automated.

## Security

Set a password before anyone else can reach the page:

```sh
node server.js set-password     # asks twice, stores an scrypt hash in config.json
```

With a password set, the page, the API and `/ota` need a sign-in. The session cookie is
`HttpOnly` and `SameSite=Strict`, sessions last 30 idle days and end when the password changes,
and five wrong passwords lock sign-in for a minute. Requests from another site are refused.
These work without a session, each with its own check:

- `POST /api/traces`, which apps use to upload traces. Only traces signed by a key in this
  Mac's ledger are kept.
- OTA manifests and IPAs, which the iOS installer fetches without the page's cookie. Their links
  carry an HMAC signature that expires after an hour, and only the signed-in page hands them out.
- The AltStore source, when enabled. Its URL carries a lasting token (`altSource.token`, else one
  derived from `data/secret`; delete that file to rotate it, which also signs every OTA link out),
  and the download and icon links inside it carry their own signatures. Anyone holding the source
  URL can start builds of the listed commits.
- Mac app downloads, through a link the signed-in page hands out (the curl button). The
  signature covers the app and branch and lasts a week; anyone holding it can start builds of
  that branch's head.

Without a password, everything is open to whoever can reach the port, and the page says so.
The server listens on `127.0.0.1`; the intended way to reach it from devices is `tailscale serve`
over a private tailnet. Don't expose it to the internet, password or not.

- The trace key ships inside each IPA, so a valid signature proves "a build this Mac made",
  not "a genuine device".
- Installation still requires each device's UDID to be in the app's provisioning profile.

## License

MIT, see `LICENSE`.

### Apparition entry patching

Build requests accept `injectBackend: "dynamic" | "apparition"` alongside `inject`.
The default remains dynamic replacement. `build-and-sign --inject-backend apparition`
uses separate `DerivedData-inject-apparition` products and DebugTrace's explicit native
entry-boundary backend. See `DebugTrace/Docs/live-patching.md` for required declarations
and unsupported edits; this does not make existing Swift object layouts mutable.

The authenticated native service advertises `entry-patching` and accepts
`POST /api/service/v1/inject` with `{repo, device, targetPlatform, files?: string[]}`.
Only configured targets can be patched, with one request per app/device in progress.
It invokes the current Apparition CLI on this Mac, using `apparitionCli` from config
(or `apparition` on PATH) and `debugtraceTool` for compilation/discovery. Install an
Apparition CLI with `inject` support. The CLI verifies the live session, attaches only
for the native entry backend, signs patches, requests device approval and detaches.
No build or installation occurs. Native and desktop Apparition expose this as
**Patch running app**, with the usual device code-change approval.

Apparition backend builds now save optimizer remarks and collect them into
`build/inject/optimization-records/` before the final provenance snapshot. Query
observed inline/specialization dependencies with DebugTrace's `debugtrace-inject
callers --dir build/inject --file Sources/Model.swift --line 42 --transitive`.
These are incomplete observations, including retained incremental records; they
do not authorize patching arbitrary optimized functions. Existing boundaries and
ABI restrictions remain in force.
