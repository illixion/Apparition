//! Account-free probe: does Apple's GrandSlam accept Apparition's built-in (Android) anisette?
//!
//! Starts the built-in anisette server, claims Android (Apparition's production path for the
//! Android libraries), provisions a fresh identity, and fires a GrandSlam `init` with a reserved
//! `@example.invalid` address. No real account. The init's result tells us whether the anisette
//! and cpd got past Apple's checks (it then fails on the dummy password) or were rejected up front
//! — the signal for whether the `X-Mme-Legacy-Device-Id` / `X-Apple-I-MD-LU` cpd fields this build
//! now sends are what Apple began requiring in October 2026.
//!
//! Prerequisites (why it can't just run on the dev Mac):
//! - Clean Apple egress: `gsd.apple.com` rate-limits a heavily-used IP with HTTP 403, and refuses
//!   datacenter IPs — so a residential network that hasn't been hammered today.
//! - Android mode: on macOS `anisette::start` auto-selects the Mac's native AuthKit, not the
//!   Android libraries. Run this on Linux (where native is never available) to exercise the
//!   Android path, or add a switch to force `native = false`.
//!
//! Run: `cargo run --example init_probe --features anisette`

use apparition_core::claimed_platform::{ANDROID_CLIENT_INFO, Claim, ClaimedPlatform, Served, USER_AGENT};
use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
use isideload::auth::apple_account::{AppleAccount, TwoFactorCallbackResponse};
use isideload::util::storage::InMemoryStorage;

const LIB_DIR: &str = "/tmp/apparition-adi-android-research";

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let server = apparition_core::anisette::start(std::path::Path::new(LIB_DIR), ([127, 0, 0, 1], 0).into())
        .await
        .expect("start built-in anisette server");
    println!("server native mode: {} (Android path needs this false)", server.native);
    let url = server.url();

    let claim = Claim { served: Served::Android, client_info: ANDROID_CLIENT_INFO.to_string(), user_agent: USER_AGENT.to_string() };
    let provider = ClaimedPlatform::new(
        RemoteV3AnisetteProvider::new(&url, Box::new(InMemoryStorage::new()), "0".to_string()).unwrap(),
        claim,
    );

    let email = format!(
        "probe-{}@example.invalid",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    );
    println!("GrandSlam init for {email} ...");
    let response = AppleAccount::builder(&email)
        .anisette_provider(provider)
        .login("not-a-real-password", |_| async { Ok(TwoFactorCallbackResponse::Abort) })
        .await;
    match response {
        Ok(_) => println!("LOGIN: unexpected success"),
        Err(e) => println!("LOGIN result (read the GrandSlam ec / error below):\n{e:?}"),
    }
}
