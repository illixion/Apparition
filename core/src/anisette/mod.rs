//! Anisette data without a server: Apple's own Android libraries, run in this process.
//!
//! Apple ID sign-in needs "anisette" headers that prove the request comes from a provisioned
//! device. Apple's Apple Music app for Android carries the library that makes them for any
//! device. This module downloads the two libraries from Apple's CDN (nothing of Apple's is
//! redistributed), loads them (`elf`, with the libc they expect in `shim`), drives them (`adi`),
//! and serves what they make over the v3 protocol isideload speaks (`server`).
//!
//! On a Mac, the headers come from macOS itself when it gives them (`native`): Apple has refused
//! the Android libraries' since 2026-10-12, and still accepts a Mac's own.

mod adi;
mod apk;
mod elf;
#[cfg(target_os = "macos")]
pub mod native;
mod server;
mod shim;

pub use server::{Server, start};
