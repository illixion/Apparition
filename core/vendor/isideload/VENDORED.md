# isideload 0.5.0, vendored

From https://github.com/nab138/isideload at fc7a81a (0.5.0 on crates.io; MIT,
LICENSE alongside), used as core's `isideload` dependency by path. `readme` in Cargo.toml points
at this folder's README.md instead of the repository's. Moved up from 0bee45d (0.4.4) with
`git merge-file`, upstream's 0bee45d..fc7a81a over this copy.

Eleven changes. The first, for sign-in claiming Android (core/src/claimed_platform.rs): since 2026-10-12 Apple
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
valentinBoch's PR #6). Since about 2026-10-22 Apple answers an Android-claimed Xcode token request
-80046 and pushes no code; the eighth change gets the token anyway.

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

An eighth change: `GrandSlam::claiming` gives the same client and URL bag claiming other client
info (src/auth/grandslam.rs). Apple refuses the Xcode token to a sign-in claiming Android
(-80046) but issues it to the same sign-in when the token request claims a Mac, so core swaps the
account's client for a Mac-claiming one once the password is accepted, for the token and the
developer requests (core/src/lib.rs, `Claim::after_sign_in`). Checked 2026-10-24 on one account
with the built-in Android anisette: the token request claiming Android got -80046 whatever else
changed, claiming a Mac got the token, and the developer services then listed the certificates.

A ninth change: `sign_app` accepts an optional assigned portal identifier, used instead of
appending the team suffix. Special-app detection happens before renaming, and child bundle
identifiers keep their suffixes under the assigned parent. Desktop records assignments
before signing so registrations remain associated with an app even if installation fails.

An eleventh change: GrandSlam requests send `Content-Type: text/xml` (src/auth/grandslam.rs
`base_headers`). From about 2026-10-29 an Apple edge firewall 403s `text/x-xml-plist` on `gsd.apple.com`
and `application/x-xml-plist` on `gsa.apple.com`; `text/xml` passes both. The next paragraph's edge
retry was the same-day workaround while the rule was on some edges only, and is now a general
retry on another edge.

A tenth change: the GrandSlam client retries a request an Apple edge server refused or couldn't be reached on another
(src/auth/edge.rs, wired in `build_reqwest_client`; tokio's `net` feature off wasm). Which edge
an Apple host resolves to depends on the resolver: on 2026-10-29, from one home network, Cloudflare's
1.1.1.1 always gave `gsd.apple.com` 17.171.11.101, which answers every GrandSlam request with an
HTML 403 before Apple reads it, so sign-in failed "403 Forbidden" there. The router's resolver and
Google's gave edges that answer. The system's resolver is still used first. An edge that answers
an HTML 403 is avoided for an hour, so it isn't asked again and again, and the request goes to
another: one from the system's answer, else from DNS-over-HTTPS (Google with the asker's subnet
and without, and Cloudflare). After three refusing edges, or once every edge found is avoided, the
request fails saying Apple turned the network away and what to try. The connection still names
the host, so the certificate is checked as before. A connection to an edge that can't be made or
doesn't answer within 10 seconds gets one retry on the other edges DNS-over-HTTPS knows, for that
request only and with nothing remembered, so a retry later can succeed; an HTTP 5xx is no refusal
(GrandSlam answers an occasional 503 to a request that then goes through). Core formats these
errors plainly (`core/src/errors.rs`) and tells the apps they can be retried (`failure_kind`). `cargo test -p isideload --lib auth::edge --
--include-ignored` runs the check against Apple (the edge that refused, now answering with `text/xml`).

An eleventh change: forward every anisette header the server gives, not just the two
(`src/anisette/mod.rs`, `src/anisette/remote_v3/mod.rs`). The `Headers` reply keeps any extra
fields (`#[serde(flatten)]`) and `get_headers` sends them along with `X-Apple-I-MD-RINFO`. Since
about 2026-10-29 Apple answers a macOS virtual machine's sign-in with `-3000076` unless the
request carries the host's attestation (`x-apple-baa-*`), which AuthKit puts on the request and
Apparition's server (core/src/anisette/server.rs, from the entitled helper) now passes through.
A real Mac's anisette, which has no such headers, is unaffected.

Not a change to its code: Cargo.toml asks for `zip` 9.0.5 rather than 8.3, matching core, for the
"zip cloaking" advisory (dual-header parser differential, fixed in 9.0.5). isideload's own code
compiles unchanged against zip 9.

A twelfth change: developer-services requests send `Content-Type` and `Accept` as
`text/x-xml-plist` (`src/dev/developer_session.rs`). These requests POST XML property lists, and
Apple's `listTeams` endpoint returned HTTP 415 when the media type was omitted.
