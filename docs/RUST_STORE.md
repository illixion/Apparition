# Apparition Store in Rust (scope)

Goal: the desktop app serves what `store/` serves today, from Rust in its own process, with no
Node.js and no bash on the Mac. Node bundling is explicitly out of scope: the Node server stays
only as the fallback until the Rust service passes the parity checklist, then goes.

## Decisions (2026-11-04)

- **Mac only.** The feature is hidden on Windows and Linux; the `service` crate compiles to
  nothing off macOS and the desktop app leaves the Developer build tab out of those builds.
- **`~/Projects/appstore` is archived and superseded** by this service once it reaches parity.
  The Node `store/` stays only as the fallback until phase 6, then goes.
- **Signing moves to the core's identity store.** A Mac that builds with a login-keychain
  certificate imports it once (`import_identity`); that user-visible step is accepted.

## Status (2026-11-04)

Every ported route has a test against the real Node store (or, for the MCP server, the Python
`debugtrace-mcp` it replaces) so the two cannot drift unnoticed. 49 tests,
`APPARITION_REQUIRE_NODE=1 cargo test -p apparition-service` (without the variable, the oracle
tests say SKIPPED when Node doesn't run).

**Running now (macOS):** the desktop app runs this service in-process on loopback beside the Node
build service, and offers it to computers paired for developer access over the companion channel.
`apparition mcp` and `apparition deploy` drive it from Windows, Linux or another Mac:
`docs/REMOTE_DEVELOPER.md`. Builds, device installs and the web pages still come from Node.

Done (phase 0 and 1, plus the first-party DebugTrace half of the goal):
- Auth, byte-compatible with `lib/auth.js`: password hash, `sessions.json`, `data/secret`, signed links.
- Web gate, login, the hosted-app contract (`/host*`, one-time `/host-login`), config writes that
  keep unknown keys, the web UI compiled in (`assets.rs`, test fails if `store/public` drifts).
- `/api/state`, `state.json`, git info, presence, install-mode resolution.
- Traces: strict upload verifier (19-archive parity), list, files, zip, delete; links crash logs.
- Kept builds (list, manifest, streamed download); crash logs (read side, shared index).
- Debug: own Bonjour browse of `_debugtrace._tcp`, ledger recognition by HMAC tag (checked against
  Python), launch records and dead-console recovery, the proxy for help/call/logs (SSE)/panels.
  The Python tool could not make LAN connections; this runs where the Local Network permission is.
- MCP server at `POST /mcp` (bearer, same as the service API): `apps`, `help`, `call`, `trace`,
  `panel`, with the Python tool's schemas, forgiving arguments, reply spilling and panel validation. `inject` runs the
  external `debugtrace-inject` binary (`DEBUGTRACE_INJECT_TOOL`, else the DebugTrace checkout, else
  the PATH) from the app's project root, one patch at a time per build (`transaction.lock`), so the
  sources must be on the Mac.
- Deploys from other computers: `POST /api/service/v1/deploy` keeps an IPA as a build the device's
  Apparition installs (Developer › Sent from your computers). The stdio entry is `apparition mcp`,
  a bridge to the running app's `/mcp` (loopback on the Mac, the companion channel elsewhere), so
  the Local Network permission stays with the app.
- Hardened before going remote (2026-11-04 review): no token for a copyable advertisement,
  app paths kept on the app's origin, endpoint and bundle names checked before they name files,
  capped replies and trace inflation, DNS rebinding refused on a password-less store.

Not done:
- Phase 2 delivery (OTA, AltSource feed, Mac zips, signed links, trace upload from the web page).
- Phase 3 devices through the core (replaces devicectl): `devices`, `deviceApps`, installs, JIT.
- Phase 4 re-sign. Phase 5 builds (`build-and-sign`), queue, `macApps`. Phase 6 desktop wiring,
  hiding the feature off macOS, retiring `store/` and the bundled Node host.
- Unported routes answer 501 and the capabilities list stays empty until a feature works.

## Constraint that does not move

`xcodebuild` exists only on a Mac with Xcode. The Rust service runs it as a child process, the
way `bin/build-and-sign` does. What goes away is Node, bash, `devicectl`, `codesign` and the login
keychain dance, not Xcode. Building without a Mac (xtool, a remote Mac) is a later, separate
builder behind the same job interface.

## What exists in `store/` (6.5k lines)

| Piece | Lines | Rust home |
|---|---|---|
| `server.js` HTTP routing, config, state, queues, OTA, traces, Mac builds | 2230 | `service/` crate |
| `lib/altsource.js` AltStore feed | 332 | `service/src/altsource.rs` |
| `lib/resign.js`, `artifacts.js`, `installs.js`, `install-mode.js` | ~560 | `service/` + core (`ipa`, `identity`) |
| `lib/auth.js`, `service.js`, `set-password.js` | ~365 | `service/src/auth.rs`, `host.rs` |
| `lib/crashlogs.js` (pull is off since 2026-10-31), `debug.js` | ~400 | `service/` |
| `lib/project*.js`, `xcode.js`, `macapps.js` | ~590 | `service/` |
| `bin/build-and-sign` | 1820 | `service/src/build/` plus core |
| `bin/crash-reports`, `build-manifest`, `xcode-handoff` | ~180 | CLI subcommands of the same crate |

## What the core already covers

`core/src/api.rs` has install (`install`, `install_signed_ipa`), launch with console, JIT serving,
container pull/push/migrate, uninstall, running-pid, app-ID and certificate listing, offline
signing (`sign_offline`, `import_identity`, `import_profile`) and Apple-ID signing
(`sign_with_apple_id`). Those replace `devicectl` and the signing half of `build-and-sign`. Gaps
the port has to fill:

1. **Discovering devices without `devicectl`**: usbmuxd plus the paired-device store
   (`DeviceStore`), including Wi-Fi-only headsets and the quiet-device rule (never poke a sleeping
   Apple TV).
2. **Signing an `xcodebuild` product** with the account's identity and a profile per target,
   wildcard-to-concrete App ID rewriting, entitlement merging, associated-domains substitution,
   embedded trace credential and debug-server plist keys (all in `build-and-sign` today).
3. **`--inject`**: the DebugTrace inject tool is a separate Swift binary. It stays an external
   child process; Rust only passes the same inputs.
4. **Build clones**: `cp -c` clone per branch with `cloneExclude`, locks per checkout, reaper.
   `libc::clonefile` replaces the `cp -c` call.

## Contract to preserve (the parity checklist)

The UI (`store/public`) and every client keep working unchanged, so the route surface is the spec:

- Browser/web: `/`, `/login`, `/api/login|logout|auth|state|build|builds/:id/{log,cancel}`,
  `/api/refresh-devices`, `/api/resign*`, `/api/artifacts*`, `/api/debug/*`, `/api/crashlogs*`,
  `/api/traces*`, `/api/altsource`, `/api/ota/*`, `/api/mac/*`.
- Machine routes that bypass the web gate: `POST /api/traces` (signed upload, key ledger in
  `~/.local/state/debugtrace/keys/`), `/ota/*`, `/altsource/*`, `/mac/*` with `?t=<expiry>.<hmac>`
  signed links.
- Native clients: `/api/service/v1/*` bearer API (`capabilities`, the 19 routes in `lib/service.js`)
  and the hosted contract: 43-character token on stdin, `GET /host`, `POST /host/web-ui`,
  `POST /host/web-link` and the one-time `/host-login` code.
- On-disk formats, read as they are so nobody migrates: `config.json` (including the
  `passwordHash` scrypt string), `data/state.json`, `data/sessions.json` (SHA-256 of tokens),
  the build manifest store, the OTA directory, `data/traces/<id>/`, `service.pid`.
- Behaviours the tests pin: queue per repo, cancel, re-sign data-move path, install modes,
  presence, ledger-only devices.

## Architecture

- New workspace crate `service/` (`apparition-service`): a library, so it is testable without
  Tauri, plus a thin `bin` for headless use. `desktop` depends on it and starts it in-process on a
  tokio task, replacing `host.rs`'s child-process supervision and the stdin token (an in-process
  token needs no pipe). The standalone/headless mode keeps the password-based gate.
- `axum` 0.8, already in the lock file through the core's anisette server. Static UI embedded
  with `include_dir`-style assets from `store/public` (single copy, shared with the Node fallback
  until it is removed).
- Auth: same scrypt parameters (N=2^15, r=8, p=1), same cookie, same HMAC path signing, so links
  and sessions survive the switch.
- Jobs: a `Job` trait (`Build`, `Resign`, `AltSourceBuild`, `MacBuild`) with the existing
  per-repo queue, live log buffer, cancel by process group of the child `xcodebuild` only.

## Phases (each ends with the Node fallback still green)

0. **Scope and skeleton** (this commit): crate, config model, capabilities route, tests that load
   the real `config.example.json`.
1. **Read-only service**: config/state/sessions/auth, static UI, `/api/state`, traces (verify and
   list), crash logs, artifacts list, `/api/service/v1/*` reads, hosted contract. Run beside the
   Node service on another port and diff `GET` responses.
2. **Delivery**: OTA manifest and IPA, AltSource feed, Mac zip, signed links, trace upload.
3. **Devices**: device discovery and installed-app inventory through the core; install, launch,
   uninstall, JIT, container moves; `/api/refresh-devices`.
4. **Re-sign**: `resign` and `resign-build` on `core::ipa` and `identity`.
5. **Build**: clone, `xcodebuild`, sign, install; then `--run`, `--log`, `--mcp`, `--inject`, `--jit`.
   Port `build-and-sign` behaviour by behaviour with a fixture app per flag.
6. **Cutover**: desktop uses the in-process service by default, Node host removed with
   `bundle-service.sh`, docs and `README.md` updated, `store/` kept as the self-host option or
   retired by decision.

## Risks and open questions

- Phase 5 is most of the work and the part only a real device verifies. Parity there needs
  on-device runs, not just unit tests.
- `~/Projects/appstore` has its own remote and a standalone install base (launchd, systemd).
  Decision needed: keep `store/` as the server edition, or retire it after cutover.
- Signing identity source: `build-signing.conf` plus the login keychain today; the Rust path
  uses the core's identity store. A Mac that still builds with a keychain certificate needs an
  import step (`import_identity`), which is a user-visible change.
- In-process means the build runner shares an address space with the signing keys. Repo-supplied
  commands (`PRE_BUILD_HOOK`) must run as child processes with a scrubbed environment; check what
  `build-and-sign` does today before deciding the rule.
