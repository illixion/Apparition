# Building and debugging through a Mac from any computer

A Mac running Apparition is a build and deploy server for other computers: a Windows or Linux
PC, or another Mac, anywhere the companion reaches (the LAN at home, the tailnet away). An agent
on that computer gets the Mac's MCP server for the apps' debug servers, and builds made there
(xtool, Xcode, anything that makes an `.ipa`) go to the Mac and on to a device, where Apparition
installs them.

```
 Windows / Linux / Mac            the Mac (Apparition app)                 iPhone, Vision Pro, Apple TV
 ┌──────────────────────┐        ┌───────────────────────────────┐        ┌────────────────────────┐
 │ agent ── apparition  │ TLS-PSK│ companion channel :49162      │        │ Apparition             │
 │          mcp (stdio) ├────────► /developer/* ─► Store loopback├─Bonjour► DebugTrace apps (MCP)  │
 │ xtool ── apparition  │ LAN or │   (app token added,           │        │                        │
 │          deploy      │ tailnet│    computer's name attached)  ├────────► Developer › Sent from  │
 └──────────────────────┘        └───────────────────────────────┘ HTTPS  │ your computers: Install│
                                                                           └────────────────────────┘
```

## Security: the pairing is the credential

Remote access reuses the companion's pairing rather than the Store's password or token:

- **Approved per computer, on the Mac.** `apparition mac pair <address>` (or the app's Pair with a
  Mac) runs the numeric comparison the devices use: both screens show a six-digit code and the
  person at the Mac allows that computer. The prompt says when it asks for developer access, which
  lets it send builds to the devices and drive the apps' debug servers.
- **A grant of its own.** Pairing now grants what the computer asks for, of what the Mac offers:
  `anisette`, `appleRelay`, and the new `developer`. A Mac offers `developer` while its Store runs,
  including on macOS 27, which has no anisette to give. Old pairings keep what they had; asking
  for more means pairing again.
- **One token per computer, never typed.** The Mac mints it into the sealed grant; the computer
  keeps it in `paired-mac.json` (mode 0600). Unpairing on the Mac revokes it at once, for that
  computer only. Nothing here knows the Store's password or its token.
- **Encrypted and authenticated.** TLS 1.2-PSK with the per-computer key, then the identify proof
  bound to the TLS exporter, as the devices' channel does (`COMPANION_ANISETTE_CHANNEL.md`).
- **The Store never faces the network.** It listens on loopback. The channel forwards
  `/developer/*` to it with the app's own token in place of whatever credentials the request
  carried, drops `Host`, `Cookie`, proxy and `X-Apparition-*` headers, adds the paired computer's
  name (`X-Apparition-Computer`), and allows one request per connection.

The alternative, exposing the Store's HTTP port with its password session, would put a password
on every agent machine, give every computer the same credential, and depend on HTTPS being set up
in front of it. The pairing already exists, is per computer, and already reaches the Mac over the
LAN and the tailnet.

On the Mac itself, agents use the app's Store directly: the app leaves `developer.json` (the
loopback URL and token, mode 0600) in `~/Library/Application Support/Apparition/Store` while it
runs, and `apparition mcp` reads it.

## Set up

On the Mac, open Apparition, with the companion on (Companion › Relay). Then, on the other
computer, with the `apparition` command line:

```sh
apparition mac pair mac-mini.local        # or its LAN or Tailscale address
# Code: 123 456 — allow this computer on the Mac if it shows the same code
apparition mac status
```

Add the MCP server to an agent as a command:

```sh
claude mcp add apps -- apparition mcp
```

It offers the Mac's tools (`apps`, `help`, `call`, `trace`, `panel`, `inject`) and adds `deploy`, which
reads a file on this computer and sends it to the Mac.

## Deploy a build made here (xtool)

```sh
xtool dev build                                   # makes the .ipa
apparition deploy xtool/MyApp.ipa --device ixPhone
# MyApp is installed on ixPhone.
```

The Mac keeps the IPA as a build (`apparition-build/1`, origin `upload`, with the computer's and
device's names). With a device named, it then tries to finish the install itself, over its own
pairing with the device (the core's tunnel: no `devicectl`, so a sleeping Apple TV stays asleep):

- It signs the app with a certificate and profiles imported on the Mac, when one covers the device
  and the app. Import them once, on the Mac: `apparition deploy-signing import cert.p12 --profile
  App.mobileprovision --device ixPhone` (`deploy-signing list` shows what is there). Nothing about
  the certificate travels to the computer that sent the build.
- With none, the IPA goes as it is, which works only for one already signed for that device.
- If the Mac can't install it (not paired with the device, no certificate, the platform doesn't fit,
  the device is away), the build stays kept and the reply says why and to open Apparition on the
  device: its Developer tab lists it under **Sent from your computers**, and Install signs it there
  with the device's own signing. `--keep` skips the Mac's attempt.

A build whose bundle ID a Store project already builds shows as that project's update on the device.

The Store also answers `GET /api/service/v1/devices`, `GET .../devices/<device>/apps` and
`POST .../builds/<id>/install?device=<device>` for the paired computers and agents (the
`developer` grant), so a kept build can be installed later or on another device.

## Build on the Mac, edit from anywhere

Xcode builds and live patching need the sources on the Mac, and nothing copies them there: the
project lives on the Mac and the other computer edits it in place. Two ways, both nothing to set up
in Apparition:

- **VS Code Remote SSH** (or any editor's remote mode) opens the Mac's checkout. Its terminal, and an
  agent run in it, are on the Mac, so they build there: `apparition build --run` (or
  `build-and-sign`), and `inject` sees the files being edited.
- **File sharing**: mount the Mac's projects folder over SMB (System Settings › General › Sharing ›
  File Sharing) and edit it with anything. Build on the Mac (`ssh mac 'cd ~/Projects/MyApp &&
  apparition build --run'`), and let an agent here drive the app through `apparition mcp`.

`apparition build` is `build-and-sign` in Rust: the same confs, flags, `build/` folder and report.
It signs with the conf's certificate and profile through the core's signer (no login keychain),
keeps the build and its dSYMs in the build store, and installs and launches over Apparition's own
pairing with the device rather than `devicectl`. A build made elsewhere (xtool) goes through
`apparition deploy` instead, below.

## Without a Mac: xtool on Windows or Linux

The Store runs in the Apparition app on Windows and Linux too, so a PC that builds with
[xtool](https://github.com/xtool-org/xtool) deploys and debugs with no Mac in the loop. Pair the
device with the PC's Apparition as usual, then:

```sh
apparition deploy-signing import cert.p12 --profile App.mobileprovision --device ixPhone   # once
xtool dev build
apparition deploy xtool/MyApp.ipa --device ixPhone   # the PC's own Store, over its own pairing
claude mcp add apps -- apparition mcp                # debug servers of builds the Store knows
```

`apparition deploy` and `apparition mcp` use the local app's Store when it runs
(`developer.json` in `%APPDATA%\Apparition\Store`, or `~/.local/share/Apparition/Store`), and
the paired Mac otherwise. What a PC can't do:

- **Sign with a free Apple ID headless.** The PC's Store signs with an imported paid
  certificate. Free-account signing needs a Mac's anisette (a paired Mac, or a server with a Mac
  behind it), and an interactive sign-in the Store can't do on its own.
- **Live patching** (`inject`) compiles with the app's Xcode toolchain; the tool says so.
- **Let the device fetch a build the PC couldn't install.** A Mac's device reaches its Node
  build service for that; nothing on a PC answers it yet (it will be the companion channel).
- **Attach to an xtool build's debug server.** The Store attaches only to apps whose per-build
  token it knows, which `build-and-sign` records when it embeds it. An xtool build has none, so
  `apparition mcp` lists it as not attachable. The fix is for the Store to embed the token when it
  signs a sent build, which is part of the signing port (phase 5).

## What isn't here yet

- **Installing from the Mac covers only what the Mac's signing can.** It signs offline with an
  imported certificate; signing with an Apple ID needs an interactive sign-in (and its two-factor
  step) the Store can't do on its own, so a Mac that only has the Apple ID in the Apparition app
  leaves the install to the device. Run for real on 2026-11-04: a Windows PC paired, drove MCP and
  sent a build already signed for a Vision Pro, which the Mac installed (46 MB in 38 s). Signing on
  the Mac with an imported certificate is tested with a stand-in only.
- **The device reaching the Mac away from home** still goes through the Store's HTTPS address and
  password (Developer › Connect), not the channel.
- **No build tool over MCP.** An agent elsewhere builds through the Mac's shell (Remote SSH, `ssh`),
  not through `apparition mcp`.
- **`inject` patches sources that are on the Mac.** It runs `debugtrace-inject` there, from the
  app's project root, so the project has to be the Mac's own checkout, edited in place (above).
- **Older builds aren't attached to over Bonjour.** The Store sends no token to an address that
  only advertises: an app found by Bonjour gets signed requests (`GET /_nonce`, then
  `Authorization: DebugTrace-HMAC`, a proof over the nonce, method, target and body; the token never
  crosses the network). A build from before that (`txtvers` 1 or 2) is listed as not attachable
  until it is rebuilt with build-and-sign. Launch records, whose address build-and-sign recorded
  itself, still use the bearer. What a copied advertisement can still do is answer a request meant
  for the app with lies, as anything can on a plain-HTTP network: use the tailnet away from home.
  Protocol: DebugTrace `Docs/server-and-security.md`.
