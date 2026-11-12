# Plan: DebugTrace in the public Apparition

Status: agreed 2026-11-01. Done: release dSYMs; in DebugTrace (`2f1fef8`), capture categories,
the per-capture privacy override, staged capture (`stage` → `remove` → `seal`), HPKE `.trace`
encryption and `debugtrace-decrypt`. Apparition is the first public
app to ship DebugTrace's debug server and trace reports to everyone. Because it doesn't ship on
the App Store, the server can be offered to anyone. A person with a free Apple ID can then help
develop it from their own Mac. The DebugTrace parts are generic, so later public apps reuse them.

## What exists today

- The GitHub Actions IPAs link `DebugTrace`, `DebugTraceServer` and `DebugTraceUI`. The server starts
  only with an `apparition build` credential or `DEBUGTRACE_SERVER=1`, so in a shipped IPA it never
  listens. `APPARITION_RELEASE` drops only the experiment endpoints and the release-site override.
- Upload goes to the Apparition Store, which accepts only keys in this Mac's ledger. Public copies have
  no upload URL, so `canUpload` is false and MetricKit reports are never sent.
- A copy with an upload URL (build-and-sign, `apparition build`) shows DebugTraceUI's "Debug Trace…"
  under Settings > Troubleshooting, and sends the trace to its Store. Public copies don't show it;
  Report a Problem (below) is what they get.
- A copy that Apparition signs on the device embeds a development profile, so
  `DebugPrivacyMode.detected` is `development`. Today, shipped copies log without redaction.

## 1. Report a Problem: encrypted traces on a public issue

- Settings gets "Report a Problem…". It captures a trace, previews its contents (`DebugTraceView`),
  and offers two ways to send it:
  - **Encrypted (default):** HPKE (CryptoKit) to a maintainer public key built into the app. The
    `.trace` file can go on a public GitHub issue, and only the maintainer can open it. The sheet
    says so.
  - **Plain zip:** shared by the person themselves.
- After sending, it opens `issues/new?template=bug_report.yml` with the version, platform and OS
  filled in. The form fields need `id:`s for that. The bug and App support templates get a
  "Trace" field.
- **Categories** are picked as checkboxes: logs, breadcrumbs, app state, and crash reports.
  Crash reports are **off by default**: MetricKit's exception reason can carry strings.
- **Privacy:** report traces are always redacted (`.release`), whatever the server's privacy
  setting.
- **Preview every file before sending**, as Apple's Feedback Assistant does. The person sees exactly
  the bytes that will be zipped and encrypted. This is built in DebugTraceUI, so every DebugTrace
  app gets it. Today's `DebugTraceView` lists files flat and shows only the last 200 KB of the
  text ones. It gains:
  - **Folders** (`crashes/`, `attachments/`), one level per screen. Each file shows its size and
    one line saying what it is ("Apparition's own log, this run").
  - **The whole file** in a lazily drawn line list, with search (`.searchable`, matches
    highlighted). Logs open at their end. JSON is shown pretty-printed.
  - **Crash reports** as a readable summary (kind, time, build, top frames), with the raw JSON
    one tap away.
  - **Other types** (images, attachments) in Quick Look on iOS and visionOS. tvOS has no Quick
    Look and no share sheet, so it shows text only.
  - **Remove** on any file (swipe or context menu), next to the category checkboxes. The
    manifest is rebuilt, and re-signed where a credential exists, without it. Encryption happens
    only after the person confirms the set they saw.

  **The model is Mountgomery's mobile `BrowserView`:** a `NavigationStack` with one folder per
  screen, tap to open (`.quickLookPreview`), and a context menu for Share. Only the pattern is
  borrowed, not the code:
  - Mountgomery's browser sits on ArchiveCore (decompressing on demand, extraction, passwords).
  - It needs iOS and visionOS 27 and has no tvOS. Apparition needs 26 and tvOS, and DebugTrace's
    floor is iOS 17.
  - A trace's files are already in memory before they are zipped, so there is no archive to read.
  - DebugTraceUI can't link Mountgomery's packages, and a general file browser doesn't belong in
    DebugTrace.
- **Nothing is sent on its own.** Crash reports wait on the device and go into a report only
  when the person includes them.
- **Done:** the audit of what a report can carry (snapshot providers, log values, attachments), and
  the seeded-value leak test (`ApparitionTests`, run by `scripts/test-report-privacy.sh`). No
  Apparition endpoint is `releaseSafe`, so a report's `snapshot.json` holds `_info` and `_features`
  only. One leak was found and fixed: error text logged as public could name a companion or a
  server by host name, a device by its name or identifier, and IPv6 and hardware addresses
  (`LogScrub`). The rules are in CONTRIBUTING.md, "What a problem report can carry".
- **The maintainer's side:** `debugtrace-decrypt <file>`, or dropping the `.trace` file onto the
  Apparition Store page. It unpacks the trace into `data/traces/` like any other, so `crash-reports` and
  symbolication work.
- One recipient for now, since no other maintainers are planned. A second key can be added
  later.

## 2. Debug access on the device

Settings → Developer → "Allow debugging from a computer", off by default. Asked each time it is
turned on: **run for an hour** (ends by the clock, survives suspension, not restarted after a
relaunch past the deadline) or **forever** (restarted at every launch). Settings shows the time
left and a Stop button. The Local Network prompt text stays as it is. Every MCP command is
available.

### Pairing with a six-digit code, encrypted transport (the default)

Pairing reuses RAVECompanion, the portfolio's code-sharing rule: Longwave's numeric-comparison
exchange and TLS 1.2 PSK channels with identify proofs.

1. The Mac's helper (below) finds the device and starts the exchange. Both screens show the same
   six digits. The person taps Allow on the device if they match.
2. The device is the side that approves and mints, the "host" role in RAVECompanion's terms. It
   mints a token for that Mac alone and seals it to the Mac under the exchange key.
3. Every later connection is TLS-PSK under that token, with the identify proof. On a LAN, the
   requests, the replies and the credential are all encrypted.
4. **Approval:** pairing approves the client, tied to its own credential, not to the name it
   sends. A paired Mac is not asked "Allow debug access?" again. The separate prompts for each
   code change and screen capture stay.
5. Settings lists paired computers, each with Remove. Removing one revokes only that one.

RAVECompanion gets its own namespace for this (Bonjour stems, HKDF labels, port), so it never
answers Longwave. Its service types go into `NSBonjourServices`.

### Password (advanced)

- A random password that persists, generated rather than chosen. It lives in the Keychain as
  this-device-only.
- It is used as the bearer token on the plain-HTTP listener, shown with Copy and a QR code, and
  has a "New password" button. A new password also clears every remembered approval.
- The UI and docs stress using it **only over Tailscale**. On a LAN, plain HTTP lets anyone
  capture the password together with a client's name.
- `_debugtrace._tcp` goes into `NSBonjourServices`. This also fixes dev builds never
  advertising.

### Privacy setting

- Server settings carry a switch: "Show private values to connected computers". It is off by
  default, because agents send what they read to cloud models.
- It sets DebugTrace's privacy mode, which applies to lines logged from then on. Earlier lines
  stay as they were stored.
- `.sensitive` values are never shown in either mode. Report traces stay redacted regardless.

### Token precedence

Launch env `DEBUGTRACE_TOKEN` > `apparition build` credential > Apparition's paired or password access.
Dev builds behave exactly as now.

## 3. Where the code goes

DebugTrace can't import RAVESDK, because RAVESDK depends on it. So:

- **DebugTrace (generic):**
  - start and stop explicitly (a public `start(configuration:)` that sets `shared`)
  - a transport seam: app-supplied `NWParameters` for an extra listener, plus a per-connection
    authenticator that names the client
  - a per-capture privacy override and capture categories
  - staging a capture: build the file set, let the person preview it and remove files, then seal
    the manifest and zip it
  - the preview browser in DebugTraceUI (above)
  - HPKE trace encryption plus the decrypt tool
  - approvals tied to a client credential rather than the client's name
  - documenting that privacy mode can change at runtime
  - `server-and-security.md` gains a section on public builds
- **RAVESDK, a new target** linking `DebugTraceServer` and `RAVECompanion`:
  - device side: pairing UI hooks, keyring, PSK listener through the seam
  - a macOS **helper executable**, because `debugtrace-mcp` is dependency-free Python and can't do
    X25519, ChaChaPoly or TLS-PSK. It pairs, then proxies `127.0.0.1:<port>` → TLS-PSK to the
    device.
  - `debugtrace-mcp` starts the helper and lists paired apps from its records (mode 600, under
    `~/.local/state/debugtrace/`).
  - Any RAVE app can adopt the target.
- **Apparition:** Settings (Developer and Report a Problem), the run-for prompt, the timer, the
  privacy switch, the Bonjour types, the issue templates, and a contributor section in
  `docs/DEVELOPER.md`.
- **Later:** Apparition's own nearby-signing pairing does its own numeric comparison in the core.
  By the code-sharing rule it should move onto RAVECompanion too.

## 4. Release symbols (done)

- `scripts/package-release.sh` writes `build/dsyms/<version>/Apparition-<version>-<platform>-dSYMs.zip`
  and refuses a dSYM whose UUIDs differ from the app binary's.
- The workflow attests the dSYMs and uploads them as `dsyms-<version>`.
- `/release` puts them on the GitHub release, never on the site: `publish.py` refuses other names.
- `core/Cargo.toml` builds with line tables, so Rust frames get file and line too.
- The shipped binary's debug map is stripped (`strip -S`) once the dSYM exists. That is 3.5 MB of
  stabs, about 670 KB compressed, and the UUID is checked again after stripping.
- **Still to do:** import a release's dSYMs into the local archive
  (`~/.local/state/debugtrace/dsyms/`, keyed by UUID; public builds have no build id), so
  `crash-reports` symbolicates public crash reports.

## Order

1. DebugTrace: start and stop, categories and privacy override (done), staged capture (done) with
   the preview browser, HPKE plus decrypt (done), approvals tied to a credential, the transport
   seam (`swift test`).
2. Apparition Phase 1: Report a Problem. It can ship alone.
3. The RAVESDK target, the helper, and `debugtrace-mcp` support.
4. Apparition Phase 2: pairing, the password, the timer, privacy.
5. Docs.

## Must be checked on a device

A simulator build proves none of these:

- pairing and the approval window on visionOS, iOS and tvOS
- TLS-PSK on Wi-Fi and over Tailscale
- Bonjour advertising from an Apparition-signed GitHub Actions build
- the hour timer across suspension
- the share sheet and issue link
- decrypting a real report
