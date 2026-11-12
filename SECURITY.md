# Security

## Reporting a vulnerability

Please report security problems privately, through
[GitHub's private vulnerability reporting](https://github.com/illixion/Apparition/security/advisories/new),
and not in a public issue. Say what you found, how to reproduce it and which version you ran; I'll answer as
soon as I can and credit you in the fix if you'd like.

## What is worth reporting

Apparition handles an Apple ID's sign-in, signing keys and a channel between your computer and your devices, so
these matter most:

- the Apple ID password, a signing key or a session token leaving the places they are meant to stay (the log, a
  trace, another device, the network in the clear);
- pairing or the companion's encrypted channel accepting a device that was not approved;
- the debug server, the Store or the update path accepting a request, a file or a release that was not signed or
  approved;
- a downloaded release or library being used without its hash and signature being checked.

## Releases

Every release's files carry a build attestation (`gh attestation verify <file> -R illixion/Apparition`) and are
listed in a `SHA256SUMS` signed with the release keys in [`release-signers`](release-signers). The desktop app checks
both before it installs an update. [docs/RELEASES.md](docs/RELEASES.md) describes how.

## Supported versions

Only the latest release is supported. Apple changes what it accepts without notice, so a fix often ships as a new
release rather than a patch to an old one.
