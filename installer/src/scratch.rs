//! Scratch folders named after the process that made them, so that one a killed run left behind
//! is removed by the next run. The Store cancels a build by sending its process group SIGTERM,
//! which ends this program before it can clean up, and a signing folder holds a .p12's
//! decrypted key until then.

use std::path::{Path, PathBuf};

/// `apparition-<kind>-<pid>`: a folder only this run uses.
pub fn folder(kind: &str) -> PathBuf {
    std::env::temp_dir().join(format!("apparition-{kind}-{}", std::process::id()))
}

/// `apparition-build-<pid>-<tag>`, for a run that makes several.
pub fn tagged_folder(kind: &str, tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("apparition-{kind}-{}-{tag}", std::process::id()))
}

/// The process a scratch folder's name says made it. A package's unpacked app
/// (`apparition-unpacked-`) is what `sign` leaves behind on purpose, so it isn't one.
fn owner(name: &str) -> Option<u32> {
    let (kind, rest) = name.strip_prefix("apparition-")?.split_once('-')?;
    let (pid, tag) = match rest.split_once('-') {
        Some((pid, tag)) => (pid, Some(tag)),
        None => (rest, None),
    };
    let shaped = match kind {
        "sign" | "installer" => tag.is_none(),
        "build" => tag.is_some_and(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_hexdigit())),
        _ => false,
    };
    if !shaped || pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

/// Removes this user's scratch folders whose process has ended.
pub fn sweep() {
    #[cfg(unix)]
    sweep_in(&std::env::temp_dir(), |pid| {
        // Signal 0 checks for the process without sending anything; EPERM means it exists
        // but is another user's.
        // SAFETY: kill with signal 0 has no effect on the target.
        let found = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        found || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    });
}

#[cfg(unix)]
fn sweep_in(dir: &Path, alive: impl Fn(u32) -> bool) {
    use std::os::unix::fs::MetadataExt;
    let me = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(owner) else { continue };
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if pid == std::process::id() || !meta.is_dir() || meta.uid() != me || alive(pid) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_own_kinds_are_scratch() {
        assert_eq!(owner("apparition-sign-123"), Some(123));
        assert_eq!(owner("apparition-installer-9"), Some(9));
        assert_eq!(owner("apparition-build-77-0af3"), Some(77));
        for name in ["apparition-unpacked-5", "apparition-build-0123456789abcdef", "apparition-sign-12-x", "apparition-sign-", "apparition-release-4", "other-sign-4", "apparition-build-5-"] {
            assert_eq!(owner(name), None, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_dead_runs_folders_go_and_a_live_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["apparition-sign-100", "apparition-build-100-ab", "apparition-sign-200", "apparition-unpacked-100"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("apparition-installer-100"), b"a file, not ours").unwrap();
        sweep_in(dir.path(), |pid| pid == 200);
        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["apparition-installer-100", "apparition-sign-200", "apparition-unpacked-100"]);
    }
}
