//! Sharing the Apple ID's signing certificate between Apparition installs, so a free account's
//! two certificate slots aren't used up one per computer and headset.
//!
//! isideload keeps one private key per Apple ID (`<sha256(email)>/key` in storage.json) and
//! reuses the account's certificate whose machine name is "Apparition" and whose public key
//! matches it. Every Apparition signs under that machine name, so handing the key over is
//! enough: the receiver then finds the same certificate instead of making another.
//!
//! The file (`.apparitionsigning`, JSON) carries the private key in the clear. It goes only
//! over a channel that is already trusted or encrypted: house_arrest from a paired computer.
//! Anywhere else (AirDrop, Messages, a USB stick), it goes sealed under five words the
//! sending device shows (`.apparitionkey`, below).

use std::{ffi::c_char, path::Path, path::PathBuf};

use base64::prelude::*;
use rootcause::prelude::*;
use rsa::{RsaPrivateKey, pkcs8::DecodePrivateKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{FileStorage, SESSION, account::MACHINE_NAME, reply, text};

/// The public half (PKCS#1 DER) of the certificate key this install keeps for `email`.
pub(crate) fn stored_public_key(data_dir: &Path, email: &str) -> Option<Vec<u8>> {
    use rsa::pkcs1::EncodeRsaPublicKey;
    let encoded = FileStorage::open(data_dir.join("storage.json")).get(&key_name(email))?;
    let key = RsaPrivateKey::from_pkcs8_der(&BASE64_STANDARD.decode(encoded).ok()?).ok()?;
    Some(key.to_public_key().to_pkcs1_der().ok()?.as_bytes().to_vec())
}

const FORMAT: &str = "apparition-signing";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SigningFile {
    format: String,
    version: u32,
    kind: String,
    apple_id: String,
    team_id: Option<String>,
    machine_name: String,
    /// PKCS#8 DER, base64, as isideload stores it.
    private_key: String,
    /// Who made it, for the receiver to show ("Apparition Installer on <computer>").
    from: String,
}

pub(crate) fn key_name(email: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{}/key", hex(&Sha256::digest(email.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `.apparitionsigning` file for the Apple ID's key in `<data_dir>/storage.json`, or an
/// error when this install hasn't made one yet (it does at its first signing).
pub fn export_apple_id_key(data_dir: &Path, email: &str, team_id: Option<&str>, from: &str) -> Result<Vec<u8>, Report> {
    let storage = FileStorage::open(data_dir.join("storage.json"));
    let key = storage
        .get(&key_name(email))
        .ok_or_else(|| report!("No certificate key for {email} yet: sign something with it first"))?;
    let file = SigningFile {
        format: FORMAT.into(),
        version: 1,
        kind: "appleIDKey".into(),
        apple_id: email.into(),
        team_id: team_id.map(str::to_string),
        machine_name: MACHINE_NAME.into(),
        private_key: key,
        from: from.into(),
    };
    Ok(serde_json::to_vec_pretty(&file)?)
}

/// Imports a `.apparitionsigning` file into `<data_dir>/storage.json`. Returns `{kind,
/// appleId, teamId, from, replaced}`; `replaced` says this install had its own key for that
/// Apple ID, whose certificate stays on the account until revoked.
pub fn import_signing_file(data_dir: &Path, data: &[u8]) -> Result<Value, Report> {
    let file: SigningFile = serde_json::from_slice(data).map_err(|_| report!("This isn't an Apparition signing file"))?;
    if file.format != FORMAT || file.version != 1 {
        bail!("This signing file is from a newer Apparition (format {} {})", file.format, file.version);
    }
    if file.kind != "appleIDKey" {
        bail!("Unknown kind of signing file: {}", file.kind);
    }
    if file.machine_name != MACHINE_NAME {
        bail!("The key is for certificates named {}, not {MACHINE_NAME}", file.machine_name);
    }
    let der = BASE64_STANDARD.decode(&file.private_key).map_err(|_| report!("The key in this file is damaged"))?;
    RsaPrivateKey::from_pkcs8_der(&der).map_err(|_| report!("The key in this file isn't an RSA private key"))?;

    std::fs::create_dir_all(data_dir)?;
    let storage = FileStorage::open(data_dir.join("storage.json"));
    // isideload hashes the email as typed at sign-in; Apple IDs ignore case.
    let mut names = vec![key_name(&file.apple_id), key_name(&file.apple_id.to_lowercase())];
    names.dedup();
    let replaced = names.iter().any(|name| storage.get(name).is_some_and(|old| old != file.private_key));
    for name in &names {
        storage.put(name, &file.private_key)?;
    }
    tracing::info!("imported a shared Apple ID certificate key");
    Ok(json!({
        "kind": file.kind,
        "appleId": file.apple_id,
        "teamId": file.team_id,
        "from": file.from,
        "replaced": replaced,
    }))
}

// MARK: Sealed files, for AirDrop and other channels nobody vouches for

// A `.apparitionkey` file is a signing file sealed under five words the sending device shows:
// Argon2id turns the words into a key, and ChaCha20-Poly1305 seals the signing file with it.
// The header (everything but the sealed bytes) is the associated data, so it can't be changed
// either. Only `from` is readable without the words, for the receiver to say whose it is.

/// The extension of a sealed signing file.
pub const SEALED_EXTENSION: &str = "apparitionkey";
const SEALED_FORMAT: &str = "apparition-signing-sealed";
const WORDS: usize = 5;
// Argon2id's cost, about a second on a phone: 64 MiB, four passes. A file may ask for more,
// up to the limits below, so a later Apparition can raise them.
const MEMORY_KIB: u32 = 64 * 1024;
const ITERATIONS: u32 = 4;
const MAX_MEMORY_KIB: u32 = 256 * 1024;
const MAX_ITERATIONS: u32 = 16;

/// The EFF's short wordlist 2.0 (CC BY 3.0 US, eff.org/dice): 1,296 words, each known by its
/// first three letters, chosen to be easy to type and dictate. "yo-yo" is left out, since
/// dictation and keyboards split it; five of the other 1,295 are about 52 bits.
fn wordlist() -> &'static [&'static str] {
    static WORDLIST: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    WORDLIST.get_or_init(|| {
        include_str!("../assets/eff_short_wordlist_2_0.txt")
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|word| word.chars().all(|c| c.is_ascii_lowercase()))
            .collect()
    })
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SealedHeader {
    format: String,
    version: u32,
    /// Readable without the words: who sealed it ("Apparition on Ixion's Vision Pro").
    from: String,
    kdf: String,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
    salt: String,
    cipher: String,
    nonce: String,
}

#[derive(Serialize, Deserialize)]
struct SealedFile {
    #[serde(flatten)]
    header: SealedHeader,
    sealed: String,
}

fn random<const N: usize>() -> Result<[u8; N], Report> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; N];
    SystemRandom::new().fill(&mut bytes).map_err(|_| report!("No random numbers on this system"))?;
    Ok(bytes)
}

/// Five words from the list, chosen uniformly.
fn new_passphrase() -> Result<String, Report> {
    let list = wordlist();
    let mut words = Vec::with_capacity(WORDS);
    while words.len() < WORDS {
        let draw = u16::from_le_bytes(random::<2>()?);
        // Rejecting the top of the range keeps every word equally likely.
        let limit = u16::MAX - u16::MAX % list.len() as u16;
        if draw < limit {
            words.push(list[usize::from(draw) % list.len()]);
        }
    }
    Ok(words.join(" "))
}

/// The words as the sealer chose them, from however they were typed or dictated: any case,
/// any separators, each word known by its first three letters (so a typo after them, or an
/// autocorrected ending, doesn't matter).
fn canonical_passphrase(typed: &str) -> Result<String, Report> {
    let tokens: Vec<String> = typed
        .split(|c: char| !c.is_alphabetic())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect();
    if tokens.len() != WORDS {
        bail!("Type all {WORDS} words, as the other device shows them");
    }
    let list = wordlist();
    tokens
        .iter()
        .map(|token| {
            let prefix: String = token.chars().take(3).collect();
            list.iter()
                .find(|word| prefix.chars().count() == 3 && word.starts_with(&prefix))
                .copied()
                .ok_or_else(|| report!("\"{token}\" isn't one of the words Apparition uses; check it against the other device"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|words| words.join(" "))
}

fn derive_key(passphrase: &str, header: &SealedHeader) -> Result<[u8; 32], Report> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let salt = BASE64_STANDARD.decode(&header.salt).map_err(|_| report!("This file is damaged"))?;
    let params = Params::new(header.memory_kib, header.iterations, header.parallelism, Some(32))
        .map_err(|_| report!("This file asks for key settings Apparition doesn't use"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
        .map_err(|_| report!("Couldn't derive the key"))?;
    Ok(key)
}

fn aead_key(key: &[u8; 32]) -> Result<ring::aead::LessSafeKey, Report> {
    let unbound = ring::aead::UnboundKey::new(&ring::aead::CHACHA20_POLY1305, key).map_err(|_| report!("Couldn't set up the cipher"))?;
    Ok(ring::aead::LessSafeKey::new(unbound))
}

/// Seals a signing file under five new words. Returns the sealed file and the words.
pub fn seal_signing_file(plain: &[u8], from: &str) -> Result<(Vec<u8>, String), Report> {
    use ring::aead::{Aad, Nonce};
    let passphrase = new_passphrase()?;
    let nonce = random::<12>()?;
    let header = SealedHeader {
        format: SEALED_FORMAT.into(),
        version: 1,
        from: from.into(),
        kdf: "argon2id".into(),
        memory_kib: MEMORY_KIB,
        iterations: ITERATIONS,
        parallelism: 1,
        salt: BASE64_STANDARD.encode(random::<16>()?),
        cipher: "chacha20-poly1305".into(),
        nonce: BASE64_STANDARD.encode(nonce),
    };
    let key = aead_key(&derive_key(&passphrase, &header)?)?;
    let mut sealed = plain.to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(serde_json::to_vec(&header)?), &mut sealed)
        .map_err(|_| report!("Couldn't seal the file"))?;
    let file = SealedFile { header, sealed: BASE64_STANDARD.encode(sealed) };
    Ok((serde_json::to_vec_pretty(&file)?, passphrase))
}

/// Who sealed a sealed file, if `data` is one (readable without the words).
pub fn sealed_from(data: &[u8]) -> Option<String> {
    let file: SealedFile = serde_json::from_slice(data).ok()?;
    (file.header.format == SEALED_FORMAT).then_some(file.header.from)
}

/// Opens a sealed file with the words typed for it, giving the signing file inside.
pub fn open_sealed(data: &[u8], typed: &str) -> Result<Vec<u8>, Report> {
    use ring::aead::{Aad, Nonce};
    let file: SealedFile = serde_json::from_slice(data).map_err(|_| report!("This isn't an Apparition key file"))?;
    let header = &file.header;
    if header.format != SEALED_FORMAT || header.version != 1 {
        bail!("This key file is from a newer Apparition ({} {})", header.format, header.version);
    }
    if header.kdf != "argon2id" || header.cipher != "chacha20-poly1305" {
        bail!("This key file uses {} and {}, which Apparition doesn't", header.kdf, header.cipher);
    }
    // A file made to tie up the device: refuse costs no Apparition chooses.
    if header.memory_kib > MAX_MEMORY_KIB || header.iterations > MAX_ITERATIONS || header.parallelism != 1 {
        bail!("This key file asks for key settings Apparition doesn't use");
    }
    let passphrase = canonical_passphrase(typed)?;
    let nonce: [u8; 12] = BASE64_STANDARD
        .decode(&header.nonce)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| report!("This key file is damaged"))?;
    let mut sealed = BASE64_STANDARD.decode(&file.sealed).map_err(|_| report!("This key file is damaged"))?;
    let key = aead_key(&derive_key(&passphrase, header)?)?;
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(serde_json::to_vec(header)?), &mut sealed)
        .map_err(|_| report!("Those words don't open this file: check them against {}", header.from))?;
    Ok(plain.to_vec())
}

/// The Apple ID's key as a sealed file (see `export_apple_id_key`), and the words that open it.
pub fn export_apple_id_key_sealed(data_dir: &Path, email: &str, team_id: Option<&str>, from: &str) -> Result<(Vec<u8>, String), Report> {
    seal_signing_file(&export_apple_id_key(data_dir, email, team_id, from)?, from)
}

/// The signing file in `data`: itself, or what a sealed one holds (opened with `passphrase`).
fn unseal(data: Vec<u8>, passphrase: Option<&str>) -> Result<Vec<u8>, Report> {
    match (sealed_from(&data), passphrase) {
        (Some(_), Some(words)) => open_sealed(&data, words),
        (Some(from), None) => bail!("This file from {from} needs the five words shown when it was shared"),
        (None, _) => Ok(data),
    }
}

/// Imports a signing file, plain or sealed (with `passphrase`, the words typed for it).
pub fn import_any_signing_file(data_dir: &Path, data: &[u8], passphrase: Option<&str>) -> Result<Value, Report> {
    import_signing_file(data_dir, &unseal(data.to_vec(), passphrase)?)
}

/// Imports a signing file, plain (`.apparitionsigning`) or sealed (`.apparitionkey`, with the
/// words typed for it in `passphrase`, which may be null for a plain one). Ends the
/// signed-in session first, so its copy of the storage can't write the old key back; the
/// next signing signs in again (silently, with a remembered password). Returns `{ok, kind,
/// appleId, ...}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_import_signing_file(data_dir: *const c_char, path: *const c_char, passphrase: *const c_char) -> *mut c_char {
    reply((|| {
        let data_dir = PathBuf::from(unsafe { text(data_dir) }?);
        let data = std::fs::read(unsafe { text(path) }?)?;
        let passphrase = unsafe { crate::identity::optional(passphrase) }?;
        // Open it before ending the session, so wrong words change nothing.
        let plain = unseal(data, passphrase.as_deref())?;
        *SESSION.lock().unwrap() = None;
        import_signing_file(&data_dir, &plain)
    })())
}

/// Who sealed a `.apparitionkey` file: `{ok, from}`, readable without the words.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_signing_file_from(path: *const c_char) -> *mut c_char {
    reply((|| {
        let data = std::fs::read(unsafe { text(path) }?)?;
        let from = sealed_from(&data).ok_or_else(|| report!("This isn't an Apparition key file"))?;
        Ok(json!({ "from": from }))
    })())
}

/// Seals the Apple ID's certificate key for another device into `<out_dir>/<name>.apparitionkey`.
/// Returns `{ok, path, passphrase}`: the words to show, which open it. `team_id` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_export_signing_share(
    data_dir: *const c_char,
    email: *const c_char,
    team_id: *const c_char,
    from: *const c_char,
    out_dir: *const c_char,
) -> *mut c_char {
    reply((|| {
        let data_dir = PathBuf::from(unsafe { text(data_dir) }?);
        let email = unsafe { text(email) }?;
        let team_id = unsafe { crate::identity::optional(team_id) }?;
        let from = unsafe { text(from) }?;
        let out_dir = PathBuf::from(unsafe { text(out_dir) }?);
        let (file, passphrase) = export_apple_id_key_sealed(&data_dir, &email, team_id.as_deref(), &from)?;
        std::fs::create_dir_all(&out_dir)?;
        let path = out_dir.join(format!("Apple ID signing.{SEALED_EXTENSION}"));
        std::fs::write(&path, file)?;
        Ok(json!({ "path": path.to_string_lossy(), "passphrase": passphrase }))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join(format!("apparition-share-{}", std::process::id()));
        let (from, to) = (dir.join("from"), dir.join("to"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&from).unwrap();
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
        let der = rsa::pkcs8::EncodePrivateKey::to_pkcs8_der(&key).unwrap();
        let encoded = BASE64_STANDARD.encode(der.as_bytes());
        FileStorage::open(from.join("storage.json")).put(&key_name("Someone@Example.com"), &encoded).unwrap();

        assert!(export_apple_id_key(&from, "other@example.com", None, "test").is_err());
        let file = export_apple_id_key(&from, "Someone@Example.com", Some("TEAM123456"), "test").unwrap();
        let imported = import_signing_file(&to, &file).unwrap();
        assert_eq!(imported["replaced"], false);
        let storage = FileStorage::open(to.join("storage.json"));
        assert_eq!(storage.get(&key_name("Someone@Example.com")).as_deref(), Some(encoded.as_str()));
        assert_eq!(storage.get(&key_name("someone@example.com")).as_deref(), Some(encoded.as_str()));
        assert!(import_signing_file(&to, b"{}").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wordlist_is_the_eff_short_list_without_yoyo() {
        let list = wordlist();
        assert_eq!(list.len(), 1295);
        let mut prefixes: Vec<String> = list.iter().map(|word| word[..3].to_string()).collect();
        prefixes.sort();
        prefixes.dedup();
        assert_eq!(prefixes.len(), list.len(), "each word is known by its first three letters");
    }

    #[test]
    fn sealed_round_trip() {
        let dir = std::env::temp_dir().join(format!("apparition-sealed-{}", std::process::id()));
        let (from, to) = (dir.join("from"), dir.join("to"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&from).unwrap();
        let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 1024).unwrap();
        let encoded = BASE64_STANDARD.encode(rsa::pkcs8::EncodePrivateKey::to_pkcs8_der(&key).unwrap().as_bytes());
        FileStorage::open(from.join("storage.json")).put(&key_name("someone@example.com"), &encoded).unwrap();

        let (file, words) = export_apple_id_key_sealed(&from, "someone@example.com", None, "Apparition on a test").unwrap();
        assert_eq!(words.split(' ').count(), WORDS);
        assert!(!String::from_utf8_lossy(&file).contains(&encoded), "the key isn't readable in the file");
        assert!(!String::from_utf8_lossy(&file).contains("someone@example.com"), "nor the Apple ID");
        assert_eq!(sealed_from(&file).as_deref(), Some("Apparition on a test"));

        assert!(import_any_signing_file(&to, &file, None).is_err(), "needs the words");
        let mut wrong: Vec<&str> = words.split(' ').collect();
        wrong.swap(0, 1);
        if wrong[0] != wrong[1] {
            assert!(import_any_signing_file(&to, &file, Some(&wrong.join(" "))).is_err(), "words in the wrong order");
        }
        assert!(import_any_signing_file(&to, &file, Some("only three words")).is_err());

        // As typed on a phone: capitalised, a comma, extra spaces, a misspelled ending.
        let mut typed: Vec<String> = words.split(' ').map(str::to_string).collect();
        typed[0] = typed[0][..1].to_uppercase() + &typed[0][1..];
        typed[1] = format!("{}xyz", &typed[1][..3]);
        let typed = format!("{},  {}", typed[0], typed[1..].join("  "));
        let imported = import_any_signing_file(&to, &file, Some(&typed)).unwrap();
        assert_eq!(imported["appleId"], "someone@example.com");
        assert_eq!(FileStorage::open(to.join("storage.json")).get(&key_name("someone@example.com")).as_deref(), Some(encoded.as_str()));

        // The header is bound to the contents: changing "from" breaks it.
        let mut tampered: serde_json::Value = serde_json::from_slice(&file).unwrap();
        tampered["from"] = "someone else".into();
        let tampered = serde_json::to_vec(&tampered).unwrap();
        assert!(open_sealed(&tampered, &words).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod local {
    /// `APPARITION_SIGNING_FILE=<file>`: imports it into a scratch data dir.
    #[test]
    #[ignore]
    fn imports_file() {
        let path = std::env::var("APPARITION_SIGNING_FILE").unwrap();
        let dir = std::env::temp_dir().join(format!("apparition-share-file-{}", std::process::id()));
        let result = super::import_signing_file(&dir, &std::fs::read(path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        println!("{:?}", result.map_err(|e| e.to_string()));
    }
}
