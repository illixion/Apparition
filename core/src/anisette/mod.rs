//! Anisette served over the v3 protocol isideload speaks: this Mac's own, or a third-party server's.
//!
//! Apple ID sign-in needs "anisette" headers that prove the request comes from a provisioned
//! device. A Mac is provisioned by macOS already, so the headers come from macOS itself
//! (`native`) and the sign-in claims a Mac, as Xcode does (see claimed_platform). `server`
//! serves them to this process's sign-in and, through the companion's encrypted channel, to
//! paired devices and computers.
//!
//! Apple Music's Android libraries, run in this process (`android`, with `apk`, `adi`, `elf` and `shim`), are
//! the other source, for a computer that has no Mac's anisette of its own (macOS 27, Linux, Windows). Apple has shut
//! that route and reopened it more than once, so it is switched by one constant, [`ANDROID_LIBRARIES`], and
//! its code stays in the tree whether or not it is on.
//!
//! `native` and `mac` are only on macOS: Apparition runs no Apple code of another platform. A computer
//! that isn't a Mac gets its anisette from a Mac it pairs with (companion_client), or from a
//! third-party anisette server the person gives the address of. Such a computer's companion can
//! offer that server to its paired devices too (`remote`), on every platform.

/// Whether Apparition runs Apple Music's Android libraries for its own anisette where a Mac's isn't available:
/// macOS 27 outside a prepared VM, Linux and Windows. Switch it off when Apple turns that anisette away for good;
/// everything it needs stays compiled in, and sign-in then asks for a paired Mac or a server's address, as it
/// did from 3 to 12 Nov 2026 (`13e1452`, brought back in this form on 12 Nov).
pub const ANDROID_LIBRARIES: bool = true;

#[cfg(feature = "anisette")]
mod adi;
#[cfg(feature = "anisette")]
mod android;
#[cfg(feature = "anisette")]
mod apk;
#[cfg(feature = "anisette")]
mod elf;
#[cfg(feature = "anisette")]
mod shim;
#[cfg(all(feature = "anisette", target_os = "macos"))]
mod mac;
#[cfg(all(feature = "anisette", target_os = "macos"))]
pub mod native;
pub mod remote;
mod server;

pub use remote::normalize as normalize_server_address;
#[cfg(all(feature = "anisette", target_os = "macos"))]
pub use server::start;
#[cfg(all(feature = "desktop", feature = "anisette", target_os = "macos"))]
pub use server::start_vm;
#[cfg(feature = "anisette")]
pub use apk::{ChecksumMismatch, Imported, LibraryDownload};
#[cfg(feature = "anisette")]
pub use server::start_android;
pub use server::{Access, Server, start_remote};

/// Takes the anisette libraries from files the person got themselves (see `apk::import`).
#[cfg(feature = "anisette")]
pub async fn import_libraries(data_dir: &std::path::Path, paths: &[std::path::PathBuf], accept_unverified: bool) -> Result<Imported, rootcause::Report> {
    apk::import(&data_dir.join("libraries"), paths, accept_unverified).await
}

/// Whether the libraries in `data_dir` include one the person supplied that didn't match the
/// checksum: an unsupported setup.
#[cfg(feature = "anisette")]
pub fn libraries_unverified(data_dir: &std::path::Path) -> bool {
    apk::unverified(&data_dir.join("libraries"))
}

/// Forgets the cached libraries, so the next start fetches Apple's own.
#[cfg(feature = "anisette")]
pub fn reset_libraries(data_dir: &std::path::Path) -> Result<(), rootcause::Report> {
    apk::reset(&data_dir.join("libraries"))
}
