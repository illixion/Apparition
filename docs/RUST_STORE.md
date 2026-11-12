# The Store in Rust (architecture)

How the Apparition Store is built. What it does, and how to use it, is in
**[STORE.md](STORE.md)**. The Store began as a Node.js server with a bash build script, and was
rewritten in Rust so that the desktop app serves it from its own process, with no Node, no bash
and no `devicectl` on the Mac. The Node server is gone; this page is the map of what replaced it.

## Constraint that does not move

`xcodebuild` exists only on a Mac with Xcode. The Store runs it as a child process. What went away
is Node, Homebrew bash, `devicectl`, `codesign` and the login keychain, not Xcode. Building without
a Mac (xtool, another Mac) goes through `apparition deploy`, which sends the finished IPA to the Store.

## Crate layout

Three crates carry it, plus the desktop app that hosts it.

- **`service/` (`apparition-service`)**: the Store, as a library, so it tests without Tauri. It
  knows nothing of devices or of the signer. The host supplies them.
  - `server.rs` and `server/`: the HTTP front (axum). One dispatch order: cross-site check, the
    `/api/service` family, the hosted-app routes, the web gate (off unless the pages are on, with
    the machine routes exempt), login, static files, signed links, then the session-gated API.
    `server/delivery.rs` has OTA, the AltStore feed and the Mac zips with their long polls,
    `server/routes.rs` the service API's operations that change things (build, inject, report an
    install, add a project, re-sign).
  - `config.rs`: `config.json`, read as written, unknown keys kept.
  - `auth.rs`: scrypt password hash, `sessions.json` (SHA-256 of tokens), `data/secret`, signed
    path links and the lasting feed token.
  - `state.rs` and `install_mode.rs`: `data/state.json` (the install ledger, device app
    inventories, OTA copies), the `/api/state` bundle, and which device's Apparition is open.
  - `queue.rs`: the build queue (below). `clones.rs`: build clones. `updates.rs`: which installed
    apps are behind their sources. `resign.rs`: re-signing. `projects.rs`: project discovery and
    registration, and the install a device reports for itself.
  - `artifacts.rs`: kept builds, read and verified against their manifests. `deploy.rs`: a build
    that arrives from another computer. `devices.rs`: the `Devices` trait.
  - `traces.rs`, `crashlogs.rs`, `debug.rs`, `mcp.rs`: trace verification and storage, crash logs
    (read side), the apps' debug servers (own Bonjour browse, the proxy, panels) and the MCP server
    at `/mcp`. `reports.rs`: opening encrypted problem reports (`.trace`), DebugTrace's one HPKE
    suite over `x25519-dalek` and ring, checked against RFC 9180's vectors and a CryptoKit-sealed
    fixture; `traces::receive_report` is their intake (STORE.md, "Problem reports from public copies"). `altsource.rs`, `macapps.rs`: the two feeds. `assets.rs`: the web pages, compiled in
    from `service/public`.
- **`installer/` (the `apparition` command line)**: `builder/` is `apparition build` (below),
  `developer.rs` is `apparition mac`, `deploy`, `mcp` and `updates` (a client of the Store over the
  companion channel or the app's loopback), `crash_reports.rs` reads the Store's data folder
  directly, and `inject.rs` runs live patches.
- **`core/` (`apparition-core`)**: pairing and tunnels, installing, launching with console and JIT,
  app data containers, uninstall, the signer, and the companion channel the other computers come
  through. The Store reaches all of it through the `Devices` trait and the command line.
- **`desktop/`**: hosts the Store. `host.rs` and `developer.rs` start it in-process on a tokio task,
  bound to `127.0.0.1` on `config.json`'s `port`, with a token made at launch and held in memory,
  and write `developer.json` for agents on the same computer. `store_devices.rs` implements
  `Devices` over the core's pairings. The companion channel forwards `/developer/*` from paired
  computers to the Store with the app's token added. Stopping the app drops the task: the Store
  never outlives it.

## `apparition build`

`installer/src/builder/` is the builder, a single command with the flags of the script it replaced.

- `conf.rs` reads `~/.config/build-signing/global.conf` and the repo's conf with the system's
  `/bin/bash` (they are bash, and some branch on `$PLATFORM`), and takes back the variables.
- `mod.rs` runs the build: lock, optional pre-build hook, `xcodebuild`, `bundle.rs` (bundle IDs, build
  number, the DebugTrace credential and key-ledger record), signing through the core, `keep.rs` (the
  signed artifact, its manifest and the dSYMs), `provenance.rs` (what the build was made from,
  before and after) and `launch.rs` (install, `--run`, `--log`, `--mcp`, `--jit`).
- It runs as its own process, not a thread of the app, so a repo's `PRE_BUILD_HOOK` never shares
  memory with the app's keys. `--plan` prints what a conf resolves to, which the queue uses to
  share one build between platform rows that resolve alike.

## The queue and the install ledger

Every build, whichever feature asked for it, is a job in `queue.rs`. It owns the job list
(`data/queue.json`), one log per job, and these rules:

- one build per resolved platform build, installed on each of its devices, never re-signed;
- `maxConcurrentBuilds` at once, one per checkout or build clone;
- a deploy of the checked-out branch builds in the checkout, anything else in a clone at an exact
  commit (`clones.rs`: kept per repo and branch, laid out like `~/Projects`, committed state only,
  incremental, cleared after a week unused);
- installs go one device at a time after the build, and a device that can't be reached keeps the
  kept build for its own Apparition.

What a device runs is the install ledger in `state.json`, which the queue, the device's own reports
and re-signs write to, and the device's app list refines. `updates.rs` joins that with each kept
build's recorded sources to say what is behind and why. `autoUpdate` is a once-a-minute tick in
`server.rs` over the same functions.

## Delivery

AltStore downloads and Mac zips are queue jobs pinned to a commit, delivered to `altsource.rs` and
`macapps.rs` when they finish through the queue's `Deliver` trait. Their endpoints hold the request
(a long poll, a 307 back to itself) and write nothing until the build is done. OTA copies are made
for every IPA build. Signed links (`auth.rs`) let installd and clients without cookies fetch
exactly one path.

## Formats read as they are

Nothing needed migrating from the Node Store. `config.json`, `state.json`, `sessions.json`,
`secret`, `data/traces/<id>/`, the build manifest store and the key ledger keep their formats, and
some tests run against recorded fixtures of what the Node Store wrote
(`service/tests/fixtures/`). Queue job IDs start at 1000000, clear of the IDs it used.

## Deliberately not included

- **`--distribution` builds.** `apparition build` refuses the flag.
- **`EXTRA_ENTITLEMENTS_PLIST` and `ASSOCIATED_DOMAINS`**, two conf keys the script supported and no
  project of the author's sets.
- **A packaged server edition.** `apparition store serve` runs the Store without the app, but no
  launchd or systemd unit ships with it.
- **The `apparition xcode` handoff**, which registered an existing Xcode product in the build store
  or generated a scheme that did. It depended on the Node tools and has not been ported.
- **Crash-log pulling.** It was switched off before the rewrite (it woke sleeping Apple TVs) and
  was not ported; crash logs already stored are still read.
- Automatic pruning of kept builds and symbols.

## Risks that remain by design

- Signing keys and build execution share a machine, so repo-supplied commands (`PRE_BUILD_HOOK`)
  run as child processes of `apparition build`, never inside the app.
- Installs, launches, JIT and re-signs are tested against fake devices; only a real device proves
  them, so changes there need a run on hardware.
