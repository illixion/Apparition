//! Fetches the two ADI libraries out of Apple's Apple Music APK, without downloading the APK.
//!
//! An APK is a zip file, and a zip's index (the central directory) sits at its end, so a few
//! HTTP range requests reach exactly the entries wanted: the index, then each library. The file
//! comes from Apple's CDN at the address Apple's own Android page links to; nothing of Apple's is
//! kept in this repository or shipped with the app. Fetched files are cached in a folder.
//!
//! Written from the zip format (PKWARE APPNOTE): the end-of-central-directory record, central
//! directory headers, local file headers; sizes are 32-bit (an APK of this size has no zip64).
//!
//! When the CDN can't be reached, [`import`] takes the APK, or the libraries out of it, from a file
//! the person got some other way. That is the one route by which a file Apparition didn't fetch
//! gets run (in this process, by `elf`), so it is checked against a SHA-256 of the libraries this
//! version was built and tested with, and nothing else. A file that differs is refused with a
//! [`ChecksumMismatch`] unless the person chose to go on (`accept_unverified`): Apple may update the
//! libraries and take the old address down, leaving this table as the only reference there is.
//! Even then the file has to be a 64-bit ELF library for this processor, and a library of the
//! other processor's known build is refused.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rootcause::prelude::*;
use sha2::{Digest, Sha256};

const APK: &str = "https://apps.mzstatic.com/content/android-apple-music-apk/applemusic.apk";

#[cfg(target_arch = "aarch64")]
const ABI: &str = "arm64-v8a";
#[cfg(target_arch = "x86_64")]
const ABI: &str = "x86_64";

pub(crate) const SERVICES: &str = "libstoreservicescore.so";
pub(crate) const CORE: &str = "libCoreADI.so";

/// A library of the Apple Music APK that Apple's CDN served on 2026-10-29, and its SHA-256.
struct Known {
    abi: &'static str,
    name: &'static str,
    sha256: &'static str,
}

const KNOWN: [Known; 4] = [
    Known { abi: "arm64-v8a", name: SERVICES, sha256: "bd335e6963ba5ae2f5babca613435ad56d2660a9aee37df35f876c458d105ce4" },
    Known { abi: "arm64-v8a", name: CORE, sha256: "9d5b557cf3faf88c34c394ab2cd7fc1a3b4acf3aed51f28cd0bc093858107c22" },
    Known { abi: "x86_64", name: SERVICES, sha256: "d8be230df34c0044758d43e33a8dabd3236b9a4258c4d48cb366911224d52f68" },
    Known { abi: "x86_64", name: CORE, sha256: "ece4ffa6bbe4239e8f0d4714da57e62ce5e3c581d19553a83d78ac897783b905" },
];

/// Fetching the libraries from Apple's CDN failed. A marker context, so the apps can offer a retry
/// and [`import`] where they show the error.
#[derive(Debug)]
pub struct LibraryDownload;

impl std::fmt::Display for LibraryDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Couldn't fetch Apple's anisette libraries from Apple's CDN")
    }
}

impl std::error::Error for LibraryDownload {}

/// A library isn't the version Apparition was built with. A marker context: the apps show the
/// warning with a way to go on anyway.
#[derive(Debug)]
pub struct ChecksumMismatch;

impl std::fmt::Display for ChecksumMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("That isn't the version of the libraries Apparition was built with (its checksum differs from the Apple Music APK it was tested with). Apparition can't tell whether it is Apple's, newer or altered, and it runs them in its own process.")
    }
}

impl std::error::Error for ChecksumMismatch {}

/// Kept in the cache while it holds a library the person supplied that didn't match the checksum
/// (an unsupported setup the apps keep warning about), and removed when that is replaced.
const UNVERIFIED_MARKER: &str = "supplied-unverified";

/// The most a library file may be; the real ones are about 2.4 MB.
const MAX_LIBRARY: u64 = 16 * 1024 * 1024;

const END_OF_DIRECTORY: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const DIRECTORY_ENTRY: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const LOCAL_ENTRY: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];

fn u16_at(bytes: &[u8], at: usize) -> Result<usize, Report> {
    Ok(u16::from_le_bytes(bytes.get(at..at + 2).ok_or_else(|| report!("truncated zip data"))?.try_into()?).into())
}

fn u32_at(bytes: &[u8], at: usize) -> Result<usize, Report> {
    Ok(u32::from_le_bytes(bytes.get(at..at + 4).ok_or_else(|| report!("truncated zip data"))?.try_into()?) as usize)
}

/// Where the APK is read from.
enum Source {
    Remote(reqwest::Client),
    File(PathBuf),
}

/// A part of the APK: bytes `from..=to`, or the last `n`.
#[derive(Clone, Copy)]
enum Part {
    Span(usize, usize),
    Tail(usize),
}

async fn range(source: &Source, part: Part) -> Result<Vec<u8>, Report> {
    match source {
        Source::Remote(client) => {
            let range = match part {
                Part::Span(from, to) => format!("bytes={from}-{to}"),
                Part::Tail(n) => format!("bytes=-{n}"),
            };
            let response = client.get(APK).header("Range", &range).header("Accept-Encoding", "identity").send().await.context("Can't reach Apple's CDN")?;
            if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                bail!("the CDN answered {} to a range request ({range})", response.status());
            }
            Ok(response.bytes().await?.to_vec())
        }
        Source::File(path) => {
            let mut file = std::fs::File::open(path).context("Can't open the file")?;
            let length = file.metadata()?.len() as usize;
            let (from, to) = match part {
                Part::Span(from, to) => (from, to),
                Part::Tail(n) => (length.saturating_sub(n), length.saturating_sub(1)),
            };
            if length == 0 || to >= length || from > to {
                bail!("the file ends before the part of the APK wanted (is that an APK?)");
            }
            file.seek(SeekFrom::Start(from as u64))?;
            let mut bytes = vec![0; to - from + 1];
            file.read_exact(&mut bytes)?;
            Ok(bytes)
        }
    }
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

async fn extract(source: &Source, entry: &Entry) -> Result<Vec<u8>, Report> {
    // The local header repeats the name and has its own extra field (often padding for
    // alignment), so its length decides where the data starts.
    let header = range(source, Part::Span(entry.header_at, entry.header_at + 29)).await?;
    if header.get(..4) != Some(&LOCAL_ENTRY) {
        bail!("not a zip local header");
    }
    let start = entry.header_at + 30 + u16_at(&header, 26)? + u16_at(&header, 28)?;
    let data = range(source, Part::Span(start, start + entry.compressed - 1)).await?;
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

/// The two libraries' contents, `(libstoreservicescore, libCoreADI)`, out of the APK at `source`.
async fn fetch(source: &Source) -> Result<(Vec<u8>, Vec<u8>), Report> {
    // The record that ends the file is 22 bytes plus a comment of up to 64 KiB.
    let tail = range(source, Part::Tail(65557)).await?;
    let eocd = tail.windows(4).rposition(|w| w == END_OF_DIRECTORY).ok_or_else(|| report!("no zip end record: is that an APK?"))?;
    let (size, offset) = (u32_at(&tail, eocd + 12)?, u32_at(&tail, eocd + 16)?);
    let directory = range(source, Part::Span(offset, offset + size - 1)).await?;
    let wanted = [format!("lib/{ABI}/{SERVICES}"), format!("lib/{ABI}/{CORE}")];
    let entries = find(&directory, &wanted)?;
    let services = extract(source, &entries[0]).await?;
    let core = extract(source, &entries[1]).await?;
    Ok((services, core))
}

/// Saves a library under `name` in `cache`. Written to a name of its own first, so a download
/// that stops halfway can't be mistaken for a complete one; a rename replaces the file whole.
fn save(cache: &Path, name: &str, bytes: &[u8]) -> Result<(), Report> {
    std::fs::create_dir_all(cache)?;
    // Named for this process too, as two processes may fetch at the same time.
    let partial = cache.join(format!("{name}.{}.part", std::process::id()));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, cache.join(name))?;
    Ok(())
}

/// The two libraries' contents, `(libstoreservicescore, libCoreADI)`, from `cache` when they are
/// there, else from Apple (and then saved to `cache`).
pub(crate) async fn libraries(cache: &Path) -> Result<(Vec<u8>, Vec<u8>), Report> {
    if let (Ok(services), Ok(core)) = (std::fs::read(cache.join(SERVICES)), std::fs::read(cache.join(CORE))) {
        return Ok((services, core));
    }
    let download = async {
        let client = reqwest::Client::builder().user_agent(concat!("Apparition/", env!("CARGO_PKG_VERSION"))).build()?;
        let (services, core) = fetch(&Source::Remote(client)).await?;
        save(cache, SERVICES, &services)?;
        save(cache, CORE, &core)?;
        let _ = std::fs::remove_file(cache.join(UNVERIFIED_MARKER));
        Ok::<_, Report>((services, core))
    };
    Ok(download.await.context(LibraryDownload)?)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// ELF header values for a 64-bit little-endian shared library of this processor.
#[cfg(target_arch = "aarch64")]
const MACHINE: u16 = 183;
#[cfg(target_arch = "x86_64")]
const MACHINE: u16 = 62;

/// Whether `bytes` starts like a 64-bit little-endian ELF shared object for this processor.
fn is_library_for_this_processor(bytes: &[u8]) -> bool {
    bytes.len() > 20 && bytes[..4] == *b"\x7fELF" && bytes[4] == 2 && bytes[5] == 1 && u16::from_le_bytes([bytes[18], bytes[19]]) == MACHINE
}

/// Which library `bytes` is, and whether it matched the table. `name` is what the file or APK
/// entry is called, which decides when the hash doesn't (only with `accept_unverified`).
fn identify(bytes: &[u8], name: Option<&str>, accept_unverified: bool) -> Result<(&'static str, bool), Report> {
    let hash = sha256(bytes);
    match KNOWN.iter().find(|known| known.sha256 == hash) {
        Some(known) if known.abi == ABI => return Ok((known.name, true)),
        Some(known) => bail!("That is {}, but for {} processors; this computer needs the {ABI} one.", known.name, known.abi),
        None => {}
    }
    if !accept_unverified {
        return Err(report!(ChecksumMismatch).into());
    }
    if !is_library_for_this_processor(bytes) {
        bail!("That isn't a 64-bit library for this computer's processor ({ABI}).");
    }
    match [SERVICES, CORE].into_iter().find(|known| Some(*known) == name) {
        Some(known) => Ok((known, false)),
        None => bail!("Can't tell which library that is: it has to be called {SERVICES} or {CORE}."),
    }
}

/// What [`import`] left in the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    /// `libstoreservicescore.so` is in the cache.
    pub services: bool,
    /// `libCoreADI.so` is in the cache.
    pub core: bool,
    /// A file taken now didn't match the checksum, and was used because the person said to.
    pub unverified: bool,
}

impl Imported {
    /// Both libraries are there, so the built-in anisette can start.
    pub fn complete(self) -> bool {
        self.services && self.core
    }
}

/// Takes the libraries from files the person provides: the Apple Music APK, or `libstoreservicescore.so`
/// and `libCoreADI.so` (from the APK's `lib/<abi>/` folder) singly or together. Each library has to
/// hash to the one this version was built with ([`identify`]), or be accepted anyway with
/// `accept_unverified`. Nothing is saved unless everything given is accepted.
pub(crate) async fn import(cache: &Path, paths: &[PathBuf], accept_unverified: bool) -> Result<Imported, Report> {
    let mut accepted: Vec<(&'static str, Vec<u8>)> = Vec::new();
    let mut unverified = false;
    for path in paths {
        let shown = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
        let result: Result<(), Report> = async {
            let mut start = [0u8; 4];
            std::fs::File::open(path).context("Can't open the file")?.read_exact(&mut start).context("The file is too short to be an APK or a library")?;
            if start == LOCAL_ENTRY || start == END_OF_DIRECTORY {
                let (services, core) = fetch(&Source::File(path.clone())).await?;
                for (bytes, name) in [(services, SERVICES), (core, CORE)] {
                    let (name, verified) = identify(&bytes, Some(name), accept_unverified)?;
                    unverified |= !verified;
                    accepted.push((name, bytes));
                }
            } else if start == *b"\x7fELF" {
                if std::fs::metadata(path)?.len() > MAX_LIBRARY {
                    bail!("That file is too large to be one of the libraries.");
                }
                let bytes = std::fs::read(path)?;
                let (name, verified) = identify(&bytes, path.file_name().and_then(|name| name.to_str()), accept_unverified)?;
                unverified |= !verified;
                accepted.push((name, bytes));
            } else {
                bail!("That is neither an APK nor one of the libraries (.so files).");
            }
            Ok(())
        }
        .await;
        result.context(format!("Can't use {shown}"))?;
    }
    for (name, bytes) in &accepted {
        save(cache, name, bytes)?;
    }
    if unverified {
        std::fs::write(cache.join(UNVERIFIED_MARKER), "a library here was supplied and didn't match the checksum\n")?;
    } else if [SERVICES, CORE].iter().all(|name| std::fs::read(cache.join(name)).is_ok_and(|bytes| is_known(name, &bytes))) {
        // Both in place are the tested version now.
        let _ = std::fs::remove_file(cache.join(UNVERIFIED_MARKER));
    }
    Ok(Imported { services: cache.join(SERVICES).is_file(), core: cache.join(CORE).is_file(), unverified: self::unverified(cache) })
}

fn is_known(name: &str, bytes: &[u8]) -> bool {
    let hash = sha256(bytes);
    KNOWN.iter().any(|known| known.abi == ABI && known.name == name && known.sha256 == hash)
}

/// Whether the cache holds a library the person supplied that didn't match the checksum.
pub(crate) fn unverified(cache: &Path) -> bool {
    cache.join(UNVERIFIED_MARKER).is_file()
}

/// Forgets the cached libraries, supplied or fetched, so the next start fetches Apple's again.
pub(crate) fn reset(cache: &Path) -> Result<(), Report> {
    for name in [SERVICES, CORE, UNVERIFIED_MARKER] {
        match std::fs::remove_file(cache.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(report!(error).context(format!("Can't remove {name}")).into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip with each `(name, data)` stored, no compression: local headers, then the central
    /// directory, then the end record.
    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, data) in entries {
            let at = out.len() as u32;
            let crc = crc32fast::hash(data);
            out.extend(LOCAL_ENTRY);
            out.extend([20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend(crc.to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((data.len() as u32).to_le_bytes());
            out.extend((name.len() as u16).to_le_bytes());
            out.extend([0, 0]);
            out.extend(name.as_bytes());
            out.extend(*data);
            directory.extend(DIRECTORY_ENTRY);
            directory.extend([20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            directory.extend(crc.to_le_bytes());
            directory.extend((data.len() as u32).to_le_bytes());
            directory.extend((data.len() as u32).to_le_bytes());
            directory.extend((name.len() as u16).to_le_bytes());
            directory.extend([0; 12]);
            directory.extend(at.to_le_bytes());
            directory.extend(name.as_bytes());
        }
        let offset = out.len() as u32;
        out.extend(&directory);
        out.extend(END_OF_DIRECTORY);
        out.extend([0, 0, 0, 0]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((directory.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apparition-apk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_table_covers_both_libraries_of_this_processor() {
        let mine: Vec<&str> = KNOWN.iter().filter(|known| known.abi == ABI).map(|known| known.name).collect();
        assert_eq!(mine, [SERVICES, CORE]);
        assert!(KNOWN.iter().all(|known| known.sha256.len() == 64 && known.sha256.bytes().all(|b| b.is_ascii_hexdigit())));
    }

    #[test]
    fn a_library_of_another_version_is_refused() {
        let error = identify(b"\x7fELF but not Apple's", Some(SERVICES), false).unwrap_err();
        assert!(error.iter_reports().any(|cause| cause.downcast_current_context::<ChecksumMismatch>().is_some()), "{error}");
        assert!(error.to_string().contains("isn't the version of the libraries Apparition was built with"), "{error}");
    }

    #[tokio::test]
    async fn an_apk_of_another_version_is_refused_and_nothing_is_saved() {
        let dir = scratch("other-version");
        let apk = dir.join("applemusic.apk");
        std::fs::write(&apk, zip(&[(&format!("lib/{ABI}/{SERVICES}"), b"\x7fELF one"), (&format!("lib/{ABI}/{CORE}"), b"\x7fELF two")])).unwrap();
        let cache = dir.join("libraries");
        let error = import(&cache, &[apk], false).await.unwrap_err();
        let text = crate::errors::describe(&error);
        assert!(text.contains("Can't use applemusic.apk") && text.contains("isn't the version of the libraries Apparition was built with"), "{text}");
        assert!(error.iter_reports().any(|cause| cause.downcast_current_context::<ChecksumMismatch>().is_some()));
        assert!(!cache.exists());
    }

    #[tokio::test]
    async fn files_that_are_neither_are_refused() {
        let dir = scratch("neither");
        let (text, short, missing) = (dir.join("notes.txt"), dir.join("short"), dir.join("absent.apk"));
        std::fs::write(&text, "hello there").unwrap();
        std::fs::write(&short, "PK").unwrap();
        for path in [text, short, missing] {
            let error = import(&dir.join("libraries"), &[path], true).await.unwrap_err();
            assert!(crate::errors::describe(&error).contains("Can't use"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_truncated_apk_is_refused() {
        let dir = scratch("truncated");
        let apk = dir.join("cut.apk");
        let whole = zip(&[(&format!("lib/{ABI}/{SERVICES}"), b"\x7fELF one")]);
        std::fs::write(&apk, &whole[..whole.len() - 10]).unwrap();
        // Starts like an APK, but its end record is gone.
        assert!(import(&dir.join("libraries"), &[apk], true).await.is_err());
    }

    /// A library-shaped file for this processor that isn't Apple's: the header of a 64-bit
    /// little-endian shared object, then `tail`.
    fn library_shaped(tail: &str) -> Vec<u8> {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 0];
        bytes.extend(MACHINE.to_le_bytes());
        bytes.extend(tail.as_bytes());
        bytes
    }

    #[tokio::test]
    async fn a_newer_apk_is_used_when_the_person_says_so() {
        let dir = scratch("override");
        let apk = dir.join("newer.apk");
        std::fs::write(&apk, zip(&[(&format!("lib/{ABI}/{SERVICES}"), &library_shaped("services v2")), (&format!("lib/{ABI}/{CORE}"), &library_shaped("core v2"))])).unwrap();
        let cache = dir.join("libraries");
        assert!(import(&cache, &[apk.clone()], false).await.is_err());
        assert!(!cache.exists());
        let imported = import(&cache, &[apk], true).await.unwrap();
        assert_eq!(imported, Imported { services: true, core: true, unverified: true });
        assert!(std::fs::read(cache.join(CORE)).unwrap().ends_with(b"core v2"));
        // The warning outlives the import, until the libraries are replaced or forgotten.
        assert!(unverified(&cache));
        reset(&cache).unwrap();
        assert!(!unverified(&cache) && !cache.join(CORE).exists() && !cache.join(SERVICES).exists());
        reset(&cache).unwrap();
    }

    #[tokio::test]
    async fn going_on_anyway_still_needs_a_library_for_this_processor_with_a_name() {
        let dir = scratch("override-limits");
        let wrong_cpu = dir.join(SERVICES);
        let mut other = library_shaped("x");
        other[18] = 40; // an ARM 32-bit machine number
        std::fs::write(&wrong_cpu, other).unwrap();
        let unnamed = dir.join("something.so");
        std::fs::write(&unnamed, library_shaped("x")).unwrap();
        let named = dir.join(CORE);
        std::fs::write(&named, library_shaped("x")).unwrap();
        let cache = dir.join("libraries");
        let text = crate::errors::describe(&import(&cache, &[wrong_cpu], true).await.unwrap_err());
        assert!(text.contains("64-bit library for this computer's processor"), "{text}");
        let text = crate::errors::describe(&import(&cache, &[unnamed], true).await.unwrap_err());
        assert!(text.contains("Can't tell which library"), "{text}");
        let imported = import(&cache, &[named], true).await.unwrap();
        assert_eq!(imported, Imported { services: false, core: true, unverified: true });
    }

    /// Reaches the network: `APPARITION_NET=1`.
    #[tokio::test]
    async fn fetches_both_libraries() {
        if std::env::var_os("APPARITION_NET").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = scratch("net");
        let (services, core) = super::libraries(&dir).await.unwrap();
        assert_eq!(&services[..4], b"\x7fELF");
        assert_eq!(&core[..4], b"\x7fELF");
        // What came from Apple's CDN is what the table pins: this is also how the table is checked.
        assert_eq!(identify(&services, None, false).unwrap(), (SERVICES, true));
        assert_eq!(identify(&core, None, false).unwrap(), (CORE, true));
        let (again, _) = super::libraries(&dir).await.unwrap();
        assert_eq!(again, services);

        // The same two files, handed over singly in the wrong order, and a third file refused with them.
        let (cache, mine) = (scratch("net-import"), scratch("net-files"));
        std::fs::write(mine.join("a.bin"), &core).unwrap();
        std::fs::write(mine.join("b.bin"), &services).unwrap();
        let first = import(&cache.join("libraries"), &[mine.join("a.bin")], false).await.unwrap();
        assert_eq!(first, Imported { services: false, core: true, unverified: false });
        let both = import(&cache.join("libraries"), &[mine.join("b.bin")], false).await.unwrap();
        assert!(both.complete());
        assert_eq!(std::fs::read(cache.join("libraries").join(SERVICES)).unwrap(), services);
        assert!(!unverified(&cache.join("libraries")));
    }
}
