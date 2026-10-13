# isideload 0.4.4, vendored

From https://github.com/nab138/isideload at 0bee45d8cdb07ca24524a26f38fdda0dbb4e04b3 (MIT,
LICENSE alongside), used as core's `isideload` dependency by path. `readme` in Cargo.toml points
at this folder's README.md instead of the repository's.

Four changes. The first, for sign-in claiming Android (core/src/claimed_platform.rs): since 2026-10-12 Apple
refuses (-80045) anisette whose platform doesn't match the claimed one, and the Android
libraries' anisette must claim Android. Such a sign-in gets no 2FA step and no Xcode token
(-22411) until the code Apple pushes to the trusted devices is checked, and the trusted-device
2FA endpoints (phone numbers, sending a code) answer 401. So `code_after_login` (a builder option
and a public field) makes `login` ask for that code once the password is accepted, check it with
`validateCode` alone, and sign in again (src/auth/builder.rs, src/auth/apple_account.rs:
`code_after_login`, `code_checked`). Core sets it only after the Xcode token is refused, since
Apple stops pushing codes once the anisette identity is trusted.

A second change: the GrandSlam client also trusts Apple Root CA - G3
(src/assets/AppleRootCA-G3.cer, src/auth/grandslam.rs). gsd.apple.com, where machine
provisioning goes, chains to it rather than to Apple Root CA; macOS trusts it anyway, but Linux's
trust store doesn't, so provisioning failed there with "error sending request".

A third change: it signs with core's signer (`apparition-signer`, core/signer, over indygreg's
apple-codesign, MPL-2.0) instead of `apple-codesign-quick`, which is LGPL. Cargo.toml depends on
it by path, and src/sideload/sign.rs, cert_identity.rs and sideloader.rs import
`ProvisioningProfile`, `BundleSigningSettings`, `Signer` (was `RustCryptoCmsSigner`, whose `new`
now returns a `Result`) and `sign_bundle` from it.

A fourth change: a saved anisette identity the server refuses ("isn't provisioned", say one
provisioned with another server) is provisioned again, once, instead of failing every sign-in.
The remote v3 provider reports that refusal as `SideloadError::AnisetteNotProvisioned`, and
`AnisetteDataGenerator` then calls the new `AnisetteProvider::forget_provisioning` (a no-op by
default) and provisions (src/anisette/mod.rs, src/anisette/remote_v3/mod.rs).
