//! Apparition Store in Rust. Scope and phases: docs/RUST_STORE.md.
//!
//! A library, so it tests without Tauri; the desktop app runs it in-process.

//! It runs on macOS, Windows and Linux. What needs a Mac (Xcode builds, `inject`, the Node build
//! service beside it) says so when asked for elsewhere; kept builds, deploys from other
//! computers, traces and the apps' debug servers work everywhere, so a PC that builds with xtool
//! has the Store without a Mac.

pub mod artifacts;
pub mod assets;
pub mod auth;
pub mod config;
pub mod crashlogs;
pub mod debug;
pub mod deploy;
pub mod devices;
pub mod git;
pub mod install_mode;
pub mod mcp;
pub mod state;
pub mod traces;
pub mod server;

/// What replies to other computers call the computer this Store runs on.
pub const HERE: &str = if cfg!(target_os = "macos") { "the Mac" } else { "the PC" };

/// The user's home directory: `HOME` on macOS and Linux, the profile folder on Windows.
pub fn home() -> std::path::PathBuf {
    std::env::home_dir().unwrap_or_default()
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
