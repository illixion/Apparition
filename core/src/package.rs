//! Apps that come as something other than an IPA.
//!
//! A `.deb` is how Kodi, and many apps from the jailbreak era, are published: an `ar` archive
//! whose data archive (a tar, usually xz-compressed) holds the app as `Applications/<Name>.app`.
//! [`convert`] repacks that app as an IPA, so signing, installing and refreshing only ever see
//! IPAs. The tar is streamed into the zip, so a 400 MB app never sits in memory or needs a
//! second copy on disk.

use std::{
    collections::{HashMap, HashSet},
    ffi::c_char,
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

use rootcause::prelude::*;
use serde_json::json;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

use crate::{reply, text};

/// What a file is, by its first bytes rather than its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// A zip: an IPA, or anything else zipped.
    Zip,
    /// A Debian package.
    Deb,
    Unknown,
}

pub fn format(path: &Path) -> std::io::Result<Format> {
    let mut head = [0u8; 8];
    let read = File::open(path)?.read(&mut head)?;
    Ok(match &head[..read] {
        [b'P', b'K', ..] => Format::Zip,
        b"!<arch>\n" => Format::Deb,
        _ => Format::Unknown,
    })
}

/// What [`convert`] made.
#[derive(Debug)]
pub struct Converted {
    /// The app's folder name, e.g. `Kodi.app`.
    pub app: String,
    pub files: usize,
    /// Size of the IPA.
    pub bytes: u64,
    /// Other apps the package holds and `convert` left out.
    pub others: Vec<String>,
}

/// `path` itself when it is an IPA (or any zip), else a package converted into `scratch` as an
/// IPA named after it. Fails, saying what the file is, for anything else.
pub fn ensure_ipa(path: &Path, scratch: &Path) -> Result<PathBuf, Report> {
    match format(path).context("Reading the file failed")? {
        Format::Zip => Ok(path.to_path_buf()),
        Format::Deb => {
            std::fs::create_dir_all(scratch)?;
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "app".into());
            let out = scratch.join(format!("{stem}.ipa"));
            convert(path, &out)?;
            Ok(out)
        }
        Format::Unknown => bail!("{} isn't an IPA or a .deb package", path.display()),
    }
}

/// Repacks the app in the `.deb` at `package` as an IPA at `out`. The app is the one under
/// `Applications/` (also where rootless jailbreaks put it: `var/jb/Applications/`); of several,
/// the biggest. A package holding an `.ipa` instead gives that IPA.
pub fn convert(package: &Path, out: &Path) -> Result<Converted, Report> {
    let result = convert_into(package, out);
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn convert_into(package: &Path, out: &Path) -> Result<Converted, Report> {
    // First pass: which apps the package holds, and which files other entries hard-link to
    // (the tar keeps the data once, with the first path).
    let mut apps: HashMap<String, u64> = HashMap::new();
    let mut ipas: Vec<(String, u64)> = Vec::new();
    let mut link_targets: HashSet<String> = HashSet::new();
    let mut seen = Vec::new();
    for entry in data_archive(package)?.entries()? {
        let entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        let size = entry.size();
        if let Some((app, _)) = locate(&entry.path()?) {
            *apps.entry(app).or_default() += size;
        } else if entry.header().entry_type().is_file() && path.to_ascii_lowercase().ends_with(".ipa") {
            ipas.push((path.clone(), size));
        }
        if entry.header().entry_type().is_hard_link()
            && let Some(target) = entry.link_name()?
        {
            link_targets.insert(clean(&target));
        }
        if seen.len() < 6
            && let Some(Component::Normal(first)) = entry.path()?.components().find(|c| !matches!(c, Component::CurDir))
        {
            let first = first.to_string_lossy().to_string();
            if !seen.contains(&first) {
                seen.push(first);
            }
        }
    }

    let chosen = apps.iter().max_by_key(|(name, size)| (**size, std::cmp::Reverse((*name).clone()))).map(|(name, _)| name.clone());
    let Some(app) = chosen else {
        if let Some((name, _)) = ipas.iter().max_by_key(|(_, size)| *size) {
            return copy_ipa(package, name, out);
        }
        bail!(
            "This package has no app in it (no Applications/<name>.app{}). It holds: {}",
            if ipas.is_empty() { "" } else { ", no .ipa" },
            if seen.is_empty() { "nothing".to_string() } else { seen.join(", ") }
        );
    };
    let mut others: Vec<String> = apps.keys().filter(|name| **name != app).cloned().collect();
    others.sort();

    // Second pass: the app's entries, as an IPA.
    let mut writer = ZipWriter::new(BufWriter::new(File::create(out)?));
    writer.add_directory("Payload/", SimpleFileOptions::default())?;
    let mut files = 0;
    let mut has_info_plist = false;
    let mut kept: HashMap<String, Vec<u8>> = HashMap::new();
    for entry in data_archive(package)?.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let Some((name, relative)) = locate(&path) else { continue };
        if name != app {
            continue;
        }
        let target = format!("Payload/{app}/{}", relative.join("/"));
        let mode = entry.header().mode().unwrap_or(0o644) & 0o7777;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            let dir = if relative.is_empty() { format!("Payload/{app}/") } else { format!("{target}/") };
            writer.add_directory(dir, SimpleFileOptions::default().unix_permissions(if mode == 0 { 0o755 } else { mode }))?;
        } else if kind.is_symlink() {
            let link = entry.link_name()?.ok_or_else(|| report!("{} is a link to nothing", path.display()))?;
            writer.add_symlink(target, link.to_string_lossy(), SimpleFileOptions::default())?;
        } else if kind.is_hard_link() {
            let source = entry.link_name()?.ok_or_else(|| report!("{} is a link to nothing", path.display()))?;
            let data = kept.get(&clean(&source)).ok_or_else(|| report!("{} is a hard link to {}, which isn't in the app", path.display(), source.display()))?;
            writer.start_file(target, file_options(mode, data.len() as u64))?;
            writer.write_all(data)?;
            files += 1;
        } else if kind.is_file() {
            if relative.len() == 1 && relative[0] == "Info.plist" {
                has_info_plist = true;
            }
            writer.start_file(target, file_options(mode, entry.size()))?;
            if link_targets.contains(&clean(&path)) {
                let mut data = Vec::with_capacity(entry.size() as usize);
                entry.read_to_end(&mut data)?;
                writer.write_all(&data)?;
                kept.insert(clean(&path), data);
            } else {
                std::io::copy(&mut entry, &mut writer)?;
            }
            files += 1;
        }
    }
    writer.finish()?.flush()?;
    if !has_info_plist {
        bail!("{app} in this package has no Info.plist, so it isn't an iOS app");
    }
    Ok(Converted { app, files, bytes: std::fs::metadata(out)?.len(), others })
}

/// A package whose only app is an `.ipa` inside it: that file, as the IPA.
fn copy_ipa(package: &Path, name: &str, out: &Path) -> Result<Converted, Report> {
    for entry in data_archive(package)?.entries()? {
        let mut entry = entry?;
        if entry.path()?.to_string_lossy() == name {
            let mut file = BufWriter::new(File::create(out)?);
            std::io::copy(&mut entry, &mut file)?;
            file.flush()?;
            let bytes = std::fs::metadata(out)?.len();
            let app = Path::new(name).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            return Ok(Converted { app, files: 1, bytes, others: Vec::new() });
        }
    }
    bail!("{name} vanished from the package")
}

fn file_options(mode: u32, size: u64) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
        .unix_permissions(if mode == 0 { 0o644 } else { mode })
        .large_file(size >= u32::MAX as u64)
}

/// An entry's path as the tar spells it, minus a leading `./`.
fn clean(path: &Path) -> String {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Where an entry is in an app: the app's folder name and the path inside it. `None` for
/// anything outside an app in `Applications/`, and for paths that climb out of it.
fn locate(path: &Path) -> Option<(String, Vec<String>)> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir | Component::RootDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    let at = parts.iter().position(|part| part == "Applications")?;
    let app = parts.get(at + 1)?;
    if !app.ends_with(".app") || app.len() <= 4 {
        return None;
    }
    Some((app.clone(), parts[at + 2..].to_vec()))
}

// MARK: The package's files

struct Member {
    name: String,
    offset: u64,
    size: u64,
}

/// The members of an `ar` archive: 8 bytes of magic, then a 60-byte header (name, date, owner,
/// group, mode, size, end mark) and the data, padded to an even length, for each.
fn members(file: &mut File) -> Result<Vec<Member>, Report> {
    let length = file.metadata()?.len();
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic).context("This isn't a .deb package")?;
    if &magic != b"!<arch>\n" {
        bail!("This isn't a .deb package");
    }
    let mut found = Vec::new();
    let mut at = 8u64;
    while at + 60 <= length {
        file.seek(SeekFrom::Start(at))?;
        let mut header = [0u8; 60];
        file.read_exact(&mut header)?;
        if &header[58..60] != b"`\n" {
            bail!("This .deb package is damaged (bad member header at byte {at})");
        }
        let name = String::from_utf8_lossy(&header[..16]).trim_end().trim_end_matches('/').to_string();
        let size: u64 = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .map_err(|_| report!("This .deb package is damaged (bad size for {name})"))?;
        found.push(Member { name, offset: at + 60, size });
        at += 60 + size + (size & 1);
    }
    Ok(found)
}

/// The package's data archive, decompressed as it is read.
fn data_archive(package: &Path) -> Result<tar::Archive<Box<dyn Read>>, Report> {
    let mut file = File::open(package).context("Opening the package failed")?;
    let members = members(&mut file)?;
    let member = members
        .iter()
        .find(|m| m.name == "data.tar" || m.name.starts_with("data.tar."))
        .ok_or_else(|| report!("This .deb package has no data.tar (it holds: {})", members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>().join(", ")))?;
    file.seek(SeekFrom::Start(member.offset))?;
    let raw = BufReader::new(file.take(member.size));
    let reader: Box<dyn Read> = match member.name.strip_prefix("data.tar").unwrap_or("") {
        "" => Box::new(raw),
        ".gz" => Box::new(flate2::read::GzDecoder::new(raw)),
        ".xz" => Box::new(xz2::read::XzDecoder::new(raw)),
        ".zst" => Box::new(ruzstd::StreamingDecoder::new(raw).map_err(|e| report!("The package's zstd data is damaged: {e}"))?),
        other => bail!("This .deb package's data is compressed as {other}, which Apparition can't unpack (it reads .gz, .xz and .zst)"),
    };
    Ok(tar::Archive::new(reader))
}

// MARK: C ABI

/// Repacks the app in the `.deb` at `package` as an IPA at `out`.
/// Returns `{ok, app, files, bytes, others}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_convert_package(package: *const c_char, out: *const c_char) -> *mut c_char {
    reply((|| {
        let package = PathBuf::from(unsafe { text(package) }?);
        let out = PathBuf::from(unsafe { text(out) }?);
        let converted = convert(&package, &out)?;
        Ok(json!({"app": converted.app, "files": converted.files, "bytes": converted.bytes, "others": converted.others}))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tar_with(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        builder.into_inner().unwrap()
    }

    fn add_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8], mode: u32) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, path, data).unwrap();
    }

    fn add_dir(builder: &mut tar::Builder<Vec<u8>>, path: &str) {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Directory);
        builder.append_data(&mut header, path, std::io::empty()).unwrap();
    }

    fn add_link(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str, kind: tar::EntryType) {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o755);
        header.set_entry_type(kind);
        builder.append_link(&mut header, path, target).unwrap();
    }

    fn member(name: &str, data: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", 0, 0, 0, "100644", data.len()).into_bytes();
        bytes.extend(data);
        if data.len() % 2 == 1 {
            bytes.push(b'\n');
        }
        bytes
    }

    fn deb(data_name: &str, data: &[u8]) -> Vec<u8> {
        let mut bytes = b"!<arch>\n".to_vec();
        bytes.extend(member("debian-binary", b"2.0\n"));
        bytes.extend(member("control.tar.gz", b"ignored"));
        bytes.extend(member(data_name, data));
        bytes
    }

    fn xz(data: &[u8]) -> Vec<u8> {
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 1);
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apparition-package-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app_tar() -> Vec<u8> {
        tar_with(|b| {
            add_dir(b, "./");
            add_dir(b, "./Applications/");
            add_dir(b, "./Applications/Kodi.app/");
            add_file(b, "./Applications/Kodi.app/Info.plist", b"<plist/>", 0o644);
            add_file(b, "./Applications/Kodi.app/Kodi", b"\xcf\xfa\xed\xfe binary", 0o755);
            add_dir(b, "./Applications/Kodi.app/Frameworks/");
            add_file(b, "./Applications/Kodi.app/Frameworks/libx.dylib", b"x", 0o644);
            add_link(b, "./Applications/Kodi.app/Frameworks/libx.1.dylib", "libx.dylib", tar::EntryType::Symlink);
            add_link(b, "./Applications/Kodi.app/Frameworks/liby.dylib", "./Applications/Kodi.app/Frameworks/libx.dylib", tar::EntryType::Link);
            add_file(b, "./Library/MobileSubstrate/tweak.dylib", b"not part of the app", 0o644);
        })
    }

    fn entries(ipa: &Path) -> Vec<(String, Vec<u8>, Option<u32>)> {
        let mut archive = zip::ZipArchive::new(File::open(ipa).unwrap()).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut entry = archive.by_index(i).unwrap();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                (entry.name().to_string(), data, entry.unix_mode())
            })
            .collect()
    }

    #[test]
    fn converts_an_xz_deb_into_an_ipa() {
        let dir = scratch("xz");
        let package = dir.join("kodi.deb");
        std::fs::write(&package, deb("data.tar.xz", &xz(&app_tar()))).unwrap();
        assert_eq!(format(&package).unwrap(), Format::Deb);
        let out = dir.join("kodi.ipa");
        let converted = convert(&package, &out).unwrap();
        assert_eq!(converted.app, "Kodi.app");
        assert!(converted.others.is_empty());
        assert_eq!(format(&out).unwrap(), Format::Zip);

        let found = entries(&out);
        let names: Vec<&str> = found.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"Payload/Kodi.app/Info.plist"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("MobileSubstrate")), "{names:?}");
        let get = |name: &str| found.iter().find(|(n, _, _)| n == name).unwrap();
        assert_eq!(get("Payload/Kodi.app/Kodi").2.unwrap() & 0o777, 0o755);
        assert_eq!(get("Payload/Kodi.app/Info.plist").2.unwrap() & 0o777, 0o644);
        // A symlink keeps its target as the entry's data and its link type in the mode.
        let link = get("Payload/Kodi.app/Frameworks/libx.1.dylib");
        assert_eq!(link.1, b"libx.dylib");
        assert_eq!(link.2.unwrap() & 0o170000, 0o120000);
        // A hard link becomes a copy.
        assert_eq!(get("Payload/Kodi.app/Frameworks/liby.dylib").1, b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_gzip_and_plain_data() {
        let dir = scratch("gz");
        for (name, data) in [("data.tar.gz", gz(&app_tar())), ("data.tar", app_tar())] {
            let package = dir.join(format!("{name}.deb"));
            std::fs::write(&package, deb(name, &data)).unwrap();
            let out = dir.join(format!("{name}.ipa"));
            assert_eq!(convert(&package, &out).unwrap().app, "Kodi.app", "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn takes_the_biggest_app_and_a_rootless_prefix() {
        let dir = scratch("many");
        let package = dir.join("many.deb");
        let data = tar_with(|b| {
            add_file(b, "./var/jb/Applications/Small.app/Info.plist", b"<plist/>", 0o644);
            add_file(b, "./var/jb/Applications/Big.app/Info.plist", b"<plist/>", 0o644);
            add_file(b, "./var/jb/Applications/Big.app/Big", &[7u8; 4096], 0o755);
        });
        std::fs::write(&package, deb("data.tar.gz", &gz(&data))).unwrap();
        let converted = convert(&package, &dir.join("many.ipa")).unwrap();
        assert_eq!(converted.app, "Big.app");
        assert_eq!(converted.others, vec!["Small.app".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gives_the_ipa_a_package_carries() {
        let dir = scratch("inner");
        let package = dir.join("inner.deb");
        let inner = {
            let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
            writer.start_file("Payload/Thing.app/Info.plist", SimpleFileOptions::default()).unwrap();
            writer.write_all(b"<plist/>").unwrap();
            writer.finish().unwrap().into_inner()
        };
        let data = tar_with(|b| add_file(b, "./var/mobile/Thing.ipa", &inner, 0o644));
        std::fs::write(&package, deb("data.tar.gz", &gz(&data))).unwrap();
        let out = dir.join("out.ipa");
        let converted = convert(&package, &out).unwrap();
        assert_eq!(converted.app, "Thing.ipa");
        assert_eq!(std::fs::read(&out).unwrap(), inner);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn says_what_is_wrong() {
        let dir = scratch("bad");
        let say = |name: &str, bytes: Vec<u8>| {
            let package = dir.join(name);
            std::fs::write(&package, bytes).unwrap();
            let out = dir.join(format!("{name}.ipa"));
            let error = convert(&package, &out).unwrap_err().to_string();
            assert!(!out.exists(), "{name} left a partial IPA");
            error
        };
        let tweak = tar_with(|b| add_file(b, "./Library/MobileSubstrate/tweak.dylib", b"x", 0o644));
        let error = say("tweak.deb", deb("data.tar.gz", &gz(&tweak)));
        assert!(error.contains("no app") && error.contains("Library"), "{error}");
        let no_plist = tar_with(|b| add_file(b, "./Applications/A.app/A", b"x", 0o755));
        assert!(say("noplist.deb", deb("data.tar.gz", &gz(&no_plist))).contains("Info.plist"));
        assert!(say("bz.deb", deb("data.tar.bz2", b"BZh")).contains("bz2"));
        assert!(say("notdeb.deb", b"hello hello hello".to_vec()).contains("isn't a .deb"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ignores_paths_that_climb_out() {
        assert!(locate(Path::new("./Applications/../../etc/App.app/x")).is_none());
        assert_eq!(locate(Path::new("./Applications/A.app/b/c")), Some(("A.app".into(), vec!["b".into(), "c".into()])));
        assert!(locate(Path::new("Applications/.app/x")).is_none());
        assert!(locate(Path::new("Applications/A.app")).is_some());
    }

    #[test]
    fn ensure_ipa_passes_zips_through() {
        let dir = scratch("ensure");
        let package = dir.join("kodi.deb");
        std::fs::write(&package, deb("data.tar.gz", &gz(&app_tar()))).unwrap();
        let ipa = ensure_ipa(&package, &dir.join("out")).unwrap();
        assert_eq!(ipa, dir.join("out/kodi.ipa"));
        assert_eq!(ensure_ipa(&ipa, &dir.join("out2")).unwrap(), ipa);
        assert!(!dir.join("out2").exists());
        let text = dir.join("notes.txt");
        std::fs::write(&text, "hello").unwrap();
        assert!(ensure_ipa(&text, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
