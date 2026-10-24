# isideload 0.5.0, vendored

From https://github.com/nab138/isideload at fc7a81a (0.5.0 on crates.io; MIT,
LICENSE alongside), used as core's `isideload` dependency by path. `readme` in Cargo.toml points
at this folder's README.md instead of the repository's. Moved up from 0bee45d (0.4.4) with
`git merge-file`, upstream's 0bee45d..fc7a81a over this copy.

Seven changes. The first, for sign-in claiming Android (core/src/claimed_platform.rs): since 2026-10-12 Apple
refuses (-80045) anisette whose platform doesn't match the claimed one, and the Android
libraries' anisette must claim Android. Such a sign-in gets no 2FA step and no Xcode token
(-22411) until the code Apple pushes to the trusted devices is checked, and the trusted-device
2FA endpoints (phone numbers, sending a code) answer 401. So `code_after_login` (a builder option
and a public field) makes `login` ask for that code once the password is accepted, check it with
`validateCode` alone, and sign in again (src/auth/builder.rs, src/auth/apple_account.rs:
`code_after_login`, `code_checked`). Core sets it only after the Xcode token is refused, since
Apple stops pushing codes once the anisette identity is trusted. Upstream 0.5.0 also asks for the
code when the password step answers hsc 409 ("code already sent"); a code taken that way counts as
checked, so `code_after_login` doesn't ask for a second one. And once Apple has taken a code, being asked
for another after signing in again ends the sign-in instead of prompting up to 15 times (from
valentinBoch's PR #6). Since about 2026-10-23 Apple answers an Android-claimed Xcode token request
-80046 and pushes no code; core reports that rather than asking (core/src/lib.rs).

A second change: the GrandSlam client also trusts Apple Root CA - G3
(src/assets/AppleRootCA-G3.cer, src/auth/grandslam.rs). gsd.apple.com, where machine
provisioning goes, chains to it rather than to Apple Root CA; macOS trusts it anyway, but Linux's
trust store doesn't, so provisioning failed there with "error sending request".

A third change: it signs with core's signer (`apparition-signer`, core/signer, over indygreg's
apple-codesign, MPL-2.0) instead of `apple-codesign-quick`, which is LGPL. Cargo.toml depends on
it by path, and src/sideload/sign.rs, cert_identity.rs and sideloader.rs import
`ProvisioningProfile`, `BundleSigningSettings`, `Signer` (was `RustCryptoCmsSigner`, whose `new`
now returns a `Result`) and `sign_bundle` from it.

A fourth change: our server refuses a saved anisette identity it doesn't know with "isn't
provisioned" (with ADI's code), not upstream's -45061, so the remote v3 provider takes any message
saying "provision" as `IncorrectlyProvisioned` too, and upstream's generator provisions again
(src/anisette/remote_v3/mod.rs). Until 0.5.0 this was our own `forget_provisioning`.

A fifth change: `sign_app` and `install_app` take the portal capability IDs to turn on for the main
App ID (`capabilities: &[String]`) instead of an `increased_memory_limit` flag, and
`add_increased_memory_limit` became `AppIdsApi::enable_capabilities`. It sends them in one JSON:API
PATCH to `v1/bundleIds/{id}`, which adds to the App ID's capabilities without removing the
existing ones (App Groups stays), and its error carries Apple's `detail`. If Apple refuses the set,
sign_app asks for each one alone, so a refused one (EXTENDED_VIRTUAL_ADDRESSING on a free team:
"not … eligible for this feature") doesn't cost the others (src/dev/app_ids.rs,
src/sideload/sideloader.rs).

A sixth change, from valentinBoch (Apparition PR #1): a tvOS app gets a tvOS profile. Without it
Apple makes an iOS one and a free team answers 8220 ("no devices from which to generate a
provisioning profile") even with the Apple TV registered. The request stays on the `ios/`
endpoint (`tvos/` answers 404) and adds `subPlatform: tvOS`. The platform comes from the app being
signed (UIDeviceFamily 3 or CFBundleSupportedPlatforms AppleTVOS), or from running on tvOS, not
from the computer signing it (src/dev/app_ids.rs, src/sideload/sideloader.rs).

A seventh change: upstream 0.5.0 fetches GrandSlam's URL bag claiming a Mac whatever the sign-in
claims ("sending the android client info gets us the wrong url bag"). With our Android claim that
makes GSA answer the first SRP request 503; the bag fetched with the real claim works. Checked
2026-10-24 with a made-up account and the built-in (Android) anisette from this Mac: 3 of 3 runs
got 503 with the Mac bag, 2 of 2 got -20970 ("entered incorrectly", past the anisette check) with
the real claim, as 0.4.4 did (src/auth/grandslam.rs).
