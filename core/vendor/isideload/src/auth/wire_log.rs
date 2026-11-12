//! A record of what a sign-in sends to Apple and what comes back, for finding out what Apple
//! changed. Off unless [`set_enabled`] switches it on, and then every request through the
//! GrandSlam client is written as one line, with its answer: the method and address (no query),
//! the headers, and the body's structure.
//!
//! It records the structure, not the secrets. A name is written out only when it is on a short
//! list of values that identify nothing (a protocol flag, a header Apple's clients all send, an
//! error code and Apple's own message for it, an Apple URL); every other string is its length,
//! and every key, signature and token is its size. The password-derived values of the sign-in,
//! the one-time password and machine ID of the anisette, the account, its team and the
//! tokens therefore never reach the file. What does can still name the person: Apple's error
//! text now and then repeats a team or an app ID, so the file is for reading before it is shared.
//!
//! The lines go out under [`TARGET`], which the host program routes to a file of its own
//! (apparition-core's `init_logging`), never the ordinary log.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use plist::{Dictionary, Value};
use tracing::info;

/// The `tracing` target every record is written under.
pub const TARGET: &str = "apparition::wire";

static ON: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Switches the record on or off for every client in the process.
pub fn set_enabled(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Longest a record is written, in bytes. A sign-in's biggest request is a few kilobytes.
const LONGEST: usize = 16 * 1024;
/// Longest piece of an error page that is quoted.
const PAGE: usize = 600;

/// Names whose string values are written out whole, in a plist, a JSON body or a header: what
/// Apple's own clients always send, and what Apple answers with to say why it refused.
const SHOWN: &[&str] = &[
    // The request's own flags and the protocol it speaks.
    "bootstrap", "icscrec", "loc", "pbe", "prkgen", "svct", "ckgen", "cou", "dc", "capp", "winprot", "wpbe", "o", "ps", "sp",
    "Version", "protocolVersion", "clientId", "userLocale",
    // The headers every client sends, and the two a provider may leave to us.
    "Content-Type", "Accept", "Connection", "Content-Length", "User-Agent", "Server", "X-Mme-Client-Info", "X-Xcode-Version",
    "X-Apple-App-Info", "X-Apple-I-MD-RINFO", "X-Apple-I-Client-Time", "X-Apple-I-TimeZone", "X-Apple-Locale",
    "X-Apple-I-Device-Configuration-Mode",
    // Why Apple refused.
    "em", "resultString", "userString", "resultCode", "ec",
];

fn shown(name: &str) -> bool {
    SHOWN.iter().any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// An Apple address with nothing that could be personal in it: the URL bag is a list of these.
fn apple_url(value: &str) -> bool {
    value.starts_with("https://")
        && value.split('/').nth(2).is_some_and(|host| host.ends_with(".apple.com"))
        && !value.contains(['?', '#', '@'])
}

fn cut(text: &str, longest: usize) -> String {
    if text.len() <= longest {
        return text.to_string();
    }
    let mut end = longest;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

fn string(name: &str, value: &str) -> String {
    // An address in a message is the person's; so is anything that reads like one.
    if (shown(name) && !value.contains('@')) || apple_url(value) {
        format!("{:?}", cut(value, 300))
    } else {
        format!("<string:{}>", value.len())
    }
}

/// A property-list value as it is written to the record: its structure, and its content where
/// the name allows it.
pub fn describe(name: &str, value: &Value) -> String {
    match value {
        Value::String(text) => string(name, text),
        Value::Data(bytes) => format!("<data:{}>", bytes.len()),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::Date(_) => "<date>".to_string(),
        Value::Array(items) => format!("[{}]", items.iter().map(|item| describe(name, item)).collect::<Vec<_>>().join(", ")),
        Value::Dictionary(entries) => describe_dictionary(entries),
        _ => "<value>".to_string(),
    }
}

pub fn describe_dictionary(entries: &Dictionary) -> String {
    format!("{{{}}}", entries.iter().map(|(key, value)| format!("{key}={}", describe(key, value))).collect::<Vec<_>>().join(", "))
}

fn describe_json(name: &str, value: &serde_json::Value) -> String {
    use serde_json::Value as Json;
    match value {
        Json::String(text) => string(name, text),
        Json::Array(items) => format!("[{}]", items.iter().map(|item| describe_json(name, item)).collect::<Vec<_>>().join(", ")),
        Json::Object(entries) => format!("{{{}}}", entries.iter().map(|(key, item)| format!("{key}={}", describe_json(key, item))).collect::<Vec<_>>().join(", ")),
        Json::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Headers, with the value of the ones that identify nothing.
pub fn describe_headers(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .iter()
        .map(|(name, value)| match value.to_str() {
            Ok(text) if !value.is_sensitive() => format!("{name}={}", string(name.as_str(), text)),
            _ => format!("{name}=<{} bytes>", value.len()),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A request or response body: a property list or JSON document as its structure, anything else
/// as its size.
fn describe_body(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "<empty>".to_string();
    }
    if let Ok(value) = plist::from_bytes::<Value>(bytes) {
        return describe("", &value);
    }
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        return describe_json("", &value);
    }
    format!("<{} bytes>", bytes.len())
}

/// The address without its query or fragment, which is where a team or a token would be.
fn address(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            parsed.to_string()
        }
        Err(_) => "<address>".to_string(),
    }
}

/// An error page, flattened onto one line.
fn page(body: &str) -> String {
    cut(&body.split_whitespace().collect::<Vec<_>>().join(" "), PAGE)
}

fn write(line: String) {
    info!(target: TARGET, "{}", cut(&line, LONGEST));
}

/// What a request's answer said, from the place that reads it: a plist's structure, or for a
/// refusal the page that came back. The middleware sees the status and headers, but the body
/// is read by whoever sent the request.
pub fn response_body(url: &str, status: u16, body: &str) {
    if !enabled() {
        return;
    }
    let shape = match plist::from_bytes::<Value>(body.as_bytes()) {
        Ok(value) => describe("", &value),
        Err(_) if (200..300).contains(&status) => format!("<{} bytes, not a property list>", body.len()),
        Err(_) => format!("page {:?}", page(body)),
    };
    write(format!("   body of {} ({status}): {shape}", address(url)));
}

/// Writes each request through the client and what came back. Added last, so it is the layer
/// nearest the network and sees every attempt the ones before it make.
#[cfg(not(feature = "wasm"))]
pub struct WireLog;

#[cfg(not(feature = "wasm"))]
#[async_trait::async_trait]
impl reqwest_middleware::Middleware for WireLog {
    async fn handle(
        &self,
        request: reqwest::Request,
        extensions: &mut http::Extensions,
        next: reqwest_middleware::Next<'_>,
    ) -> reqwest_middleware::Result<reqwest::Response> {
        if !enabled() {
            return next.run(request, extensions).await;
        }
        let ticket = NEXT.fetch_add(1, Ordering::Relaxed);
        let body = request.body().and_then(|body| body.as_bytes()).map(describe_body).unwrap_or_else(|| "<none>".to_string());
        write(format!(
            "#{ticket} -> {} {} headers[{}] body {body}",
            request.method(),
            address(request.url().as_str()),
            describe_headers(request.headers())
        ));
        let started = web_time::Instant::now();
        let result = next.run(request, extensions).await;
        let took = started.elapsed().as_millis();
        match &result {
            Ok(response) => write(format!(
                "#{ticket} <- {} in {took} ms headers[{}]",
                response.status(),
                describe_headers(response.headers())
            )),
            Err(error) => write(format!("#{ticket} <- no answer after {took} ms: {}", failure(error))),
        }
        result
    }
}

/// Why a request got no answer, without the address a reqwest error carries in its own text.
#[cfg(not(feature = "wasm"))]
fn failure(error: &reqwest_middleware::Error) -> String {
    let reqwest_middleware::Error::Reqwest(error) = error else {
        return error.to_string();
    };
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else if error.is_redirect() {
        "redirect loop"
    } else if error.is_body() {
        "body failed"
    } else if error.is_decode() {
        "undecodable answer"
    } else {
        "request failed"
    };
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        causes.push(cause.to_string());
        source = cause.source();
    }
    if causes.is_empty() { kind.to_string() } else { format!("{kind}: {}", causes.join(": ")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist_macro::plist;

    #[test]
    fn a_request_is_written_as_its_structure_and_none_of_its_secrets() {
        let request = plist!(dict {
            "Header": { "Version": "1.0.1" },
            "Request": {
                "cpd": {
                    "bootstrap": "true",
                    "X-Apple-I-MD": "the-one-time-password-must-not-appear",
                    "X-Apple-I-MD-M": "the-machine-id-must-not-appear",
                    "X-Apple-I-MD-RINFO": "17106176",
                    "X-Apple-I-Client-Time": "2026-11-12T09:30:05Z",
                    "X-Mme-Device-Id": "THE-DEVICE-ID-MUST-NOT-APPEAR"
                },
                "o": "init",
                "ps": ["s2k", "s2k_fo"],
                "u": "someone@example.com"
            }
        });
        let mut request = request;
        request.get_mut("Request").and_then(Value::as_dictionary_mut).unwrap().insert("A2k".into(), Value::Data(vec![7; 256]));
        let text = describe_dictionary(&request);
        for secret in ["one-time-password", "machine-id", "DEVICE-ID", "someone", "example.com"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        for kept in [r#"bootstrap="true""#, r#"X-Apple-I-MD-RINFO="17106176""#, r#"X-Apple-I-Client-Time="2026-11-12T09:30:05Z""#, r#"o="init""#, r#"ps=["s2k", "s2k_fo"]"#, "A2k=<data:256>", "u=<string:19>", "X-Apple-I-MD=<string:", r#"Version="1.0.1""#] {
            assert!(text.contains(kept), "{kept} missing: {text}");
        }
    }

    #[test]
    fn apples_reason_for_a_refusal_is_written_but_an_address_in_it_is_not() {
        let mut status = Dictionary::new();
        status.insert("ec".into(), Value::Integer((-20970_i64).into()));
        status.insert("em".into(), Value::String("This Apple Account can't be used here.".into()));
        let mut answer = Dictionary::new();
        answer.insert("Status".into(), Value::Dictionary(status));
        answer.insert("ed".into(), Value::String("a detail that stays out".into()));
        let text = describe_dictionary(&answer);
        assert!(text.contains("ec=-20970") && text.contains("This Apple Account can't be used here."), "{text}");
        assert!(text.contains("ed=<string:"), "{text}");
        assert_eq!(describe("em", &Value::String("account someone@example.com is locked".into())), "<string:37>");
    }

    #[test]
    fn apple_urls_are_written_and_other_strings_are_sizes() {
        assert_eq!(describe("gsService", &Value::String("https://gsa.apple.com/grandslam/GsService2".into())), r#""https://gsa.apple.com/grandslam/GsService2""#);
        for hidden in ["https://gsa.apple.com/a?token=secret", "https://example.com/a"] {
            assert_eq!(describe("x", &Value::String(hidden.into())), format!("<string:{}>", hidden.len()));
        }
    }

    #[test]
    fn an_address_loses_its_query_and_credentials() {
        assert_eq!(address("https://user:pass@developerservices2.apple.com/services/x/listTeams.action?teamId=ABC#frag"), "https://developerservices2.apple.com/services/x/listTeams.action");
        assert_eq!(address("not a url"), "<address>");
    }

    #[test]
    fn headers_show_what_every_client_sends_and_the_size_of_the_rest() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("Content-Type", "text/xml".parse().unwrap());
        headers.insert("X-Apple-GS-Token", "a-token-that-must-not-appear".parse().unwrap());
        let mut private = reqwest::header::HeaderValue::from_static("hidden");
        private.set_sensitive(true);
        headers.insert("Authorization", private);
        let text = describe_headers(&headers);
        assert!(text.contains(r#"content-type="text/xml""#), "{text}");
        assert!(text.contains("x-apple-gs-token=<string:28>"), "{text}");
        assert!(!text.contains("must not appear") && !text.contains("hidden"), "{text}");
        assert!(text.contains("authorization=<6 bytes>"), "{text}");
    }

    #[test]
    fn json_and_unknown_bodies_are_described_the_same_way() {
        assert_eq!(describe_body(br#"{"mode":"sms","phoneNumber":{"id":2},"code":"123456"}"#), r#"{code=<string:6>, mode=<string:3>, phoneNumber={id=2}}"#);
        assert_eq!(describe_body(b"\x00\x01\x02"), "<3 bytes>");
        assert_eq!(describe_body(b""), "<empty>");
    }

    #[test]
    fn an_error_page_is_one_cut_line() {
        let text = page(&format!("<html>\n  <body>\n{}\n</body></html>", "x".repeat(2000)));
        assert!(!text.contains('\n') && text.len() < PAGE + 40, "{}", text.len());
        assert!(text.starts_with("<html> <body> xxx"), "{text}");
    }
}
