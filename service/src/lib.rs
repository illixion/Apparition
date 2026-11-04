//! Apparition Store in Rust. Scope and phases: docs/RUST_STORE.md.
//!
//! A library, so it tests without Tauri; the desktop app runs it in-process, and so does
//! `apparition store serve` (`hosted`, with the `hosted` feature).

//! It runs on macOS, Windows and Linux. What needs a Mac (Xcode builds, `inject`) says so when
//! asked for elsewhere; kept builds, deploys from other
//! computers, traces and the apps' debug servers work everywhere, so a PC that builds with xtool
//! has the Store without a Mac.

pub mod artifacts;
pub mod assets;
pub mod resign;
pub mod projects;
pub mod macapps;
pub mod altsource;
pub mod auth;
pub mod clones;
pub mod config;
pub mod crashlogs;
pub mod debug;
pub mod deploy;
pub mod devices;
pub mod git;
pub mod install_mode;
pub mod mcp;
pub mod queue;
pub mod state;
pub mod traces;
pub mod updates;
pub mod server;
#[cfg(feature = "hosted")]
pub mod hosted;
#[cfg(feature = "hosted")]
pub mod paired_devices;

/// What replies to other computers call the computer this Store runs on.
pub const HERE: &str = if cfg!(target_os = "macos") { "the Mac" } else { "the PC" };

/// The user's home directory: `HOME` on macOS and Linux, the profile folder on Windows.
pub fn home() -> std::path::PathBuf {
    std::env::home_dir().unwrap_or_default()
}

/// A path segment that names a file inside its folder. On macOS and Linux anything without a
/// separator does. Windows reads more into a name: `C:x` is a drive, `a:b` a stream of `a`, and
/// `CON`, `NUL`, or a name ending in a dot or a space aren't the file they look like.
pub fn plain_segment(segment: &str) -> bool {
    if !cfg!(windows) {
        return true;
    }
    let stem = segment.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    !segment.contains(':') && !segment.ends_with(['.', ' ']) && !reserved
        && !segment.bytes().any(|b| b < 0x20 || b"<>\"|?*".contains(&b))
}

/// Options for a file only its owner reads: mode 0600 on Unix. On Windows the profile folder's
/// ACL already keeps other users out.
pub(crate) fn private_file() -> std::fs::OpenOptions {
    #[allow(unused_mut)]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_special_names_are_not_plain() {
        for name in ["file.txt", "a b", "Logs", "console.log", "com10"] {
            assert!(super::plain_segment(name), "{name}");
        }
        for name in ["C:x", "a:b", "CON", "nul.txt", "COM1", "lpt9.log", "trailing.", "space ", "a|b", "a?b"] {
            assert_eq!(super::plain_segment(name), !cfg!(windows), "{name}");
        }
    }
}
