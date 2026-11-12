# Contributing

Thanks for wanting to help. Bug reports, app compatibility reports and pull requests are all welcome.

## Reporting a problem

Use the [issue templates](https://github.com/illixion/Apparition/issues/new/choose). For an app that won't
install or run, check [docs/SUPPORTED_APPS.md](docs/SUPPORTED_APPS.md) first. For a sign-in that fails, run with
`APPARITION_LOG=<file>`, which writes the sign-in's progress without secrets, and attach it. Security problems go
through [SECURITY.md](SECURITY.md), not an issue.

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
  anything from the user, the network or the filesystem stays private.
