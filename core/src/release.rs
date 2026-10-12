//! Official Apparition releases, as apparition.illixion.com publishes them, verified before
//! anything downloaded is used.
//!
//! The site serves `release.json`: each release's version, its assets (name, kind,
//! platform, size, SHA-256) and any number of download URLs for each. Only the release's
//! `SHA256SUMS` is trusted, and only once its SSHSIG signature checks out against a key in
//! the repo's `release-signers`, which is compiled in. So:
//!
//! - the URLs are hints: an asset may come from GitHub or any other host, and a host that
//!   serves something else is skipped for the next URL;
//! - `release.json` itself proves nothing: its hashes have to agree with the signed manifest,
//!   and an asset the manifest doesn't list is refused;
//! - the version is signed too, because every signed file name carries it
//!   (`Apparition-<version>-visionOS.ipa`), so a site can't relabel an old release as new.
//!
//! What the signature can't stop is a site that keeps serving an older (genuine) release;
//! callers refuse to go backwards with [`is_newer`].

use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};

use base64::prelude::*;
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

/// Where official releases are listed.
pub const SITE: &str = "https://apparition.illixion.com/";
/// The SSHSIG namespace releases are signed under (`ssh-keygen -Y sign -n apparition-release`).
/// Not "file", so a signature made for something else with the same keys can't pass here.
pub const NAMESPACE: &str = "apparition-release";
const SIGNERS: &str = include_str!("../../release-signers");
/// Larger than any asset we ship; a host that keeps sending is cut off here.
const MAX_ASSET_BYTES: u64 = 2 << 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub format: String,
    pub version: u32,
    pub latest: String,
    pub releases: Vec<Release>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub build: Option<u64>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Paths of the signed manifest and its signature, relative to the site.
    pub sums: String,
    pub signature: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    /// "app" (the IPA) or "installer".
    pub kind: String,
    /// "visionos", "macos-universal", "windows-x86_64", "linux-x86_64", ...
    pub platform: String,
    pub size: u64,
    pub sha256: String,
    pub urls: Vec<String>,
}

/// A release whose manifest is signed by a pinned key, and whose listed assets all match it.
#[derive(Debug, Clone)]
pub struct Verified {
    pub release: Release,
    /// The comment of the key that signed it ("ixion@YubiKey5-gpg").
    pub signer: String,
    sums: BTreeMap<String, String>,
}

impl Verified {
    pub fn asset(&self, kind: &str, platform: &str) -> Option<&Asset> {
        self.release.assets.iter().find(|a| a.kind == kind && a.platform == platform)
    }
}

fn http() -> Result<reqwest::Client, Report> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("Apparition/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, Report> {
    let response = client.get(url).send().await.context(format!("Can't reach {url}"))?;
    if !response.status().is_success() {
        bail!("{url}: {}", response.status());
    }
    Ok(response.bytes().await?.to_vec())
}

/// The latest release at `site` (normally [`SITE`]), verified. Fails rather than returning
/// anything unverified.
pub async fn latest(site: &str) -> Result<Verified, Report> {
    latest_signed_by(site, SIGNERS).await
}

async fn latest_signed_by(site: &str, signers: &str) -> Result<Verified, Report> {
    let site = reqwest::Url::parse(site).context("bad release site URL")?;
    let client = http()?;
    let index: Index = serde_json::from_slice(&fetch(&client, site.join("release.json")?.as_str()).await?)
        .context("release.json isn't a release list")?;
    if index.format != "apparition-releases" || index.version != 1 {
        bail!("release.json is format {} {}, which this version can't read", index.format, index.version);
    }
    let release = index
        .releases
        .iter()
        .find(|r| r.version == index.latest)
        .ok_or_else(|| report!("release.json names {} as latest but doesn't list it", index.latest))?
        .clone();
    let sums = fetch(&client, site.join(&release.sums)?.as_str()).await?;
    let signature = fetch(&client, site.join(&release.signature)?.as_str()).await?;
    verify_release(release, &sums, &String::from_utf8_lossy(&signature), signers)
}

/// Checks `sums` against `signature` and `signers`, then `release`'s assets against `sums`.
pub fn verify_release(release: Release, sums: &[u8], signature: &str, signers: &str) -> Result<Verified, Report> {
    let signer = verify_sshsig(sums, signature, NAMESPACE, signers)?;
    let sums = parse_sums(&String::from_utf8_lossy(sums))?;
    let tag = format!("-{}-", release.version);
    for name in sums.keys() {
        if !name.starts_with("Apparition-") || !name.contains(&tag) {
            bail!("The signed manifest lists {name}, which isn't part of Apparition {}", release.version);
        }
    }
    for asset in &release.assets {
        match sums.get(&asset.name) {
            Some(hash) if hash.eq_ignore_ascii_case(&asset.sha256) => {}
            Some(_) => bail!("release.json's hash for {} doesn't match the signed manifest", asset.name),
            None => bail!("{} isn't in the signed manifest", asset.name),
        }
    }
    Ok(Verified { release, signer, sums })
}

/// Downloads `asset` into `dir`, trying its URLs in turn, and returns the file only when its
/// SHA-256 matches the signed manifest.
pub async fn download(
    verified: &Verified,
    asset: &Asset,
    dir: &Path,
    progress: impl Fn(u64, u64),
) -> Result<PathBuf, Report> {
    let expected = verified
        .sums
        .get(&asset.name)
        .ok_or_else(|| report!("{} isn't in the signed manifest", asset.name))?
        .to_ascii_lowercase();
    if asset.name.contains(['/', '\\']) || asset.name.starts_with('.') {
        bail!("bad asset name {}", asset.name);
    }
    if asset.urls.is_empty() {
        bail!("release.json gives no download URL for {}", asset.name);
    }
    std::fs::create_dir_all(dir)?;
    let client = http()?;
    let target = dir.join(&asset.name);
    let partial = dir.join(format!("{}.partial", asset.name));
    let mut failures = Vec::new();
    for url in &asset.urls {
        match download_one(&client, url, &partial, &expected, asset.size, &progress).await {
            Ok(()) => {
                std::fs::rename(&partial, &target)?;
                return Ok(target);
            }
            Err(error) => {
                let _ = std::fs::remove_file(&partial);
                tracing::warn!("{url}: {error}");
                failures.push(format!("{url}: {error}"));
            }
        }
    }
    bail!("Couldn't download {}:\n{}", asset.name, failures.join("\n"))
}

async fn download_one(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    expected: &str,
    size: u64,
    progress: &impl Fn(u64, u64),
) -> Result<(), Report> {
    let mut response = client.get(url).send().await?;
    if !response.status().is_success() {
        bail!("{}", response.status());
    }
    let mut file = std::fs::File::create(path)?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await? {
        received += chunk.len() as u64;
        if received > MAX_ASSET_BYTES || (size > 0 && received > size) {
            bail!("sent more than the release's {size} bytes");
        }
        hash.update(&chunk);
        file.write_all(&chunk)?;
        progress(received, size);
    }
    file.flush()?;
    let got = hex(&hash.finalize());
    if got != expected {
        bail!("its SHA-256 is {got}, not the signed {expected}");
    }
    Ok(())
}

/// Whether dotted version `candidate` is newer than `current` ("0.10.0" > "0.9.2").
/// Anything after a '-' or '+' is ignored.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split(['-', '+']).next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// `sha256sum`/`shasum -a 256` output: `<hex>  <name>` per line (a `*` before the name, for
/// binary mode, is allowed). Name -> lowercase hex.
pub fn parse_sums(text: &str) -> Result<BTreeMap<String, String>, Report> {
    let mut sums = BTreeMap::new();
    for line in text.lines().map(str::trim_end).filter(|l| !l.is_empty()) {
        let (hash, name) = line.split_once(char::is_whitespace).ok_or_else(|| report!("bad SHA256SUMS line: {line}"))?;
        let name = name.trim_start().trim_start_matches('*');
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) || name.is_empty() {
            bail!("bad SHA256SUMS line: {line}");
        }
        if sums.insert(name.to_string(), hash.to_ascii_lowercase()).is_some() {
            bail!("SHA256SUMS lists {name} twice");
        }
    }
    if sums.is_empty() {
        bail!("SHA256SUMS is empty");
    }
    Ok(sums)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// MARK: SSHSIG
//
// OpenSSH's PROTOCOL.sshsig. The signature file (armored) is
//   "SSHSIG" | u32 version | str publickey | str namespace | str reserved | str hash | str signature
// and what the key signs is
//   "SSHSIG" | str namespace | str reserved | str hash | str H(message)
// where str is a big-endian u32 length and the bytes. Only ed25519 keys are accepted.

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Report> {
        if self.0.len() < n {
            bail!("truncated SSH signature");
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32, Report> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<&'a [u8], Report> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

fn ssh_string(bytes: &[u8]) -> Vec<u8> {
    let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(bytes);
    out
}

/// The ed25519 keys in an allowed_signers (or authorized_keys) text: (key blob, comment).
fn signer_keys(signers: &str) -> Vec<(Vec<u8>, String)> {
    signers
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            let i = parts.iter().position(|p| *p == "ssh-ed25519")?;
            let blob = BASE64_STANDARD.decode(parts.get(i + 1)?).ok()?;
            Some((blob, parts[i + 2..].join(" ")))
        })
        .collect()
}

/// Verifies a detached, armored SSHSIG over `data` made under `namespace` by one of
/// `signers`, and returns that signer's comment. Any shortfall is an error.
pub fn verify_sshsig(data: &[u8], armored: &str, namespace: &str, signers: &str) -> Result<String, Report> {
    let body = armored
        .split("-----BEGIN SSH SIGNATURE-----")
        .nth(1)
        .and_then(|rest| rest.split("-----END SSH SIGNATURE-----").next())
        .ok_or_else(|| report!("not an SSH signature"))?;
    let blob = BASE64_STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| report!("the SSH signature is damaged"))?;
    let mut r = Reader(&blob);
    if r.take(6)? != b"SSHSIG" {
        bail!("not an SSHSIG signature");
    }
    if r.u32()? != 1 {
        bail!("unsupported SSHSIG version");
    }
    let public_key = r.string()?;
    let signed_namespace = r.string()?;
    let reserved = r.string()?;
    let hash_name = r.string()?;
    let signature = r.string()?;

    if signed_namespace != namespace.as_bytes() {
        bail!("signed for \"{}\", not \"{namespace}\"", String::from_utf8_lossy(signed_namespace));
    }
    let digest = match hash_name {
        b"sha512" => Sha512::digest(data).to_vec(),
        b"sha256" => Sha256::digest(data).to_vec(),
        other => bail!("unsupported SSHSIG hash {}", String::from_utf8_lossy(other)),
    };
    let signers = signer_keys(signers);
    let (_, signer) = signers
        .iter()
        .find(|(key, _)| key == public_key)
        .ok_or_else(|| report!("signed by a key that isn't a pinned release signer"))?;

    let mut key = Reader(public_key);
    if key.string()? != b"ssh-ed25519" {
        bail!("the signing key isn't ed25519");
    }
    let key: [u8; 32] = key.string()?.try_into().map_err(|_| report!("bad ed25519 key"))?;
    let mut sig = Reader(signature);
    if sig.string()? != b"ssh-ed25519" {
        bail!("the signature isn't ed25519");
    }
    let sig: [u8; 64] = sig.string()?.try_into().map_err(|_| report!("bad ed25519 signature"))?;

    let mut message = b"SSHSIG".to_vec();
    message.extend(ssh_string(signed_namespace));
    message.extend(ssh_string(reserved));
    message.extend(ssh_string(hash_name));
    message.extend(ssh_string(&digest));
    ed25519_dalek::VerifyingKey::from_bytes(&key)
        .map_err(|_| report!("bad ed25519 key"))?
        .verify_strict(&message, &ed25519_dalek::Signature::from_bytes(&sig))
        .map_err(|_| report!("the release signature doesn't verify"))?;
    Ok(signer.clone())
}

#[cfg(test)]
mod local {
    /// `APPARITION_RELEASE_SITE=<url> APPARITION_RELEASE_SIGNERS=<file> APPARITION_RELEASE_OUT=<dir>`:
    /// reads the site's latest release and downloads its visionOS app.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn downloads_latest() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let site = std::env::var("APPARITION_RELEASE_SITE").unwrap();
        let signers = std::fs::read_to_string(std::env::var("APPARITION_RELEASE_SIGNERS").unwrap()).unwrap();
        let out = std::path::PathBuf::from(std::env::var("APPARITION_RELEASE_OUT").unwrap());
        let verified = super::latest_signed_by(&site, &signers).await.map_err(|e| e.to_string()).unwrap();
        println!("{} signed by {}", verified.release.version, verified.signer);
        let asset = verified.asset("app", "visionos").unwrap().clone();
        let path = super::download(&verified, &asset, &out, |_, _| {}).await.map_err(|e| e.to_string());
        println!("{path:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// A throwaway ed25519 key and ssh-keygen's signature over `data`, or None without
    /// ssh-keygen.
    fn sign(dir: &Path, data: &[u8], namespace: &str) -> Option<(String, String)> {
        let key = dir.join("key");
        if !key.exists() {
            let ok = Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "test@signer", "-f"])
                .arg(&key)
                .status()
                .ok()?
                .success();
            assert!(ok);
        }
        let file = dir.join("SHA256SUMS");
        std::fs::write(&file, data).unwrap();
        let _ = std::fs::remove_file(dir.join("SHA256SUMS.sig"));
        let ok = Command::new("ssh-keygen").args(["-Y", "sign", "-q", "-n", namespace, "-f"]).arg(&key).arg(&file).status().ok()?.success();
        assert!(ok);
        let public = std::fs::read_to_string(dir.join("key.pub")).unwrap();
        let signers = format!("releases@apparition.illixion.com {public}");
        Some((std::fs::read_to_string(dir.join("SHA256SUMS.sig")).unwrap(), signers))
    }

    fn release(assets: &[(&str, &str)]) -> Release {
        Release {
            version: "1.2.0".into(),
            build: None,
            date: None,
            notes: None,
            sums: "releases/1.2.0/SHA256SUMS".into(),
            signature: "releases/1.2.0/SHA256SUMS.sig".into(),
            assets: assets
                .iter()
                .map(|(name, hash)| Asset {
                    name: name.to_string(),
                    kind: "app".into(),
                    platform: "visionos".into(),
                    size: 0,
                    sha256: hash.to_string(),
                    urls: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn sshsig_and_manifest() {
        let dir = std::env::temp_dir().join(format!("apparition-release-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hash = "ab".repeat(32);
        let sums = format!("{hash}  Apparition-1.2.0-visionOS.ipa\n");
        let Some((signature, signers)) = sign(&dir, sums.as_bytes(), NAMESPACE) else {
            eprintln!("no ssh-keygen; skipped");
            return;
        };

        assert_eq!(verify_sshsig(sums.as_bytes(), &signature, NAMESPACE, &signers).unwrap(), "test@signer");
        // Tampered data, another namespace, an unpinned key.
        assert!(verify_sshsig(b"other", &signature, NAMESPACE, &signers).is_err());
        assert!(verify_sshsig(sums.as_bytes(), &signature, "file", &signers).is_err());
        assert!(verify_sshsig(sums.as_bytes(), &signature, NAMESPACE, SIGNERS).is_err());

        let ok = verify_release(release(&[("Apparition-1.2.0-visionOS.ipa", &hash)]), sums.as_bytes(), &signature, &signers);
        assert!(ok.is_ok());
        let wrong_hash = verify_release(release(&[("Apparition-1.2.0-visionOS.ipa", &"cd".repeat(32))]), sums.as_bytes(), &signature, &signers);
        assert!(wrong_hash.is_err());
        let unsigned = verify_release(release(&[("Apparition-1.2.0-extra.zip", &hash)]), sums.as_bytes(), &signature, &signers);
        assert!(unsigned.is_err());
        // A genuine older manifest relabelled as 1.2.0.
        let old = format!("{hash}  Apparition-1.1.0-visionOS.ipa\n");
        let (old_signature, _) = sign(&dir, old.as_bytes(), NAMESPACE).unwrap();
        assert!(verify_release(release(&[]), old.as_bytes(), &old_signature, &signers).is_err());
        // Signed under "file" (as Longwave releases are).
        let (file_signature, _) = sign(&dir, sums.as_bytes(), "file").unwrap();
        assert!(verify_release(release(&[]), sums.as_bytes(), &file_signature, &signers).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_signers_parse() {
        let keys = signer_keys(SIGNERS);
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|(blob, _)| blob.len() == 51));
    }

    #[test]
    fn versions() {
        assert!(is_newer("0.10.0", "0.9.2"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.2.0-abc", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2"));
    }

    #[test]
    fn sums() {
        let hash = "AB".repeat(32);
        let parsed = parse_sums(&format!("{hash}  a.ipa\n{hash} *b.zip\n")).unwrap();
        assert_eq!(parsed["a.ipa"], "ab".repeat(32));
        assert!(parsed.contains_key("b.zip"));
        assert!(parse_sums("").is_err());
        assert!(parse_sums(&format!("{hash}  a\n{hash}  a\n")).is_err());
        assert!(parse_sums("xyz  a\n").is_err());
    }
}
