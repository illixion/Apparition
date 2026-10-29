//! Anisette data without a server: Apple's own Android libraries, run in this process.
//!
//! Apple ID sign-in needs "anisette" headers that prove the request comes from a provisioned
//! device. Apple's Apple Music app for Android carries the library that makes them for any
//! device. This module downloads the two libraries from Apple's CDN (nothing of Apple's is
//! redistributed), loads them (`elf`, with the libc they expect in `shim`), drives them (`adi`),
//! and serves what they make over the v3 protocol isideload speaks (`server`).
//!
//! On a Mac, the headers come from macOS itself when it gives them (`native`), and the sign-in
//! then claims a Mac rather than Android (see claimed_platform).

mod adi;
mod apk;
mod elf;
#[cfg(target_os = "macos")]
pub mod native;
mod server;
mod shim;

pub use apk::{ChecksumMismatch, Imported, LibraryDownload};
pub use server::{Server, start};

/// Takes the anisette libraries from files the person got themselves (see `apk::import`).
pub async fn import_libraries(data_dir: &std::path::Path, paths: &[std::path::PathBuf], accept_unverified: bool) -> Result<Imported, rootcause::Report> {
    apk::import(&data_dir.join("libraries"), paths, accept_unverified).await
}

/// Whether the libraries in `data_dir` include one the person supplied that didn't match the
/// checksum: an unsupported setup.
pub fn libraries_unverified(data_dir: &std::path::Path) -> bool {
    apk::unverified(&data_dir.join("libraries"))
}

/// Forgets the cached libraries, so the next start fetches Apple's own.
pub fn reset_libraries(data_dir: &std::path::Path) -> Result<(), rootcause::Report> {
    apk::reset(&data_dir.join("libraries"))
}
