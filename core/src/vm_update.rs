//! Updates of the anisette VM's programs from Apparition's own signed releases, handed by the app to the running
//! guest: no recoveryOS, no cold boot, and the VM keeps serving (each program it replaces restarts in about a second).
//!
//! - **What it takes**: a release's `Apparition-VMGuest-<version>-macos-arm64.zip` (scripts/package-release.sh),
//!   built and attested on GitHub Actions like the rest of the release, with that release's `SHA256SUMS` and its
//!   SSHSIG signature, exactly as the release site publishes them. The app only fetches and carries them.
//! - **Who checks**: the guest's own installed `apparition`, against the release signers compiled into it
//!   (release-signers), the same check the app makes of its own updates. Nothing on the host can sign a package, so
//!   a tampered app can't put its own programs into the VM; only a release can.
//! - **What it can change**: only the programs in [`PROGRAMS`], each to its fixed place, and the release has to hold
//!   all of them. Launchd jobs, the trim, SIP and the account stay as recoveryOS left them (a change to those is a
//!   `GUEST_VERSION` bump, vm.rs).
//! - **No going back**: the guest records the release it took (`release-version`) and refuses an older one. The
//!   version is signed: it is in the file name `SHA256SUMS` lists.
//! - **The way in**: the runner (selfhost/macos-vm/main.swift, control operation `update`) carries the package over
//!   vsock 6972, which only this VM's own device reaches, to guest-bridge, which runs the installed
//!   `apparition vm guest-apply` as root with the connection as its input and output ([`guest_apply`]).
//!
//! A build of one's own isn't a release, so it has none of this: `apparition vm update --recovery` puts its programs
//! in from the VM's recoveryOS instead.
//!
//! The package: a line `APPARITION-GUEST-RELEASE 1`, then three parts, each a line `<size> <name>` and that many
//! bytes: `SHA256SUMS`, `SHA256SUMS.sig` and the release's zip. Every program is unpacked and written beside its
//! destination before anything is replaced; one that is already the same is left alone (and its job isn't restarted).

use std::io::{BufRead, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use rootcause::prelude::*;
use sha2::{Digest, Sha256};

use crate::release;

/// The guest's vsock port for updates (guest-bridge.c).
pub const PORT: u32 = 6972;
const MAGIC: &str = "APPARITION-GUEST-RELEASE 1";
const MAX_LINE: usize = 1024;
const MAX_SUMS: u64 = 64 << 10;
const MAX_SIGNATURE: u64 = 16 << 10;
/// The zip, and what it unpacks to, all files together.
const MAX_PROGRAMS: u64 = 512 << 20;

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

/// The programs an update replaces, all of them in every release: name, place, mode, the job that runs it.
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

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

// MARK: The host's half

/// The package for the guest: the release's signed manifest, its signature and the VM's programs, as published.
pub fn package(sums: &[u8], signature: &str, name: &str, zip: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(zip.len() + sums.len() + signature.len() + 128);
    out.extend_from_slice(format!("{MAGIC}\n").as_bytes());
    for (part, bytes) in [("SHA256SUMS", sums), ("SHA256SUMS.sig", signature.as_bytes()), (name, zip)] {
        out.extend_from_slice(format!("{} {part}\n", bytes.len()).as_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

/// What the guest answered: the release it now has and the programs it replaced (none when it had them all).
pub fn parse_reply(reply: &str) -> Result<(String, Vec<String>), Report> {
    let line = reply.lines().next().unwrap_or_default();
    match line.strip_prefix("ok ") {
        Some(rest) => {
            let mut words = rest.split(' ');
            let version = words.next().unwrap_or_default().to_string();
            let replaced = words.next().unwrap_or_default().split(',').filter(|n| !n.is_empty()).map(str::to_string).collect();
            Ok((version, replaced))
        }
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

/// One part: its `<size> <name>` line, then its bytes.
fn read_part(input: &mut impl BufRead, limit: u64) -> Result<(String, Vec<u8>), Report> {
    let line = read_line(input)?;
    let (size, name) = line.split_once(' ').ok_or_else(|| report!("the update's parts are malformed"))?;
    let size: u64 = size.parse().map_err(|_| report!("the update's parts are malformed"))?;
    if size > limit {
        bail!("{name} in the update is too large");
    }
    let mut bytes = Vec::with_capacity(size as usize);
    input.take(size).read_to_end(&mut bytes).context("reading the update")?;
    if bytes.len() as u64 != size {
        bail!("the update ended in the middle of {name}");
    }
    Ok((name.to_string(), bytes))
}

/// Reads, checks and installs one package from `input`, trusting releases signed by `signers` only. Returns the
/// release's version and the programs it replaced.
pub fn apply(input: &mut impl BufRead, layout: &Layout, signers: &str) -> Result<(String, Vec<String>), Report> {
    if read_line(input)? != MAGIC {
        bail!("not an update package");
    }
    let (_, sums) = read_part(input, MAX_SUMS)?;
    let (_, signature) = read_part(input, MAX_SIGNATURE)?;
    let (name, zip) = read_part(input, MAX_PROGRAMS)?;
    release::verify_sshsig(&sums, &String::from_utf8_lossy(&signature), release::NAMESPACE, signers)
        .map_err(|error| report!("the update isn't a signed Apparition release: {}", error.format_current_context()))?;
    let listed = release::parse_sums(&String::from_utf8_lossy(&sums))?;
    // The version comes from the signed name, never from anything unsigned.
    let version = name
        .strip_prefix("Apparition-VMGuest-")
        .and_then(|rest| rest.strip_suffix("-macos-arm64.zip"))
        .filter(|v| !v.is_empty() && v.split('.').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
        .ok_or_else(|| report!("{name} isn't the VM's programs from a release"))?
        .to_string();
    if release::vm_guest_name(&version) != name {
        bail!("{name} isn't the VM's programs from a release");
    }
    match listed.get(&name) {
        Some(hash) if hash.eq_ignore_ascii_case(&sha256(&zip)) => {}
        Some(_) => bail!("{name} doesn't match the release's signed manifest"),
        None => bail!("the release's signed manifest doesn't list {name}"),
    }
    let version_path = layout.bootstrap.join("release-version");
    if let Some(had) = std::fs::read_to_string(&version_path).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
        && release::is_newer(&had, &version)
    {
        bail!("the VM has Apparition {had}'s programs, newer than {version}'s");
    }

    // Unpacked and checked whole before anything is written.
    let mut archive = zip::ZipArchive::new(Cursor::new(&zip)).map_err(|_| report!("{name} isn't a zip"))?;
    let mut files: Vec<(&'static (&'static str, Place, u32, Job), Vec<u8>)> = Vec::new();
    let mut total = 0u64;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|_| report!("{name} is damaged"))?;
        let entry_name = entry.name().map_err(|_| report!("{name} is damaged"))?.to_string();
        let known = program(&entry_name).filter(|_| entry.is_file()).ok_or_else(|| report!("{entry_name} isn't one of the programs an update may replace"))?;
        if files.iter().any(|(p, _)| p.0 == known.0) {
            bail!("{entry_name} is in the update twice");
        }
        let mut bytes = Vec::new();
        (&mut entry).take(MAX_PROGRAMS - total + 1).read_to_end(&mut bytes).map_err(|_| report!("{name} is damaged"))?;
        total += bytes.len() as u64;
        if total > MAX_PROGRAMS {
            bail!("{name} unpacks to too much");
        }
        files.push((known, bytes));
    }
    if let Some((missing, ..)) = PROGRAMS.iter().find(|(p, ..)| !files.iter().any(|(f, _)| f.0 == *p)) {
        bail!("the release's programs for the VM lack {missing}");
    }

    // Everything beside its destination first; nothing is replaced until all of it is written.
    let mut staged: Vec<(PathBuf, PathBuf, &str)> = Vec::new();
    let clean = |staged: &[(PathBuf, PathBuf, &str)]| staged.iter().for_each(|(from, ..)| drop(std::fs::remove_file(from)));
    for ((program_name, place, mode, _), bytes) in &files {
        let dir = layout.dir(*place);
        let target = dir.join(program_name);
        let same = std::fs::read(&target).is_ok_and(|had| had == *bytes);
        let incoming = dir.join(format!(".{program_name}.incoming"));
        if same {
            continue;
        }
        let written = (|| -> Result<(), Report> {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            std::fs::create_dir_all(dir)?;
            let _ = std::fs::remove_file(&incoming);
            let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&incoming).context(format!("writing {}", incoming.display()))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(*mode))?;
            Ok(())
        })();
        staged.push((incoming, target, program_name));
        if let Err(error) = written {
            clean(&staged);
            return Err(error);
        }
    }
    let mut replaced = Vec::new();
    for (incoming, target, program_name) in &staged {
        // A rename: a program that is running keeps the file it started from.
        if let Err(error) = std::fs::rename(incoming, target) {
            clean(&staged);
            return Err(report!("installing {}: {error}", target.display()));
        }
        replaced.push(program_name.to_string());
    }
    std::fs::write(&version_path, format!("{version}\n")).context("recording the release")?;
    Ok((version, replaced))
}

/// The jobs to restart for `replaced`, in order: the bridge last, since this update runs under it.
pub fn jobs(replaced: &[String]) -> Vec<Job> {
    let mut jobs: Vec<Job> = replaced.iter().filter_map(|name| program(name).map(|p| p.3)).collect();
    jobs.sort();
    jobs.dedup();
    jobs
}

/// `apparition vm guest-apply`, as root in the guest, run by guest-bridge for a connection on vsock 6972 (standard
/// input and output). Answers `ok <version> <replaced,...>` or `error <why>`, then restarts what it replaced.
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
    let result = apply(&mut input, &layout, release::signers());
    let reply = match &result {
        Ok((version, replaced)) => format!("ok {version} {}\n", replaced.join(",")),
        Err(error) => format!("error {}\n", error.format_current_context().to_string().replace('\n', " ")),
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(reply.as_bytes());
    let _ = out.flush();
    // The host reads to the end: tell it this is the end, before restarting anything.
    unsafe { libc::shutdown(1, libc::SHUT_WR) };
    drop(lock);
    let (_, replaced) = result?;
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
    use std::process::Command;

    /// A throwaway release signer: ssh-keygen's key and its allowed-signers line, or None without ssh-keygen.
    struct Signer {
        dir: tempfile::TempDir,
        signers: String,
    }

    impl Signer {
        fn new() -> Option<Signer> {
            let dir = tempfile::tempdir().unwrap();
            let key = dir.path().join("key");
            let made = Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "test@signer", "-f"]).arg(&key).status().ok()?;
            assert!(made.success());
            let public = std::fs::read_to_string(dir.path().join("key.pub")).unwrap();
            Some(Signer { signers: format!("releases@apparition.illixion.com {public}"), dir })
        }

        fn sign(&self, sums: &[u8]) -> String {
            let file = self.dir.path().join("SHA256SUMS");
            std::fs::write(&file, sums).unwrap();
            let _ = std::fs::remove_file(self.dir.path().join("SHA256SUMS.sig"));
            let ok = Command::new("ssh-keygen").args(["-Y", "sign", "-q", "-n", release::NAMESPACE, "-f"]).arg(self.dir.path().join("key")).arg(&file).status().unwrap();
            assert!(ok.success());
            std::fs::read_to_string(self.dir.path().join("SHA256SUMS.sig")).unwrap()
        }

        /// Release `version`'s package of `files` (name, contents), signed.
        fn release(&self, version: &str, files: &[(&str, Vec<u8>)]) -> Vec<u8> {
            let zip = zip_of(files);
            let name = release::vm_guest_name(version);
            let sums = format!("{}  {name}\n{}  Apparition-Installer-{version}-macos-universal.zip\n", sha256(&zip), "ab".repeat(32));
            package(sums.as_bytes(), &self.sign(sums.as_bytes()), &name, &zip)
        }
    }

    fn zip_of(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in files {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn programs(version: &str) -> Vec<(&'static str, Vec<u8>)> {
        PROGRAMS.iter().map(|(name, ..)| (*name, format!("{name} {version}").into_bytes())).collect()
    }

    fn guest() -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout { programs: dir.path().join("programs"), bootstrap: dir.path().join("bootstrap") };
        std::fs::create_dir_all(&layout.bootstrap).unwrap();
        (dir, layout)
    }

    fn apply_bytes(bytes: &[u8], layout: &Layout, signer: &Signer) -> Result<(String, Vec<String>), Report> {
        apply(&mut Cursor::new(bytes), layout, &signer.signers)
    }

    #[test]
    fn installs_a_releases_programs_and_refuses_an_older_release() {
        let Some(signer) = Signer::new() else { return eprintln!("no ssh-keygen; skipped") };
        let (_dir, layout) = guest();
        let (version, replaced) = apply_bytes(&signer.release("1.2.0", &programs("1")), &layout, &signer).unwrap();
        assert_eq!((version.as_str(), replaced.len()), ("1.2.0", PROGRAMS.len()));
        assert_eq!(std::fs::read(layout.programs.join("apparition")).unwrap(), b"apparition 1");
        assert_eq!(std::fs::read(layout.bootstrap.join("guest-bridge")).unwrap(), b"guest-bridge 1");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(layout.bootstrap.join("report-status.sh")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::read_to_string(layout.bootstrap.join("release-version")).unwrap(), "1.2.0\n");

        // Only what changed is replaced, and only its job restarts.
        let mut next = programs("1");
        next[0].1 = b"apparition 2".to_vec();
        let (_, replaced) = apply_bytes(&signer.release("1.10.0", &next), &layout, &signer).unwrap();
        assert_eq!(replaced, ["apparition"]);
        assert_eq!(jobs(&replaced), [Job::Anisette]);
        assert!(!layout.programs.join(".apparition.incoming").exists());

        // The same release again changes nothing; an older one is refused.
        assert!(apply_bytes(&signer.release("1.10.0", &next), &layout, &signer).unwrap().1.is_empty());
        let error = apply_bytes(&signer.release("1.9.0", &programs("old")), &layout, &signer).unwrap_err();
        assert!(error.to_string().contains("newer than"), "{error}");
        assert_eq!(std::fs::read(layout.programs.join("apparition")).unwrap(), b"apparition 2");
    }

    #[test]
    fn refuses_anything_a_release_signer_did_not_sign() {
        let Some(signer) = Signer::new() else { return eprintln!("no ssh-keygen; skipped") };
        let Some(other) = Signer::new() else { return };
        let (_dir, layout) = guest();
        // Another key.
        assert!(apply_bytes(&other.release("1.2.0", &programs("1")), &layout, &signer).unwrap_err().to_string().contains("signed"));

        // The zip changed after signing: its hash no longer matches the signed manifest.
        let good = signer.release("1.2.0", &programs("1"));
        let mut changed = good.clone();
        let last = changed.len() - 30;
        changed[last] ^= 1;
        assert!(apply_bytes(&changed, &layout, &signer).is_err());

        // The manifest changed after signing.
        let text = String::from_utf8_lossy(&good).replacen("1.2.0-macos-arm64", "9.2.0-macos-arm64", 1).into_bytes();
        assert!(apply_bytes(&text, &layout, &signer).is_err());

        // Cut short.
        assert!(apply_bytes(&good[..good.len() - 3], &layout, &signer).is_err());

        // Signed, but another release file (the desktop installer) offered as the VM's programs.
        let zip = zip_of(&programs("1"));
        let installer = "Apparition-Installer-1.2.0-macos-universal.zip";
        let sums = format!("{}  {installer}\n", sha256(&zip));
        let bytes = package(sums.as_bytes(), &signer.sign(sums.as_bytes()), installer, &zip);
        assert!(apply_bytes(&bytes, &layout, &signer).unwrap_err().to_string().contains("isn't the VM's programs"));
        assert!(!layout.programs.join("apparition").exists(), "nothing was installed");
    }

    #[test]
    fn refuses_a_release_with_other_files_or_missing_ones() {
        let Some(signer) = Signer::new() else { return eprintln!("no ssh-keygen; skipped") };
        let (_dir, layout) = guest();
        let mut extra = programs("1");
        extra.push(("../../etc/sudoers", b"x".to_vec()));
        assert!(apply_bytes(&signer.release("1.2.0", &extra), &layout, &signer).unwrap_err().to_string().contains("isn't one of"));
        let missing = programs("1")[..4].to_vec();
        assert!(apply_bytes(&signer.release("1.2.0", &missing), &layout, &signer).unwrap_err().to_string().contains("lack guest-bridge"));
        assert!(!layout.programs.join("apparition").exists());
    }

    #[test]
    fn replies_and_jobs() {
        assert_eq!(parse_reply("ok 1.2.0 apparition,guest-bridge\n").unwrap(), ("1.2.0".to_string(), vec!["apparition".to_string(), "guest-bridge".to_string()]));
        assert!(parse_reply("ok 1.2.0 \n").unwrap().1.is_empty());
        assert!(parse_reply("error the update isn't a signed Apparition release\n").unwrap_err().to_string().contains("refused"));
        assert!(parse_reply("").is_err());
        assert_eq!(jobs(&["guest-bridge".into(), "report-status.sh".into(), "apparition".into()]), [Job::Status, Job::Anisette, Job::Bridge]);
    }
}
