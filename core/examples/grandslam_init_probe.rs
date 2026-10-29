//! Credential-free reproduction of the initial GrandSlam request.
//! Sends one init using this Mac's built-in anisette and a reserved .invalid account, named
//! afresh each run: Apple locks a username (-20209) after about seven wrong passwords.
//! Run with `cargo run --release --example grandslam_init_probe --features desktop,anisette`.

use apparition_core::api::{self, TwoFactorCallbackResponse};

#[tokio::main]
async fn main() {
    api::init();
    let dir = dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Apparition Installer/apple-id");
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
    let result = api::sign_in(
        &format!("probe-nobody-{stamp}@example.invalid"),
        "not-a-real-password".to_string(),
        api::BUILTIN_ANISETTE,
        &dir,
        |_| TwoFactorCallbackResponse::Abort,
    )
    .await;
    match result {
        Ok(_) => println!("unexpected success for a reserved .invalid account"),
        Err(error) => println!("{}", api::describe(&error)),
    }
}
