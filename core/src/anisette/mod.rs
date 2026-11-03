//! Anisette from this Mac: macOS's own, served over the v3 protocol isideload speaks.
//!
//! Apple ID sign-in needs "anisette" headers that prove the request comes from a provisioned
//! device. A Mac is provisioned by macOS already, so the headers come from macOS itself
//! (`native`) and the sign-in claims a Mac, as Xcode does (see claimed_platform). `server`
//! serves them to this process's sign-in and, through the companion's encrypted channel, to
//! paired devices and computers.
//!
//! Only on macOS: Apparition runs no Apple code of another platform. A computer that isn't a Mac
//! gets its anisette from a Mac it pairs with (companion_client), or from an anisette server
//! the person gives the address of.

pub mod native;
mod server;

pub use server::{Access, Server, start};
