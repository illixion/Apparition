//! Self-updates for apps that ship outside a store, verified against keys pinned in the app.
//!
//! A release is one `SHA256SUMS` listing its files, signed with `ssh-keygen -Y sign` (SSHSIG) by a
//! key the app has compiled in. Everything else is a hint: the updater asks each [`Source`] in
//! turn (apps.illixion.com, which only redirects to wherever the release is hosted, then GitHub
//! Releases directly) for the newest release's manifest, believes the first one whose signature
//! verifies, and downloads the file from any source whose copy hashes as the signed manifest says.
//! A source that is down costs nothing; one that lies is refused. See `docs/PROTOCOL.md`.
//!
//! ```no_run
//! # #[cfg(feature = "http")]
//! # async fn demo() -> signed_update::Result<()> {
//! use signed_update::{Config, Source, check};
//! let config = Config::new("Vergence", "vergence-release", include_str!("../release-signers"))
//!     .source(Source::apps("vergence"))
//!     .source(Source::github("illixion/vergence"));
//! if let Some(update) = check(&config, env!("CARGO_PKG_VERSION"), "Vergence-{version}-macos-universal.zip").await? {
//!     let package = update.download(&std::env::temp_dir(), |_bytes| {}).await?;
//!     if let Some(bundle) = signed_update::install::current_bundle() {
//!         signed_update::install::install_bundle(&package, &bundle)?;
//!     }
//! }
//! # Ok(()) }
//! ```

mod error;
pub mod install;
mod manifest;
mod source;
mod sshsig;
mod version;

#[cfg(feature = "http")]
mod check;
#[cfg(feature = "ech")]
mod dns;
#[cfg(feature = "ech")]
mod ech;
#[cfg(feature = "http")]
mod net;
#[cfg(feature = "http")]
mod tls;

pub use error::{Error, Result};
pub use manifest::{Manifest, parse_sums};
pub use source::{Config, Privacy, Source};
pub use sshsig::{signer_keys, verify_sshsig};
pub use version::is_newer;

#[cfg(feature = "http")]
pub use check::{Update, check};
/// The rustls a [`Config::verifier`] is written against.
#[cfg(feature = "http")]
pub use rustls;
