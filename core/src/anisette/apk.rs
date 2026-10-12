//! Fetches the two ADI libraries out of Apple's Apple Music APK, without downloading the APK.
//!
//! An APK is a zip file, and a zip's index (the central directory) sits at its end, so a few
//! HTTP range requests reach exactly the entries wanted: the index, then each library. The file
//! comes from Apple's CDN at the address Apple's own Android page links to; nothing of Apple's is
//! kept in this repository or shipped with the app. Fetched files are cached in a folder.
//!
//! Written from the zip format (PKWARE APPNOTE): the end-of-central-directory record, central
//! directory headers, local file headers; sizes are 32-bit (an APK of this size has no zip64).

use std::io::Read;
use std::path::Path;

use rootcause::prelude::*;

const APK: &str = "https://apps.mzstatic.com/content/android-apple-music-apk/applemusic.apk";

#[cfg(target_arch = "aarch64")]
const ABI: &str = "arm64-v8a";
#[cfg(target_arch = "x86_64")]
const ABI: &str = "x86_64";

pub(crate) const SERVICES: &str = "libstoreservicescore.so";
pub(crate) const CORE: &str = "libCoreADI.so";

const END_OF_DIRECTORY: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const DIRECTORY_ENTRY: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const LOCAL_ENTRY: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];

fn u16_at(bytes: &[u8], at: usize) -> Result<usize, Report> {
    Ok(u16::from_le_bytes(bytes.get(at..at + 2).ok_or_else(|| report!("truncated zip data"))?.try_into()?).into())
}

fn u32_at(bytes: &[u8], at: usize) -> Result<usize, Report> {
    Ok(u32::from_le_bytes(bytes.get(at..at + 4).ok_or_else(|| report!("truncated zip data"))?.try_into()?) as usize)
}

async fn range(client: &reqwest::Client, range: &str) -> Result<Vec<u8>, Report> {
    let response = client.get(APK).header("Range", range).header("Accept-Encoding", "identity").send().await.context("Can't reach Apple's CDN")?;
    if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        bail!("the CDN answered {} to a range request ({range})", response.status());
    }
    Ok(response.bytes().await?.to_vec())
}

struct Entry {
    method: usize,
    crc: u32,
    compressed: usize,
    size: usize,
    header_at: usize,
}

/// The entries of the central directory named in `wanted`.
fn find(directory: &[u8], wanted: &[String]) -> Result<Vec<Entry>, Report> {
    let mut found: Vec<Option<Entry>> = wanted.iter().map(|_| None).collect();
    let mut at = 0;
    while directory.get(at..at + 4) == Some(&DIRECTORY_ENTRY) {
        let (name_length, extra_length, comment_length) = (u16_at(directory, at + 28)?, u16_at(directory, at + 30)?, u16_at(directory, at + 32)?);
        let name = directory.get(at + 46..at + 46 + name_length).ok_or_else(|| report!("truncated zip directory"))?;
        if let Some(index) = wanted.iter().position(|w| w.as_bytes() == name) {
            found[index] = Some(Entry {
                method: u16_at(directory, at + 10)?,
                crc: u32_at(directory, at + 16)? as u32,
                compressed: u32_at(directory, at + 20)?,
                size: u32_at(directory, at + 24)?,
                header_at: u32_at(directory, at + 42)?,
            });
        }
        at += 46 + name_length + extra_length + comment_length;
    }
    found.into_iter().zip(wanted).map(|(entry, name)| entry.ok_or_else(|| report!("the APK has no {name}"))).collect()
}

async fn extract(client: &reqwest::Client, entry: &Entry) -> Result<Vec<u8>, Report> {
    // The local header repeats the name and has its own extra field (often padding for
    // alignment), so its length decides where the data starts.
    let header = range(client, &format!("bytes={}-{}", entry.header_at, entry.header_at + 29)).await?;
    if header.get(..4) != Some(&LOCAL_ENTRY) {
        bail!("not a zip local header");
    }
    let start = entry.header_at + 30 + u16_at(&header, 26)? + u16_at(&header, 28)?;
    let data = range(client, &format!("bytes={start}-{}", start + entry.compressed - 1)).await?;
    let bytes = match entry.method {
        0 => data,
        8 => {
            let mut out = Vec::with_capacity(entry.size);
            flate2::read::DeflateDecoder::new(&data[..]).read_to_end(&mut out)?;
            out
        }
        other => bail!("zip compression method {other} isn't supported"),
    };
    if bytes.len() != entry.size || crc32fast::hash(&bytes) != entry.crc {
        bail!("a library from the APK is corrupt (size or checksum differs)");
    }
    Ok(bytes)
}

/// The two libraries' contents, `(libstoreservicescore, libCoreADI)`, from `cache` when they are
/// there, else from Apple (and then saved to `cache`).
pub(crate) async fn libraries(cache: &Path) -> Result<(Vec<u8>, Vec<u8>), Report> {
    if let (Ok(services), Ok(core)) = (std::fs::read(cache.join(SERVICES)), std::fs::read(cache.join(CORE))) {
        return Ok((services, core));
    }
    let client = reqwest::Client::builder().user_agent(concat!("Apparition/", env!("CARGO_PKG_VERSION"))).build()?;
    // The record that ends the file is 22 bytes plus a comment of up to 64 KiB.
    let tail = range(&client, "bytes=-65557").await?;
    let eocd = tail.windows(4).rposition(|w| w == END_OF_DIRECTORY).ok_or_else(|| report!("no zip end record: is that an APK?"))?;
    let (size, offset) = (u32_at(&tail, eocd + 12)?, u32_at(&tail, eocd + 16)?);
    let directory = range(&client, &format!("bytes={offset}-{}", offset + size - 1)).await?;
    let wanted = [format!("lib/{ABI}/{SERVICES}"), format!("lib/{ABI}/{CORE}")];
    let entries = find(&directory, &wanted)?;
    let services = extract(&client, &entries[0]).await?;
    let core = extract(&client, &entries[1]).await?;
    std::fs::create_dir_all(cache)?;
    // Written to a name of its own first, so a download that stops halfway can't be mistaken
    // for a complete one; a rename replaces the file whole.
    for (name, bytes) in [(SERVICES, &services), (CORE, &core)] {
        // Named for this process too, as two processes may fetch at the same time.
        let partial = cache.join(format!("{name}.{}.part", std::process::id()));
        std::fs::write(&partial, bytes)?;
        std::fs::rename(&partial, cache.join(name))?;
    }
    Ok((services, core))
}

#[cfg(test)]
mod tests {
    /// Reaches the network: `APPARITION_NET=1`.
    #[tokio::test]
    async fn fetches_both_libraries() {
        if std::env::var_os("APPARITION_NET").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("apparition-apk-{}", std::process::id()));
        let (services, core) = super::libraries(&dir).await.unwrap();
        assert_eq!(&services[..4], b"\x7fELF");
        assert_eq!(&core[..4], b"\x7fELF");
        let (again, _) = super::libraries(&dir).await.unwrap();
        assert_eq!(again, services, "second call reads the cache");
        std::fs::remove_dir_all(&dir).ok();
    }
}
