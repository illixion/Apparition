//! What the server answers from this Mac's own anisette (see `native`), and from a macOS anisette VM
//! (whose runner answers the same way, in the guest).
//!
//! A Mac is provisioned by macOS already, so provisioning only takes the client's identifier and answers
//! at once with a placeholder `adi.pb` the client stores (and sends back, unused). The headers come from
//! macOS. The client's identifier still gives its own `X-Mme-Device-Id` and `X-Apple-I-MD-LU`, which
//! Apple takes alongside a Mac's headers.

use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rootcause::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

const RINFO: &str = "17106176";
/// How long the server waits for the client's next message while provisioning.
const STEP: Duration = Duration::from_secs(60);

/// What a client stores as its `adi.pb`: nothing of this Mac's, which keeps its own.
const PLACEHOLDER_ADI_PB: &[u8] = b"apparition-mac-anisette";

/// Why a Mac has no anisette to give.
pub const UNAVAILABLE: &str = "This Mac doesn't give its anisette to other programs. Apparition needs a Mac on macOS 26 or earlier, or a macOS virtual machine prepared with selfhost/macos-vm/setup.sh (see selfhost/macos-vm/README.md); on macOS 27, pair this Mac with such a VM instead.";

/// ADI's identifier for the client's 16 bytes.
fn adi_identifier(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    // The first 16 characters of the hyphenated UUID: 8 digits, a hyphen, 4 digits, a hyphen, 2.
    format!("{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..14])
}

fn identifier_from(value: &str) -> Option<String> {
    let bytes: [u8; 16] = B64.decode(value).ok()?.try_into().ok()?;
    Some(adi_identifier(&bytes))
}

pub(super) fn client_info() -> Json<Value> {
    use crate::claimed_platform::{Served, USER_AGENT};
    // AuthKit's whole set of headers, from the helper, is claimed as this Mac (see `native::client_info`):
    // a VM is refused when it claims another model. AOSKit's two headers keep the usual Mac claim, which a
    // real Mac's sign-in gets through more reliably than one that claims the Mac's own model.
    if super::native::source() == Some(super::native::Source::Helper) {
        let (client_info, user_agent) = super::native::client_info();
        return Json(json!({ "client_info": client_info, "user_agent": user_agent, "apparition_anisette": Served::Mac.name() }));
    }
    Json(json!({ "client_info": Served::Mac.client_info(), "user_agent": USER_AGENT, "apparition_anisette": Served::Mac.name() }))
}

#[derive(Deserialize)]
struct HeadersRequest {
    // The client's `identifier` and `adi_pb` come too, unused: this Mac's headers don't depend on them.
    /// Apple's resync data, sent by isideload 0.5.0 and later after GrandSlam answers hsc=434.
    #[serde(default)]
    sim: Option<String>,
}

pub(super) async fn get_headers(body: Value) -> Value {
    let request = match serde_json::from_value::<HeadersRequest>(body) {
        Ok(request) => request,
        Err(error) => return json!({ "result": "GetHeadersError", "message": format!("bad request: {error}") }),
    };
    match headers_for(request).await {
        Ok(headers) => headers,
        Err(error) => json!({ "result": "GetHeadersError", "message": format!("{error}") }),
    }
}

async fn headers_for(request: HeadersRequest) -> Result<Value, Report> {
    // Resync isn't done here, and ordinary headers in its place would be refused again. The
    // message mustn't say "provision": isideload's client reprovisions on that word.
    if request.sim.is_some() {
        bail!("Apple asked for an anisette resync (hsc 434), which this server can't do");
    }
    // This Mac's own headers, whatever the client's identity: its identifier still gives its
    // X-Mme-Device-Id and X-Apple-I-MD-LU, which Apple takes alongside them.
    let headers = tokio::task::spawn_blocking(super::native::headers).await.context("the native anisette task failed")??;
    let mut reply = json!({
        "result": "Headers",
        "X-Apple-I-MD-M": headers.machine_id,
        "X-Apple-I-MD": headers.one_time_password,
        "X-Apple-I-MD-RINFO": RINFO,
    });
    for (key, value) in headers.extra {
        reply[key] = Value::String(value);
    }
    Ok(reply)
}

/// Sends `{"result": …, …}` to the client.
async fn say(socket: &mut WebSocket, result: &str, extra: Value) -> Result<(), Report> {
    let mut message = json!({ "result": result });
    if let (Some(into), Value::Object(from)) = (message.as_object_mut(), extra) {
        into.extend(from);
    }
    socket.send(Message::text(message.to_string())).await.context("the client went away")?;
    Ok(())
}

/// The client's next message as JSON, or the reason there isn't one.
async fn hear(socket: &mut WebSocket) -> Result<Value, &'static str> {
    loop {
        match tokio::time::timeout(STEP, socket.recv()).await {
            Err(_) => return Err("Timeout"),
            Ok(None | Some(Err(_))) => return Err("Closed"),
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(text.as_str()).map_err(|_| "Closed"),
            Ok(Some(Ok(Message::Close(_)))) => return Err("Closed"),
            // Pings are answered by the library; anything else isn't part of the protocol.
            Ok(Some(Ok(_))) => {}
        }
    }
}

/// A Mac is provisioned by macOS: take the client's identifier (it keeps its own device id
/// from it) and answer with the placeholder at once, without asking Apple for anything.
pub(super) async fn provision(mut socket: WebSocket) -> Result<(), Report> {
    say(&mut socket, "GiveIdentifier", json!({})).await?;
    let reply = match hear(&mut socket).await {
        Ok(value) => value,
        Err("Timeout") => return say(&mut socket, "Timeout", json!({})).await,
        Err(_) => return Ok(()),
    };
    if reply.get("identifier").and_then(Value::as_str).and_then(identifier_from).is_none() {
        return say(&mut socket, "InvalidIdentifier", json!({})).await;
    }
    say(&mut socket, "ProvisioningSuccess", json!({ "adi_pb": B64.encode(PLACEHOLDER_ADI_PB) })).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_headers_request_may_carry_resync_data() {
        let legacy: HeadersRequest = serde_json::from_value(json!({
            "identifier": "AAAAAAAAAAAAAAAAAAAAAA==", "adi_pb": "AA=="
        })).unwrap();
        assert!(legacy.sim.is_none());
        let resync: HeadersRequest = serde_json::from_value(json!({
            "identifier": "AAAAAAAAAAAAAAAAAAAAAA==", "adi_pb": "AA==",
            "sim": "AQID", "device_id": "some-device", "md_lu": "some-local-user"
        })).unwrap();
        assert!(resync.sim.is_some());
    }

    #[test]
    fn identifier_is_the_first_sixteen_characters_of_the_uppercase_uuid() {
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe];
        assert_eq!(adi_identifier(&bytes), "01234567-89AB-CD");
        assert_eq!(adi_identifier(&bytes).len(), 16);
    }
}
