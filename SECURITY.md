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
- a downloaded release being used without its hash and signature being checked, or a library file you import
  without its hash (Apple's own download of the libraries is trusted for coming from Apple over HTTPS);
- a connection accepting a national interception certificate authority (below).

## Connections

Every HTTPS connection Apparition makes is checked by the operating system, as usual, and first refused when its
certificates are, or were issued by, the national root CAs that Russia and Kazakhstan ask people to install (the
list, the certificates themselves and why are in `core/vendor/isideload/src/tls.rs`), even where the device trusts
them; a connection to Apple's own servers is also refused when any CA in either country issued a certificate in it.
Such a CA can read what goes through the connection, an Apple ID password included. Only updates use a fixed set of
roots of their own (Mozilla's, through signed-update), and are signature-checked besides.

## Releases

Every release's files carry a build attestation (`gh attestation verify <file> -R illixion/Apparition`) and are
listed in a `SHA256SUMS` signed with the release keys in [`release-signers`](release-signers). The desktop app checks
the `SHA256SUMS` signature and each download's hash before it installs an update; the attestation is for you to
verify with `gh`. [docs/RELEASES.md](docs/RELEASES.md) describes how.

## Supported versions

Only the latest release is supported. Apple changes what it accepts without notice, so a fix often ships as a new
release rather than a patch to an old one.
