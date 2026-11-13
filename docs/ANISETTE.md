# Anisette and signing in

Signing in with a free Apple ID needs "anisette" data from a computer Apple recognises, and where it
comes from decides how your sign-in behaves. This page says what each source needs and costs, how to
run your own, how Apparition's built-in one works and what to do when it can't fetch Apple's libraries.
A paid account's certificate and profile need none of this.

## Choosing a source

**Apparition works best with a Mac you own**, at home or reached over Tailscale. The Mac does
two jobs that nothing else can, and everything else works without one:

| What | Needs | Without a Mac |
|---|---|---|
| Signing with a free Apple ID | A Mac's anisette: a Mac on macOS 26 or earlier (best, SIP stays on), a Mac on macOS 27 with SIP and AMFI off (works, not recommended), a macOS 26 VM on an Apple-silicon Mac (SMS codes only); or, with no Mac, Apparition's own Android anisette (macOS, Windows and Linux) or a third-party server | A paid account's certificate and profile, which need no sign-in |
| Building your own apps with Xcode, and live patching (`inject`) | A Mac with Xcode | [xtool](https://github.com/xtool-org/xtool) builds on Linux, and on Windows through WSL; `apparition deploy` installs it and `apparition mcp` debugs it from the PC ([how](REMOTE_DEVELOPER.md#without-a-mac-xtool-on-windows-or-linux)) |
| Installing, refreshing, JIT, the companion | Nothing in particular | Any computer (macOS, Windows, Linux), or the device itself with LocalDevVPN |

With a Mac, the rest of your work can happen on it or on a Windows or Linux PC: the PC pairs with
the Mac, its agents drive your apps' debug servers through the Mac, and its builds go to your
devices through it ([remote development](REMOTE_DEVELOPER.md)).

**Signing with a free Apple ID needs anisette, and the best source is your own Mac.** Apple gives the
developer access that signing needs to a sign-in that ends as a Mac's, so Apparition gets anisette
from macOS on your own Mac, as Xcode does. In order of preference:

1. **A Mac on macOS 26 (Tahoe) or earlier: the best way.** It needs no setup and no change to the
   Mac's security (SIP and AMFI stay on), and the sign-in behaves exactly as Xcode's, including the 2FA
   push on your other Apple devices, with SMS offered as well. Tested on a 2019 MBP with macOS 15.8 and SIP/AMFI kept enabled.
2. **A Mac on macOS 27, where macOS keeps its anisette to itself.**
   There are two ways around that:
   - a small **macOS virtual machine** on it (Companion › **Set up the anisette VM**, or
     `apparition vm setup`; or make one in UTM, [how](../selfhost/macos-vm/README.md)). It costs no security
     on the Mac you use, but **Apple never prompts your devices for a sign-in made through a VM**:
     you get the 2FA code by SMS to your trusted phone number only, where a real Mac's sign-in
     gets the push on your devices and offers SMS too. The VM's guest is macOS 26.6.2, and that
     setup is verified
     ([details](../selfhost/macos-vm/README.md#known-limitation-sign-ins-through-the-vm-get-sms-not-a-device-prompt));
   - or turning **SIP and AMFI off** on the Mac itself, which is expected to keep the device push
     (with SMS available), since real hardware doesn't present itself as a VM, but lowers that Mac's
     security. The reasoning is untested on real hardware, though the same entitled helper runs in the
     macOS 26 VM. Apparition doesn't do this for you and recommends against it on a Mac
     you use for anything else.
3. **Apparition's own Android anisette**, on macOS, Windows and Linux, with no Mac at all.
   Apparition fetches Apple Music's Android libraries from Apple on first use and runs them in its
   own process, nothing to install or run. The sign-in claims Android, then asks for the Xcode token
   as a Mac, which Apple accepted in a full sign-in on 12 Nov 2026 (2FA, token, teams, app IDs,
   certificates). Apple has changed what it accepts several times since October, once turning this
   (`anisette::ANDROID_LIBRARIES`) and a future build may turn it off. It signs in on this
   computer, and the companion can offer it to your devices too (Settings > Companion > Anisette for
   your devices), which is how a Mac with 8 GB, too little for the anisette VM, or a Mac on macOS 27
   serves them without a VM. On Windows the libraries run through the same loader as elsewhere:
   it maps them with the Win32 memory calls and answers their file calls itself, so there is no
   WSL, emulator or Android image behind it.
4. **A third-party anisette server**, given by address, on any computer, with no Mac at all.
   Apparition supports using one, but doesn't provide or recommend one: you trust whoever runs it,
   and whether it works is Apple's to say. A public Android-based server passed a full sign-in on
   12 Nov 2026.

Other computers, and devices away from home, pair with a Mac for anisette over an encrypted
channel. **Without a Mac you can still start** with Apparition's own Android anisette on any
computer, or with a third-party server given by address (Settings > Anisette for your devices, or the
sign-in's Anisette data), on any computer. Apple changes what it accepts without notice, so a Mac
you own stays the supported setup. Without any of them, only a paid developer account's own
certificate and profile sign apps (they need no sign-in). The Apple ID is yours too: use a spare
one for sideloading if you can.

## Run your own anisette server with Docker

For a sign-in with no Mac and no server from anyone else, with Apple's libraries kept out of
Apparition's own process or shared by several computers, run the
anisette-v3 server Apparition pins in Docker (Docker Desktop on Windows or macOS, Docker Engine on Linux,
or Docker inside WSL):

    docker compose -f selfhost/anisette/compose.yaml up -d

It listens only on this computer, on port 6970. In the sign-in, choose Anisette data > From a
third-party anisette server and enter `http://localhost:6970` (or pass `--anisette http://localhost:6970`
to `apparition install`), then use Check server to see what it serves. The image is pinned by digest, so an
upstream change can't alter what runs without a deliberate update in `compose.yaml`, and a volume keeps the
library it downloads from Apple between restarts. The server sees this computer's anisette identity and
IP address, never the Apple ID or password, which go straight to Apple.

## How anisette works

> [!NOTE]
> **Sign-in works with the built-in anisette again (from 0.4.3).** From 29 October 2026 Apple
> refused it, and for a day or so nothing but a Mac's anisette signed in. The anisette was never
> the problem: Apple had put a firewall rule in front of its sign-in servers that rejected the
> `Content-Type` sideloaders have always sent, so the request was turned away before Apple looked
> at it. Apparition sends `text/xml` now and Apple accepts the Android libraries' anisette as
> before. A Mac's anisette, a macOS VM or a third-party server still work, if you prefer one.
> Details [below](#why-sign-in-stopped-in-october-2026).

Signing in to Apple needs "anisette" data that identifies the computer. On macOS, Linux and
Windows (arm64 and x86_64) the installer makes its own: it fetches two libraries from Apple's CDN the first time (the ones Apple Music for
Android uses; nothing of Apple's is stored in this repository), loads them in its own process
(`core/src/anisette/`) and talks to Apple through them. Nothing to run or install. Wherever
you prefer it, use a server: Apparition ships none, so enter the URL of
any anisette-v3 server, or self-host one with `docker compose -f selfhost/anisette/compose.yaml
up -d` (Dadoum's anisette-v3-server, pulled from its own registry and pinned by
digest), and enter `http://<host>:6969`. The server sees this device's anisette identity
and IP address, never the Apple ID or password, which go straight to Apple. A third-party
server is optional now; use one only if you'd rather not have the built-in one.

**Since 12 October 2026 Apple checks that the anisette matches the device the sign-in claims
to be**, and refuses the sign-in (`-80045`, "This action cannot be completed at this time")
when it doesn't. isideload, like most sideloaders, claimed a Mac while sending the Android
libraries' anisette. Apparition claims Android for that anisette (`core/src/claimed_platform.rs`),
which Apple accepts. Where the libraries are used — a server that makes its anisette with them,
say — the claim still follows the server
(`Claim::after_sign_in`; Dadoum's anisette-v3-server reports a Mac but runs those libraries, so it
gets the Android claim, while jkcoxson's native-c server reports a Mac and is claimed a Mac). A
sign-in Apple asks a 2FA code for goes through the usual prompt; one it doesn't is asked for the
code Apple pushes to your trusted devices if the Xcode token is refused (`core/vendor/isideload`,
`code_after_login`).

On a Mac the built-in provider asks macOS for the Mac's own anisette instead, and the sign-in
claims a Mac and goes through the usual 2FA (`core/src/anisette/native.rs`):

- **macOS 26 and earlier:** AOSKit gives it to any program.
- **macOS 27:** only programs with AuthKit's private entitlements get it. The release ships
  `apparition-anisette-helper` signed with them, next to `apparition` and inside the app, and
  macOS runs it only with SIP off and `sudo nvram boot-args="amfi_get_out_of_my_way=1"`.
  Otherwise the Android libraries are used, which sign in as before.

In a **macOS virtual machine**, Apple also needs the host's attestation, which the helper forwards
along with the rest of AuthKit's headers; it's only valid inside a desktop session, so the server
runs as a login item. [`selfhost/macos-vm/setup.sh`](../selfhost/macos-vm/setup.sh) arranges all of
this — see **[Self-hosting anisette in a macOS VM](../selfhost/macos-vm/README.md)**.

Provisioning goes through the Android libraries either way, so a device's stored identity works
with any server. With a paid developer account there is also a route with no sign-in at all:
make the certificate and a wildcard or single-App-ID profile on Apple's developer website and
sign with them (`--p12 … --profile …`, or the app's Signing screen).

Release builds aren't notarized, so the built-in provider needs no entitlement today. It maps
the libraries into memory and makes them executable itself, so if the macOS apps are ever
notarized (hardened runtime), their entitlements must include
`com.apple.security.cs.allow-unsigned-executable-memory`, or loading fails.

The companion can offer the headset an anisette server too: this computer's own (built in,
served to the network on port 6969), or the same compose file run with your Docker (Docker
Desktop, OrbStack or Colima), from the desktop app's Companion tab or `apparition relay
--anisette builtin|local`. It then advertises the server, and Apparition offers "Use its
anisette server" next to the companion; nothing switches on its own.

Apple's servers sometimes refuse a request that another one would take: sign-in then fails "403
Forbidden". Apparition retries on another server (`core/vendor/isideload/src/auth/edge.rs`): it
avoids the server that refused for an hour and asks another, looked up over DNS-over-HTTPS (Google's
and Cloudflare's) if your own DNS knows no other, and does the same once when a server can't be
connected to. Only that lookup goes elsewhere, never the Apple ID. If every server it finds
refuses, the error says so; try again later or on another network.

The header corner of the desktop app always says where this computer's sign-in gets its anisette
(built-in, Docker or another server) and what the companion offers, so a screenshot shows the
setup.

## Seeing what a sign-in sent to Apple and what came back

When a sign-in fails in a way the error doesn't explain (a bare 403, a code you didn't expect, a
source that worked yesterday), the verbose record shows each request Apparition made to Apple and the
answer. Apple changes what it accepts without notice, and this is how to see what changed.

Turn it on, repeat the failing sign-in, then read the file:

| Where | How | The file |
|---|---|---|
| The device | Settings > Troubleshooting > Record sign-in requests (Share the record sends it from there) | `apparition-verbose.log` in Apparition's folder in the Files app |
| The desktop app | Companion tab > Troubleshooting > Record what a sign-in sends to Apple (Show the file opens it) | `apparition-verbose.log` beside `apparition-core.log`, in the `Apparition Installer` data folder |
| The command line | `apparition --verbose <command>`, or `APPARITION_VERBOSE=1` in the environment | the same file; the command says where |
| An agent | the app's debug server: `setVerboseLog` (`enabled`, `clear`), then `verboseLog` returns the end of the file | no need to pull it off the device |

Each request is one line, with its answer under it:

    #4 -> POST https://gsd.apple.com/grandslam/GsService2/ headers[content-type="text/xml", …] body {Header={Version="1.0.1"}, Request={A2k=<data:256>, cpd={bootstrap="true", …, X-Apple-I-Client-Time="2026-11-12T15:36:19Z", X-Apple-I-MD=<string:40>, X-Apple-I-MD-RINFO="17106176"}, o="init", ps=["s2k", "s2k_fo"], u=<string:52>}}
    #4 <- 200 OK in 603 ms headers[server="Apple", content-type="text/x-xml-plist;charset=UTF-8", …]
       body of https://gsd.apple.com/grandslam/GsService2/ (200): {Response={Status={hsc=200, ec=-20970, em="Your account information was entered incorrectly."}, …}}

**It holds structure, not secrets.** A value is written out only for fields that identify nothing: the
request's protocol flags, the headers Apple's own clients send, Apple's URLs, and Apple's error code and
message. Every other string is its length (`<string:40>`) and every key, signature and token its size
(`<data:256>`), so the password and its derived values, the one-time password and machine ID, the Apple
ID, the team, the device IDs and the tokens are never in it. Apple's error text can still repeat a team
or an app ID, so read the file before you post it. It is a file of its own, kept apart from
`apparition-core.log` (so nothing that reports that log includes it), stops growing at about 4 MB (it
keeps one older file) and is off until you turn it on.

## Why sign-in stopped in October 2026

From about 29 October Apple refused the anisette the built-in provider makes: provisioning
(`gsd.apple.com`) and sign-in (`gsa.apple.com`) answered an HTML `403 Forbidden` with no Apple
error code. It looked like Apple had stopped accepting the Android libraries' anisette, and for
a day it was worked around by choosing a different Apple server, since the first servers to
refuse were only some of them. On 30 October it was traced to the request's `Content-Type`:
Apple's servers reject the legacy `text/x-xml-plist` at provisioning, and (the opposite way)
`application/x-xml-plist` at sign-in. `text/xml` passes both, Apple parses the body the same and
answers as usual. With that one header the same anisette is accepted, so it never was the
anisette, the serial or the network. Apple may change the rule again; the same symptom would be
a 403 before any Apple error code.

## When Apparition can't fetch Apple's libraries

The built-in anisette needs two libraries, `libstoreservicescore.so` and `libCoreADI.so`, from
Apple's CDN (`apps.mzstatic.com`), once. Where that fails (a firewall or proxy, no route to the
CDN, Apple moving the file), signing in stops with "Couldn't fetch Apple's anisette libraries from
Apple's CDN". Work through this in order:

1. **Try again.** The desktop app shows a Try again button, which keeps what you typed for five
   minutes; on the command line run the command again. Most failures are the network. Check that
   `apps.mzstatic.com` opens from this computer, and a VPN or proxy that blocks it.
2. **Give Apparition the files yourself.** Only after step 1 fails does the desktop app show
   "Advanced: use Apple Music's libraries from a file", under the Try again button; it isn't there
   on a working setup, so nobody is sent looking for files they don't need. On the command line
   it is always available:

       apparition anisette import ~/Downloads/applemusic.apk
       apparition anisette import libstoreservicescore.so libCoreADI.so

   Either the Apple Music APK for Android, or both libraries from its `lib/arm64-v8a` or
   `lib/x86_64` folder (the one for this computer's processor). What you give replaces what
   Apparition would otherwise use, until you replace it again.
3. **Where to get them.** From Apple only. The APK is at
   `https://apps.mzstatic.com/content/android-apple-music-apk/applemusic.apk`, the address
   Apple's own Android page links to; download it on a network that reaches it, such as a phone's
   hotspot, and import it. Or copy the two libraries from another computer where Apparition has
   already signed in: they are in the `anisette` folder beside its `apple-id` folder (macOS
   `~/Library/Application Support/Apparition Installer/anisette`, Linux
   `~/.local/share/Apparition Installer/anisette`, in the Flatpak
   `~/.var/app/com.illixion.apparition.desktop/data/Apparition Installer/anisette`). Don't use an APK
   from anywhere else: these libraries run inside Apparition, with everything Apparition can do,
   and the app can't tell that a file came from Apple.
4. **If no file matches.** Apparition checks one thing: each file's SHA-256 against the version it
   was tested with (the APK Apple's CDN held on 29 October 2026), and refuses a mismatch with
   "isn't the version of the libraries Apparition was built with". If Apple has updated the
   libraries and taken the old address down, the desktop app offers "Use these files anyway", and
   on the command line `--accept-unverified` does the same:

       apparition anisette import --accept-unverified libstoreservicescore.so libCoreADI.so

   It still checks that each is a 64-bit library for this computer's processor, under the right name.
   This is an **unsupported setup**, and says so for as long as it lasts: in red in the desktop
   app's header corner and above the sign-in form, and at every command that starts the built-in
   anisette. Sign-in may fail or behave unpredictably; please say so in any bug report.
5. **Going back.** `apparition anisette import --reset` (the desktop app's "Go back to Apple's own
   libraries", on the red banner) forgets the cached libraries, supplied or fetched, and the next
   sign-in fetches Apple's own.
