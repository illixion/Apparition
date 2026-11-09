use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result, fail};
use crate::manifest::Manifest;
use crate::net::Net;
use crate::source::{Config, Source};
use crate::version::is_newer;

/// Larger than any file we ship; a host that keeps sending is cut off here.
const MAX_ASSET_BYTES: u64 = 2 << 30;
/// A manifest and a signature are small; anything bigger isn't one.
const MAX_METADATA_BYTES: usize = 1 << 20;
/// All of one manifest or signature request, redirects and body included. Per-read timeouts alone
/// let a host that sends a byte every few seconds hold a check (and the next source) forever.
const METADATA_DEADLINE: Duration = Duration::from_secs(30);
/// Until a download's response headers arrive, redirects included.
const DOWNLOAD_START_DEADLINE: Duration = Duration::from_secs(60);
/// A download slower than this over a whole window is a stalled one, and the next copy is tried.
const MIN_DOWNLOAD_BYTES_PER_WINDOW: u64 = 64 << 10;
const DOWNLOAD_WINDOW: Duration = Duration::from_secs(60);

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
    net: Net,
}

async fn fetch(net: &Net, url: &str) -> Result<Vec<u8>> {
    match tokio::time::timeout(METADATA_DEADLINE, fetch_body(net, url)).await {
        Ok(result) => result,
        Err(_) => fail!("{url}: no complete answer in {}s", METADATA_DEADLINE.as_secs()),
    }
}

async fn fetch_body(net: &Net, url: &str) -> Result<Vec<u8>> {
    let mut response = net.get(url).await?;
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
async fn newest(net: &Net, config: &Config, source: &Source) -> Result<(Manifest, Vec<u8>, String)> {
    let sums = fetch(net, &source.latest_url("SHA256SUMS")).await?;
    let signature = fetch(net, &source.latest_url("SHA256SUMS.sig")).await?;
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
    let net = Net::new(config)?;
    let mut failures = Vec::new();
    for source in &config.sources {
        let attempt = async {
            let (manifest, sums, signature) = newest(&net, config, source).await?;
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
                net: net.clone(),
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
    /// Changes how the download reaches the sources, for a setting changed since the check: the
    /// [`Privacy`](crate::Privacy) the check ran with is otherwise used.
    pub fn with_privacy(mut self, privacy: crate::Privacy) -> Self {
        self.net.set_privacy(privacy);
        self
    }

    /// Downloads the file into `dir`, trying each source's copy in turn, and returns its path only
    /// when the SHA-256 is the signed one. `progress` gets the bytes received so far.
    pub async fn download(&self, dir: &Path, progress: impl Fn(u64)) -> Result<PathBuf> {
        if self.name.contains(['/', '\\']) || self.name.starts_with('.') {
            fail!("bad asset name {}", self.name);
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let target = dir.join(&self.name);
        let partial = dir.join(format!("{}.partial", self.name));
        let mut failures = Vec::new();
        for url in &self.urls {
            match download_one(&self.net, url, &partial, &self.sha256, &progress).await {
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

async fn download_one(net: &Net, url: &str, path: &Path, expected: &str, progress: &impl Fn(u64)) -> Result<()> {
    let mut response = match tokio::time::timeout(DOWNLOAD_START_DEADLINE, net.get(url)).await {
        Ok(response) => response?,
        Err(_) => fail!("no answer in {}s", DOWNLOAD_START_DEADLINE.as_secs()),
    };
    if !response.status().is_success() {
        fail!("{}", response.status());
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    let (mut window_start, mut window_bytes) = (Instant::now(), 0u64);
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        received += chunk.len() as u64;
        window_bytes += chunk.len() as u64;
        if window_start.elapsed() >= DOWNLOAD_WINDOW {
            if window_bytes < MIN_DOWNLOAD_BYTES_PER_WINDOW {
                fail!("stalled: {window_bytes} bytes in {}s", window_start.elapsed().as_secs());
            }
            (window_start, window_bytes) = (Instant::now(), 0);
        }
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
