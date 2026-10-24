//! Handing the Apple ID's certificate key to Apparition on a nearby device or computer over the
//! local network, checked by a six-digit code both screens show. share.rs says why the key is
//! shared at all; its sealed files, opened with five words, are for every other way of sending it.
//!
//! The install that needs the key (the receiver) advertises `_apparition-key._tcp` and listens;
//! the one that has it (the sender) connects. It is numeric comparison, as Bluetooth pairing and
//! Longwave's companion pairing do it. The sender commits to its X25519 key and a nonce before it
//! sees the receiver's, and reveals them only after. Something in between would have had to fix
//! its keys on each side before learning what the other side contributes, so its two codes match
//! by chance one time in a million. Both screens show a code drawn from the whole exchange, and
//! each person confirms on their own device that the two match: the sender's before the key
//! leaves, the receiver's before it is used. The sender seals the signing file
//! (`.apparitionsigning`) with the agreed key, and the receiver imports it.
//!
//! Messages are JSON, each after its length (four bytes, little-endian), on a plain TCP
//! connection: what needs protecting is protected by the exchange. The app does the networking
//! itself (Network.framework, for Bonjour and the Local Network prompt) and runs the exchange here
//! through `apparition_nearby_*`; the command line and the desktop app use `net` below.

use std::{
    collections::HashMap,
    ffi::c_char,
    path::PathBuf,
    sync::{LazyLock, Mutex, atomic::{AtomicU64, Ordering}},
};

use base64::prelude::*;
use ring::{
    aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey},
    agreement::{EphemeralPrivateKey, UnparsedPublicKey, X25519, agree_ephemeral},
    hkdf::{HKDF_SHA256, Salt},
    rand::{SecureRandom, SystemRandom},
};
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{SESSION, reply, text};

const VERSION: u32 = 1;
/// The largest message either side takes: a signing file is about 3 KB.
pub const MAX_MESSAGE: usize = 16 * 1024;
const TRANSCRIPT_TAG: &[u8] = b"Apparition-Nearby-Signing-v1";
const KEY_INFO: &[u8] = b"Apparition-Nearby-Signing-Key-v1";

/// What travels, in order: sender `commit`, receiver `hello`, sender `reveal`; then, each once
/// its person has compared the codes, sender `key` and receiver `done`. Either side may send
/// `cancel` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Message {
    /// `commitment`: SHA-256 of the sender's public key and nonce, revealed later.
    Commit { version: u32, name: String, commitment: String },
    Hello { version: u32, name: String, public_key: String, nonce: String },
    Reveal { public_key: String, nonce: String },
    /// The signing file, sealed with the agreed key.
    Key { sealed: String },
    /// The receiver used the key, or `error` says why it couldn't.
    Done { error: Option<String> },
    Cancel { reason: Option<String> },
}

impl Message {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(body: &[u8]) -> Result<Message, Report> {
        if body.len() > MAX_MESSAGE {
            bail!("The other device sent too much");
        }
        serde_json::from_slice(body).map_err(|_| report!("The other device isn't speaking Apparition's nearby signing (or a newer version of it)"))
    }
}

/// Where an exchange is, for the screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    /// Waiting for the other device's next message.
    #[default]
    Waiting,
    /// Show `code` and ask whether the other device shows the same.
    Compare,
    /// This person said the codes match; waiting for the other device's.
    Confirmed,
    /// The key was handed over and used.
    Done,
    /// Someone said the codes differ, or the other device couldn't use the key: `reason` says.
    Cancelled,
}

/// What a message or a person's answer changed: messages to send, in order, and what to show.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Step {
    #[serde(skip)]
    pub send: Vec<Message>,
    pub state: State,
    /// Six digits, once both keys are known.
    pub code: Option<String>,
    /// What the other device calls itself (unchecked until the codes match).
    pub peer: Option<String>,
    /// The receiver's import (`{appleId, from, replaced}`, as share.rs returns it), once done.
    pub result: Option<Value>,
    pub reason: Option<String>,
}

fn random<const N: usize>() -> Result<[u8; N], Report> {
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).map_err(|_| report!("No random numbers on this system"))?;
    Ok(bytes)
}

fn decode<const N: usize>(text: &str) -> Result<[u8; N], Report> {
    BASE64_STANDARD
        .decode(text)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| report!("The other device sent a damaged key"))
}

fn commitment(public_key: &[u8], nonce: &[u8]) -> [u8; 32] {
    Sha256::new().chain_update(public_key).chain_update(nonce).finalize().into()
}

/// A display name as the other side gave it: one line, at most 80 characters.
fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(80).collect();
    match name.trim() {
        "" => "Apparition".to_string(),
        name => name.to_string(),
    }
}

/// One side's key pair and nonce.
struct Half {
    private: Option<EphemeralPrivateKey>,
    public: [u8; 32],
    nonce: [u8; 32],
}

impl Half {
    fn new() -> Result<Half, Report> {
        let private = EphemeralPrivateKey::generate(&X25519, &SystemRandom::new()).map_err(|_| report!("Couldn't make a key"))?;
        let public = private.compute_public_key().map_err(|_| report!("Couldn't make a key"))?;
        let public: [u8; 32] = public.as_ref().try_into().map_err(|_| report!("Couldn't make a key"))?;
        Ok(Half { private: Some(private), public, nonce: random()? })
    }
}

/// The code to compare and the key both ends now share.
struct Agreed {
    code: String,
    key: LessSafeKey,
    transcript: [u8; 32],
}

/// Runs on both ends once the sender has revealed. Fails when the revealed key and nonce aren't
/// what the sender committed to.
fn agree(mine: &mut Half, theirs: &[u8; 32], commitment_sent: &[u8; 32], receiver: (&[u8; 32], &[u8; 32]), sender: (&[u8; 32], &[u8; 32])) -> Result<Agreed, Report> {
    if commitment(sender.0, sender.1) != *commitment_sent {
        bail!("The other device's key isn't the one it committed to");
    }
    let transcript: [u8; 32] = Sha256::new()
        .chain_update(TRANSCRIPT_TAG)
        .chain_update(commitment_sent)
        .chain_update(receiver.0)
        .chain_update(receiver.1)
        .chain_update(sender.0)
        .chain_update(sender.1)
        .finalize()
        .into();
    let private = mine.private.take().ok_or_else(|| report!("This exchange already agreed a key"))?;
    let mut key = [0u8; 32];
    agree_ephemeral(private, &UnparsedPublicKey::new(&X25519, theirs), |shared| {
        Salt::new(HKDF_SHA256, &transcript).extract(shared).expand(&[KEY_INFO], HKDF_SHA256).and_then(|okm| okm.fill(&mut key))
    })
    .map_err(|_| report!("The other device sent a key that doesn't work"))?
    .map_err(|_| report!("Couldn't derive the key"))?;
    let key = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key).map_err(|_| report!("Couldn't set up the cipher"))?);
    let number = u32::from_be_bytes([transcript[0], transcript[1], transcript[2], transcript[3]]) % 1_000_000;
    Ok(Agreed { code: format!("{number:06}"), key, transcript })
}

/// The install that has the key: it connects to the receiver.
pub struct Sender {
    payload: Vec<u8>,
    half: Half,
    commitment: [u8; 32],
    agreed: Option<Agreed>,
    peer: Option<String>,
    state: State,
}

impl Sender {
    /// `payload` is the signing file to hand over (`share::export_apple_id_key`). Returns the
    /// sender and its first message.
    pub fn new(name: &str, payload: Vec<u8>) -> Result<(Sender, Message), Report> {
        let half = Half::new()?;
        let commitment = commitment(&half.public, &half.nonce);
        let first = Message::Commit { version: VERSION, name: clean_name(name), commitment: BASE64_STANDARD.encode(commitment) };
        Ok((Sender { payload, half, commitment, agreed: None, peer: None, state: State::Waiting }, first))
    }

    fn step(&self, send: Vec<Message>) -> Step {
        Step { send, state: self.state, code: self.agreed.as_ref().map(|a| a.code.clone()), peer: self.peer.clone(), ..Step::default() }
    }

    pub fn handle(&mut self, message: Message) -> Result<Step, Report> {
        match (self.state, message) {
            (State::Done | State::Cancelled, _) => bail!("This exchange is over"),
            (_, Message::Cancel { reason }) => {
                self.state = State::Cancelled;
                Ok(Step { reason: Some(reason.unwrap_or_else(|| "The other device cancelled".into())), ..self.step(vec![]) })
            }
            (State::Waiting, Message::Hello { version, name, public_key, nonce }) if self.agreed.is_none() => {
                if version != VERSION {
                    bail!("The other device runs another version of Apparition's nearby signing ({version}); update both");
                }
                let (public_key, nonce) = (decode::<32>(&public_key)?, decode::<32>(&nonce)?);
                let (mine, sent) = (self.half.public, self.half.nonce);
                self.agreed = Some(agree(&mut self.half, &public_key, &self.commitment, (&public_key, &nonce), (&mine, &sent))?);
                self.peer = Some(clean_name(&name));
                self.state = State::Compare;
                Ok(self.step(vec![Message::Reveal { public_key: BASE64_STANDARD.encode(mine), nonce: BASE64_STANDARD.encode(sent) }]))
            }
            (State::Confirmed, Message::Done { error: None }) => {
                self.state = State::Done;
                Ok(self.step(vec![]))
            }
            (State::Confirmed, Message::Done { error: Some(error) }) => {
                self.state = State::Cancelled;
                Ok(Step { reason: Some(format!("{} couldn't use the key: {error}", self.peer.as_deref().unwrap_or("The other device"))), ..self.step(vec![]) })
            }
            _ => bail!("The other device sent something out of turn"),
        }
    }

    /// The person compared the codes: seals and sends the key when they match.
    pub fn confirm(&mut self, matches: bool) -> Result<Step, Report> {
        let agreed = self.agreed.as_ref().filter(|_| self.state == State::Compare).ok_or_else(|| report!("There's no code to compare yet"))?;
        if !matches {
            self.state = State::Cancelled;
            let reason = "The codes didn't match, so the key wasn't sent".to_string();
            return Ok(Step { reason: Some(reason.clone()), ..self.step(vec![Message::Cancel { reason: Some(reason) }]) });
        }
        let nonce = random::<12>()?;
        let mut sealed = self.payload.clone();
        agreed
            .key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(agreed.transcript), &mut sealed)
            .map_err(|_| report!("Couldn't seal the key"))?;
        let mut framed = nonce.to_vec();
        framed.extend(sealed);
        self.state = State::Confirmed;
        Ok(self.step(vec![Message::Key { sealed: BASE64_STANDARD.encode(framed) }]))
    }
}

/// Uses a signing file the receiver was handed (share::import_signing_file and what goes with it).
pub type Import = Box<dyn FnMut(&[u8]) -> Result<Value, Report> + Send>;

/// The install that needs the key: it listens.
pub struct Receiver {
    name: String,
    half: Half,
    commitment: Option<[u8; 32]>,
    agreed: Option<Agreed>,
    peer: Option<String>,
    confirmed: bool,
    /// The opened signing file, until this person confirms.
    payload: Option<Vec<u8>>,
    import: Import,
    state: State,
}

impl Receiver {
    pub fn new(name: &str, import: Import) -> Result<Receiver, Report> {
        Ok(Receiver {
            name: clean_name(name),
            half: Half::new()?,
            commitment: None,
            agreed: None,
            peer: None,
            confirmed: false,
            payload: None,
            import,
            state: State::Waiting,
        })
    }

    fn step(&self, send: Vec<Message>) -> Step {
        Step { send, state: self.state, code: self.agreed.as_ref().map(|a| a.code.clone()), peer: self.peer.clone(), ..Step::default() }
    }

    pub fn handle(&mut self, message: Message) -> Result<Step, Report> {
        match (self.state, message) {
            (State::Done | State::Cancelled, _) => bail!("This exchange is over"),
            (_, Message::Cancel { reason }) => {
                self.state = State::Cancelled;
                self.payload = None;
                Ok(Step { reason: Some(reason.unwrap_or_else(|| "The other device cancelled".into())), ..self.step(vec![]) })
            }
            (State::Waiting, Message::Commit { version, name, commitment }) if self.commitment.is_none() => {
                if version != VERSION {
                    bail!("The other device runs another version of Apparition's nearby signing ({version}); update both");
                }
                self.commitment = Some(decode::<32>(&commitment)?);
                self.peer = Some(clean_name(&name));
                let hello = Message::Hello {
                    version: VERSION,
                    name: self.name.clone(),
                    public_key: BASE64_STANDARD.encode(self.half.public),
                    nonce: BASE64_STANDARD.encode(self.half.nonce),
                };
                Ok(self.step(vec![hello]))
            }
            (State::Waiting, Message::Reveal { public_key, nonce }) if self.agreed.is_none() => {
                let committed = self.commitment.ok_or_else(|| report!("The other device revealed a key before committing to one"))?;
                let (public_key, nonce) = (decode::<32>(&public_key)?, decode::<32>(&nonce)?);
                let (mine, sent) = (self.half.public, self.half.nonce);
                self.agreed = Some(agree(&mut self.half, &public_key, &committed, (&mine, &sent), (&public_key, &nonce))?);
                self.state = State::Compare;
                Ok(self.step(vec![]))
            }
            (State::Compare | State::Confirmed, Message::Key { sealed }) if self.payload.is_none() => {
                let agreed = self.agreed.as_ref().ok_or_else(|| report!("The other device sent a key before agreeing one"))?;
                let framed = BASE64_STANDARD.decode(sealed).map_err(|_| report!("The other device sent a damaged key"))?;
                if framed.len() < 12 {
                    bail!("The other device sent a damaged key");
                }
                let (nonce, sealed) = framed.split_at(12);
                let mut sealed = sealed.to_vec();
                let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| report!("The other device sent a damaged key"))?;
                let plain = agreed
                    .key
                    .open_in_place(nonce, Aad::from(agreed.transcript), &mut sealed)
                    .map_err(|_| report!("The key from the other device doesn't open with the agreed key"))?;
                self.payload = Some(plain.to_vec());
                if self.confirmed { self.finish() } else { Ok(self.step(vec![])) }
            }
            _ => bail!("The other device sent something out of turn"),
        }
    }

    /// The person compared the codes. When they match, the key is used as soon as it arrives.
    pub fn confirm(&mut self, matches: bool) -> Result<Step, Report> {
        if self.agreed.is_none() || self.state != State::Compare {
            bail!("There's no code to compare yet");
        }
        if !matches {
            self.state = State::Cancelled;
            self.payload = None;
            let reason = "The codes didn't match, so the key wasn't used".to_string();
            return Ok(Step { reason: Some(reason.clone()), ..self.step(vec![Message::Cancel { reason: Some(reason) }]) });
        }
        self.confirmed = true;
        if self.payload.is_some() {
            return self.finish();
        }
        self.state = State::Confirmed;
        Ok(self.step(vec![]))
    }

    fn finish(&mut self) -> Result<Step, Report> {
        let payload = self.payload.take().ok_or_else(|| report!("No key yet"))?;
        match (self.import)(&payload) {
            Ok(result) => {
                self.state = State::Done;
                Ok(Step { result: Some(result), ..self.step(vec![Message::Done { error: None }]) })
            }
            Err(error) => {
                self.state = State::Cancelled;
                let text = crate::errors::describe(&error);
                Ok(Step { reason: Some(text.clone()), ..self.step(vec![Message::Done { error: Some(text) }]) })
            }
        }
    }
}

// MARK: C ABI, for the app (it does the networking)

enum Exchange {
    Sender(Sender),
    Receiver(Receiver),
}

static EXCHANGES: LazyLock<Mutex<HashMap<u64, Exchange>>> = LazyLock::new(Default::default);
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn step_reply(step: Step) -> Value {
    let send: Vec<String> = step.send.iter().map(Message::to_json).collect();
    let mut value = serde_json::to_value(&step).unwrap_or_default();
    value["send"] = json!(send);
    value
}

fn with_exchange(handle: u64, body: impl FnOnce(&mut Exchange) -> Result<Step, Report>) -> Result<Value, Report> {
    let mut exchanges = EXCHANGES.lock().unwrap();
    let exchange = exchanges.get_mut(&handle).ok_or_else(|| report!("That exchange has ended"))?;
    body(exchange).map(step_reply)
}

/// Starts handing the Apple ID's certificate key to a nearby receiver, as `name` ("Apparition on
/// Ixion's iPhone"). Returns `{ok, handle, send: [message]}`: send each, framed, once connected.
/// `team_id` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_nearby_send_begin(data_dir: *const c_char, email: *const c_char, team_id: *const c_char, name: *const c_char) -> *mut c_char {
    reply((|| {
        let data_dir = PathBuf::from(unsafe { text(data_dir) }?);
        let email = unsafe { text(email) }?;
        let team_id = unsafe { crate::identity::optional(team_id) }?;
        let name = unsafe { text(name) }?;
        let payload = crate::share::export_apple_id_key(&data_dir, &email, team_id.as_deref(), &name)?;
        let (sender, first) = Sender::new(&name, payload)?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        EXCHANGES.lock().unwrap().insert(handle, Exchange::Sender(sender));
        Ok(json!({ "handle": handle, "send": [first.to_json()], "state": State::Waiting }))
    })())
}

/// Starts waiting for a nearby sender's key, as `name`. A key the person accepts is imported into
/// `<data_dir>/storage.json` and ends the signed-in session, as `apparition_import_signing_file`
/// does. Returns `{ok, handle}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_nearby_receive_begin(data_dir: *const c_char, name: *const c_char) -> *mut c_char {
    reply((|| {
        let data_dir = PathBuf::from(unsafe { text(data_dir) }?);
        let name = unsafe { text(name) }?;
        let import: Import = Box::new(move |file: &[u8]| {
            *SESSION.lock().unwrap() = None;
            crate::share::import_signing_file(&data_dir, file)
        });
        let receiver = Receiver::new(&name, import)?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        EXCHANGES.lock().unwrap().insert(handle, Exchange::Receiver(receiver));
        Ok(json!({ "handle": handle, "state": State::Waiting }))
    })())
}

/// A message from the other device (one frame's JSON). Returns `{ok, send: [message], state,
/// code, peer, result, reason}`; an error means the other device broke the exchange: close it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_nearby_handle(handle: u64, message: *const c_char) -> *mut c_char {
    reply((|| {
        let message = Message::from_json(unsafe { text(message) }?.as_bytes())?;
        with_exchange(handle, |exchange| match exchange {
            Exchange::Sender(sender) => sender.handle(message),
            Exchange::Receiver(receiver) => receiver.handle(message),
        })
    })())
}

/// The person's answer to "does the other device show the same code?". Returns what
/// `apparition_nearby_handle` does.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_nearby_confirm(handle: u64, matches: bool) -> *mut c_char {
    reply(with_exchange(handle, |exchange| match exchange {
        Exchange::Sender(sender) => sender.confirm(matches),
        Exchange::Receiver(receiver) => receiver.confirm(matches),
    }))
}

/// Forgets an exchange (and any key it was holding).
#[unsafe(no_mangle)]
pub extern "C" fn apparition_nearby_end(handle: u64) {
    EXCHANGES.lock().unwrap().remove(&handle);
}

// MARK: The network, for the command line and the desktop app

#[cfg(feature = "desktop")]
pub mod net {
    use std::{
        net::{IpAddr, SocketAddr},
        path::PathBuf,
        sync::Arc,
        time::Duration,
    };

    use rootcause::prelude::*;
    use serde::Serialize;
    use serde_json::Value;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    use super::{MAX_MESSAGE, Message, Receiver, Sender, State, Step};

    /// The Bonjour service a receiver advertises (the app's NSBonjourServices lists it too).
    pub const SERVICE: &str = "_apparition-key._tcp";

    /// Asks the person whether the other device (`peer`) shows `code`. Called off the runtime, so
    /// it may block on a prompt.
    pub type Compare = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

    /// How long to wait for the other device, whose person may still be comparing.
    const PATIENCE: Duration = Duration::from_secs(300);

    fn service_type() -> String {
        format!("{SERVICE}.local.")
    }

    async fn read(stream: &mut TcpStream) -> Result<Message, Report> {
        let length = tokio::time::timeout(PATIENCE, stream.read_u32_le())
            .await
            .map_err(|_| report!("The other device stopped answering"))?
            .map_err(|_| report!("The other device closed the connection"))? as usize;
        if length > MAX_MESSAGE {
            bail!("The other device sent too much");
        }
        let mut body = vec![0u8; length];
        tokio::time::timeout(PATIENCE, stream.read_exact(&mut body))
            .await
            .map_err(|_| report!("The other device stopped answering"))?
            .map_err(|_| report!("The other device closed the connection"))?;
        Message::from_json(&body)
    }

    async fn write(stream: &mut TcpStream, messages: &[Message]) -> Result<(), Report> {
        for message in messages {
            let body = message.to_json().into_bytes();
            stream.write_u32_le(body.len() as u32).await?;
            stream.write_all(&body).await?;
        }
        stream.flush().await?;
        Ok(())
    }

    async fn compare(compare: &Compare, step: &Step) -> bool {
        let (compare, code, peer) = (compare.clone(), step.code.clone().unwrap_or_default(), step.peer.clone().unwrap_or_default());
        tokio::task::spawn_blocking(move || compare(&code, &peer)).await.unwrap_or(false)
    }

    /// A receiver seen on the network.
    #[derive(Debug, Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Found {
        pub name: String,
        pub addresses: Vec<IpAddr>,
        pub port: u16,
    }

    impl Found {
        /// An IPv4 address if it has one (link-local IPv6 needs a scope), else any.
        pub fn address(&self) -> Option<SocketAddr> {
            let ip = self.addresses.iter().find(|a| a.is_ipv4()).or(self.addresses.first())?;
            Some(SocketAddr::new(*ip, self.port))
        }
    }

    /// Receivers waiting on this network, browsed for `seconds`. On macOS a program outside an
    /// app bundle multicasts only with its terminal's Local Network access (Local Network
    /// privacy), so it may find nothing there: pass the address the receiver shows instead.
    pub async fn browse(seconds: u64) -> Result<Vec<Found>, Report> {
        let daemon = mdns_sd::ServiceDaemon::new()?;
        let events = daemon.browse(&service_type())?;
        let mut found: Vec<Found> = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv_async()).await {
            if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                let name = info.get_fullname().trim_end_matches(&format!(".{}", service_type())).replace("\\032", " ");
                let addresses: Vec<IpAddr> = info.get_addresses().iter().map(|a| a.to_ip_addr()).collect();
                match found.iter_mut().find(|f| f.name == name && f.port == info.get_port()) {
                    Some(existing) => existing.addresses.extend(addresses.into_iter().filter(|a| !existing.addresses.contains(a)).collect::<Vec<_>>()),
                    None => found.push(Found { name, addresses, port: info.get_port() }),
                }
            }
        }
        let _ = daemon.stop_browse(&service_type());
        let _ = daemon.shutdown();
        Ok(found)
    }

    /// Hands `payload` (a signing file) to the receiver at `to`, as `name`. Returns the receiver's
    /// name once it has used the key.
    pub async fn send(to: SocketAddr, name: &str, payload: Vec<u8>, compare_codes: Compare) -> Result<String, Report> {
        let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(to))
            .await
            .map_err(|_| report!("{to} didn't answer"))?
            .context(format!("Couldn't reach {to}"))?;
        let (mut sender, first) = Sender::new(name, payload)?;
        write(&mut stream, &[first]).await?;
        loop {
            let step = sender.handle(read(&mut stream).await?)?;
            write(&mut stream, &step.send).await?;
            let step = match step.state {
                State::Compare => {
                    let step = sender.confirm(compare(&compare_codes, &step).await)?;
                    write(&mut stream, &step.send).await?;
                    step
                }
                _ => step,
            };
            match step.state {
                State::Done => return Ok(step.peer.unwrap_or_default()),
                State::Cancelled => bail!("{}", step.reason.unwrap_or_else(|| "Cancelled".into())),
                _ => {}
            }
        }
    }

    /// Where a receiver listens, for a sender that can't browse (to type in).
    #[derive(Debug, Clone)]
    pub struct Listening {
        pub port: u16,
        /// This computer's address on the network its default route uses.
        pub address: Option<IpAddr>,
        /// Whether mDNS took the advertisement; senders can still connect to the address.
        pub advertised: bool,
    }

    /// This computer's address toward the network (no packet is sent: a UDP socket is only
    /// pointed somewhere, and asked which address it would use).
    fn local_address() -> Option<IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        socket.local_addr().ok().map(|address| address.ip()).filter(|ip| !ip.is_unspecified())
    }

    /// Waits for a nearby sender as `name`, asks the person to compare the codes, and imports the
    /// key into `<data_dir>/storage.json`. Senders that break off before a code to compare are
    /// skipped (five at most); the first exchange that gets to one decides. Returns the import
    /// (`{appleId, from, replaced}`), or None once `stop` finishes first.
    pub async fn receive(
        data_dir: PathBuf,
        name: &str,
        listening: impl FnOnce(&Listening),
        compare_codes: Compare,
        stop: impl std::future::Future<Output = ()>,
    ) -> Result<Option<Value>, Report> {
        let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
        let port = listener.local_addr()?.port();
        let daemon = mdns_sd::ServiceDaemon::new().ok();
        // A host name of its own: the computer's is often taken (the companion's relay advertises
        // it too), and mDNS renames a taken one while senders are already resolving it.
        let host = format!("apparition-key-{port}.local.");
        let advertised = daemon.as_ref().and_then(|daemon| {
            let info = mdns_sd::ServiceInfo::new(&service_type(), name, &host, "", port, &[("v", "1")][..]).ok()?.enable_addr_auto();
            let fullname = info.get_fullname().to_string();
            daemon.register(info).ok().map(|()| fullname)
        });
        listening(&Listening { port, address: local_address(), advertised: advertised.is_some() });
        let waiting = async {
            let mut failures = 0;
            loop {
                let (mut stream, _) = listener.accept().await?;
                let directory = data_dir.clone();
                let import: super::Import = Box::new(move |file: &[u8]| crate::share::import_signing_file(&directory, file));
                let mut receiver = Receiver::new(name, import)?;
                match exchange(&mut stream, &mut receiver, &compare_codes).await {
                    Ok(Some(result)) => return Ok(result),
                    Ok(None) => {}
                    Err((error, compared)) => {
                        if compared {
                            return Err(error);
                        }
                        failures += 1;
                        tracing::warn!("a nearby sender broke off: {}", crate::errors::describe(&error));
                        if failures >= 5 {
                            return Err(error.context("Five tries broke off before a code to compare").into_dynamic());
                        }
                    }
                }
            }
        };
        let result = tokio::select! {
            result = waiting => result.map(Some),
            () = stop => Ok(None),
        };
        if let Some(daemon) = daemon {
            // Says goodbye, so senders stop listing it now rather than when their cache expires.
            if let Some(Ok(goodbye)) = advertised.map(|fullname| daemon.unregister(&fullname)) {
                let _ = tokio::time::timeout(Duration::from_secs(1), goodbye.recv_async()).await;
            }
            let _ = daemon.shutdown();
        }
        result
    }

    /// One sender's exchange. `Ok(None)`: it went away before a code; the error says whether the
    /// person had a code to compare (the exchange then decided).
    async fn exchange(stream: &mut TcpStream, receiver: &mut Receiver, compare_codes: &Compare) -> Result<Option<Value>, (Report, bool)> {
        let mut compared = false;
        loop {
            let message = match read(stream).await {
                Ok(message) => message,
                Err(_) if !compared => return Ok(None),
                Err(error) => return Err((error, true)),
            };
            let step = receiver.handle(message).map_err(|error| (error, compared))?;
            write(stream, &step.send).await.map_err(|error| (error, compared))?;
            let step = match step.state {
                State::Compare if !compared => {
                    compared = true;
                    let step = receiver.confirm(compare(compare_codes, &step).await).map_err(|error| (error, true))?;
                    write(stream, &step.send).await.map_err(|error| (error, true))?;
                    step
                }
                _ => step,
            };
            match step.state {
                State::Done => return Ok(step.result),
                State::Cancelled => return Err((report!("{}", step.reason.unwrap_or_else(|| "Cancelled".into())).into_dynamic(), true)),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_receiver(imported: std::sync::Arc<Mutex<Option<Vec<u8>>>>) -> Receiver {
        Receiver::new("Apparition on the Apple TV", Box::new(move |file: &[u8]| {
            *imported.lock().unwrap() = Some(file.to_vec());
            Ok(json!({ "appleId": "someone@example.com" }))
        }))
        .unwrap()
    }

    /// Runs `sender` and `receiver` to the point where both show a code.
    fn to_codes(sender: &mut Sender, first: Message, receiver: &mut Receiver) -> (Step, Step) {
        let hello = receiver.handle(first).unwrap();
        assert_eq!(hello.peer.as_deref(), Some("Apparition on an iPhone"));
        let sender_step = sender.handle(hello.send[0].clone()).unwrap();
        let receiver_step = receiver.handle(sender_step.send[0].clone()).unwrap();
        (sender_step, receiver_step)
    }

    #[test]
    fn hands_the_key_over_once_both_confirm() {
        let imported = std::sync::Arc::new(Mutex::new(None));
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"the signing file".to_vec()).unwrap();
        let mut receiver = new_receiver(imported.clone());
        let (sender_step, receiver_step) = to_codes(&mut sender, first, &mut receiver);
        assert_eq!(sender_step.state, State::Compare);
        assert_eq!(receiver_step.state, State::Compare);
        let code = sender_step.code.unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(receiver_step.code.as_deref(), Some(code.as_str()));
        assert_eq!(sender_step.peer.as_deref(), Some("Apparition on the Apple TV"));

        // The key arrives before the receiver's person has confirmed: it waits, unused.
        let key = sender.confirm(true).unwrap();
        assert_eq!(key.state, State::Confirmed);
        let waiting = receiver.handle(key.send[0].clone()).unwrap();
        assert_eq!(waiting.state, State::Compare);
        assert!(imported.lock().unwrap().is_none());
        let done = receiver.confirm(true).unwrap();
        assert_eq!(done.state, State::Done);
        assert_eq!(imported.lock().unwrap().as_deref(), Some(&b"the signing file"[..]));
        assert_eq!(done.result.unwrap()["appleId"], "someone@example.com");
        assert_eq!(sender.handle(done.send[0].clone()).unwrap().state, State::Done);
    }

    #[test]
    fn a_receiver_that_confirms_first_uses_the_key_when_it_arrives() {
        let imported = std::sync::Arc::new(Mutex::new(None));
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(imported.clone());
        to_codes(&mut sender, first, &mut receiver);
        assert_eq!(receiver.confirm(true).unwrap().state, State::Confirmed);
        let key = sender.confirm(true).unwrap();
        let done = receiver.handle(key.send[0].clone()).unwrap();
        assert_eq!(done.state, State::Done);
        assert!(imported.lock().unwrap().is_some());
    }

    #[test]
    fn codes_that_differ_send_nothing() {
        let imported = std::sync::Arc::new(Mutex::new(None));
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(imported.clone());
        to_codes(&mut sender, first, &mut receiver);
        let cancel = sender.confirm(false).unwrap();
        assert_eq!(cancel.state, State::Cancelled);
        assert!(matches!(cancel.send[0], Message::Cancel { .. }));
        assert_eq!(receiver.handle(cancel.send[0].clone()).unwrap().state, State::Cancelled);
        assert!(imported.lock().unwrap().is_none());

        // The receiver's person can refuse too, even with the key already there.
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(imported.clone());
        to_codes(&mut sender, first, &mut receiver);
        receiver.handle(sender.confirm(true).unwrap().send[0].clone()).unwrap();
        let refused = receiver.confirm(false).unwrap();
        assert_eq!(refused.state, State::Cancelled);
        assert!(imported.lock().unwrap().is_none());
        assert_eq!(sender.handle(refused.send[0].clone()).unwrap().state, State::Cancelled);
    }

    /// Something in between, running an exchange with each side, gets two different codes.
    #[test]
    fn a_man_in_the_middle_shows_different_codes() {
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(Default::default());
        // To the sender it plays a receiver; to the receiver, a sender with its own key.
        let mut fake_receiver = Receiver::new("Apparition on the Apple TV", Box::new(|_: &[u8]| Ok(json!({})))).unwrap();
        let (mut fake_sender, fake_first) = Sender::new("Apparition on an iPhone", b"theirs".to_vec()).unwrap();

        let hello_to_sender = fake_receiver.handle(first).unwrap();
        let hello_from_receiver = receiver.handle(fake_first).unwrap();
        let reveal = sender.handle(hello_to_sender.send[0].clone()).unwrap();
        let fake_reveal = fake_sender.handle(hello_from_receiver.send[0].clone()).unwrap();
        let receiver_code = receiver.handle(fake_reveal.send[0].clone()).unwrap().code.unwrap();
        let sender_code = reveal.code.unwrap();
        fake_receiver.handle(reveal.send[0].clone()).unwrap();
        // One in a million: if this ever fails, run it again.
        assert_ne!(sender_code, receiver_code);
    }

    #[test]
    fn a_reveal_that_isnt_what_was_committed_fails() {
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(Default::default());
        let hello = receiver.handle(first).unwrap();
        let reveal = sender.handle(hello.send[0].clone()).unwrap();
        let Message::Reveal { nonce, .. } = &reveal.send[0] else { panic!("not a reveal") };
        let other = Half::new().unwrap();
        let swapped = Message::Reveal { public_key: BASE64_STANDARD.encode(other.public), nonce: nonce.clone() };
        assert!(receiver.handle(swapped).is_err());
    }

    #[test]
    fn a_key_sealed_for_another_exchange_doesnt_open() {
        let (mut sender, first) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(Default::default());
        to_codes(&mut sender, first, &mut receiver);
        let (mut other_sender, other_first) = Sender::new("Apparition on an iPhone", b"other".to_vec()).unwrap();
        let mut other_receiver = receiver_for_test();
        to_codes(&mut other_sender, other_first, &mut other_receiver);
        let other_key = other_sender.confirm(true).unwrap();
        assert!(receiver.handle(other_key.send[0].clone()).is_err());
    }

    fn receiver_for_test() -> Receiver {
        new_receiver(Default::default())
    }

    #[test]
    fn messages_out_of_turn_fail() {
        let (mut sender, _) = Sender::new("Apparition on an iPhone", b"file".to_vec()).unwrap();
        let mut receiver = new_receiver(Default::default());
        assert!(receiver.handle(Message::Key { sealed: "AAAA".into() }).is_err());
        assert!(sender.handle(Message::Done { error: None }).is_err());
        assert!(sender.confirm(true).is_err(), "no code yet");
        assert!(receiver.handle(Message::Commit { version: 2, name: "x".into(), commitment: BASE64_STANDARD.encode([0u8; 32]) }).is_err());
    }

    #[test]
    fn messages_round_trip_as_json() {
        let message = Message::Hello { version: 1, name: "n".into(), public_key: "a".into(), nonce: "b".into() };
        let json = message.to_json();
        assert!(json.contains("\"type\":\"hello\"") && json.contains("\"publicKey\":\"a\""));
        assert_eq!(Message::from_json(json.as_bytes()).unwrap(), message);
        assert!(Message::from_json(b"{\"type\":\"nonsense\"}").is_err());
        assert_eq!(clean_name("  Living\nRoom  "), "LivingRoom");
    }

    #[cfg(feature = "desktop")]
    #[tokio::test(flavor = "multi_thread")]
    async fn hands_the_key_over_the_network() {
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!("apparition-nearby-{}", std::process::id()));
        let (from, to) = (dir.join("from"), dir.join("to"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&from).unwrap();
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
        let encoded = BASE64_STANDARD.encode(rsa::pkcs8::EncodePrivateKey::to_pkcs8_der(&key).unwrap().as_bytes());
        crate::FileStorage::open(from.join("storage.json")).put(&crate::share::key_name("someone@example.com"), &encoded).unwrap();
        let payload = crate::share::export_apple_id_key(&from, "someone@example.com", None, "Apparition on a test").unwrap();

        let codes = Arc::new(Mutex::new(Vec::new()));
        let seen = codes.clone();
        let compare: net::Compare = Arc::new(move |code: &str, _: &str| {
            seen.lock().unwrap().push(code.to_string());
            true
        });
        let (port_tx, port_rx) = tokio::sync::oneshot::channel();
        let receiving = tokio::spawn({
            let compare = compare.clone();
            async move { net::receive(to, "Apparition on a receiver", move |l| { let _ = port_tx.send(l.port); }, compare, std::future::pending()).await }
        });
        let port = port_rx.await.unwrap();
        let peer = net::send(([127, 0, 0, 1], port).into(), "Apparition on a sender", payload, compare).await.unwrap();
        assert_eq!(peer, "Apparition on a receiver");
        let imported = receiving.await.unwrap().unwrap().unwrap();
        assert_eq!(imported["appleId"], "someone@example.com");
        let codes = codes.lock().unwrap();
        assert_eq!(codes.len(), 2);
        assert_eq!(codes[0], codes[1]);
        let stored = crate::FileStorage::open(dir.join("to").join("storage.json")).get(&crate::share::key_name("someone@example.com"));
        assert_eq!(stored.as_deref(), Some(encoded.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
