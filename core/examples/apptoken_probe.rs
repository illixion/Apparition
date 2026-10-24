//! Experiment: one Apple ID sign-in, then the Xcode app-token request in several variants, to see
//! what Apple's -80046 for Android-claimed sign-ins depends on. Prints only error codes and
//! messages, never tokens.
//!
//!     cargo run --example apptoken_probe --features anisette -- <email> [<anisette url>]


use apparition_core::claimed_platform::{self, ClaimedPlatform};
use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
use isideload::auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse};
use isideload::util::storage::InMemoryStorage;
use reqwest::header::{HeaderMap, HeaderValue};

const XCODE: &str = "com.apple.gs.xcode.auth";
const ANDROID_XCODE: &str = "<Pixel 8><Android; 14; UQ1A.240205.004><com.apple.AuthKit/1 (com.apple.dt.Xcode/3594.4.19)>";

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
    println!("2FA step asked for (numbers listed: {})", params.numbers.len());
    for number in &params.numbers {
        println!("  sms {}: {}", number.id, number.number_with_dial_code);
    }
    let answer = line("Two-factor code (or `sms <id>`, empty to cancel): ", true);
    match answer.split_once(' ') {
        Some(("sms", id)) => id.trim().parse().map(TwoFactorCallbackResponse::SendSms).unwrap_or(TwoFactorCallbackResponse::Abort),
        _ if answer.is_empty() => TwoFactorCallbackResponse::Abort,
        _ => TwoFactorCallbackResponse::SubmitCode(answer.chars().filter(char::is_ascii_digit).collect()),
    }
}

struct Variant {
    name: &'static str,
    app: &'static str,
    client_info: String,
    xcode_headers: bool,
    svct: Option<&'static str>,
}

/// The app-token request as isideload's `get_app_token` sends it, with the variant's changes.
/// Returns Apple's status (ec, em) or the transport error.
async fn app_token(account: &mut AppleAccount, user_agent: &str, variant: &Variant) -> String {
    let anisette = match account.anisette_generator.get_anisette_data(account.grandslam_client.clone()).await {
        Ok(anisette) => anisette,
        Err(error) => return format!("anisette failed: {error}"),
    };
    let spd = account.spd.as_ref().unwrap();
    let text = |key: &str| spd.get(key).and_then(|value| value.as_string()).unwrap_or_default().to_string();
    let data = |key: &str| spd.get(key).and_then(|value| value.as_data()).unwrap_or_default().to_vec();
    let (dsid, token, session_key, c) = (text("adsid"), text("GsIdmsToken"), data("sk"), data("c"));
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &session_key);
    let mut context = ring::hmac::Context::with_key(&key);
    context.update(b"apptokens");
    context.update(dsid.as_bytes());
    context.update(variant.app.as_bytes());
    let checksum = context.sign().as_ref().to_vec();

    let mut cpd = anisette.get_client_provided_data();
    match variant.svct {
        Some(svct) => {
            cpd.insert("svct".into(), svct.into());
        }
        None => {}
    }
    let mut request = plist::Dictionary::new();
    request.insert("app".into(), plist::Value::Array(vec![variant.app.into()]));
    request.insert("c".into(), plist::Value::Data(c));
    request.insert("checksum".into(), plist::Value::Data(checksum));
    request.insert("cpd".into(), plist::Value::Dictionary(cpd));
    request.insert("o".into(), "apptokens".into());
    request.insert("u".into(), dsid.into());
    request.insert("t".into(), token.into());
    let mut header = plist::Dictionary::new();
    header.insert("Version".into(), "1.0.1".into());
    let mut body = plist::Dictionary::new();
    body.insert("Header".into(), plist::Value::Dictionary(header));
    body.insert("Request".into(), plist::Value::Dictionary(request));
    let mut xml = Vec::new();
    plist::to_writer_xml(&mut xml, &plist::Value::Dictionary(body)).unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static("text/x-xml-plist"));
    headers.insert("Accept", HeaderValue::from_static("text/x-xml-plist"));
    headers.insert("X-Mme-Client-Info", HeaderValue::from_str(&variant.client_info).unwrap());
    headers.insert("User-Agent", HeaderValue::from_str(user_agent).unwrap());
    if variant.xcode_headers {
        headers.insert("X-Xcode-Version", HeaderValue::from_static("27.0 (27A5218g)"));
        headers.insert("X-Apple-App-Info", HeaderValue::from_static(XCODE));
    }
    headers.insert("Connection", HeaderValue::from_static("close"));

    let url = match account.grandslam_client.get_url("gsService") {
        Ok(url) => url,
        Err(error) => return format!("no gsService URL: {error}"),
    };
    let response = match account.grandslam_client.client.post(&url).headers(headers).body(xml).send().await {
        Ok(response) => response,
        Err(error) => return format!("request failed: {error}"),
    };
    let http = response.status();
    let reply = response.text().await.unwrap_or_default();
    let Ok(plist::Value::Dictionary(reply)) = plist::from_bytes::<plist::Value>(reply.as_bytes()) else {
        return format!("HTTP {http}, not a plist ({} bytes)", reply.len());
    };
    let status = reply.get("Response").and_then(|r| r.as_dictionary()).and_then(|r| r.get("Status")).and_then(|s| s.as_dictionary());
    let ec = status.and_then(|s| s.get("ec")).and_then(|v| v.as_signed_integer()).unwrap_or(0);
    let em = status.and_then(|s| s.get("em")).and_then(|v| v.as_string()).unwrap_or("");
    let has_token = reply.get("Response").and_then(|r| r.as_dictionary()).is_some_and(|r| r.contains_key("et"));
    format!("HTTP {http}, ec {ec} {em}{}", if has_token { " (token issued)" } else { "" })
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let args: Vec<String> = std::env::args().collect();
    let email = args.get(1).expect("usage: apptoken_probe <email> [<anisette url>]").clone();
    let scratch = std::env::temp_dir().join("apparition-apptoken-probe");
    let _server;
    let url = match args.get(2) {
        Some(url) => url.clone(),
        None => {
            let server = apparition_core::anisette::start(&scratch.join("anisette"), ([127, 0, 0, 1], 0).into()).await.expect("built-in anisette");
            let url = server.url();
            _server = server;
            url
        }
    };
    let claim = claimed_platform::claim_for(&url).await;
    println!("anisette {url}, claiming {} ({})", claim.served.name(), claim.client_info);
    let user_agent = claim.user_agent.clone();
    let claimed = claim.client_info.clone();
    let provider = ClaimedPlatform::new(RemoteV3AnisetteProvider::new(&url, Box::new(InMemoryStorage::new()), "0".to_string()).unwrap(), claim);

    let password = line(&format!("Apple ID password for {email}: "), false);
    let callback = |params: TwoFactorCallbackParams| {
        let response = two_factor(&params);
        async move { Ok(response) }
    };
    let mut account = match AppleAccount::builder(&email).anisette_provider(provider).login(&password, callback).await {
        Ok(account) => account,
        Err(error) => {
            println!("sign-in failed:\n{error}");
            return;
        }
    };
    drop(password);
    let mut keys: Vec<&String> = account.spd.as_ref().map(|spd| spd.keys().collect()).unwrap_or_default();
    keys.sort();
    println!("signed in; spd keys: {keys:?}");

    match account.get_app_token("xcode.auth").await {
        Ok(_) => println!("isideload get_app_token(xcode.auth): token issued"),
        Err(error) => println!("isideload get_app_token(xcode.auth): {}", error.to_string().lines().last().unwrap_or("")),
    }

    let variants = [
        Variant { name: "as isideload", app: XCODE, client_info: claimed.clone(), xcode_headers: true, svct: None },
        Variant { name: "no Xcode headers", app: XCODE, client_info: claimed.clone(), xcode_headers: false, svct: None },
        Variant { name: "Android claim naming Xcode", app: XCODE, client_info: ANDROID_XCODE.to_string(), xcode_headers: true, svct: None },
        Variant { name: "Mac claim", app: XCODE, client_info: claimed_platform::MAC_CLIENT_INFO.to_string(), xcode_headers: true, svct: None },
        Variant { name: "svct Xcode", app: XCODE, client_info: claimed.clone(), xcode_headers: true, svct: Some("Xcode") },
        Variant { name: "iCloud token", app: "com.apple.gs.icloud.auth", client_info: claimed.clone(), xcode_headers: false, svct: None },
        Variant { name: "iTunes token", app: "com.apple.gs.itunes.auth", client_info: claimed.clone(), xcode_headers: false, svct: None },
    ];
    for variant in &variants {
        println!("{:<28} {}", variant.name, app_token(&mut account, &user_agent, variant).await);
    }
}
