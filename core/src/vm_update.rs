//! Signed updates of the anisette VM's programs, handed by the app to the running guest: no recoveryOS, no cold boot,
//! and the VM keeps serving (each program it replaces restarts in about a second).
//!
//! - **Who signs**: each VM has its own Ed25519 key, made by the host and kept in the bundle (`update-key`, mode 600).
//!   Its public half goes into the guest from recoveryOS, with everything else setup.sh puts there
//!   (`/Library/Application Support/ApparitionBootstrap/update-key.pub`, root only), so only the app that set the VM
//!   up can update it, and nothing later can change which key that is without recoveryOS.
//! - **What it can change**: only the programs in [`PROGRAMS`], each to its fixed place. Launchd jobs, the trim, SIP
//!   and the account stay as recoveryOS left them (a change to those is a `GUEST_VERSION` bump, vm.rs).
//! - **No going back**: each package carries a sequence number the guest must not have seen (`update-sequence`), so an
//!   older package, sent again, is refused.
//! - **The way in**: the runner (selfhost/macos-vm/main.swift, control operation `update`) carries the package over
//!   vsock 6972, which only this VM's own device reaches, to guest-bridge, which runs the installed
//!   `apparition vm guest-apply` as root with the connection as its input and output ([`guest_apply`]).
//!
//! The package: a line `APPARITION-GUEST-UPDATE 1`, the manifest as one line of JSON, the Ed25519 signature of
//! [`CONTEXT`] followed by the manifest's bytes as one line of hex, then each file's bytes in the manifest's order.
//! Everything is read, checked against its size and SHA-256 and written beside its destination before anything is
//! replaced; a file that is already the same is left alone (and its job isn't restarted).

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The guest's vsock port for updates (guest-bridge.c).
pub const PORT: u32 = 6972;
const MAGIC: &str = "APPARITION-GUEST-UPDATE 1";
/// Signed before the manifest, so the key's signature can't be taken for anything else.
pub const CONTEXT: &[u8] = b"apparition guest update v1\0";
/// What one package may hold, all files together.
const MAX_TOTAL: u64 = 1 << 30;
const MAX_LINE: usize = 64 << 10;

/// Where a program goes in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// `/usr/local/apparition`: the command line and its entitled helper.
    Program,
    /// `/Library/Application Support/ApparitionBootstrap`: what the vsock jobs run.
    Bootstrap,
}

/// The launchd job that runs a program, restarted once it is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Job {
    /// report-status.sh (and the guest-status it runs).
    Status,
    /// The Aqua agent's `apparition anisette serve`, which runs the helper.
    Anisette,
    /// guest-bridge, which runs the update itself: restarted last.
    Bridge,
}

/// The programs an update may replace: name, place, mode, the job that runs it.
pub const PROGRAMS: &[(&str, Place, u32, Job)] = &[
    ("apparition", Place::Program, 0o755, Job::Anisette),
    ("apparition-anisette-helper", Place::Program, 0o755, Job::Anisette),
    ("report-status.sh", Place::Bootstrap, 0o700, Job::Status),
    ("guest-status", Place::Bootstrap, 0o755, Job::Status),
    ("guest-bridge", Place::Bootstrap, 0o755, Job::Bridge),
];

fn program(name: &str) -> Option<&'static (&'static str, Place, u32, Job)> {
    PROGRAMS.iter().find(|(known, ..)| *known == name)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// Higher than any package the guest took before.
    pub sequence: u64,
    pub files: Vec<Entry>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

// MARK: The host's half

/// The program set's identity: what the bundle records as the guest's (`guest-programs`), to tell whether this app's
/// programs differ from the ones the VM has.
pub fn digest(programs: &[(String, Vec<u8>)]) -> String {
    let mut lines: Vec<String> = programs.iter().map(|(name, bytes)| format!("{name} {}\n", sha256(bytes))).collect();
    lines.sort();
    sha256(lines.concat().as_bytes())
}

/// The VM's update key, made on first use: 32 random bytes as hex, mode 600, in the bundle.
pub fn signing_key(bundle: &Path) -> Result<SigningKey, Report> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = bundle.join("update-key");
    if let Ok(text) = std::fs::read_to_string(&path) {
        let seed: [u8; 32] = unhex(&text).and_then(|b| b.try_into().ok()).ok_or_else(|| report!("{} isn't a key", path.display()))?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    let mut seed = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed).map_err(|_| report!("no random numbers"))?;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).context("writing the VM's update key")?;
    file.write_all(format!("{}\n", hex(&seed)).as_bytes())?;
    Ok(SigningKey::from_bytes(&seed))
}

/// The public half, as the guest keeps it (one line of hex).
pub fn public_key_line(key: &SigningKey) -> String {
    format!("{}\n", hex(key.verifying_key().as_bytes()))
}

/// The next sequence number for this bundle's VM, recorded before the package is sent: a package that never arrived
/// only skips a number.
pub fn next_sequence(bundle: &Path) -> Result<u64, Report> {
    let path = bundle.join("update-sequence");
    let last = std::fs::read_to_string(&path).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    let next = last + 1;
    std::fs::write(&path, format!("{next}\n")).context("recording the update's sequence number")?;
    Ok(next)
}

/// A signed package of `programs` (names from [`PROGRAMS`]).
pub fn package(programs: &[(String, Vec<u8>)], sequence: u64, key: &SigningKey) -> Result<Vec<u8>, Report> {
    let mut files = Vec::new();
    for (name, bytes) in programs {
        if program(name).is_none() {
            bail!("{name} isn't one of the guest's programs");
        }
        files.push(Entry { name: name.clone(), size: bytes.len() as u64, sha256: sha256(bytes) });
    }
    let manifest = serde_json::to_vec(&Manifest { format: 1, sequence, files })?;
    let signature = key.sign(&[CONTEXT, &manifest].concat());
    let mut out = Vec::with_capacity(programs.iter().map(|(_, b)| b.len()).sum::<usize>() + manifest.len() + 256);
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&manifest);
    out.push(b'\n');
    out.extend_from_slice(hex(&signature.to_bytes()).as_bytes());
    out.push(b'\n');
    for (_, bytes) in programs {
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// What the guest answered: the programs it replaced (none when it had them all already).
pub fn parse_reply(reply: &str) -> Result<Vec<String>, Report> {
    let line = reply.lines().next().unwrap_or_default();
    match line.strip_prefix("ok") {
        Some(rest) => Ok(rest.split([' ', ',']).filter(|n| !n.is_empty()).map(str::to_string).collect()),
        None if line.is_empty() => bail!("The VM closed the connection without answering the update"),
        None => bail!("The VM refused the update: {}", line.strip_prefix("error ").unwrap_or(line)),
    }
}

// MARK: The guest's half

/// Where things are in the guest (a temporary directory in the tests).
#[derive(Debug, Clone)]
pub struct Layout {
    pub programs: PathBuf,
    pub bootstrap: PathBuf,
}

impl Layout {
    pub fn guest() -> Layout {
        Layout {
            programs: PathBuf::from("/usr/local/apparition"),
            bootstrap: PathBuf::from("/Library/Application Support/ApparitionBootstrap"),
        }
    }

    fn dir(&self, place: Place) -> &Path {
        match place {
            Place::Program => &self.programs,
            Place::Bootstrap => &self.bootstrap,
        }
    }
}

fn read_line(input: &mut impl BufRead) -> Result<String, Report> {
    let mut line = Vec::new();
    input.take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).context("reading the update")?;
    if line.last() != Some(&b'\n') {
        bail!("the update ended early or has a line that is too long");
    }
    line.pop();
    String::from_utf8(line).map_err(|_| report!("the update isn't text where it should be"))
}

/// Reads, checks and installs one package from `input`. Returns the programs it replaced.
pub fn apply(input: &mut impl BufRead, layout: &Layout) -> Result<Vec<String>, Report> {
    if read_line(input)? != MAGIC {
        bail!("not an update package");
    }
    let manifest_line = read_line(input)?;
    let signature = unhex(&read_line(input)?).and_then(|b| <[u8; 64]>::try_from(b).ok()).ok_or_else(|| report!("the update's signature is malformed"))?;
    let key_path = layout.bootstrap.join("update-key.pub");
    let key = std::fs::read_to_string(&key_path)
        .ok()
        .and_then(|text| unhex(&text))
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .and_then(|b| VerifyingKey::from_bytes(&b).ok())
        .ok_or_else(|| report!("this VM has no update key (an update from its recoveryOS puts it in)"))?;
    key.verify_strict(&[CONTEXT, manifest_line.as_bytes()].concat(), &Signature::from_bytes(&signature))
        .map_err(|_| report!("the update isn't signed by this VM's app"))?;
    let manifest: Manifest = serde_json::from_str(&manifest_line).context("reading the update's manifest")?;
    if manifest.format != 1 {
        bail!("the update's format ({}) is newer than this VM knows", manifest.format);
    }
    let sequence_path = layout.bootstrap.join("update-sequence");
    let seen = std::fs::read_to_string(&sequence_path).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    if manifest.sequence <= seen {
        bail!("the update is no newer than the last one this VM took ({} after {seen})", manifest.sequence);
    }
    let mut total = 0u64;
    for (i, entry) in manifest.files.iter().enumerate() {
        if program(&entry.name).is_none() {
            bail!("{} isn't one of the programs an update may replace", entry.name);
        }
        if manifest.files[..i].iter().any(|other| other.name == entry.name) {
            bail!("{} is in the update twice", entry.name);
        }
        total = total.saturating_add(entry.size);
    }
    if total > MAX_TOTAL {
        bail!("the update is too large");
    }

    // Everything beside its destination first, checked; nothing is replaced until all of it is.
    let mut staged: Vec<(PathBuf, PathBuf, &str)> = Vec::new();
    let clean = |staged: &[(PathBuf, PathBuf, &str)]| staged.iter().for_each(|(from, ..)| drop(std::fs::remove_file(from)));
    for entry in &manifest.files {
        let &(name, place, mode, _) = program(&entry.name).expect("checked");
        let dir = layout.dir(place);
        let target = dir.join(name);
        let incoming = dir.join(format!(".{name}.incoming"));
        let result = (|| -> Result<(), Report> {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::create_dir_all(dir)?;
            let _ = std::fs::remove_file(&incoming);
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&incoming).context(format!("writing {}", incoming.display()))?;
            let mut digest = Sha256::new();
            let mut left = entry.size;
            let mut buffer = vec![0u8; 1 << 20];
            while left > 0 {
                let n = input.read(&mut buffer[..left.min(1 << 20) as usize]).context("reading the update")?;
                if n == 0 {
                    bail!("the update ended in the middle of {name}");
                }
                digest.update(&buffer[..n]);
                file.write_all(&buffer[..n])?;
                left -= n as u64;
            }
            if hex(&digest.finalize()) != entry.sha256 {
                bail!("{name} doesn't match the update's manifest");
            }
            file.sync_all()?;
            std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(mode))?;
            Ok(())
        })();
        staged.push((incoming, target, name));
        if let Err(error) = result {
            clean(&staged);
            return Err(error);
        }
    }

    let mut replaced = Vec::new();
    for (incoming, target, name) in &staged {
        let same = std::fs::read(target).is_ok_and(|bytes| manifest.files.iter().any(|e| e.name == *name && e.sha256 == sha256(&bytes)));
        if same {
            let _ = std::fs::remove_file(incoming);
            continue;
        }
        // A rename: a program that is running keeps the file it started from.
        if let Err(error) = std::fs::rename(incoming, target) {
            clean(&staged);
            return Err(report!("installing {}: {error}", target.display()));
        }
        replaced.push(name.to_string());
    }
    std::fs::write(&sequence_path, format!("{}\n", manifest.sequence)).context("recording the update's sequence number")?;
    Ok(replaced)
}

/// The jobs to restart for `replaced`, in order: the bridge last, since this update runs under it.
pub fn jobs(replaced: &[String]) -> Vec<Job> {
    let mut jobs: Vec<Job> = replaced.iter().filter_map(|name| program(name).map(|p| p.3)).collect();
    jobs.sort();
    jobs.dedup();
    jobs
}

/// `apparition vm guest-apply`, as root in the guest, run by guest-bridge for a connection on vsock 6972 (standard
/// input and output). Answers `ok <replaced...>` or `error <why>`, then restarts what it replaced.
pub fn guest_apply() -> Result<(), Report> {
    let layout = Layout::guest();
    // One at a time, and a host that stops sending can't hold the lock for good.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(layout.bootstrap.join("update.lock")).context("locking the update")?;
    use std::os::fd::AsRawFd;
    unsafe {
        let timeout = libc::timeval { tv_sec: 120, tv_usec: 0 };
        libc::setsockopt(0, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &timeout as *const _ as *const libc::c_void, std::mem::size_of::<libc::timeval>() as u32);
        if libc::flock(lock.as_raw_fd(), libc::LOCK_EX) != 0 {
            bail!("couldn't lock the update");
        }
    }
    let mut input = std::io::stdin().lock();
    let result = apply(&mut input, &layout);
    let reply = match &result {
        Ok(replaced) => format!("ok {}\n", replaced.join(",")),
        Err(error) => format!("error {}\n", error.format_current_context().to_string().replace('\n', " ")),
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(reply.as_bytes());
    let _ = out.flush();
    // The host reads to the end: tell it this is the end, before restarting anything.
    unsafe { libc::shutdown(1, libc::SHUT_WR) };
    drop(lock);
    let replaced = result?;
    let console = std::fs::metadata("/dev/console").map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(501);
    for job in jobs(&replaced) {
        let target = match job {
            Job::Status => "system/com.illixion.apparition.vm-status".to_string(),
            Job::Anisette => format!("gui/{console}/com.illixion.apparition.vm-anisette"),
            Job::Bridge => "system/com.illixion.apparition.vm-bridge".to_string(),
        };
        let _ = std::process::Command::new("/bin/launchctl").args(["kickstart", "-k", &target]).status();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs(version: &str) -> Vec<(String, Vec<u8>)> {
        PROGRAMS.iter().map(|(name, ..)| (name.to_string(), format!("{name} {version}").into_bytes())).collect()
    }

    fn guest(key: &SigningKey) -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout { programs: dir.path().join("programs"), bootstrap: dir.path().join("bootstrap") };
        std::fs::create_dir_all(&layout.bootstrap).unwrap();
        std::fs::write(layout.bootstrap.join("update-key.pub"), public_key_line(key)).unwrap();
        (dir, layout)
    }

    fn apply_bytes(bytes: &[u8], layout: &Layout) -> Result<Vec<String>, Report> {
        apply(&mut std::io::Cursor::new(bytes), layout)
    }

    #[test]
    fn installs_what_changed_and_refuses_going_back() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let (_dir, layout) = guest(&key);
        let first = package(&programs("1"), 1, &key).unwrap();
        let mut replaced = apply_bytes(&first, &layout).unwrap();
        replaced.sort();
        assert_eq!(replaced.len(), PROGRAMS.len());
        assert_eq!(std::fs::read(layout.programs.join("apparition")).unwrap(), b"apparition 1");
        assert_eq!(std::fs::read(layout.bootstrap.join("guest-bridge")).unwrap(), b"guest-bridge 1");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(layout.bootstrap.join("report-status.sh")).unwrap().permissions().mode() & 0o777, 0o700);

        // Only the changed one is replaced, and only its job restarts.
        let mut second = programs("1");
        second[0].1 = b"apparition 2".to_vec();
        let replaced = apply_bytes(&package(&second, 2, &key).unwrap(), &layout).unwrap();
        assert_eq!(replaced, ["apparition"]);
        assert_eq!(jobs(&replaced), [Job::Anisette]);
        assert!(!layout.programs.join(".apparition.incoming").exists());

        // The first package again (an older one), or the same number again: refused, nothing touched.
        assert!(apply_bytes(&first, &layout).unwrap_err().to_string().contains("no newer"));
        assert!(apply_bytes(&package(&programs("3"), 2, &key).unwrap(), &layout).is_err());
        assert_eq!(std::fs::read(layout.programs.join("apparition")).unwrap(), b"apparition 2");
    }

    #[test]
    fn refuses_another_key_a_changed_manifest_or_a_changed_file() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let (_dir, layout) = guest(&key);
        let other = SigningKey::from_bytes(&[8; 32]);
        assert!(apply_bytes(&package(&programs("1"), 1, &other).unwrap(), &layout).unwrap_err().to_string().contains("signed"));

        let good = package(&programs("1"), 5, &key).unwrap();
        let text = String::from_utf8_lossy(&good).replacen("\"sequence\":5", "\"sequence\":6", 1).into_bytes();
        assert!(apply_bytes(&text, &layout).unwrap_err().to_string().contains("signed"));

        // A body byte flipped: the manifest's hash catches it, and nothing is installed.
        let mut flipped = good.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(apply_bytes(&flipped, &layout).unwrap_err().to_string().contains("doesn't match"));
        assert!(!layout.programs.join("apparition").exists());
        assert!(std::fs::read_dir(&layout.bootstrap).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".incoming")));

        // Cut short.
        assert!(apply_bytes(&good[..good.len() - 3], &layout).is_err());
        assert!(!layout.programs.join("apparition").exists());

        // No key in the guest at all.
        std::fs::remove_file(layout.bootstrap.join("update-key.pub")).unwrap();
        assert!(apply_bytes(&good, &layout).unwrap_err().to_string().contains("no update key"));
    }

    #[test]
    fn refuses_names_outside_the_list() {
        let key = SigningKey::from_bytes(&[7; 32]);
        assert!(package(&[("../../etc/sudoers".into(), b"x".to_vec())], 1, &key).is_err());
        // Signed by the right key but naming something else: still refused by the guest.
        let (_dir, layout) = guest(&key);
        let manifest = serde_json::to_vec(&Manifest { format: 1, sequence: 1, files: vec![Entry { name: "sudoers".into(), size: 1, sha256: sha256(b"x") }] }).unwrap();
        let signature = key.sign(&[CONTEXT, &manifest].concat());
        let bytes = [MAGIC.as_bytes(), b"\n", &manifest, b"\n", hex(&signature.to_bytes()).as_bytes(), b"\nx"].concat();
        assert!(apply_bytes(&bytes, &layout).unwrap_err().to_string().contains("isn't one of"));
    }

    #[test]
    fn keys_sequences_digests_and_replies() {
        let dir = tempfile::tempdir().unwrap();
        let key = signing_key(dir.path()).unwrap();
        assert_eq!(signing_key(dir.path()).unwrap().to_bytes(), key.to_bytes(), "made once, then kept");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.path().join("update-key")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(public_key_line(&key).trim().len(), 64);
        assert_eq!((next_sequence(dir.path()).unwrap(), next_sequence(dir.path()).unwrap()), (1, 2));

        let mut reordered = programs("1");
        reordered.reverse();
        assert_eq!(digest(&programs("1")), digest(&reordered));
        assert_ne!(digest(&programs("1")), digest(&programs("2")));

        assert_eq!(parse_reply("ok apparition,guest-bridge\n").unwrap(), ["apparition", "guest-bridge"]);
        assert!(parse_reply("ok \n").unwrap().is_empty());
        assert!(parse_reply("error the update isn't signed by this VM's app\n").unwrap_err().to_string().contains("refused"));
        assert!(parse_reply("").is_err());
        assert_eq!(jobs(&["guest-bridge".into(), "report-status.sh".into(), "apparition".into()]), [Job::Status, Job::Anisette, Job::Bridge]);
    }
}
