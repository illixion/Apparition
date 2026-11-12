# Pairing and sharing signing

## When Apparition says it isn't paired

Apparition reaches the device it runs on with a pairing of its own,
`apparition_rp_pairing.plist` in its Documents folder. On Vision Pro it makes that itself in
Setup. On iPhone, iPad and Apple TV it gets it from the computer that paired the device:
installing Apparition hands it over. Anything that needs the device (an install, a refresh, a
JIT request from an app such as Madeira) stops at once and says which of two things is wrong:

- **Apparition isn't paired.** The file is missing, for example after Apparition was deleted
  and installed again some other way. Apparition keeps a copy in the Keychain (this device
  only, never synced) and puts the file back from it at launch, so this only happens when that
  copy is gone too (it was never made, or the new copy is signed by another team). On iPhone or
  iPad, from a computer that trusts the device, run

      apparition pair --usb
      apparition pairing hand-off [--device <name>]

  Wi-Fi is enough; the cable is only needed while the device doesn't trust the computer. On an
  Apple TV, run `apparition pair` with Remote App and Devices open (it shows a code twice), then
  `pairing hand-off`. On Vision Pro, pair again in Setup.
- **The device no longer trusts Apparition's pairing.** The file is there, but the device turns
  it down, for example after the device was erased or its trust settings were reset. Pair
  again the same way.

Settings > Forget Pairing deletes both the file and the Keychain copy.

## Sharing one certificate

Between devices, and from a device back to a computer, the key goes one of two ways.

**Nearby**, on the same network. On the device that needs it, Settings > Get Signing from
Another Device (or Get It from Another Device where signing stopped) waits for it; so do
`apparition signing receive` and the desktop app's Receive from a Nearby Device. On the one
that has it, Settings > Share Signing with Another Device lists what's waiting under Nearby;
on a computer, `apparition signing send --apple-id <email>` or the desktop app's Send to a
Nearby Device. Both screens show a six-digit code, and each person confirms that the two match.
It's numeric comparison, as Bluetooth pairing does it (core/src/nearby.rs): the sender commits
to its X25519 key before it sees the receiver's, so something in between would get two
different codes, all but one time in a million, and the key, sealed with ChaCha20-Poly1305
under the agreed key, goes only to a device whose person confirmed. Where Bonjour can't reach
(Tailscale, or a command line on macOS whose terminal has no Local Network access), type the
address the receiving screen shows (`--to <address>:<port>`). This is the way on Apple TV, which
has no AirDrop.

**As a file**, for a device on another network. Share Signing > Make a Key File in Apparition
(or `apparition signing share --apple-id <email>`) writes a `.apparitionkey` file and shows
five words. Send the file any way (AirDrop, Messages, a USB stick), open it in Apparition on
the other device, or on a computer in the desktop app's signing step or with `apparition
signing import`, and type the words. The words make the key with Argon2id (64 MiB, four
passes) and the file is ChaCha20-Poly1305, so the file alone is useless. The words come from
the [EFF's short wordlist 2.0](https://www.eff.org/dice) (CC BY 3.0 US, in `core/assets/`),
where each word is known by its first three letters, so case, spacing and a mistyped ending
don't matter.

## For other apps: a .p12

Some apps sign other apps themselves and need a certificate with its private key:
[LiveContainer](SUPPORTED_APPS.md#livecontainer)'s JIT-less mode, which iOS 26 and later
require, is the one we've tested. Settings > Share Signing with Another Device > Export
Certificate as .p12 (0.3.5 and later) writes the Apple ID's certificate and key as a `.p12`
under a password of six new words, to copy and share separately. It signs in if it has to and
makes no new certificate, so the other app signs with the one Apparition already uses. The file
is deleted when the screen closes. Anyone with the file and its password can sign apps as your
Apple ID, so give it only to an app you trust; if it gets out, revoke the certificate in
Certificates & App IDs.

An app that is open on the headset is closed before its update installs, as Xcode does, so the update applies straight away; only Apparition's update of itself waits until you close it. It never revokes a
certificate on its own: with no free slot it offers to receive another Apparition's key, then to
revoke one you choose.
