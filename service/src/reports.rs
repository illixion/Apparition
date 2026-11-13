//! Encrypted problem reports: the `.trace` file a public copy's Report a Problem makes.
//!
//! The file is DebugTrace's `DebugTraceEncryption`: a trace zip sealed to the maintainer's public
//! key with HPKE (RFC 9180) in base mode, DHKEM(X25519, HKDF-SHA256) / HKDF-SHA256 /
//! ChaCha20-Poly1305, `info` `debugtrace/hpke/1`.
//!
//! ```text
//! "DTRCHPKE"          8 bytes, magic
//! 0x01                format version
//! n, keyId            1 byte, then n bytes UTF-8: the recipient key's id
//! m, encapsulated     2 bytes big-endian, then m bytes: HPKE's encapsulated key
//! ciphertext          the rest: the zip, sealed; everything above is its AAD
//! ```
//!
//! The private keys are the ones `debugtrace-decrypt keygen` makes: `<key id>.key` files (base64
//! of the raw X25519 private key, mode 600) in `DEBUGTRACE_REPORT_KEYS`, else
//! `$XDG_STATE_HOME/debugtrace/report-keys`, else `~/.local/state/debugtrace/report-keys`. A key's
//! id is the first 8 bytes of SHA-256 over its public half, in hex, so the file's name is not
//! trusted and the id a report names is never turned into a path.
//!
//! Anyone can seal a file to a public key that ships in a public app, so opening one proves
//! nothing about who made it: see `traces::receive_report` for what the Store does with that.

use crate::traces::TraceError;
use ring::{aead, digest, hkdf, hmac};
use std::{
    fs,
    path::{Path, PathBuf},
};
use x25519_dalek::{PublicKey, StaticSecret};

/// The extension of an encrypted report.
pub const EXTENSION: &str = "trace";
const MAGIC: &[u8] = b"DTRCHPKE";
const VERSION: u8 = 1;
const INFO: &[u8] = b"debugtrace/hpke/1";

/// Where the report keys are kept; the same rule as `debugtrace-decrypt`.
pub fn default_key_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("DEBUGTRACE_REPORT_KEYS").filter(|d| !d.is_empty()) {
        return dir.into();
    }
    let state = std::env::var_os("XDG_STATE_HOME").filter(|d| !d.is_empty()).map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".local/state"));
    state.join("debugtrace/report-keys")
}

/// Whether `buf` is an encrypted report: the `.trace` file itself, or the zip a person attaches
/// to an issue, which holds it (`unwrap`).
pub fn is_report(buf: &[u8]) -> bool {
    buf.starts_with(MAGIC) || unwrap(buf).is_some()
}

/// The largest `.trace` a wrapper may hold: the same limit a trace upload has.
const MAX_WRAPPED: u64 = crate::traces::MAX_UPLOAD as u64;

/// The `.trace` inside the attachable form, `<name>.trace.zip`: GitHub takes no `.trace` file on
/// an issue, so DebugTrace's report sheet wraps it in a zip of exactly one entry, `<name>.trace`,
/// its bytes unchanged. A zip with any other shape (a trace zip with its manifest, two entries, a
/// name with a directory part or not ending `.trace`) is not a report. Decided by content, never
/// by the file's name: people rename attachments.
pub fn unwrap(buf: &[u8]) -> Option<Vec<u8>> {
    if !buf.starts_with(b"PK") {
        return None;
    }
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(buf)).ok()?;
    if archive.len() != 1 {
        return None;
    }
    let mut entry = archive.by_index(0).ok()?;
    let name = entry.name().ok()?.to_string();
    let plain = !name.contains('/') && !name.contains('\\') && !name.starts_with('.');
    if !plain || !name.ends_with(".trace") || name.len() <= ".trace".len() || entry.size() > MAX_WRAPPED {
        return None;
    }
    let mut inner = Vec::with_capacity(entry.size() as usize);
    // Bounded again while reading: the size in the header is the sender's word.
    std::io::Read::read_to_end(&mut std::io::Read::take(&mut entry, MAX_WRAPPED + 1), &mut inner).ok()?;
    (inner.len() as u64 <= MAX_WRAPPED && inner.starts_with(MAGIC)).then_some(inner)
}

fn not_a_report(what: &str) -> TraceError {
    TraceError(400, format!("not an encrypted report: {what}. A report is a .trace file, or the .trace.zip that holds one; upload a trace .zip as a trace instead"))
}

/// The parts of an encrypted file, without opening it.
pub struct Envelope<'a> {
    /// The id of the key it is sealed to, as the sender wrote it.
    pub key_id: String,
    encapsulated: &'a [u8],
    header: &'a [u8],
    ciphertext: &'a [u8],
}

pub fn envelope(buf: &[u8]) -> Result<Envelope<'_>, TraceError> {
    if !is_report(buf) {
        return Err(not_a_report("no magic"));
    }
    let mut at = MAGIC.len();
    let version = *buf.get(at).ok_or_else(|| not_a_report("it ends after the magic"))?;
    if version != VERSION {
        return Err(TraceError(400, format!("encrypted report format {version} is newer than this Store reads; update Apparition")));
    }
    at += 1;
    let id_len = *buf.get(at).ok_or_else(|| not_a_report("it ends before the key id"))? as usize;
    at += 1;
    let id = buf.get(at..at + id_len).ok_or_else(|| not_a_report("the key id is cut short"))?;
    at += id_len;
    let enc_len = buf.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize).ok_or_else(|| not_a_report("it ends before the key"))?;
    at += 2;
    let encapsulated = buf.get(at..at + enc_len).ok_or_else(|| not_a_report("the key is cut short"))?;
    at += enc_len;
    // What a person reads in an error: printable, and short.
    let key_id: String = String::from_utf8_lossy(id).chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(64).collect();
    Ok(Envelope { key_id, encapsulated, header: &buf[..at], ciphertext: &buf[at..] })
}

/// First 8 bytes of SHA-256 over the raw public key, in hex (`DebugTraceEncryption.keyId`).
pub fn key_id(public: &[u8; 32]) -> String {
    digest::digest(&digest::SHA256, public).as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The stored private key whose public half has the id `wanted`. Ids are computed from the keys,
/// as `debugtrace-decrypt` does, so a file's name decides nothing.
fn stored_key(dir: &Path, wanted: &str) -> Option<StaticSecret> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let mut names: Vec<PathBuf> = fs::read_dir(dir).ok()?.flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "key")).collect();
    names.sort();
    names.into_iter().find_map(|path| {
        let raw: [u8; 32] = STANDARD.decode(fs::read_to_string(path).ok()?.trim()).ok()?.try_into().ok()?;
        let secret = StaticSecret::from(raw);
        (key_id(PublicKey::from(&secret).as_bytes()) == wanted).then_some(secret)
    })
}

/// Opens an encrypted report with the key it names, giving the zip inside and that key's id.
pub fn open(buf: &[u8], key_dir: &Path) -> Result<(Vec<u8>, String), TraceError> {
    let unwrapped = unwrap(buf);
    let buf = unwrapped.as_deref().unwrap_or(buf);
    let envelope = envelope(buf)?;
    let secret = stored_key(key_dir, &envelope.key_id).ok_or_else(|| TraceError(403, format!(
        "this report is encrypted to key {}, which is not in {}. Open it on the Mac that holds that key (`debugtrace-decrypt keys` lists a Mac's keys)",
        if envelope.key_id.is_empty() { "(unnamed)" } else { &envelope.key_id }, key_dir.display())))?;
    let zip = hpke_open(&secret, envelope.encapsulated, INFO, envelope.header, envelope.ciphertext)
        .ok_or_else(|| TraceError(400, "the report did not decrypt: it was changed or cut short. Ask for the original file again".into()))?;
    Ok((zip, envelope.key_id))
}

// MARK: HPKE, the one suite DebugTrace uses

/// `"KEM" || I2OSP(0x0020, 2)`: DHKEM(X25519, HKDF-SHA256).
const KEM_SUITE: &[u8] = b"KEM\x00\x20";
/// `"HPKE" || kem 0x0020 || kdf 0x0001 (HKDF-SHA256) || aead 0x0003 (ChaCha20-Poly1305)`.
const HPKE_SUITE: &[u8] = b"HPKE\x00\x20\x00\x01\x00\x03";

/// RFC 9180 `LabeledExtract`. HKDF-Extract is HMAC keyed with the salt; ring's own extract hides
/// its output, and two of these are data for the key schedule.
fn labeled_extract(salt: &[u8], suite: &[u8], label: &[u8], ikm: &[u8]) -> [u8; 32] {
    let mut context = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, salt));
    for part in [b"HPKE-v1".as_slice(), suite, label, ikm] {
        context.update(part);
    }
    context.sign().as_ref().try_into().expect("SHA-256 output is 32 bytes")
}

struct Length(usize);

impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

/// RFC 9180 `LabeledExpand`, filling `out`.
fn labeled_expand(prk: &[u8; 32], suite: &[u8], label: &[u8], info: &[u8], out: &mut [u8]) {
    let length = (out.len() as u16).to_be_bytes();
    let parts: [&[u8]; 5] = [&length, b"HPKE-v1", suite, label, info];
    hkdf::Prk::new_less_safe(hkdf::HKDF_SHA256, prk)
        .expand(&parts, Length(out.len()))
        .and_then(|okm| okm.fill(out))
        .expect("an HKDF-SHA256 output this short always exists");
}

/// The AEAD key and base nonce of a base-mode context, from the Diffie-Hellman result both ends
/// reach: `ExtractAndExpand` over it, then `KeySchedule`.
fn key_schedule(dh: &x25519_dalek::SharedSecret, encapsulated: &[u8; 32], recipient: &PublicKey, info: &[u8]) -> Option<([u8; 32], [u8; 12])> {
    // An all-zero result means the other side's key was a small-order point (RFC 9180, 7.1.4).
    if !dh.was_contributory() {
        return None;
    }
    let mut kem_context = [0u8; 64];
    kem_context[..32].copy_from_slice(encapsulated);
    kem_context[32..].copy_from_slice(recipient.as_bytes());
    let eae_prk = labeled_extract(b"", KEM_SUITE, b"eae_prk", dh.as_bytes());
    let mut shared_secret = [0u8; 32];
    labeled_expand(&eae_prk, KEM_SUITE, b"shared_secret", &kem_context, &mut shared_secret);

    // mode_base (0), and the default PSK and PSK id, which are empty.
    let mut context = [0u8; 65];
    context[1..33].copy_from_slice(&labeled_extract(b"", HPKE_SUITE, b"psk_id_hash", b""));
    context[33..].copy_from_slice(&labeled_extract(b"", HPKE_SUITE, b"info_hash", info));
    let secret = labeled_extract(&shared_secret, HPKE_SUITE, b"secret", b"");
    let (mut key, mut base_nonce) = ([0u8; 32], [0u8; 12]);
    labeled_expand(&secret, HPKE_SUITE, b"key", &context, &mut key);
    labeled_expand(&secret, HPKE_SUITE, b"base_nonce", &context, &mut base_nonce);
    Some((key, base_nonce))
}

fn aead_key(key: &[u8; 32]) -> Option<aead::LessSafeKey> {
    Some(aead::LessSafeKey::new(aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).ok()?))
}

/// Opens the first (and, for a `.trace`, only) message of a base-mode context. None when the
/// key is not the recipient's or anything was changed.
fn hpke_open(recipient: &StaticSecret, encapsulated: &[u8], info: &[u8], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    let encapsulated: [u8; 32] = encapsulated.try_into().ok()?;
    let dh = recipient.diffie_hellman(&PublicKey::from(encapsulated));
    let (key, base_nonce) = key_schedule(&dh, &encapsulated, &PublicKey::from(recipient), info)?;
    let mut data = sealed.to_vec();
    // Sequence number 0, so the nonce is the base nonce itself.
    let length = aead_key(&key)?.open_in_place(aead::Nonce::assume_unique_for_key(base_nonce), aead::Aad::from(aad), &mut data).ok()?.len();
    data.truncate(length);
    Some(data)
}

/// The sender's half, with its one-time key `ephemeral`: the ciphertext of the context's first
/// message. `aad` is built from the encapsulated key, which is `ephemeral`'s public half.
fn hpke_seal(ephemeral: &StaticSecret, recipient: &PublicKey, info: &[u8], aad: &[u8], plain: &[u8]) -> Option<Vec<u8>> {
    let encapsulated = PublicKey::from(ephemeral);
    let (key, base_nonce) = key_schedule(&ephemeral.diffie_hellman(recipient), encapsulated.as_bytes(), recipient, info)?;
    let mut data = plain.to_vec();
    aead_key(&key)?.seal_in_place_append_tag(aead::Nonce::assume_unique_for_key(base_nonce), aead::Aad::from(aad), &mut data).ok()?;
    Some(data)
}

/// Seals a trace zip to a report key's public half, as an app's Report a Problem does: the
/// `.trace` file. For tests and for trying the intake; the Store itself only opens them.
pub fn seal(zip: &[u8], recipient: &[u8; 32]) -> Result<Vec<u8>, TraceError> {
    use ring::rand::SecureRandom;
    let failed = || TraceError(500, "could not seal the report".into());
    let mut one_time = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut one_time).map_err(|_| failed())?;
    let ephemeral = StaticSecret::from(one_time);
    let recipient = PublicKey::from(*recipient);
    let id = key_id(recipient.as_bytes());
    let mut file = MAGIC.to_vec();
    file.push(VERSION);
    file.push(id.len() as u8);
    file.extend_from_slice(id.as_bytes());
    file.extend_from_slice(&32u16.to_be_bytes());
    file.extend_from_slice(PublicKey::from(&ephemeral).as_bytes());
    let sealed = hpke_seal(&ephemeral, &recipient, INFO, &file, zip).ok_or_else(failed)?;
    file.extend_from_slice(&sealed);
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
    }

    // RFC 9180, A.2.1: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20Poly1305, base mode.
    const SK_R: &str = "8057991eef8f1f1af18f4a9491d16a1ce333f695d4db8e38da75975c4478e0fb";
    const PK_R: &str = "4310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a";
    const ENC: &str = "1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a";
    const VECTOR_INFO: &str = "4f6465206f6e2061204772656369616e2055726e";

    fn recipient() -> StaticSecret {
        StaticSecret::from(<[u8; 32]>::try_from(hex(SK_R)).unwrap())
    }

    // The sender's one-time key in the same vector.
    const SK_E: &str = "f4ec9b33b792c372c1d2c2063507b684ef925b8c75a42dbcbf57d63ccd381600";
    const PLAIN: &str = "4265617574792069732074727574682c20747275746820626561757479";
    const SEALED: &str = "1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db21993c62ce81883d2dd1b51a28";

    #[test]
    fn the_key_schedule_is_rfc_9180s() {
        let public = PublicKey::from(&recipient());
        assert_eq!(public.as_bytes().as_slice(), hex(PK_R));
        let encapsulated: [u8; 32] = hex(ENC).try_into().unwrap();
        let dh = recipient().diffie_hellman(&PublicKey::from(encapsulated));
        let (key, base_nonce) = key_schedule(&dh, &encapsulated, &public, &hex(VECTOR_INFO)).unwrap();
        assert_eq!(key.as_slice(), hex("ad2744de8e17f4ebba575b3f5f5a8fa1f69c2a07f6e7500bc60ca6e3e3ec1c91"));
        assert_eq!(base_nonce.as_slice(), hex("5c4d98150661b848853b547f"));
    }

    #[test]
    fn the_first_message_of_the_rfc_vector_opens() {
        let (sealed, aad) = (hex(SEALED), hex("436f756e742d30"));
        let plain = hpke_open(&recipient(), &hex(ENC), &hex(VECTOR_INFO), &aad, &sealed).unwrap();
        assert_eq!(plain, hex(PLAIN));
        assert_eq!(String::from_utf8(plain).unwrap(), "Beauty is truth, truth beauty");

        let mut changed = sealed.clone();
        changed[3] ^= 1;
        assert!(hpke_open(&recipient(), &hex(ENC), &hex(VECTOR_INFO), &aad, &changed).is_none(), "a changed ciphertext");
        assert!(hpke_open(&recipient(), &hex(ENC), &hex(VECTOR_INFO), b"Count-1", &sealed).is_none(), "other associated data");
        assert!(hpke_open(&recipient(), &hex(ENC), b"another info", &aad, &sealed).is_none(), "another info");
        assert!(hpke_open(&StaticSecret::from([7u8; 32]), &hex(ENC), &hex(VECTOR_INFO), &aad, &sealed).is_none(), "another key");
        assert!(hpke_open(&recipient(), &[0u8; 32], &hex(VECTOR_INFO), &aad, &sealed).is_none(), "a small-order sender key");
        assert!(hpke_open(&recipient(), &hex(ENC)[..31], &hex(VECTOR_INFO), &aad, &sealed).is_none(), "a short sender key");
    }

    #[test]
    fn sealing_with_the_rfcs_one_time_key_gives_its_ciphertext() {
        let ephemeral = StaticSecret::from(<[u8; 32]>::try_from(hex(SK_E)).unwrap());
        assert_eq!(PublicKey::from(&ephemeral).as_bytes().as_slice(), hex(ENC));
        let sealed = hpke_seal(&ephemeral, &PublicKey::from(&recipient()), &hex(VECTOR_INFO), &hex("436f756e742d30"), &hex(PLAIN)).unwrap();
        assert_eq!(sealed, hex(SEALED));
    }

    #[test]
    fn a_sealed_file_opens_with_its_key_and_no_other() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let keys = tempfile::tempdir().unwrap();
        let (ours, theirs) = (StaticSecret::from([3u8; 32]), StaticSecret::from([4u8; 32]));
        // The names say nothing: a key is found by the id of its public half.
        std::fs::write(keys.path().join("a.key"), format!("{}\n", STANDARD.encode(theirs.to_bytes()))).unwrap();
        std::fs::write(keys.path().join("b.key"), format!("{}\n", STANDARD.encode(ours.to_bytes()))).unwrap();
        std::fs::write(keys.path().join("c.key"), "not a key").unwrap();
        let public = PublicKey::from(&ours);
        let file = seal(b"the zip", public.as_bytes()).unwrap();
        assert!(is_report(&file));
        assert_eq!(open(&file, keys.path()).unwrap(), (b"the zip".to_vec(), key_id(public.as_bytes())));
        assert_ne!(seal(b"the zip", public.as_bytes()).unwrap(), file, "a fresh one-time key each time");

        let mut cut = file.clone();
        cut.truncate(file.len() - 1);
        let mut changed = file.clone();
        *changed.last_mut().unwrap() ^= 1;
        let mut header = file.clone();
        header[30] ^= 1; // inside the encapsulated key, which is also associated data
        for broken in [cut, changed, header] {
            let TraceError(status, message) = open(&broken, keys.path()).unwrap_err();
            assert_eq!((status, message.as_str()), (400, "the report did not decrypt: it was changed or cut short. Ask for the original file again"));
        }
        std::fs::remove_file(keys.path().join("b.key")).unwrap();
        assert_eq!(open(&file, keys.path()).unwrap_err().0, 403);
    }

    #[test]
    fn an_envelope_says_which_key_it_needs() {
        let mut file = MAGIC.to_vec();
        file.push(1);
        file.push(16);
        file.extend_from_slice(b"0123456789abcdef");
        file.extend_from_slice(&[0, 32]);
        file.extend_from_slice(&[9u8; 32]);
        file.extend_from_slice(b"sealed");
        let parts = envelope(&file).unwrap();
        assert_eq!(parts.key_id, "0123456789abcdef");
        assert_eq!(parts.encapsulated, [9u8; 32]);
        assert_eq!(parts.header.len(), file.len() - 6);
        assert_eq!(parts.ciphertext, b"sealed");

        let keys = tempfile::tempdir().unwrap();
        let TraceError(status, message) = open(&file, keys.path()).unwrap_err();
        assert_eq!(status, 403);
        assert!(message.starts_with("this report is encrypted to key 0123456789abcdef, which is not in "), "{message}");

        for (cut, what) in [(8, "it ends after the magic"), (9, "it ends before the key id"), (20, "the key id is cut short"), (27, "it ends before the key"), (40, "the key is cut short")] {
            let TraceError(status, message) = envelope(&file[..cut]).err().unwrap();
            assert_eq!((status, message.contains(what)), (400, true), "{cut}: {message}");
        }
        assert_eq!(envelope(b"PK\x03\x04 a zip").err().unwrap().0, 400);
        let mut newer = file.clone();
        newer[8] = 2;
        assert!(envelope(&newer).err().unwrap().1.contains("format 2 is newer"));
        // A key id is the sender's text: only what is safe to print comes back.
        let mut odd = file.clone();
        odd[10..26].copy_from_slice(b"../../etc/passwd");
        assert_eq!(envelope(&odd).unwrap().key_id, "etcpasswd");
    }
}
