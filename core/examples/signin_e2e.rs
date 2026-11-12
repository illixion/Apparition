//! End-to-end check of the product sign-in path against a third-party anisette server: password,
//! 2FA, the Xcode token, then several developer-services calls (teams, app IDs, certificates).
//! Prints only outcomes, never tokens, codes or numbers. Credentials are typed at the prompts.
//!
//!     cargo run --release --example signin_e2e --features desktop,anisette -- <anisette url> [<email>]

use apparition_core::api::{self, TwoFactorCallbackParams, TwoFactorCallbackResponse};

fn line(prompt: &str, echo: bool) -> String {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    let hidden = !echo && unsafe { libc::tcgetattr(0, &mut saved) } == 0;
    if hidden {
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut text = String::new();
    let _ = std::io::stdin().read_line(&mut text);
    if hidden {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &saved) };
        println!();
    }
    text.trim().to_string()
}

fn two_factor(params: &TwoFactorCallbackParams) -> TwoFactorCallbackResponse {
    println!("2FA asked for (sms: {}, numbers listed: {})", params.sms, params.numbers.len());
    for number in &params.numbers {
        println!("  sms id {}", number.id);
    }
    let answer = line("Two-factor code (or `sms <id>`, empty to cancel): ", false);
    match answer.split_once(' ') {
        Some(("sms", id)) => id.trim().parse().map(TwoFactorCallbackResponse::SendSms).unwrap_or(TwoFactorCallbackResponse::Abort),
        _ if answer.is_empty() => TwoFactorCallbackResponse::Abort,
        _ => TwoFactorCallbackResponse::SubmitCode(answer),
    }
}

#[tokio::main]
async fn main() {
    api::init();
    let args: Vec<String> = std::env::args().collect();
    let url = args.get(1).expect("usage: signin_e2e <anisette url> [<email>]").clone();
    let email = args.get(2).cloned().unwrap_or_else(|| line("Apple ID email: ", true));
    let password = line("Apple ID password: ", false);
    let dir = std::env::temp_dir().join(format!("apparition-signin-e2e-{}", std::process::id()));
    println!("signing in through {url}");
    let (mut session, result) = match api::sign_in(&email, password, &url, &dir, two_factor).await {
        Ok(ok) => ok,
        Err(error) => {
            println!("RESULT sign-in FAILED:\n{}", api::describe(&error));
            return;
        }
    };
    println!("RESULT sign-in OK: Xcode token issued, teams listed ({})", result["teams"].as_array().map_or(0, |teams| teams.len()));
    match api::list_app_ids(&mut session).await {
        Ok(ids) => println!("RESULT app IDs OK ({})", ids.as_array().map_or_else(|| ids.to_string().len(), |list| list.len())),
        Err(error) => println!("RESULT app IDs FAILED:\n{}", api::describe(&error)),
    }
    match api::list_certificates(&mut session).await {
        Ok(certs) => println!("RESULT certificates OK ({})", certs.as_array().map_or_else(|| certs.to_string().len(), |list| list.len())),
        Err(error) => println!("RESULT certificates FAILED:\n{}", api::describe(&error)),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
