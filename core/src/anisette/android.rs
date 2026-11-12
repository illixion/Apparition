//! Anisette provider backed by Apple's Android libraries, loaded in Apparition's process.
//! The provider fetches the libraries on first use, keeps them in the anisette data directory,
//! and gives each request its own working directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::ws::{Message, WebSocket};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use rootcause::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use super::adi::Adi;
use super::apk;

const RINFO: &str = "17106176";
/// How long the server waits for the client's next message while provisioning.
const STEP: Duration = Duration::from_secs(60);

/// The loaded libraries and where a request's scratch folders go.
pub(super) struct Android {
    adi: Arc<Adi>,
    work: PathBuf,
}

/// The libraries are loaded once per process, whatever number of servers use them: they keep
/// state of their own, and every call goes through one thread anyway.
async fn shared_adi(data_dir: &Path) -> Result<Arc<Adi>, Report> {
    static ADI: tokio::sync::OnceCell<Arc<Adi>> = tokio::sync::OnceCell::const_new();
    ADI.get_or_try_init(|| async {
        let (services, core) = apk::libraries(&data_dir.join("libraries")).await?;
        let work = data_dir.join("work");
        std::fs::create_dir_all(&work)?;
        Ok(Arc::new(Adi::start(services, core, &work)?))
    })
    .await
    .cloned()
}

impl Android {
    /// Fetches the libraries if they aren't in `data_dir` yet, and loads them (once per process).
    pub(super) async fn new(data_dir: &Path) -> Result<Android, Report> {
        let adi = shared_adi(data_dir).await?;
        let work = data_dir.join("work");
        std::fs::create_dir_all(&work)?;
        Ok(Android { adi, work })
    }
}

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

/// A folder only this request uses.
struct Scratch(PathBuf);

impl Scratch {
    fn new(work: &Path) -> Result<Scratch, Report> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let name = format!("{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed));
        let path = work.join(name);
        std::fs::create_dir_all(&path)?;
        Ok(Scratch(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What this server serves, with the client info the sign-in claims (see claimed_platform).
pub(super) fn client_info() -> Json<Value> {
    use crate::claimed_platform::{Served, USER_AGENT};
    Json(json!({ "client_info": Served::Android.client_info(), "user_agent": USER_AGENT, "apparition_anisette": Served::Android.name() }))
}

#[derive(Deserialize)]
struct HeadersRequest {
    identifier: String,
    adi_pb: String,
    /// Apple's resync data, sent by isideload 0.5.0 and later after GrandSlam answers hsc=434.
    #[serde(default)]
    sim: Option<String>,
}

pub(super) async fn get_headers(android: &Android, body: Value) -> Value {
    let request = match serde_json::from_value::<HeadersRequest>(body) {
        Ok(request) => request,
        Err(error) => return json!({ "result": "GetHeadersError", "message": format!("bad request: {error}") }),
    };
    match headers_for(android, request).await {
        Ok(headers) => headers,
        Err(error) => json!({ "result": "GetHeadersError", "message": format!("{error}") }),
    }
}

async fn headers_for(android: &Android, request: HeadersRequest) -> Result<Value, Report> {
    // Resync isn't done here, and ordinary headers in its place would be refused again. The
    // message mustn't say "provision": isideload's client reprovisions on that word.
    if request.sim.is_some() {
        bail!("Apple asked for an anisette resync (hsc 434), which this server can't do");
    }
    let identifier = identifier_from(&request.identifier).ok_or_else(|| report!("the identifier isn't 16 bytes of base64"))?;
    let adi_pb = B64.decode(&request.adi_pb).context("adi_pb isn't base64")?;
    let scratch = Scratch::new(&android.work)?;
    std::fs::write(scratch.0.join("adi.pb"), adi_pb)?;
    let folder = scratch.0.clone();
    let otp = android
        .adi
        .run(move |adi| -> Result<_, Report> {
            adi.select(&identifier, &folder)?;
            let code = adi.login_code();
            if code != 0 {
                bail!("this device isn't provisioned (ADI {code})");
            }
            adi.request_otp()
        })??;
    Ok(json!({
        "result": "Headers",
        "X-Apple-I-MD-M": B64.encode(otp.machine_id),
        "X-Apple-I-MD": B64.encode(otp.one_time_password),
        "X-Apple-I-MD-RINFO": RINFO,
    }))
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

fn field(value: &Value, name: &str) -> Option<Vec<u8>> {
    B64.decode(value.get(name)?.as_str()?).ok()
}

/// Provisions the client's identity, which needs two rounds with Apple that the client makes.
pub(super) async fn provision(android: &Android, mut socket: WebSocket) -> Result<(), Report> {
    macro_rules! hear {
        () => {
            match hear(&mut socket).await {
                Ok(value) => value,
                Err("Timeout") => return say(&mut socket, "Timeout", json!({})).await,
                Err(_) => return Ok(()),
            }
        };
    }

    say(&mut socket, "GiveIdentifier", json!({})).await?;
    let reply = hear!();
    let Some(identifier) = reply.get("identifier").and_then(Value::as_str).and_then(identifier_from) else {
        return say(&mut socket, "InvalidIdentifier", json!({})).await;
    };
    let scratch = Scratch::new(&android.work)?;

    say(&mut socket, "GiveStartProvisioningData", json!({})).await?;
    let reply = hear!();
    let Some(spim) = field(&reply, "spim") else {
        return say(&mut socket, "StartProvisioningError", json!({ "message": "no spim" })).await;
    };
    let (select, folder) = (identifier.clone(), scratch.0.clone());
    let started = android
        .adi
        .run(move |adi| -> Result<_, Report> {
            adi.select(&select, &folder)?;
            adi.start_provisioning(&spim)
        })?;
    let (cpim, session) = match started {
        Ok(started) => started,
        Err(error) => return say(&mut socket, "StartProvisioningError", json!({ "message": format!("{error}") })).await,
    };

    say(&mut socket, "GiveEndProvisioningData", json!({ "cpim": B64.encode(cpim) })).await?;
    let reply = hear!();
    let (Some(ptm), Some(tk)) = (field(&reply, "ptm"), field(&reply, "tk")) else {
        let _ = android.adi.run(move |adi| adi.abandon(session));
        return say(&mut socket, "EndProvisioningError", json!({ "message": "no ptm and tk" })).await;
    };
    let folder = scratch.0.clone();
    let ended = android
        .adi
        .run(move |adi| -> Result<_, Report> {
            // Another request may have pointed the library elsewhere since.
            adi.select(&identifier, &folder)?;
            adi.end_provisioning(session, &ptm, &tk)
        })?;
    if let Err(error) = ended {
        return say(&mut socket, "EndProvisioningError", json!({ "message": format!("{error}") })).await;
    }

    let adi_pb = std::fs::read(scratch.0.join("adi.pb")).context("provisioning left no adi.pb")?;
    say(&mut socket, "ProvisioningSuccess", json!({ "adi_pb": B64.encode(adi_pb) })).await
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
