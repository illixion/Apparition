//! The TLS every request uses: the operating system's trust store, through rustls-platform-verifier,
//! or the app's own verifier ([`Config::verifier`]). No list of roots is compiled in, so a CA the OS
//! stops trusting stops working here with the OS's update, and an app that refuses some CAs (one it
//! knows intercepts) refuses them for its updates too.
//!
//! Trust here only decides who can see or block a check, never what gets installed: a release is
//! believed for its signature, whichever server delivered it.

use std::sync::Arc;

use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
use rustls::{ClientConfig, ConfigBuilder, WantsVerifier};

use crate::error::{Error, Result};
use crate::source::Config;

/// The process's default provider when it installed one, else ring.
pub(crate) fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default().cloned().unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()))
}

/// [`Config::verifier`], else the operating system's.
pub(crate) fn verifier(config: &Config) -> Result<Arc<dyn ServerCertVerifier>> {
    if let Some(verifier) = &config.verifier {
        return Ok(verifier.clone());
    }
    let system = rustls_platform_verifier::Verifier::new(provider()).map_err(|e| Error::new(format!("the system's certificates: {e}")))?;
    Ok(Arc::new(system))
}

/// A client config over `verifier`, from `builder` (which chose the provider and, for ECH, the
/// mode). HTTP/1.1 only, as reqwest is built without HTTP/2.
pub(crate) fn finish(builder: ConfigBuilder<ClientConfig, WantsVerifier>, verifier: Arc<dyn ServerCertVerifier>) -> ClientConfig {
    let mut tls = builder.dangerous().with_custom_certificate_verifier(verifier).with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    tls
}

/// An ordinary client config over `verifier`.
pub(crate) fn client_config(verifier: Arc<dyn ServerCertVerifier>) -> Result<ClientConfig> {
    let builder = ClientConfig::builder_with_provider(provider()).with_safe_default_protocol_versions().map_err(|e| Error::new(e.to_string()))?;
    Ok(finish(builder, verifier))
}
