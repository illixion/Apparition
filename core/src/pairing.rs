//! Pairing a device with this companion: the host's half of RAVESDK's `RAVECompanion` pairing
//! (Longwave's protocol), so Apparition on a device runs the SDK's `RAVEPairingClient` against it.
//!
//! Numeric comparison. The device commits to its X25519 key and a nonce before it sees ours and
//! reveals them only after; both ends then show a six-digit code drawn from the whole transcript,
//! and the person approves here only if the codes match. Something in between would have had to
//! fix its keys on each side before learning what the other contributes, so its two codes match
//! by chance one time in a million. The agreed key seals the grant (a token minted for that
//! device alone) on its way to the device.
//!
//! Messages are JSON, each after its length (four bytes, little-endian), on a plain TCP
//! connection, in order: device `commit`, host `macHello`, device `reveal`, host `accept` or
//! `deny`. The field names are the SDK's wire format and stay as Longwave named them
//! (`headsetID`, `macID`); binary values are standard base64, as Swift's JSONEncoder writes
//! `Data`. `tests` pins all of it with the SDK's golden vectors (`longwave-golden.json`), under
//! Longwave's namespace; Apparition uses its own, so the two never answer each other.

use base64::prelude::*;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The largest message either side takes.
pub const MAX_MESSAGE: usize = 16 * 1024;

/// Which app's pairing this is: it goes into the key derivation, so two apps' pairings never
/// agree on a key.
#[derive(Debug, Clone, Copy)]
pub struct Namespace {
    pub label_prefix: &'static str,
}

impl Namespace {
    /// Apparition's own (the device's `RAVECompanionNamespace.apparition`).
    pub const APPARITION: Namespace = Namespace { label_prefix: "Apparition" };

    fn pair_info(&self) -> Vec<u8> {
        format!("{}-Pair-v1", self.label_prefix).into_bytes()
    }
}

mod b64 {
    use base64::prelude::*;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64_STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64_STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

/// What travels, as Swift's synthesized `Codable` writes the SDK's `RAVEPairingMessage`:
/// `{"commit":{...}}`, and `{"deny":{}}` for the case without values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Message {
    #[serde(rename_all = "camelCase")]
    Commit {
        #[serde(rename = "headsetID")]
        headset_id: String,
        headset_name: String,
        #[serde(with = "b64")]
        commitment: Vec<u8>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh_public_key: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    MacHello {
        #[serde(rename = "macID")]
        mac_id: String,
        mac_name: String,
        #[serde(with = "b64")]
        public_key: Vec<u8>,
        #[serde(with = "b64")]
        nonce: Vec<u8>,
    },
    #[serde(rename_all = "camelCase")]
    Reveal {
        #[serde(with = "b64")]
        public_key: Vec<u8>,
        #[serde(with = "b64")]
        nonce: Vec<u8>,
    },
    #[serde(rename_all = "camelCase")]
    Accept {
        #[serde(with = "b64")]
        sealed_grant: Vec<u8>,
    },
    Deny {},
}

impl Message {
    pub fn framed(&self) -> Vec<u8> {
        let body = serde_json::to_vec(self).expect("a message encodes");
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend(body);
        frame
    }

    /// Takes one message off the front of `buffer`, if it holds a whole one.
    pub fn take(buffer: &mut Vec<u8>) -> Result<Option<Message>, Report> {
        if buffer.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_le_bytes(buffer[..4].try_into().unwrap()) as usize;
        if length > MAX_MESSAGE {
            bail!("pairing message too long ({length} bytes)");
        }
        if buffer.len() < 4 + length {
            return Ok(None);
        }
        let message = serde_json::from_slice(&buffer[4..4 + length]).context("not a pairing message")?;
        buffer.drain(..4 + length);
        Ok(Some(message))
    }
}

/// What the device gets once the person approves: the SDK's `RAVEPairingGrant`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub token: String,
    #[serde(rename = "macID")]
    pub mac_id: String,
    #[serde(rename = "macName")]
    pub mac_name: String,
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<String>>,
}

/// Our side of one exchange.
pub struct Exchange {
    secret: x25519_dalek::StaticSecret,
    pub nonce: [u8; 32],
}

impl Exchange {
    pub fn new() -> Exchange {
        let random = SystemRandom::new();
        let mut key = [0u8; 32];
        let mut nonce = [0u8; 32];
        random.fill(&mut key).expect("random numbers");
        random.fill(&mut nonce).expect("random numbers");
        Exchange::from_parts(key, nonce)
    }

    pub fn from_parts(key: [u8; 32], nonce: [u8; 32]) -> Exchange {
        Exchange { secret: x25519_dalek::StaticSecret::from(key), nonce }
    }

    pub fn public_key(&self) -> [u8; 32] {
        x25519_dalek::PublicKey::from(&self.secret).to_bytes()
    }
}

/// A hash binding the device's key and nonce: its opening move.
pub fn commitment(public_key: &[u8], nonce: &[u8]) -> [u8; 32] {
    Sha256::new().chain_update(public_key).chain_update(nonce).finalize().into()
}

pub struct Agreement {
    /// Six digits, for both screens.
    pub code: String,
    pub key: [u8; 32],
}

impl Agreement {
    /// "123456" as "123 456".
    pub fn display_code(&self) -> String {
        format!("{} {}", &self.code[..3], &self.code[3..])
    }
}

/// Runs on both ends once all four values are known. `None` when the revealed device values
/// don't match the commitment, or a key is bad.
#[allow(clippy::too_many_arguments)]
pub fn agree(
    namespace: Namespace,
    mine: &Exchange,
    their_public_key: &[u8],
    commitment_bytes: &[u8],
    device_public_key: &[u8],
    device_nonce: &[u8],
    host_public_key: &[u8],
    host_nonce: &[u8],
) -> Option<Agreement> {
    if commitment(device_public_key, device_nonce).as_slice() != commitment_bytes {
        return None;
    }
    let theirs: [u8; 32] = their_public_key.try_into().ok()?;
    let shared = mine.secret.diffie_hellman(&x25519_dalek::PublicKey::from(theirs));
    if !shared.was_contributory() {
        return None;
    }
    let transcript: [u8; 32] = Sha256::new()
        .chain_update(commitment_bytes)
        .chain_update(host_public_key)
        .chain_update(host_nonce)
        .chain_update(device_public_key)
        .chain_update(device_nonce)
        .finalize()
        .into();
    let mut key = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(Some(&transcript), shared.as_bytes())
        .expand(&namespace.pair_info(), &mut key)
        .ok()?;
    let number = u32::from_be_bytes(transcript[..4].try_into().unwrap()) % 1_000_000;
    Some(Agreement { code: format!("{number:06}"), key })
}

/// ChaCha20-Poly1305 in CryptoKit's combined form: nonce (12 bytes), ciphertext, tag.
pub fn seal(grant: &Grant, key: &[u8; 32]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    SystemRandom::new().fill(&mut nonce).expect("random numbers");
    let sealing = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("a 32-byte key"));
    let mut body = serde_json::to_vec(grant).expect("a grant encodes");
    sealing
        .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut body)
        .expect("sealing");
    let mut combined = nonce.to_vec();
    combined.extend(body);
    combined
}

pub fn open(sealed: &[u8], key: &[u8; 32]) -> Option<Grant> {
    if sealed.len() < 12 + 16 {
        return None;
    }
    let opening = LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).ok()?);
    let nonce = Nonce::try_assume_unique_for_key(&sealed[..12]).ok()?;
    let mut body = sealed[12..].to_vec();
    let plain = opening.open_in_place(nonce, Aad::empty(), &mut body).ok()?;
    serde_json::from_slice(plain).ok()
}

/// A device asking to pair, for the person to compare and decide.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub device_id: String,
    pub device_name: String,
    /// "123 456": what the device shows too.
    pub code: String,
    pub peer: String,
    /// What the device is granted if the person allows it: what it asked for, of what this
    /// companion offers. Shown on the approval prompt.
    pub grants: Vec<String>,
}

/// The grants for a pairing: what the device asked for (`requested`), of what the companion
/// offers. A device that names nothing (an older one) asked for anisette and the Apple relay.
pub fn granted(requested: Option<&[String]>, offered: &[String]) -> Vec<String> {
    let legacy = ["anisette".to_string(), "appleRelay".to_string()];
    let asked = requested.unwrap_or(&legacy);
    let mut grants: Vec<String> = Vec::new();
    for grant in asked {
        if offered.contains(grant) && !grants.contains(grant) {
            grants.push(grant.clone());
        }
    }
    grants
}

/// Runs one pairing on `stream`, as the host. `grants` is what this companion offers; the device
/// gets the part of it that it asked for. `decide` gets the request once the codes are
/// known and returns the token to hand over (the person allowed it), or `None` (declined).
pub async fn serve<S, F, Fut>(
    namespace: Namespace,
    stream: &mut S,
    peer: String,
    host_id: &str,
    host_name: &str,
    addresses: Vec<String>,
    grants: Vec<String>,
    decide: F,
) -> Result<bool, Report>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnOnce(Request) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let mut buffer = Vec::new();
    let Message::Commit { headset_id, headset_name, commitment: committed, requested, .. } = next(stream, &mut buffer).await? else {
        bail!("expected the device's commitment");
    };
    let grants = granted(requested.as_deref(), &grants);
    if grants.is_empty() {
        stream.write_all(&Message::Deny {}.framed()).await?;
        bail!("the device asked for nothing this companion offers");
    }
    let mine = Exchange::new();
    let hello = Message::MacHello {
        mac_id: host_id.to_string(),
        mac_name: host_name.to_string(),
        public_key: mine.public_key().to_vec(),
        nonce: mine.nonce.to_vec(),
    };
    stream.write_all(&hello.framed()).await?;
    let Message::Reveal { public_key, nonce } = next(stream, &mut buffer).await? else {
        bail!("expected the device's reveal");
    };
    let Some(agreement) = agree(
        namespace,
        &mine,
        &public_key,
        &committed,
        &public_key,
        &nonce,
        &mine.public_key(),
        &mine.nonce,
    ) else {
        bail!("the device's reveal didn't match its commitment");
    };
    let name: String = headset_name.chars().filter(|c| !c.is_control()).take(64).collect();
    let request = Request { device_id: headset_id, device_name: name, code: agreement.display_code(), peer, grants: grants.clone() };
    match decide(request).await {
        Some(token) => {
            let grant = Grant {
                token,
                mac_id: host_id.to_string(),
                mac_name: host_name.to_string(),
                addresses,
                grants: Some(grants),
            };
            stream.write_all(&Message::Accept { sealed_grant: seal(&grant, &agreement.key) }.framed()).await?;
            Ok(true)
        }
        None => {
            stream.write_all(&Message::Deny {}.framed()).await?;
            Ok(false)
        }
    }
}

async fn next<S: tokio::io::AsyncRead + Unpin>(stream: &mut S, buffer: &mut Vec<u8>) -> Result<Message, Report> {
    loop {
        if let Some(message) = Message::take(buffer)? {
            return Ok(message);
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("the device closed the connection");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// A random token: 32 bytes, URL-safe base64, so it fits in a URL path.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).expect("random numbers");
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONGWAVE: Namespace = Namespace { label_prefix: "Longwave" };

    /// From RAVESDK's Tests/RAVECompanionTests/longwave-golden.json (generated 2026-11-01 from
    /// Longwave's own code), which the SDK's Swift side is held to as well.
    const HS_PUBLIC: &str = "7b4e909bbe7ffe44c465a220037d608ee35897d31ef972f07f74892cb0f73f13";
    const MAC_PUBLIC: &str = "0faa684ed28867b97f4a6a2dee5df8ce974e76b7018e3f22a1c4cf2678570f20";
    const COMMITMENT: &str = "490936352de1a9e674b2e72904884a1594337353ecac61ff333b1e174fea0de8";
    const CODE: &str = "741941";
    const KEY: &str = "1be2c8c031f09561104f018943935f04c12c5de64cd3c8e212285c5505b2cb40";
    const SEALED: &str = "beb70fc6d72317d2d6e371baff6b7c76a158d15270c2cbdb64412b1e67733f84ce04e8b8a4e4fe3b1bf202906fb7a8983ef2a8ed023a853ead5b235c352ea45794628bd0903e551fafdbd441e3505858bfa47d8bcda6bdc634198910631654d25a12000232ac324568034887e0188ce403be5bea7c6d31adecf5385dffe361531c2aabb806214704eb";
    const MESSAGES: [&str; 6] = [
        "bb0000007b22636f6d6d6974223a7b22686561647365744e616d65223a22566973696f6e2050726f222c22726571756573746564223a5b22617564696f222c2273637265656e225d2c22686561647365744944223a2248532d31222c227373685075626c69634b6579223a227373682d6564323535313920414141412078222c22636f6d6d69746d656e74223a2253516b324e53336871655a30737563704249684b465a517a633150737247485c2f4d7a736546305c2f714465673d227d7d",
        "780000007b22636f6d6d6974223a7b22686561647365744e616d65223a22566973696f6e2050726f222c22636f6d6d69746d656e74223a2253516b324e53336871655a30737563704249684b465a517a633150737247485c2f4d7a736546305c2f714465673d222c22686561647365744944223a2248532d31227d7d",
        "a40000007b226d616348656c6c6f223a7b226e6f6e6365223a22524552455245524552455245524552455245524552455245524552455245524552455245524552455245513d222c226d61634944223a224d41432d31222c226d61634e616d65223a2253747564696f222c227075626c69634b6579223a224436706f54744b495a376c5c2f536d6f74376c33347a70644f647263426a6a38696f6354504a6e68584479413d227d7d",
        "800000007b2272657665616c223a7b226e6f6e6365223a224d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d7a4d3d222c227075626c69634b6579223a22653036516d37355c2f5c2f6b54455a614967413331676a754e596c394d652b584c776633534a4c4c443350784d3d227d7d",
        "d80000007b22616363657074223a7b227365616c65644772616e74223a22767263507874636a46394c57343347365c2f3274386471465930564a77777376625a454572486d647a5034544f424f6934704f542b4f787679417042767436695950764b6f375149366854367457794e634e53366b5635526969394351506c55667239765551654e515746695c2f7048324c7a616139786a515a6952426a466c545357684941416a4b734d6b566f413069483442694d35414f2b572b7038625447743750553458665c2f6a59564d634b71753442694648424f733d227d7d",
        "0b0000007b2264656e79223a7b7d7d",
    ];

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn device() -> Exchange {
        Exchange::from_parts([0x11; 32], [0x33; 32])
    }

    fn host() -> Exchange {
        Exchange::from_parts([0x22; 32], [0x44; 32])
    }

    #[test]
    fn exchange_matches_the_sdk() {
        let (device, host) = (device(), host());
        assert_eq!(to_hex(&device.public_key()), HS_PUBLIC);
        assert_eq!(to_hex(&host.public_key()), MAC_PUBLIC);
        let committed = commitment(&device.public_key(), &device.nonce);
        assert_eq!(to_hex(&committed), COMMITMENT);
        for (mine, theirs) in [(&device, host.public_key()), (&host, device.public_key())] {
            let agreement = agree(
                LONGWAVE,
                mine,
                &theirs,
                &committed,
                &device.public_key(),
                &device.nonce,
                &host.public_key(),
                &host.nonce,
            )
            .unwrap();
            assert_eq!(agreement.code, CODE);
            assert_eq!(to_hex(&agreement.key), KEY);
            let grant = open(&hex(SEALED), &agreement.key).unwrap();
            assert_eq!(
                grant,
                Grant {
                    token: "tok-123".into(),
                    mac_id: "MAC-1".into(),
                    mac_name: "Studio".into(),
                    addresses: vec!["192.0.2.10".into()],
                    grants: Some(vec!["audio".into(), "screen".into()]),
                }
            );
            let resealed = seal(&grant, &agreement.key);
            assert_eq!(open(&resealed, &agreement.key), Some(grant));
        }
    }

    #[test]
    fn a_bad_reveal_is_refused() {
        let (device, host) = (device(), host());
        let committed = commitment(&device.public_key(), &device.nonce);
        assert!(agree(LONGWAVE, &host, &device.public_key(), &committed, &device.public_key(), &[0; 32],
                      &host.public_key(), &host.nonce).is_none());
    }

    #[test]
    fn every_sdk_message_decodes_and_reencodes() {
        for (index, frame) in MESSAGES.iter().enumerate() {
            let mut buffer = hex(frame);
            let message = Message::take(&mut buffer).unwrap().unwrap_or_else(|| panic!("message {index}"));
            assert!(buffer.is_empty());
            let ours: serde_json::Value = serde_json::from_slice(&message.framed()[4..]).unwrap();
            let theirs: serde_json::Value = serde_json::from_slice(&hex(frame)[4..]).unwrap();
            assert_eq!(ours, theirs, "message {index}");
        }
    }

    #[tokio::test]
    async fn serves_a_pairing_end_to_end() {
        let (mut host_side, mut device_side) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            serve(Namespace::APPARITION, &mut host_side, "peer".into(), "HOST", "Pegasus", vec![], vec!["anisette".into()],
                  |request| async move {
                      assert_eq!(request.device_name, "ixPhone");
                      Some("the-token".to_string())
                  })
            .await
            .unwrap()
        });
        let device = device();
        let committed = commitment(&device.public_key(), &device.nonce);
        let commit = Message::Commit {
            headset_id: "D".into(),
            headset_name: "ixPhone".into(),
            commitment: committed.to_vec(),
            requested: Some(vec!["anisette".into()]),
            ssh_public_key: None,
        };
        device_side.write_all(&commit.framed()).await.unwrap();
        let mut buffer = Vec::new();
        let Message::MacHello { public_key, nonce, .. } = next(&mut device_side, &mut buffer).await.unwrap() else { panic!() };
        device_side
            .write_all(&Message::Reveal { public_key: device.public_key().to_vec(), nonce: device.nonce.to_vec() }.framed())
            .await
            .unwrap();
        let agreement = agree(Namespace::APPARITION, &device, &public_key, &committed, &device.public_key(), &device.nonce,
                              &public_key, &nonce).unwrap();
        let Message::Accept { sealed_grant } = next(&mut device_side, &mut buffer).await.unwrap() else { panic!() };
        assert_eq!(open(&sealed_grant, &agreement.key).unwrap().token, "the-token");
        assert!(server.await.unwrap());
    }
}
