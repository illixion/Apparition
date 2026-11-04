//! Apparition Store in Rust. Scope and phases: docs/RUST_STORE.md.
//!
//! A library, so it tests without Tauri; the desktop app runs it in-process.

//! Mac only (decided 2026-11-04): it builds with Xcode and uses Unix file APIs, so it compiles to
//! nothing elsewhere and the desktop app leaves the feature out of its Windows and Linux builds.
#![cfg(target_os = "macos")]

pub mod artifacts;
pub mod assets;
pub mod auth;
pub mod config;
pub mod crashlogs;
pub mod debug;
pub mod deploy;
pub mod git;
pub mod install_mode;
pub mod mcp;
pub mod state;
pub mod traces;
pub mod server;
