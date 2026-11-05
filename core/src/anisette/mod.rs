//! Anisette served over the v3 protocol isideload speaks: this Mac's own, or a third-party server's.
//!
//! Apple ID sign-in needs "anisette" headers that prove the request comes from a provisioned
//! device. A Mac is provisioned by macOS already, so the headers come from macOS itself
//! (`native`) and the sign-in claims a Mac, as Xcode does (see claimed_platform). `server`
//! serves them to this process's sign-in and, through the companion's encrypted channel, to
//! paired devices and computers.
//!
//! `native` and `mac` are only on macOS: Apparition runs no Apple code of another platform. A computer
//! that isn't a Mac gets its anisette from a Mac it pairs with (companion_client), or from a
//! third-party anisette server the person gives the address of. Such a computer's companion can
//! offer that server to its paired devices too (`remote`), on every platform.

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
pub use server::{Access, Server, start_remote};
