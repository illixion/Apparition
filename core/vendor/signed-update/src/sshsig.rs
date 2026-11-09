//! OpenSSH's PROTOCOL.sshsig, verification only. The armored signature is
//!   "SSHSIG" | u32 version | str publickey | str namespace | str reserved | str hash | str signature
//! and what the key signs is
//!   "SSHSIG" | str namespace | str reserved | str hash | str H(message)
//! with `str` a big-endian u32 length and the bytes. Only ed25519 keys are accepted.

use base64::prelude::*;
use sha2::{Digest, Sha256, Sha512};

use crate::error::{Result, fail};

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.0.len() < n {
            fail!("truncated SSH signature");
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("four bytes")))
    }
    fn string(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn ssh_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}

/// The ed25519 keys in an allowed_signers text: (key blob, principal). A line without a principal
/// (an authorized_keys line) gives its comment instead. A line with options between the principal
/// and the key (`namespaces=`, `valid-before=`, `cert-authority`) pins nothing: those limits aren't
/// implemented here, and ignoring them would trust the key more widely than the file says.
pub fn signer_keys(signers: &str) -> Vec<(Vec<u8>, String)> {
    signers
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let i = parts.iter().position(|p| *p == "ssh-ed25519")?;
            if i > 1 {
                return None;
            }
            let blob = BASE64_STANDARD.decode(parts.get(i + 1)?).ok()?;
            let principal = if i > 0 { parts[..i].join(" ") } else { parts[i + 2..].join(" ") };
            Some((blob, principal))
        })
        .collect()
}

/// Verifies a detached, armored SSHSIG over `data` made under `namespace` by one of `signers`
/// (ssh-keygen's allowed_signers format), and returns that signer's principal. Any shortfall is
/// an error.
pub fn verify_sshsig(data: &[u8], armored: &str, namespace: &str, signers: &str) -> Result<String> {
    let body = armored
        .split("-----BEGIN SSH SIGNATURE-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END SSH SIGNATURE-----").next())
        .ok_or("not an SSH signature")?;
    let blob = BASE64_STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| "the SSH signature is damaged")?;
    let mut r = Reader(&blob);
    if r.take(6)? != b"SSHSIG" {
        fail!("not an SSHSIG signature");
    }
    if r.u32()? != 1 {
        fail!("unsupported SSHSIG version");
    }
    let public_key = r.string()?;
    let signed_namespace = r.string()?;
    let reserved = r.string()?;
    let hash_name = r.string()?;
    let signature = r.string()?;
    if !r.0.is_empty() {
        fail!("the SSH signature has trailing data");
    }

    if signed_namespace != namespace.as_bytes() {
        fail!("signed for \"{}\", not \"{namespace}\"", String::from_utf8_lossy(signed_namespace));
    }
    let digest = match hash_name {
        b"sha512" => Sha512::digest(data).to_vec(),
        b"sha256" => Sha256::digest(data).to_vec(),
        other => fail!("unsupported SSHSIG hash {}", String::from_utf8_lossy(other)),
    };
    let signers = signer_keys(signers);
    let (_, signer) = signers
        .iter()
        .find(|(key, _)| key == public_key)
        .ok_or("signed by a key that isn't a pinned release signer")?;

    let mut key = Reader(public_key);
    if key.string()? != b"ssh-ed25519" {
        fail!("the signing key isn't ed25519");
    }
    let key: [u8; 32] = key.string()?.try_into().map_err(|_| "bad ed25519 key")?;
    let mut sig = Reader(signature);
    if sig.string()? != b"ssh-ed25519" {
        fail!("the signature isn't ed25519");
    }
    let sig: [u8; 64] = sig.string()?.try_into().map_err(|_| "bad ed25519 signature")?;

    let mut message = b"SSHSIG".to_vec();
    message.extend(ssh_string(signed_namespace));
    message.extend(ssh_string(reserved));
    message.extend(ssh_string(hash_name));
    message.extend(ssh_string(&digest));
    ed25519_dalek::VerifyingKey::from_bytes(&key)
        .map_err(|_| "bad ed25519 key")?
        .verify_strict(&message, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| "the release signature doesn't verify")?;
    Ok(signer.clone())
}
