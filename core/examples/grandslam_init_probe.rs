//! Credential-free reproduction of the initial GrandSlam request.
//! Sends one init using this Mac's built-in anisette, or the anisette v3 server given as the
//! argument, and a reserved .invalid account, named afresh each run: Apple locks a username
//! (-20209) after about seven wrong passwords. A server's identity is kept apart from the real
//! Apple ID folder. Run with
//! `cargo run --release --example grandslam_init_probe --features desktop,anisette [-- <url>]`.

use apparition_core::api::{self, TwoFactorCallbackResponse};

#[tokio::main]
async fn main() {
    api::init();
    let server = std::env::args().nth(1);
    let dir = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(if server.is_some() { "Apparition Installer/probe" } else { "Apparition Installer/apple-id" });
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let result = api::sign_in(
        &format!("probe-nobody-{stamp}@example.invalid"),
        "not-a-real-password".to_string(),
        server.as_deref().unwrap_or(api::BUILTIN_ANISETTE),
        &dir,
        |_| TwoFactorCallbackResponse::Abort,
    )
    .await;
    match result {
        Ok(_) => println!("unexpected success for a reserved .invalid account"),
        Err(error) => println!("{}", api::describe(&error)),
    }
}
