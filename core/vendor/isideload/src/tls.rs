//! Apparition: HTTPS that refuses governments' interception certificate authorities, even where
//! the device trusts them.
//!
//! Russia and Kazakhstan run national root CAs that people are asked to install (Russia's for
//! sites that lost their ordinary certificates in 2022; Kazakhstan's, in 2019 and 2020, to read
//! all HTTPS traffic). Once installed, the operating system trusts whatever they sign, so a
//! connection to Apple through such a network could be read, and an Apple ID password with it.
//! Browsers refuse Kazakhstan's (Chromium's blocklist, `net/data/ssl/blocklist`, has these
//! files); nothing refuses them for a program that asks the operating system. So every HTTPS
//! client Apparition makes is built here ([`client_builder`]): the operating system decides as
//! usual (rustls-platform-verifier, as reqwest does by itself), but first a chain is refused when
//! any certificate in it
//! - is one of [`national`]'s (by its key), or is named as one of them, or was issued by one: so a
//!   server certificate issued by Russia's Sub CA is refused even when the server sends only its
//!   own certificate, the Sub CA being installed on the device;
//! - or, for Apple's own hosts ([`is_apple`]), was issued by any CA in Russia or Kazakhstan. Apple's
//!   certificates come from Apple's own CAs and DigiCert, so one from there means interception,
//!   by a CA not on the list yet.
//!
//! What it can't see: a CA not on the list, not in either country, or one the server doesn't name
//! in the chain it sends for a host that isn't Apple's. The operating system builds the full
//! chain itself and doesn't say which root it ended at.

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, OtherError, SignatureScheme};
use sha2::{Digest, Sha256};
use x509_cert::der::{Decode, Encode};

/// The national CAs' certificates, as published (Russia: gu-st.ru, Gosuslugi's; Kazakhstan:
/// Chromium's blocklist), with what to call them.
const NATIONAL: [(&str, &[u8]); 12] = [
    ("Russia's national root CA (Russian Trusted Root CA)", include_bytes!("tls/national/ru-russian-trusted-root-ca.der")),
    ("Russia's national CA (Russian Trusted Sub CA)", include_bytes!("tls/national/ru-russian-trusted-sub-ca.der")),
    ("Kazakhstan's interception CA (Qaznet Trust Network)", include_bytes!("tls/national/kz-qaznet-trust-network.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-61c0fc2e.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-1df696f0.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-0230a604.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-06fd2062.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-0bd39de4.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-c95c133b.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-c530fadc.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-89107c8e.der")),
    ("Kazakhstan's interception CA (Information Security Certification Authority)", include_bytes!("tls/national/kz-isca-3472e4f1.der")),
];

/// Countries whose CAs never sign for Apple's hosts.
const NOT_FOR_APPLE: [&str; 2] = ["RU", "KZ"];

/// Apple's own hosts: sign-in, developer services, the CDN, iCloud.
pub fn is_apple(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ["apple.com", "mzstatic.com", "icloud.com", "icloud-content.com", "cdn-apple.com", "apple-cloudkit.com", "aaplimg.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

/// The refusal, as the person reads it (core's error formatter finds it under reqwest's error).
#[derive(Debug, Clone)]
pub struct Intercepted(pub String);

impl std::fmt::Display for Intercepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "This connection is certified by {}, which can read what goes through it, so Apparition won't use it, even though this device trusts it. Use another network (or a VPN), or remove that certificate from this device's trusted ones.",
            self.0
        )
    }
}

impl std::error::Error for Intercepted {}

struct National {
    label: &'static str,
    /// RFC 4514 text, lower case: names compare whatever string types they were encoded with.
    name: String,
    key: [u8; 32],
}

fn national() -> &'static [National] {
    static LIST: OnceLock<Vec<National>> = OnceLock::new();
    LIST.get_or_init(|| {
        NATIONAL
            .iter()
            .map(|(label, der)| {
                let cert = x509_cert::Certificate::from_der(der).expect("a national CA's certificate parses");
                National { label, name: name_text(&cert.tbs_certificate.subject), key: key_hash(&cert).expect("its key encodes") }
            })
            .collect()
    })
}

fn name_text(name: &x509_cert::name::Name) -> String {
    name.to_string().to_ascii_lowercase()
}

fn key_hash(cert: &x509_cert::Certificate) -> Option<[u8; 32]> {
    Some(Sha256::digest(cert.tbs_certificate.subject_public_key_info.to_der().ok()?).into())
}

/// The countries (C=) in `name`.
fn countries(name: &x509_cert::name::Name) -> impl Iterator<Item = String> + '_ {
    name.0.iter().flat_map(|rdn| rdn.0.iter()).filter(|atv| atv.oid.to_string() == "2.5.4.6").map(|atv| String::from_utf8_lossy(atv.value.value()).to_ascii_uppercase())
}

/// Why the chain `certificates` (the server's, then the intermediates it sent) is refused for
/// `host`, or `None`. A certificate that doesn't parse is left to the operating system's check.
pub fn refusal(certificates: &[&[u8]], host: &str) -> Option<String> {
    let apple = is_apple(host);
    for der in certificates {
        let Ok(cert) = x509_cert::Certificate::from_der(der) else { continue };
        let (subject, issuer) = (name_text(&cert.tbs_certificate.subject), name_text(&cert.tbs_certificate.issuer));
        let key = key_hash(&cert);
        // The certificate itself first (by its key or name), then what issued it, so the message
        // names the most specific CA (Russia's Sub CA, not the root that issued it).
        let itself = national().iter().find(|ca| key == Some(ca.key) || subject == ca.name);
        if let Some(found) = itself.or_else(|| national().iter().find(|ca| issuer == ca.name)) {
            return Some(found.label.to_string());
        }
        if apple && let Some(country) = countries(&cert.tbs_certificate.issuer).find(|c| NOT_FOR_APPLE.contains(&c.as_str())) {
            return Some(format!("a certificate authority in {} ({}), which never certifies Apple's servers", if country == "RU" { "Russia" } else { "Kazakhstan" }, cert.tbs_certificate.issuer));
        }
    }
    None
}

/// The operating system's verifier, behind [`refusal`].
#[derive(Debug)]
struct RefuseNational {
    system: rustls_platform_verifier::Verifier,
}

impl ServerCertVerifier for RefuseNational {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let chain: Vec<&[u8]> = std::iter::once(end_entity.as_ref()).chain(intermediates.iter().map(|c| c.as_ref())).collect();
        if let Some(why) = refusal(&chain, &server_name.to_str()) {
            return Err(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(Intercepted(why))))));
        }
        self.system.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.system.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.system.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.system.supported_verify_schemes()
    }
}

/// The TLS settings: the operating system's trust (and `extra_roots`, as reqwest's
/// `add_root_certificate` would add them), with [`refusal`] first. HTTP/2 or 1.1.
pub fn config(extra_roots: &[&[u8]]) -> Result<rustls::ClientConfig, rustls::Error> {
    let provider = rustls::crypto::CryptoProvider::get_default().cloned().unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
    let system = if extra_roots.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())?
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra_roots.iter().map(|der| CertificateDer::from(der.to_vec())), provider.clone())?
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(RefuseNational { system }))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// A reqwest client builder with [`config`]: what every HTTPS client in Apparition starts from.
/// TLS options set on it afterwards (`add_root_certificate`, `danger_accept_invalid_certs`) are
/// ignored; give extra roots here instead.
pub fn client_builder_with_roots(extra_roots: &[&[u8]]) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder();
    match config(extra_roots) {
        Ok(config) => builder.tls_backend_preconfigured(config),
        // The operating system's trust store couldn't be read: reqwest's own build then fails
        // the same way and says so, rather than quietly connecting without the refusal.
        Err(error) => {
            tracing::warn!("TLS settings: {error}");
            builder.tls_backend_preconfigured(())
        }
    }
}

/// [`client_builder_with_roots`] with the operating system's roots only.
pub fn client_builder() -> reqwest::ClientBuilder {
    client_builder_with_roots(&[])
}

/// The refusal behind `error` (reqwest's, or anything wrapping it), if that is why it failed.
/// std's `io::Error` hides what it wraps from `source()`, so that is looked into too.
pub fn intercepted<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a Intercepted> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(found) = error.downcast_ref::<Intercepted>() {
            return Some(found);
        }
        if let Some(rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(other)))) = error.downcast_ref::<rustls::Error>()
            && let Some(found) = other.downcast_ref::<Intercepted>()
        {
            return Some(found);
        }
        // hyper wraps rustls's error in an io::Error, in another io::Error: step into each.
        current = match error.downcast_ref::<std::io::Error>().and_then(|io| io.get_ref()) {
            Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
            None => error.source(),
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOOKALIKE_SUB_CA: &[u8] = include_bytes!("tls/test/lookalike-ru-sub-ca.der");
    const LEAF_FROM_LOOKALIKE: &[u8] = include_bytes!("tls/test/leaf-from-ru-lookalike.der");
    const OTHER_RU_CA: &[u8] = include_bytes!("tls/test/other-ru-ca.der");
    const LEAF_FROM_OTHER_RU_CA: &[u8] = include_bytes!("tls/test/leaf-from-other-ru-ca.der");

    #[test]
    fn every_national_certificate_parses() {
        assert_eq!(national().len(), NATIONAL.len());
        assert!(national()[0].name.contains("cn=russian trusted root ca"), "{}", national()[0].name);
    }

    #[test]
    fn the_national_cas_themselves_are_refused_anywhere() {
        for (label, der) in NATIONAL {
            assert_eq!(refusal(&[der], "example.com").as_deref(), Some(label));
        }
    }

    /// A server certificate issued by Russia's Sub CA, sent alone (the device has the Sub CA),
    /// and sent with a certificate that has the Sub CA's name but another key.
    #[test]
    fn what_a_national_ca_issued_is_refused_by_its_issuer_name() {
        assert!(refusal(&[LEAF_FROM_LOOKALIKE], "example.com").is_some_and(|why| why.contains("Russian Trusted Sub CA")));
        assert!(refusal(&[LEAF_FROM_LOOKALIKE, LOOKALIKE_SUB_CA], "github.com").is_some());
    }

    #[test]
    fn a_ca_in_russia_is_refused_for_apple_only() {
        let chain = [LEAF_FROM_OTHER_RU_CA, OTHER_RU_CA];
        assert!(refusal(&chain, "gsa.apple.com").is_some_and(|why| why.contains("Russia")));
        assert!(refusal(&chain, "apps.mzstatic.com").is_some());
        assert_eq!(refusal(&chain, "example.ru"), None);
    }

    #[test]
    fn apples_own_chains_pass() {
        let gsa: [&[u8]; 3] = [include_bytes!("tls/test/gsa.apple.com-1.der"), include_bytes!("tls/test/gsa.apple.com-2.der"), include_bytes!("tls/test/gsa.apple.com-3.der")];
        let cdn: [&[u8]; 3] = [include_bytes!("tls/test/apps.mzstatic.com-1.der"), include_bytes!("tls/test/apps.mzstatic.com-2.der"), include_bytes!("tls/test/apps.mzstatic.com-3.der")];
        assert_eq!(refusal(&gsa, "gsa.apple.com"), None);
        assert_eq!(refusal(&cdn, "apps.mzstatic.com"), None);
    }

    #[test]
    fn apples_hosts() {
        for host in ["gsa.apple.com", "apple.com", "developerservices2.apple.com.", "apps.mzstatic.com", "updates.cdn-apple.com", "GSA.APPLE.COM"] {
            assert!(is_apple(host), "{host}");
        }
        for host in ["notapple.com", "apple.com.example", "github.com", "mzstatic.com.ru"] {
            assert!(!is_apple(host), "{host}");
        }
    }

    #[test]
    fn the_refusal_is_found_under_the_errors_that_wrap_it() {
        let tls = rustls::Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(Intercepted("X".into())))));
        let io = std::io::Error::other(tls);
        assert_eq!(intercepted(&io).map(|found| found.0.as_str()), Some("X"));
        assert!(intercepted(&std::io::Error::other("unrelated")).is_none());
    }

    /// A real handshake with a server whose certificate was issued under the Russian Sub CA's name
    /// (by a new key, here) fails, and the refusal is found under reqwest's error.
    #[test]
    fn a_real_handshake_is_refused_and_says_why() {
        use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        let mut name = DistinguishedName::new();
        name.push(DnType::CountryName, "RU");
        name.push(DnType::OrganizationName, "The Ministry of Digital Development and Communications");
        name.push(DnType::CommonName, "Russian Trusted Sub CA");
        ca.distinguished_name = name;
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let issuer = Issuer::new(ca, KeyPair::generate().unwrap());
        let key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["national.example".into()]).unwrap().signed_by(&key, &issuer).unwrap();
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone()], rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()))
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut connection = rustls::ServerConnection::new(Arc::new(server)).unwrap();
                let _ = connection.complete_io(&mut stream);
            }
        });
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        let client = client_builder().resolve("national.example", address).build().unwrap();
        let error = runtime.block_on(client.get(format!("https://national.example:{}/", address.port())).send()).unwrap_err();
        let found = intercepted(&error).unwrap_or_else(|| panic!("not found under {error:?}"));
        assert!(found.0.contains("Russian Trusted Sub CA"), "{found}");
    }

    /// A real connection to Apple with these settings works (the operating system's own check,
    /// Apple's roots added as the GrandSlam client adds them). Needs the internet: APPARITION_NET=1.
    #[test]
    fn connects_to_apple() {
        if std::env::var_os("APPARITION_NET").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let roots: [&[u8]; 2] = [crate::sideload::cert_identity::APPLE_ROOT, crate::auth::grandslam::APPLE_ROOT_G3];
        let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        for (url, roots) in [("https://gsa.apple.com/grandslam/GsService2/lookup", &roots[..]), ("https://apps.mzstatic.com/", &[][..])] {
            let response = runtime.block_on(client_builder_with_roots(roots).build().unwrap().get(url).send());
            assert!(response.is_ok(), "{url}: {response:?}");
        }
    }
}
