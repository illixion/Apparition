# Anisette VM inside the app: scope

Status, 2026-11-04: **in the app.** The Companion tab sets the VM up with one button (no administrator password, no
typing, no Python), the companion offers its anisette to paired devices, and sign-ins on the Mac use it; `apparition
vm` does the same from a terminal. A fresh setup from a local copy of the restore image took about 11 minutes and
ended with Apple accepting the anisette. This document is the scope that got it there, and what is still open.

## What the person sees

On a Mac that has no anisette of its own (macOS 27 and later), the desktop app's Companion tab offers
**Set up the anisette VM** (and "Use a restore image I have…"). One click, a progress bar, and about 11 minutes
of waiting with the restore image on the Mac, 45 with its 20 GB download. No UTM, no Terminal, no typing in
recoveryOS, no administrator password. Once set up, the companion offers it ("This Mac's anisette VM"), the sign-in
form offers it, and the group shows whether it runs or is saved, with Check with Apple and Remove.

Devices need no change. They pair with the Mac's companion exactly as they do with a Mac on macOS 26 or a
Windows PC; the VM sits behind the companion as its anisette source (see "Reaching the VM").

## What exists and works (verified on macOS 26.6.2 guests, 2026-11-04)

- Runner (`main.swift`, `apparition-vm` in the app): create, install, run, recovery boot, shutdown, and guest input
  for recoveryOS: key chords, typing, clicks, and Vision OCR of its own window (`click-text`).
- Orchestrator (`core/src/vm.rs`, replacing `vm.py`): the pinned image (a local copy first), create, install, both
  recoveryOS stages with no hands on the keyboard, the boots, the save; resumable at every step, one progress
  stream for the app and `apparition vm`.
- Guest side: first-boot account with a secure token, auto-login, SIP and AMFI off, the entitled helper in an
  Aqua LaunchAgent, status over vsock.
- Appliance trim: 156 per-user and 74 system launchd services disabled before first login, no audio
  device. Footprint 2.9 GB to about 1.0 GB, zero swap at 1.5 GiB RAM, 12 of 12 probes accepted in a 12-minute
  soak. Measured in the README, with the lesson that a disabled service something keeps asking for is worse
  than a running one (`failed-lookups.sh` finds those).
- Pinned guest: macOS 26.6.2 (25G83) from Apple's CDN, checked against the size and SHA-256 Apple publishes with
  the file; the runner refuses other builds (`verifiedBuilds`).
- Anisette over vsock and suspend when idle (`vm serve`): a host loopback front door, `guest-bridge` in the guest,
  the VM saved to disk and stopped after 5 idle minutes, resumed by the next request, the guest clock set on
  resume. Measured in the README ("Serving anisette, suspended when idle").

- Fresh setup through the no-root path, end to end, from `apparition vm setup` (2026-11-04): create and install
  3.5 min, provision stage, first boot 25 s, setup stage, boot, save, Apple's check; no SSH and no relay in the
  guest. The companion's VM backend serves isideload's own client (provisioning, headers, the guest's client info).
- Every way of stopping the runner saves the VM (the logout/shutdown quit event, SIGTERM, SIGINT, `quit`), and the
  next start resumes it in 1.0 s with Apple accepting the anisette.
- `apparition vm update` on the set-up VM with no restore image on the Mac: 152 s. The guest's programs matched
  the build byte for byte afterwards, and the next start resumed in 1.2 s with Apple accepting the anisette. The
  automatic run at app launch (once `GUEST_VERSION` is bumped) is the same code; it hasn't been run from the app.

Not verified: a real account signing in through a VM-hosted companion end to end; a device pairing with a
companion that offers the VM; the Companion tab's VM group in the running app (built, not yet clicked through);
host sleep and wake with the VM running (it suspends on the sleep notification).

## Measured facts that shape the design

| Fact | Number |
|---|---|
| Restore image Apple's catalog offers for `VirtualMac2,1` | one entry: macOS 27.2 (26B123), 25.6 GB, `Accept-Ranges`, no published digest |
| Restore image we pin | macOS 26.6.2 (25G83), 19.8 GB, not in the catalog but still on Apple's CDN with `x-amz-meta-digest-sha256` (matches ipsw.me); installed in a VM on 2026-11-04 |
| Disk after install | 24 to 27 GB allocated (64 GB virtual, sparse); peak with the image about 47 to 52 GB |
| Fresh setup, image on disk | about 11 min: hash 1, install 3.5, provision stage 1.5, first boot 0.5, setup stage 1, boot to anisette 1.5 |
| RAM floor (trimmed or not) | 1.5 GiB boots; 1.25 GiB and 1 GiB do not (black screen, no network) |
| RAM for recoveryOS | more than 2 GiB (black screen at 2 and 1.5, up in 26 s at 3); setup runs the install and both recovery boots at 4 GiB |
| Host cost of the guest | its configured RAM, from the first seconds (1.57 GB footprint for 1.5 GiB), pageable, not wired |
| Boot to Apple-accepted anisette | 36 s stock, 70 to 85 s trimmed (measured with a 4 s poll) |
| Suspend (pause, save, stop) | 0.8 s, 430 to 500 MB file; the VM process and its memory are gone, the runner keeps 23 MB |
| Resume to Apple-accepted anisette | 1.0 s restore + 0.2 s clock; a request that wakes it is answered in 2.7 to 3.9 s |
| Guest clock after a resume | behind by the time suspended until set (macOS fixes it itself about 5 s later) |
| Own-window capture permission | a freshly rebuilt ad-hoc runner captured its window with no Screen Recording prompt seen; recheck on a clean account |
| `VZMacOSRestoreImage.fetchLatestSupported` | meant to follow the host's macOS; it failed here with "restore image catalog failed to load", while the catalog file itself downloaded fine, so the app should read the catalog itself |

## Decisions, with a recommendation each

**1. Guest version. Decided (2026-11-04): pin macOS 26.6.2.** Apple's catalog only lists the newest restore image
(27.2), but the 26.6.2 file is still on Apple's CDN with its SHA-256 in the response headers, and 26 is the
version the whole flow, the trim and the RAM floor were verified on. core/src/vm.rs uses a copy already on the Mac or downloads that one URL, and checks it;
the runner refuses other builds unless `APPARITION_VM_ALLOW_UNVERIFIED=1` (the "advanced" switch, for trying the
next one, because Apple has changed what it accepts twice this autumn, 12 and 30 October). The risk is Apple
removing the file from its CDN; the pin then needs a mirror the app controls, checked against the same hash.
A macOS 27 guest is a separate milestone if 26 ever stops being accepted.

**2. Who writes into the guest disk before first boot. Done: recoveryOS, no host root.** `provision.sh`
(mounting the disk on the host as root) is gone. Before: it meant an administrator prompt and a script running as
root against a path in the user's library. The recommendation was no host root at all. Boot recoveryOS once right after the install (the driver already
does this reliably), run one script there that mounts the Data volume and writes the first-boot daemon,
the status daemon, the trim lists and the companion, then boot normally. The existing `setup.sh` already
does most of this in recovery. The cost is one more recovery boot (about two minutes) and a second
OCR-driven pass; the gain is that the app never needs root and there is a single provisioning mechanism.
Boot sequence: install, recovery (payload), boot (account is made, shuts down), recovery (`csrutil` with the
account's password), boot (AMFI boot argument, reboots), boot (live). That is what runs now. One finding on the way:
csrutil looks for its administrator on the Preboot volume, which macOS updates lazily, so the first boot runs
`diskutil apfs updatePreboot /` before it shuts down (from recoveryOS the account is listed but its password refused).

**3. Reaching the VM.** Bridged networking needs `com.apple.vm.networking`, which an ad-hoc signed app
cannot have, so the VM is NAT only. A phone cannot reach 192.168.64.x, and the host route and VPN problems
in the README are all about that subnet. Recommend: the VM exposes nothing on any network. The host app's
own companion keeps doing pairing, the encrypted channel, the Apple TLS relay, mDNS and the wake knock, and
gets its anisette from the VM:

- in the guest, `apparition anisette serve` on the guest's loopback, from an Aqua LaunchAgent, and `guest-bridge`
  (a small root daemon in C) carrying vsock 6970 to it (**done**; `relay` still runs beside it for now);
- the runner bridges host loopback port 6970 to that vsock port (**done**, the front door in `vm serve`);
- core's second `AnisetteSource`, `Vm`, proxies `client_info` and `get_headers` to that port, with the
  paired-device token check staying on the host (`anisette::start_vm`; each request starts or resumes the
  runner). **Done.** Sign-ins on the Mac itself use the front door directly (`VM_ANISETTE`, the default when the
  Mac has no anisette of its own).

This removes the DHCP-lease lookup, the subnet route, the pairing prompt problem (the LaunchAgent cannot ask
for the six-digit code; the host app already can) and the wake problem (a device knocks on the host's
address, which is the Mac that sleeps). Guest `AF_VSOCK` is already proven by `guest-status.c`.

**4. Lifecycle on an 8 GB Mac. Done in the runner (`serve`).** The guest costs its RAM, 1.5 GiB of 8, while it
runs. Setup is the exception: the install and both recovery boots need 4 GiB, so the wizard should ask the person to
close heavy apps and show it as a temporary cost. Otherwise the VM is suspended after 5 idle minutes (saved to the
bundle and stopped, 0.8 s, all of its memory back to the host) and resumed by the next request (about 1.2 s, the
guest's clock set before the request is let through). That is quick enough for every Mac, not only 8 GB ones, so
there is no separate on-demand mode: the companion stays on and the VM costs memory only around sign-ins. A wake
soon after a suspend doubles the idle period (up to an hour), so a client that polls cannot make it write 450 MB
to disk every few minutes. Every stop saves it (logout, restart, shutdown, sleep, the app quitting), so the cold
boot happens once, at the end of setup, and again only after a crash or a power cut.

**5. Keeping the guest current.** macOS updates are off in the appliance. The helper and `apparition` inside
the guest must follow the app, because Apple's acceptance changes. Recommend: the guest agent asks the host
over vsock for its expected version at start and pulls a newer pair of binaries from the host when they
differ. That needs the host to serve them and the guest to keep the helper's signature byte for byte.

## Work, in order, each with a gate

1. ~~Guest 27.2~~ Replaced by the pinned 26.6.2 (decision 1).
2. ~~Recovery-only provisioning.~~ Done: setup.sh's provision stage; the gate (a fresh bundle reaches live
   anisette with no administrator prompt) passed.
3. ~~Helper and orchestrator.~~ Done: the runner ships in `Contents/MacOS` as `apparition-vm` (install-desktop.sh,
   package-release.sh), with `apparition-vm-guest-status` and `-guest-bridge`; core/src/vm.rs replaced `vm.py`.
   The runner kept its window (recoveryOS needs one to be read), off screen in `serve`.
4. **Companion source.** `AnisetteSource::Vm` done; left is the gate: a device signs in with a real account
   through the VM.
5. **App and CLI.** Done: the Companion tab's group (progress, stop, check with Apple, remove), the companion's
   and sign-in form's VM choice, `apparition vm setup|status|serve|quit|shutdown|check|image|remove`, the bundle
   excluded from Time Machine, the image in `~/Library/Caches`. The cache's copy of the image is removed once the VM is
   set up (the person's own copy stays). Not done: a preflight for free disk (the app says what it needs; the copy
   fails when the disk does), and clicking it through in the running app.
5a. **Never made again** (2026-11-04). A set-up VM outlives everything that would otherwise mean rebuilding it
   (selfhost/macos-vm/README.md, "Keeping it"). A saved state that can't be resumed costs one cold boot. The
   guest's updates are off. A new image pin applies only to new VMs. A newer app's guest programs go in with
   `apparition vm update`, which runs the setup stage again from the VM's own recoveryOS with no image. It runs on
   its own at launch when `GUEST_VERSION` is above the bundle's `guest-version`, and `setup` runs it too.
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

- ~~`provision.sh` leaves a root-owned `provisioning/` folder.~~ No host root any more. Bundles made by it
  (the test VMs) still have the folder; Remove moves the bundle to the Trash, folder and all.
- The runner listens on loopback TCP with a token in a file; a unix socket in the bundle with mode 600 is
  simpler and removes the port.
- ~~The runner activates its window and takes focus.~~ Automated runs are an accessory app that stays behind.
- ~~The runner refuses anything but macOS 26.~~ That check is now the verified-builds list.
