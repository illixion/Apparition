//! Sharing the Apple ID's signing certificate between Apparition installs, so a free account's
//! two certificate slots aren't used up one per computer and headset.
//!
//! isideload keeps one private key per Apple ID (`<sha256(email)>/key` in storage.json) and
//! reuses the account's certificate whose machine name is "Apparition" and whose public key
//! matches it. Every Apparition signs under that machine name, so handing the key over is
//! enough: the receiver then finds the same certificate instead of making another.
//!
//! The file (`.apparitionsigning`, JSON) carries the private key in the clear. It goes only
//! over a channel that is already trusted or encrypted: house_arrest from a paired computer,
//! and later an encrypted AirDrop archive.

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

/// Imports a `.apparitionsigning` file (see `import_signing_file`). Ends the signed-in
/// session first, so its copy of the storage can't write the old key back; the next signing
/// signs in again (silently, with a remembered password). Returns `{ok, kind, appleId, ...}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_import_signing_file(data_dir: *const c_char, path: *const c_char) -> *mut c_char {
    reply((|| {
        let data_dir = PathBuf::from(unsafe { text(data_dir) }?);
        let data = std::fs::read(unsafe { text(path) }?)?;
        *SESSION.lock().unwrap() = None;
        import_signing_file(&data_dir, &data)
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
