use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result, fail};
use crate::manifest::Manifest;
use crate::source::{Config, Source};
use crate::version::is_newer;

/// Larger than any file we ship; a host that keeps sending is cut off here.
const MAX_ASSET_BYTES: u64 = 2 << 30;
/// A manifest and a signature are small; anything bigger isn't one.
const MAX_METADATA_BYTES: usize = 1 << 20;

/// A newer release whose signed manifest checked out, and the file of it for this platform.
#[derive(Debug, Clone)]
pub struct Update {
    pub version: String,
    /// The file's signed name (`Vergence-0.2.0-macos-universal.zip`).
    pub name: String,
    /// Its signed SHA-256, lowercase hex.
    pub sha256: String,
    /// The principal of the key that signed the release.
    pub signer: String,
    /// Where to download it, in the order to try: every source's copy.
    pub urls: Vec<String>,
    /// The signed manifest and its signature as served, for a helper that checks them again.
    pub manifest: Vec<u8>,
    pub signature: String,
    user_agent: String,
}

fn client(user_agent: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::new(e.to_string()))
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut response = client.get(url).send().await.map_err(|e| format!("can't reach {url}: {e}"))?;
    if !response.status().is_success() {
        fail!("{url}: {}", response.status());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("{url}: {e}"))? {
        if bytes.len() + chunk.len() > MAX_METADATA_BYTES {
            fail!("{url} is too large to be a release manifest");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// What one source says about its newest release: the verified manifest, as served.
async fn newest(client: &reqwest::Client, config: &Config, source: &Source) -> Result<(Manifest, Vec<u8>, String)> {
    let sums = fetch(client, &source.latest_url("SHA256SUMS")).await?;
    let signature = fetch(client, &source.latest_url("SHA256SUMS.sig")).await?;
    let signature = String::from_utf8_lossy(&signature).into_owned();
    let manifest = Manifest::verify(&sums, &signature, &config.namespace, &config.signers, &config.product)?;
    Ok((manifest, sums, signature))
}

/// Looks for a release newer than `current` with a file named by `pattern`
/// (`Vergence-{version}-macos-universal.zip`), asking each source in turn for its newest release
/// and believing the first whose signed manifest verifies. `Ok(None)` is "up to date", or a newest
/// release that has no file for this platform. An error means no source gave a verified manifest.
///
/// A source that is down, stale or hostile costs a log line and the next source. A stale one can
/// only make an update appear later, never a downgrade ([`is_newer`]) or a forgery (the signature).
pub async fn check(config: &Config, current: &str, pattern: &str) -> Result<Option<Update>> {
    if config.sources.is_empty() {
        fail!("no release source is configured");
    }
    let client = client(&config.user_agent)?;
    let mut failures = Vec::new();
    for source in &config.sources {
        let attempt = async {
            let (manifest, sums, signature) = newest(&client, config, source).await?;
            let Some((version, name)) = manifest.find(pattern)? else {
                return Ok(None);
            };
            if !is_newer(&version, current) {
                return Ok(None);
            }
            let sha256 = manifest.hash(&name).expect("find returns a listed name").to_string();
            let urls = config.sources.iter().map(|s| s.file_url(&version, &name)).collect();
            Ok::<_, Error>(Some(Update {
                version,
                name,
                sha256,
                signer: manifest.signer,
                urls,
                manifest: sums,
                signature,
                user_agent: config.user_agent.clone(),
            }))
        };
        match attempt.await {
            Ok(outcome) => return Ok(outcome),
            Err(error) => {
                tracing::warn!("release source {}: {error}", source.latest_url(""));
                failures.push(format!("{}: {error}", source.latest_url("")));
            }
        }
    }
    fail!("Couldn't get a signed release list from any source: {}", failures.join("; "))
}

impl Update {
    /// Downloads the file into `dir`, trying each source's copy in turn, and returns its path only
    /// when the SHA-256 is the signed one. `progress` gets the bytes received so far.
    pub async fn download(&self, dir: &Path, progress: impl Fn(u64)) -> Result<PathBuf> {
        if self.name.contains(['/', '\\']) || self.name.starts_with('.') {
            fail!("bad asset name {}", self.name);
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let client = client(&self.user_agent)?;
        let target = dir.join(&self.name);
        let partial = dir.join(format!("{}.partial", self.name));
        let mut failures = Vec::new();
        for url in &self.urls {
            match download_one(&client, url, &partial, &self.sha256, &progress).await {
                Ok(()) => {
                    let _ = std::fs::remove_file(&target);
                    std::fs::rename(&partial, &target).map_err(|e| e.to_string())?;
                    return Ok(target);
                }
                Err(error) => {
                    let _ = std::fs::remove_file(&partial);
                    tracing::warn!("{url}: {error}");
                    failures.push(format!("{url}: {error}"));
                }
            }
        }
        fail!("Couldn't download {}: {}", self.name, failures.join("; "))
    }
}

async fn download_one(client: &reqwest::Client, url: &str, path: &Path, expected: &str, progress: &impl Fn(u64)) -> Result<()> {
    let mut response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        fail!("{}", response.status());
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        received += chunk.len() as u64;
        if received > MAX_ASSET_BYTES {
            fail!("sent more than {MAX_ASSET_BYTES} bytes");
        }
        hash.update(&chunk);
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        progress(received);
    }
    file.sync_all().map_err(|e| e.to_string())?;
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != expected {
        fail!("its SHA-256 is {got}, not the signed {expected}");
    }
    Ok(())
}
