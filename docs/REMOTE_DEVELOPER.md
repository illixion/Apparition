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
# MyApp is on the Mac. Open Apparition on ixPhone to install it.
```

The Mac keeps the IPA as a build (`apparition-build/1`, origin `upload`, with the computer's and
device's names). On the device, Apparition's Developer tab lists it under **Sent from your
computers**; Install downloads it from the Mac and signs it there with the device's own signing,
like a project's kept build. A build whose bundle ID a Store project already builds shows as that
project's update instead.

## What isn't here yet

- **Installing from the Mac without the device's Apparition.** The Mac doesn't yet install a sent
  build onto a device it reaches itself: that is phase 3 of `RUST_STORE.md` (devices through the
  core). Until then the device's Apparition finishes the install.
- **The device reaching the Mac away from home** still goes through the Store's HTTPS address and
  password (Developer › Connect), not the channel.
- **Builds on the Mac over MCP** (`build-and-sign` from an agent elsewhere) need the build port
  (phase 5).
- **`inject` patches sources that are on the Mac.** It runs `debugtrace-inject` there, from the
  app's project root, so an agent on another computer can inject only when the project (or the
  files it names) lives on the Mac too; edits made on the other computer would need syncing first.
- **Older builds aren't attached to over Bonjour.** The Store sends no token to an address that
  only advertises: an app found by Bonjour gets signed requests (`GET /_nonce`, then
  `Authorization: DebugTrace-HMAC`, a proof over the nonce, method, target and body; the token never
  crosses the network). A build from before that (`txtvers` 1 or 2) is listed as not attachable
  until it is rebuilt with build-and-sign. Launch records, whose address build-and-sign recorded
  itself, still use the bearer. What a copied advertisement can still do is answer a request meant
  for the app with lies, as anything can on a plain-HTTP network: use the tailnet away from home.
  Protocol: DebugTrace `Docs/server-and-security.md`.
