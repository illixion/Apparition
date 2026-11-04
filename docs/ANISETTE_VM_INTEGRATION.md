# Anisette VM inside the app: scope

Status: scoping, 2026-11-04. The VM flow works end to end from the command line
(`selfhost/macos-vm/`, see its README). This document is what it takes to make it a button in the
desktop app that a stranger can press, and what was measured to decide that.

## What the person sees

On a Mac that has no anisette of its own (macOS 27 and later), the desktop app's Companion tab offers
**Set up the anisette VM** next to "Use this Mac" and "Use another Mac". One click, a progress bar, and
about 45 minutes of waiting (most of it two downloads and the macOS install). No UTM, no Terminal, no
typing in recoveryOS, and no administrator password if step 4 below holds.

Devices need no change. They pair with the Mac's companion exactly as they do with a Mac on macOS 26 or a
Windows PC; the VM sits behind the companion as its anisette source (see "Reaching the VM").

## What exists and works (verified on macOS 26.6.2 guests, 2026-11-04)

- Runner (`main.swift`): create, install, run, recovery boot, shutdown, and guest input for recoveryOS: key
  chords, typing, clicks, and Vision OCR of its own window (`click-text`).
- Driver (`vm.py`): the whole recoveryOS step with no hands on the keyboard (startup picker, Terminal, `csrutil`,
  password, setup script), resumable between steps. Python, development only.
- Guest side: first-boot account with a secure token, auto-login, SIP and AMFI off, the entitled helper in an
  Aqua LaunchAgent, status over vsock.
- Appliance trim (new): 156 per-user and 74 system launchd services disabled before first login, no audio
  device. Footprint 2.9 GB to about 1.0 GB, zero swap at 1.5 GiB RAM, 12 of 12 probes accepted in a 12-minute
  soak. Measured in the README, with the lesson that a disabled service something keeps asking for is worse
  than a running one (`failed-lookups.sh` finds those).

Not verified: a real account signing in through a VM-hosted companion end to end; a guest on macOS 27; a
fresh install through the final no-root path; host sleep and wake with the VM running.

## Measured facts that shape the design

| Fact | Number |
|---|---|
| Restore image Apple's catalog offers for `VirtualMac2,1` | one entry: macOS 27.2 (26B123), 25.6 GB, `Accept-Ranges`, no published digest |
| Restore image we tested with | macOS 26.6.2, 19.8 GB; not in Apple's catalog |
| Disk after install | 26.7 GB allocated (64 GB virtual, sparse); peak with the image about 52 GB |
| RAM floor (trimmed or not) | 1.5 GiB boots; 1.25 GiB and 1 GiB do not (black screen, no network) |
| RAM for recoveryOS | more than 2 GiB (black screen at 2 and 1.5, up in 26 s at 3); setup runs the install and both recovery boots at 4 GiB |
| Host cost of the guest | its configured RAM, from the first seconds (1.57 GB footprint for 1.5 GiB), pageable, not wired |
| Boot to Apple-accepted anisette | 36 s stock, 70 to 85 s trimmed (measured with a 4 s poll) |
| Own-window capture permission | a freshly rebuilt ad-hoc runner captured its window with no Screen Recording prompt seen; recheck on a clean account |
| `VZMacOSRestoreImage.fetchLatestSupported` | meant to follow the host's macOS; it failed here with "restore image catalog failed to load", while the catalog file itself downloaded fine, so the app should read the catalog itself |

## Decisions, with a recommendation each

**1. Guest version.** Apple only publishes the latest restore image, and users who need the VM are by
definition on macOS 27 or later, so the guest should be the entry in Apple's catalog (`mesu.apple.com/assets/macos/com_apple_macOSIPSW/com_apple_macOSIPSW.xml`, `VirtualMac2,1`).
The macOS 26 flow is verified; 27 is not. Recommend: make the 27 guest the first milestone, keep the
macOS 26 path as the fallback (a pinned URL from a third-party index such as ipsw.me, only if 27 fails),
and let the app list the guest builds it has verified. An unverified build is allowed only behind an
"advanced" switch, because Apple has changed what it accepts twice this autumn (12 and 30 October).

**2. Who writes into the guest disk before first boot.** Today `provision.sh` mounts the VM's disk on the host
as root, which means an administrator prompt and a script running as root against a path in the user's
library. Recommend: no host root at all. Boot recoveryOS once right after the install (the driver already
does this reliably), run one script there that mounts the Data volume and writes the first-boot daemon,
the status daemon, the trim lists and the companion, then boot normally. The existing `setup.sh` already
does most of this in recovery. The cost is one more recovery boot (about two minutes) and a second
OCR-driven pass; the gain is that the app never needs root and there is a single provisioning mechanism.
Boot sequence: install, recovery (payload), boot (account is made, shuts down), recovery (`csrutil` with the
account's password), boot (AMFI boot argument, reboots), boot (live).

**3. Reaching the VM.** Bridged networking needs `com.apple.vm.networking`, which an ad-hoc signed app
cannot have, so the VM is NAT only. A phone cannot reach 192.168.64.x, and the host route and VPN problems
in the README are all about that subnet. Recommend: the VM exposes nothing on any network. The host app's
own companion keeps doing pairing, the encrypted channel, the Apple TLS relay, mDNS and the wake knock, and
gets its anisette from the VM:

- in the guest, `apparition anisette serve` listens on **vsock** (a `--vsock <port>` option) from the Aqua
  LaunchAgent, replacing `relay`;
- the runner bridges a host loopback port to that vsock port;
- core gets a second `AnisetteSource` that proxies `client_info` and `get_headers` to that port, with the
  paired-device token check staying on the host (`core/src/anisette/server.rs` calls `native::headers`
  directly today; that call becomes a provider).

This removes the DHCP-lease lookup, the subnet route, the pairing prompt problem (the LaunchAgent cannot ask
for the six-digit code; the host app already can) and the wake problem (a device knocks on the host's
address, which is the Mac that sleeps). Guest `AF_VSOCK` is already proven by `guest-status.c`.

**4. Lifecycle on an 8 GB Mac.** The guest costs its RAM, 1.5 GiB of 8, plus the runner (about 90 MB today).
Setup is the exception: the install and both recovery boots need 4 GiB, so the wizard should ask the person to
close heavy apps and show it as a temporary cost.
Recommend: default to running while the companion is on for Macs with 16 GB or more, and start on demand for
8 GB Macs (start when the first request or knock arrives, stop after an idle period). Cold start is 70 to 85
s, which a sign-in can wait for behind a visible "starting" state. Saving and restoring VM state would make
that near instant; `VZVirtualMachine.saveMachineStateTo` exists for macOS guests but is untested here and
must be checked against the guest's clock after a restore (anisette carries a timestamp).

**5. Keeping the guest current.** macOS updates are off in the appliance. The helper and `apparition` inside
the guest must follow the app, because Apple's acceptance changes. Recommend: the guest agent asks the host
over vsock for its expected version at start and pulls a newer pair of binaries from the host when they
differ. That needs the host to serve them and the guest to keep the helper's signature byte for byte.

## Work, in order, each with a gate

1. **Guest 27.2** (about a day). Run the existing flow on the 27.2 image, regenerate the trim lists from the
   running guest, find the RAM floor again. Gate: Apple accepts the anisette at 1.5 GiB on 27.
2. **Recovery-only provisioning** (a day or two). Fold `provision.sh` into a recovery stage; the host server
   serves the stage's files; delete the `sudo`/`osascript` path. Gate: a fresh bundle reaches a live
   companion with no administrator prompt.
3. **Helper and orchestrator** (several days). Split `main.swift` into a headless helper (create, install,
   run, stop, status, vsock bridge, recovery input) built by `scripts/package-release.sh`, ad-hoc signed
   with `com.apple.security.virtualization`, placed in `Contents/MacOS` like the anisette helper. A Rust step
   engine in `core` replaces `vm.py`: persisted state in the bundle, one event stream for the CLI and the
   app, restart-safe at every step. The helper runs as an accessory app that never takes focus.
4. **vsock transport and companion source** (several days). Guest `anisette serve --vsock`, runner bridge,
   `AnisetteSource::Vm`, start/stop policy. Gate: a device signs in with a real account through the VM.
5. **App and CLI** (several days). A wizard in the Companion tab (preflight for chip, macOS version, free
   disk, RAM; download with resume; install; the recovery stages with live status; delete and rebuild),
   `apparition vm setup|start|stop|status|remove`, an exclusion from Time Machine for the bundle, the IPSW
   in `~/Library/Caches` and removed after install. Intel Macs hide the whole thing.
6. **Hardening** (ongoing). 24-hour soak, host sleep and wake, VPN on and off, a crash at every step then
   resume, a full disk, an unclean stop, the guest build changing under an update, uninstall. Docs.

## Risks

- **Apple's acceptance.** The appliance depends on Apple accepting a VM's anisette with a forwarded host
  attestation. That already changed once. The in-app version must make a refusal legible ("Apple stopped
  accepting this VM", not a raw `-3000076`) and keep the self-test (credential-free init probe) as a
  button.
- **OCR-driven recovery.** The recoveryOS screens are Apple's and change between releases. Today's driver
  matches text and retries; the in-app version needs a screenshot-bearing failure ("recovery looked
  different; here is what it showed") and a manual fallback with the exact line to type.
- **Private and unusual configuration.** SIP off, AMFI off and entitlements Apple reserves are acceptable only
  inside a disposable VM that exists for this, which is why nothing in the app should offer them for the
  host.
- **Disk and download size.** About 52 GB peak and a 20 to 26 GB download. Preflight and resume are not
  optional.
- **Remote Login.** The development flow enables SSH in the guest. The recovery script now does that only for
  the automated development flow; the shipped path should have no SSH at all.

## Smaller items found on the way

- `provision.sh` leaves a root-owned `provisioning/` folder with a copy of the password inside the bundle.
  It is mode 700, but it should be deleted after use (needs the root that item 2 removes).
- The runner listens on loopback TCP with a token in a file; a unix socket in the bundle with mode 600 is
  simpler and removes the port.
- The runner activates its window and takes focus. Automated runs should be an accessory app that stays
  behind.
- The runner refuses anything but macOS 26 (`main.swift`, in `create` and `install`); that check becomes the
  verified-builds list.
