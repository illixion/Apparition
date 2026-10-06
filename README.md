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

- macOS with Xcode (`xcodebuild`, `xcrun devicectl`) and Node 18 or newer.
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

4. Create the server's config and start it:

   ```sh
   cp config.example.json config.json   # then edit; it is gitignored
   node server.js                       # http://127.0.0.1:8484
   ```

   To keep it running, wrap `node server.js` in a launchd LaunchAgent with `KeepAlive`.
   `config.json` changes need a restart. Don't start a second copy by hand while the agent
   is running: the port is taken.

## How it works

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

## Debug traces

Apps that link DebugTrace can upload a trace zip to `POST /api/traces`. `build-and-sign` generates
a fresh Ed25519 key for each build and records its public half in
`~/.local/state/debugtrace/keys/`. The server only keeps traces whose signature verifies against
that ledger. Set `DEBUGTRACE_UPLOAD_URL` in `global.conf` to the server's URL.

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
Two things work without a session, each with its own check:

- `POST /api/traces`, which apps use to upload traces. Only traces signed by a key in this
  Mac's ledger are kept.
- OTA manifests and IPAs, which the iOS installer fetches without the page's cookie. Their links
  carry an HMAC signature that expires after an hour, and only the signed-in page hands them out.

Without a password, everything is open to whoever can reach the port, and the page says so.
The server listens on `127.0.0.1`; the intended way to reach it from devices is `tailscale serve`
over a private tailnet. Don't expose it to the internet, password or not.

- The trace key ships inside each IPA, so a valid signature proves "a build this Mac made",
  not "a genuine device".
- Installation still requires each device's UDID to be in the app's provisioning profile.

## License

MIT, see `LICENSE`.
