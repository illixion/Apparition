# Contributing

Thanks for wanting to help. Bug reports, app compatibility reports and pull requests are all welcome.

## Reporting a problem

Use the [issue templates](https://github.com/illixion/Apparition/issues/new/choose). There are separate forms for
the **Apparition app** on a Vision Pro, iPhone, iPad or Apple TV, for the **desktop app and the command line**, and
for an **app that won't install or run** (check [docs/SUPPORTED_APPS.md](docs/SUPPORTED_APPS.md) first). Security
problems go through [SECURITY.md](SECURITY.md), not an issue.

**In the Apparition app,** open **Settings > Report a Problem…**. It makes a report you can attach to the issue: you
read every file in it first and choose what stays, and it is encrypted so only the maintainer can open it, even on
a public issue. It is a `.trace.zip`; attach it as it is, to the **Trace** box of the form it opens with the version,
device and OS filled in.

**In the desktop app,** open **Settings > Troubleshooting > Report a problem…**. It opens the desktop form with the
version and your computer's OS filled in; the app itself sends and attaches nothing. The desktop app and the command
line don't make an encrypted report yet, so an issue there is public: describe the problem and paste the error text,
and don't attach log files unless you're asked for a specific one. Logs, the sign-in record
([how](docs/ANISETTE.md#seeing-what-a-sign-in-sent-to-apple-and-what-came-back)) and screenshots can show your Apple
ID, device names, team or app IDs and network addresses, so read and cover those first.

## Building

The Apple app builds against two sibling packages by path (`project.yml`), so clone
[DebugTrace](https://github.com/illixion/DebugTrace) and [RAVESDK](https://github.com/illixion/RAVESDK) next to this
repo as `../DebugTrace` and `../RAVESDK`. The desktop app and the command line need only this repo and Rust.

```sh
scripts/build-core.sh          # the Rust core for visionOS and iOS
xcodegen                       # generates Apparition.xcodeproj from project.yml
cp build-signing.conf.example build-signing.conf   # then set your device names
apparition build               # or open Apparition.xcodeproj and Run
```

The desktop app and the `apparition` command line are one Cargo workspace:

```sh
cargo test -p apparition-core --features desktop,anisette --lib
cd desktop && cargo tauri build --bundles app
```

[docs/DESKTOP_APP.md](docs/DESKTOP_APP.md#building-it) has the other platforms. A few of the core's socket tests can
fail when the whole suite runs in parallel; run them alone (`-- <test name>`) before treating one as a regression.

## What a problem report can carry

**Settings > Report a Problem…** captures a debug trace the way a release build would and encrypts it to the
maintainer. What is in it may end up on a public issue, so these rules hold, and `scripts/test-report-privacy.sh`
checks them with seeded values (the `ApparitionTests` target, hosted by the app on the visionOS simulator):

- **App state.** Only debug endpoints marked `releaseSafe` go in. Of Apparition's, only `debugAccess` is
  (whether debugging from a computer is on, its ports, how many computers are paired: no names, addresses or
  secrets); every other returns something of the person's (team, device name and identifier, anisette server,
  companion address, the apps they installed). A report names those under `withheldInRelease` and holds
  DebugTrace's own `_info` and `_features`. Mark an endpoint `releaseSafe` only when all it returns is counts,
  modes and versions.
- **Logs.** A value is in the report only when its call site marks it `.public`. Error text from Apple, installd
  and the core is logged as public after `LogScrub.text`, which takes out emails, team IDs, URLs, addresses (IPv4,
  IPv6, hardware), a named host with its port, `.local` and tailnet names, device identifiers, long runs of hex
  (a certificate's serial number), and the names this copy knows: team and certificate names, the paired device's
  name, the companion's name and host, the anisette server's host. The name of the app being installed stays,
  because a report about an app is no use without it.
- **Never in a report:** the Apple ID's password, the pairing record, signing keys, the verbose sign-in record and
  the core's log file. Apparition registers no trace attachments. A report carries only those registered
  `releaseSafe`, so one that is added needs the same care.
- **`info.json`** says the device's model, OS version, locale and time zone.

A new kind of personal value gets a seed in `ApparitionTests/ReportPrivacyTests.swift`.

## Debugging your copy from your Mac

Any copy of Apparition, the release site's included, can let a Mac you pair with it read its logs and state and
run its debug commands, the same tools the maintainer uses. It is off until you turn it on:

1. Clone [DebugTrace](https://github.com/illixion/DebugTrace) and [RAVESDK](https://github.com/illixion/RAVESDK)
   side by side (the same layout as for building).
2. In Apparition, **Settings > Developer > Debugging from a Computer**: turn it on (for an hour, or until you turn
   it off), then **Pair a Computer…**.
3. On the Mac, `../DebugTrace/Tools/debugtrace-mcp pair` (it builds RAVESDK's `rave-debug` helper the first time).
   Both screens show six digits; if they match, tap **Allow** on the device. A device Bonjour can't see (another
   network, Tailscale): `pair --host <its address or tailnet name>`.
4. Register `debugtrace-mcp` with your agent (`claude mcp add -s user apps -- <path>/DebugTrace/Tools/debugtrace-mcp`).
   Apparition is then in `apps`, and `help`, `call` and `trace` work on it.

The Mac reaches the app over TLS with a key only those two hold, and nothing listens while the setting is off.
Code changes and screen captures still ask on the device every time. **Share personal detail with connected
computers** (off by default) adds the system log, debug-level lines, screenshots and full snapshots: leave it off
if your agent sends what it reads to a cloud model. The password listener under **Advanced** is for tools that
can't pair, and only over Tailscale or another VPN, since it is not encrypted. `debugtrace-mcp paired` lists your
pairings and `forget <app>` drops one; **Remove** in the app takes a Mac's key away. The design is in
[docs/design/PUBLIC_DEBUGGING_PLAN.md](docs/design/PUBLIC_DEBUGGING_PLAN.md) and DebugTrace's
`Docs/server-and-security.md`, "Public builds".

## Pull requests

- Keep a change to one purpose, and say in the description what you ran it on. Anything that touches pairing,
  installs or JIT should have been tried on a device, and a simulator build says nothing about those.
- **The Xcode project is generated** from `project.yml`. Never edit `Apparition.xcodeproj`; run `xcodegen`.
- **Licence.** Apparition is MIT. Depend on MIT and other permissive crates only, and never copy code from GPL,
  LGPL or AGPL projects (SideStore, AltStore, StikDebug and Sideloader are (A)GPL: read them for behaviour, not code).
  MPL-2.0 is fine unmodified from crates.io.
- **No app groups or app extensions** in the app, so a free Apple ID can build and run it straight from Xcode as the
  first install.
- **Comments say why.** A comment should hold what the code can't: the rule Apple enforces, the failure it avoids,
  the order something must happen in. Leave out dates, probe logs and measurements.
- **Logging goes through `DebugLogger`** (from DebugTrace) with honest privacy labels: `.public` only for values the code defines, and
  anything from the user, the network or the filesystem stays private. Error text you didn't write goes through `LogScrub.text`
  before it is logged as public (see "What a problem report can carry").
