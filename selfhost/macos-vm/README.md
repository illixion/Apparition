# The anisette VM

A macOS 26 virtual machine whose only job is the Mac's own anisette, for a Mac on macOS 27 with SIP and AMFI on, which keeps
its own to itself (`core/src/anisette/native.rs`). The desktop app's Companion tab sets it up with one button and offers its
anisette to paired devices; `apparition vm` does the same from a terminal. Apple silicon only.

Apple sends sign-ins made through the VM an SMS code, never a device prompt (see
[the known limitation](#known-limitation-sign-ins-through-the-vm-get-sms-not-a-device-prompt)), so a Mac on macOS 26 or
earlier, Intel included, is the better companion when you have one.

What is here:

- `main.swift`, the runner (`apparition-vm` in Apparition.app): create, install, run, recoveryOS, the anisette front door,
  suspend and resume, and input to its own window for recoveryOS. `build.sh` builds it and the two guest programs into
  `build/macos-vm`; `scripts/install-desktop.sh` and `package-release.sh` put them in the app.
- `guest-status.c` and `guest-bridge.c`, root daemons in the guest: setup state to the host over vsock, and the guest half
  of the anisette front door (vsock to the guest's loopback, and the clock).
- `setup.sh`, served to recoveryOS by the host (`core/src/vm_setup.rs`) in two stages, with `first-boot.sh` and
  `report-status.sh`, which the first stage writes into the guest, and the trim (`appliance-trim.sh`, two lists).
- The orchestration is Rust, in `core/src/vm.rs`: the pinned image, setup, running, status.

## Setting it up

```sh
apparition vm setup        # about 45 minutes, no questions; run it again to carry on after an interruption
apparition vm status
apparition vm check        # does Apple accept the VM's anisette? (a test sign-in, no Apple ID)
```

In a checkout, run `bash selfhost/macos-vm/build.sh` first; the command line then finds the runner in `build/macos-vm`.
The VM lives in `~/Library/Application Support/Apparition/anisette.vm` (`--bundle` names another) and is excluded from Time
Machine.

The guest is pinned to **macOS 26.6.2 (25G83)**, the build everything here is verified with (`RESTORE_IMAGE` in `vm.rs`,
`verifiedBuilds` in `main.swift`; the runner refuses others unless `APPARITION_VM_ALLOW_UNVERIFIED=1`). The image comes from
Apple's CDN, 19.8 GB, checked against the size and SHA-256 Apple publishes. A copy already on this Mac is used first (the
cache, `--ipsw <file>`, or the app's "Use a restore image I have"), and it is removed once the VM is set up, since nothing
reads it again.

Setup runs these steps, each skipped when its result is already there:

1. **Create and install** the guest.
2. **recoveryOS, provision stage.** The host types one `curl` line into recoveryOS's Terminal, reading the window with Vision
   OCR, and the stage it fetches writes the first boot into the guest's Data volume. Root in recoveryOS is the only way to
   write the guest's disk without an administrator password on the host, and everything is served under a one-off token.
3. **First boot**, network off: makes the first user, turns on automatic login and shuts down.
4. **recoveryOS, setup stage:** turns SIP off, sets the AMFI boot argument and installs `apparition` with its entitled helper
   into `/usr/local/apparition`.
5. **Boot**, then **save** the VM and ask Apple whether it accepts the anisette.

## Keeping it

Nothing after setup reads the restore image, so a VM is set up once and not made again:

| What changes | What happens |
|---|---|
| A saved state can't be resumed | It is deleted and the VM boots cold once (about 70 s), with the same disk, account and provisioning |
| The runner stops without saving (a crash, a power cut) | The next start boots cold, once |
| macOS in the guest | Never updates itself: the trim turns `softwareupdated` off |
| A newer release's programs for the guest | Taken from that release and handed to the running VM, which checks the release's signature itself: seconds, no cold boot |
| What only recoveryOS can write (launchd jobs, the trim) | `apparition vm update`, or the app at launch, runs the setup stage again from the VM's own recoveryOS |

Every release publishes the guest's programs as `Apparition-VMGuest-<version>-macos-arm64.zip`, attested and listed in the
signed `SHA256SUMS`. The app carries the package over vsock, and the guest checks the signature, the version and the file list
with the release signers compiled into its own `apparition`, so a tampered app can't put programs of its own into the VM.

## Running it

`apparition vm serve`, and the app whenever its companion offers the VM's anisette, runs the finished VM as an appliance:
network on, its window off screen, and the guest's anisette on `http://127.0.0.1:6970` on the host. Sign-ins on this Mac use
it when it has no anisette of its own (`apparition anisette status`), and the companion serves it to paired devices through its
encrypted channel. Nothing in the guest listens on a network: the runner carries each connection over vsock to `guest-bridge`.

After five minutes without a connection (`idleSuspendSeconds`, 0 never) the runner suspends the VM and gives all its memory back
to the host; the next connection resumes it in about a second. Every way the runner stops saves the VM, so the cold boot
happens once, during setup.

**Running small.** The VM only has to produce anisette, so setup trims almost all of macOS 26 (`appliance-trim.sh`, from the
two lists beside it) and the appliance has no audio device. A trimmed guest runs in 1.5 GiB and holds about 1 GB on the host,
against about 3 GB for a stock one, which matters on a Mac with 8 GB. Do the trim before the first login, with files: booting
services out of a running session logs the session out. recoveryOS needs 3 GiB or more, so setup temporarily costs the host
4 GiB.

## Known limitation: sign-ins through the VM get SMS, not a device prompt

When Apparition signs in through the VM's anisette, Apple answers the password step with "no trusted devices" and offers only
SMS to the account's trusted phone number, even for accounts that have Apple devices. The same account signed in through a real
Mac's anisette gets the normal push on its devices, with SMS available as well. A sign-in made by hand inside the VM behaves the
same way, so this comes from the VM itself (it presents as `VirtualMac2,1` with virtual-machine attestation), not from the
account or from how Apparition makes its requests. A macOS 15 guest behaves the same as the macOS 26 one, so the guest's version
doesn't matter. Apple doesn't document it.

What Apparition does about it: it detects the "no trusted devices" answer and asks for the SMS code instead of waiting for a
prompt that will never arrive. Apple texts the code itself, so Apparition doesn't ask for a second one. If two texts still
arrive, use the newest.

What to do: if your account needs the device prompt, or you'd rather not rely on SMS, use a real Mac as the companion (the
desktop app's Companion tab, no VM). The VM is for when no Mac is available.

## Without Apparition's app: UTM

You can also make the VM yourself. Install macOS 26 or later in UTM, go through Setup Assistant, turn on automatic login, then
boot the VM into recoveryOS and run `setup.sh` from Utilities > Terminal (the command is at the top of the script). It turns SIP
off, sets the AMFI boot argument and installs the companion as a login-session LaunchAgent. Keep networking off during Setup
Assistant, which can hang with NAT attached. Everything else on this page is for the app's own VM.
