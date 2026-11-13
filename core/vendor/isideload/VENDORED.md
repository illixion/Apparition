# isideload 0.5.0, vendored

Source: [`nab138/isideload`](https://github.com/nab138/isideload), revision
`fc7a81a` (0.5.0 on crates.io; MIT, with its license alongside). Apparition uses
this copy by path from `core/Cargo.toml`. Its README points to this directory's
README. The copy was updated from `0bee45d` (0.4.4) with `git merge-file`.

The local changes below describe the complete maintained delta from the vendored
upstream revision. They record behavior and affected areas, not private test
captures or reverse-engineering notes.

1. **Android claim and verified-code fallback** (`core/src/claimed_platform.rs`,
   `src/auth/builder.rs`, `src/auth/apple_account.rs`): claim the platform reported
   by the anisette provider and support completing sign-in with a verified code
   when the normal token flow requires it. Includes upstream's handling of a code
   already sent and ends sign-in rather than repeatedly requesting codes.
2. **Apple Root CA - G3 trust** (`src/assets/AppleRootCA-G3.cer`,
   `src/auth/grandslam.rs`): trust the additional Apple root used by the
   provisioning service.
3. **Apparition signer integration** (`Cargo.toml`, `src/sideload/sign.rs`,
   `src/sideload/cert_identity.rs`, `src/sideload/sideloader.rs`): use the
   `apparition-signer` crate for bundle signing.
4. **Provisioning identity recovery** (`src/anisette/remote_v3/mod.rs`): treat the
   server's unrecognized saved identity response as a provisioning failure so the
   provider can provision again.
5. **Portal capability selection** (`src/dev/app_ids.rs`, `src/sideload/sideloader.rs`):
   enable requested capability identifiers together, preserving existing
   capabilities, and retry individual requests when a set is not accepted.
6. **tvOS provisioning profiles** (`src/dev/app_ids.rs`,
   `src/sideload/sideloader.rs`): request a tvOS profile when signing a tvOS app.
7. **Client-specific URL bags** (`src/auth/grandslam.rs`): fetch the URL bag for
   the client profile used by the sign-in.
8. **Developer-token client selection** (`src/auth/grandslam.rs`,
   `core/src/lib.rs`): allow the developer-token request to use its own client
   profile after the password step.
9. **Assigned portal identifiers** (`src/sideload/sideloader.rs`): accept an
   explicitly assigned App ID and preserve child bundle identifiers under it.
10. **Retry on alternate service addresses** (`src/auth/edge.rs`): retry eligible
    transient or refused requests using another resolved address while keeping
    normal host and TLS verification.
11. **GrandSlam plist media type** (`src/auth/grandslam.rs`): send the media type
    used by the supported sign-in flow.
12. **Forward provider-supplied anisette headers** (`src/anisette/mod.rs`,
    `src/anisette/remote_v3/mod.rs`): retain and forward additional response
    headers from a remote anisette provider.
13. **Developer-services plist headers** (`src/dev/developer_session.rs`): set
    `Content-Type` and `Accept` on property-list requests.
14. **Native HTTPS CONNECT proxying** (`src/auth/grandslam.rs`, `src/auth/builder.rs`):
    support CONNECT for native clients while retaining the existing WASM proxy
    behavior; require HTTPS and certificate verification for proxied requests.
15. **Windows sign-in compatibility profile** (`src/auth/grandslam.rs`): support
    the Windows-specific request profile in the developer-token flow.
16. **Scope Windows compatibility to token requests** (`src/auth/grandslam.rs`):
    keep sign-in requests on the regular profile and apply the Windows compatibility
    profile only where needed.
17. **Additional two-factor states** (`src/auth/apple_account.rs`): recognize
    phone-code and trusted-device response states and route the user to the
    corresponding verification step, including when a code has already been sent.
18. **GrandSlam service URL ends in a slash** (`src/auth/grandslam.rs`): Apple's
    edge answers a POST to exactly `/grandslam/GsService2` with a bare HTML 403 and
    accepts `/grandslam/GsService2/`, and the URL bag omits the slash. Without it
    every sign-in looked refused, whichever anisette it came from. Found by
    adriano6574 (illixion/Apparition PR #17), who also reported it as not an
    anisette problem. `core/examples/signin_e2e.rs` re-checks it.
19. **Remote anisette provider validation** (`src/anisette/remote_v3/mod.rs`):
    normalize server addresses, bound connection and request time, and reject
    incomplete provider metadata and required headers. Taken from adriano6574's
    PR #17 and reworked: the timeout allows for a provider that needs time to
    resume or start, and a missing scheme is http only for a local address.
20. **Routing info and request time in every sign-in** (`src/anisette/mod.rs`):
    send `X-Apple-I-MD-RINFO` whenever the provider supplied routing info (it
    must), not only when it also supplied other headers, and add
    `X-Apple-I-Client-Time` at request time unless the provider gave one. The
    macOS and Windows clients send both.
21. **Request record for troubleshooting** (`src/auth/wire_log.rs`, `src/auth/grandslam.rs`,
    `src/dev/developer_session.rs`): an opt-in middleware that writes each request
    through the client and its answer as structure only (names, sizes, flags,
    Apple's own error text; no secret values), under its own tracing target.
    Off unless the host enables it.
22. **Refuse national interception CAs** (`src/tls.rs`, `src/tls/`, and every client:
    `src/auth/grandslam.rs`, `src/auth/edge.rs`, `src/anisette/remote_v3/mod.rs`): each
    HTTPS client is built from `tls::client_builder()`, which runs
    rustls-platform-verifier (the operating system's check, with Apple's roots added for
    GrandSlam) behind a refusal of Russia's and Kazakhstan's national root CAs, by key and
    by name, issued-by included, and of any CA in those countries for Apple's hosts.
    Windows connections now go through it too, not native-tls, which upstream used to get
    around a platform-verifier bug with Apple's root (since fixed; checked against
    gsa.apple.com on Windows 10).
    The anisette provisioning WebSocket (`src/anisette/remote_v3/websocket.rs`) connects
    with `tls::websocket_config()`, the same check over HTTP/1.1, and tokio-tungstenite is
    built with `__rustls-tls` alone, so it carries no webpki-roots of its own.

The vendored manifest requests `zip` 9.0.5 to match the core dependency and avoid
the known parser differential addressed by that release; this is a dependency
version adjustment, not an upstream source-code change.
