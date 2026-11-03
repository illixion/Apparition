#[cfg(feature = "wasm")]
use super::middleware::WasmProxyMiddleware;
use plist::Dictionary;
use plist_macro::plist_to_xml_string;
use plist_macro::pretty_print_dictionary;
#[cfg(not(feature = "wasm"))]
use reqwest::Certificate;
use reqwest::{
    ClientBuilder,
    header::{HeaderMap, HeaderValue},
};
use reqwest_middleware::ClientBuilder as MwClientBuilder;
use rootcause::prelude::*;
use tracing::debug;
use tracing::warn;

#[cfg(not(feature = "wasm"))]
use crate::sideload::cert_identity::APPLE_ROOT;

/// gsd.apple.com (machine provisioning) chains to this root, which Linux's and Windows' trust
/// stores may not have; macOS's does.
pub(crate) const APPLE_ROOT_G3: &[u8] = include_bytes!("../assets/AppleRootCA-G3.cer");
use crate::{SideloadError, anisette::AnisetteClientInfo, util::plist::PlistDataExtract};

const URL_BAG: &str = "https://gsa.apple.com/grandslam/GsService2/lookup";

pub struct GrandSlam {
    pub client: reqwest_middleware::ClientWithMiddleware,
    pub client_info: AnisetteClientInfo,
    url_bag: Dictionary,
    windows_adi: bool,
}

impl GrandSlam {
    /// Create a new GrandSlam instance
    ///
    /// # Arguments
    /// - `client`: The reqwest client to use for requests
    pub async fn new(
        client_info: AnisetteClientInfo,
        debug: bool,
        proxy_url: Option<String>,
    ) -> Result<Self, Report> {
        let client =
            Self::build_reqwest_client(debug, proxy_url).context("Failed to build HTTP client")?;
        let base_headers = Self::base_headers(&client_info, false)?;
        let url_bag = Self::fetch_url_bag(&client, base_headers).await?;
        let windows_adi = client_info.client_info.contains("<Windows;");
        Ok(Self {
            client,
            client_info,
            url_bag,
            windows_adi,
        })
    }

    pub fn claiming(&self, client_info: AnisetteClientInfo) -> Self {
        Self {
            client: self.client.clone(),
            client_info,
            url_bag: self.url_bag.clone(),
            windows_adi: self.windows_adi,
        }
    }

    pub fn client_provided_data(&self, data: &crate::anisette::AnisetteData) -> Dictionary {
        let mut cpd = data.get_client_provided_data();
        if self.windows_adi { windows_client_provided_data(&mut cpd); }
        cpd
    }

    /// Fetch the URL bag from GrandSlam and cache it
    pub async fn fetch_url_bag(
        client: &reqwest_middleware::ClientWithMiddleware,
        base_headers: HeaderMap,
    ) -> Result<Dictionary, Report> {
        debug!("Fetching URL bag from GrandSlam");
        let resp = client
            .get(URL_BAG)
            .headers(base_headers)
            .send()
            .await
            .context("Failed to fetch URL Bag")?
            .text()
            .await
            .context("Failed to read URL Bag response text")?;

        let dict: Dictionary =
            plist::from_bytes(resp.as_bytes()).context("Failed to parse URL Bag plist")?;
        let urls = dict
            .get("urls")
            .and_then(|v| v.as_dictionary())
            .cloned()
            .ok_or_else(|| report!("URL Bag plist missing 'urls' dictionary"))?;

        Ok(urls)
    }

    pub fn get_url(&self, key: &str) -> Result<String, Report> {
        let url = self
            .url_bag
            .get_string(key)
            .context("Unable to find key in URL bag")?;
        Ok(url)
    }

    pub fn get(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .get(url)
            .headers(Self::base_headers(&self.client_info, false)?);

        Ok(builder)
    }

    pub fn get_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .get(url)
            .headers(Self::base_headers(&self.client_info, true)?);

        Ok(builder)
    }

    pub fn put_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .put(url)
            .headers(Self::base_headers(&self.client_info, true)?);

        Ok(builder)
    }

    pub fn post(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .post(url)
            .headers(Self::base_headers(&self.client_info, false)?);

        Ok(builder)
    }

    pub fn post_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .post(url)
            .headers(Self::base_headers(&self.client_info, true)?);

        Ok(builder)
    }

    pub fn patch(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .patch(url)
            .headers(Self::base_headers(&self.client_info, false)?);

        Ok(builder)
    }

    pub async fn plist_request(
        &self,
        url: &str,
        body: &Dictionary,
        additional_headers: Option<HeaderMap>,
        retry_limit: Option<u32>,
    ) -> Result<Dictionary, Report> {
        let result = self
            .post(url)?
            .headers(
                additional_headers
                    .clone()
                    .unwrap_or_else(reqwest::header::HeaderMap::new),
            )
            .body(plist_to_xml_string(body))
            .send()
            .await
            .context("Failed to send grandslam request")?;

        if let Some(retries) = retry_limit
            && result.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            if retries > 0 {
                warn!(
                    "Received 429 Too Many Requests, retrying request ({} retries left)",
                    retries
                );
                return Box::pin(self.plist_request(
                    url,
                    body,
                    additional_headers,
                    Some(retries - 1),
                ))
                .await;
            } else {
                bail!("Received 429 Too Many Requests, no retries left");
            }
        }

        let resp = result
            .error_for_status()
            .context("Received error response from grandslam")?
            .text()
            .await
            .context("Failed to read grandslam response as text")?;

        let dict: Dictionary = plist::from_bytes(resp.as_bytes())
            .context("Failed to parse grandslam response plist")
            .attach_with(|| resp.clone())?;

        let response_plist = dict
            .get("Response")
            .and_then(|v| v.as_dictionary())
            .cloned()
            .ok_or_else(|| {
                report!("grandslam response missing 'Response'")
                    .attach(pretty_print_dictionary(&dict))
            })?;

        Ok(response_plist)
    }

    fn base_headers(
        client_info: &AnisetteClientInfo,
        sms: bool,
    ) -> Result<reqwest::header::HeaderMap, Report> {
        let mut headers = reqwest::header::HeaderMap::new();
        if !sms {
            headers.insert("Content-Type", HeaderValue::from_static("text/xml"));
            headers.insert("Accept", HeaderValue::from_static("text/x-xml-plist"));
        } else {
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));
            headers.insert("Accept", HeaderValue::from_static("application/json"));
        }
        headers.insert(
            "X-Mme-Client-Info",
            HeaderValue::from_str(&client_info.client_info)?,
        );
        headers.insert(
            "User-Agent",
            HeaderValue::from_str(&client_info.user_agent)?,
        );
        headers.insert("X-Xcode-Version", HeaderValue::from_static("27.0 (27A5218g)"));
        headers.insert("X-Apple-App-Info", HeaderValue::from_static("com.apple.gs.xcode.auth"));
        headers.insert(
            reqwest::header::CONNECTION,
            HeaderValue::from_static("close"),
        );

        Ok(headers)
    }

    /// Build a reqwest client with the Apple root certificate
    ///
    /// # Arguments
    /// - `debug`: DANGER, If true, accept invalid certificates and enable verbose connection logging
    /// # Errors
    /// Returns an error if the reqwest client cannot be built
    pub fn build_reqwest_client(
        #[cfg(not(feature = "wasm"))] debug: bool,
        #[cfg(feature = "wasm")] _debug: bool,
        proxy_url: Option<String>,
    ) -> Result<reqwest_middleware::ClientWithMiddleware, Report> {
        #[cfg(not(feature = "wasm"))]
        let cert = Certificate::from_der(APPLE_ROOT)?;
        #[cfg(not(feature = "wasm"))]
        let cert_g3 = Certificate::from_der(APPLE_ROOT_G3)?;
        #[cfg(not(feature = "wasm"))]
        let client = ClientBuilder::new()
            .add_root_certificate(cert)
            .add_root_certificate(cert_g3)
            .http1_title_case_headers()
            .danger_accept_invalid_certs(debug)
            .connection_verbose(debug)
            // An edge that doesn't answer is given up on, and another tried (edge.rs).
            .connect_timeout(std::time::Duration::from_secs(10))
            // An edge that answers 403 is avoided and the request goes to another (edge.rs).
            .dns_resolver(super::edge::EdgeResolver::system());
        #[cfg(not(feature = "wasm"))]
        let client = if let Some(ref proxy) = proxy_url {
            if debug { bail!("Apple relay requires certificate verification"); }
            client.https_only(true).proxy(reqwest::Proxy::https(proxy)?)
        } else { client };
        #[cfg(not(feature = "wasm"))]
        let client = client.build()?;
        #[cfg(feature = "wasm")]
        let client = ClientBuilder::new().build()?;

        let builder = MwClientBuilder::new(client);
        #[cfg(not(feature = "wasm"))]
        let builder = if proxy_url.is_none() {
            builder.with(super::edge::EdgeAvoidance)
        } else { builder };
        #[cfg(feature = "wasm")]
        let builder = if let Some(proxy_url) = proxy_url {
            builder.with(WasmProxyMiddleware::new(proxy_url))
        } else {
            builder
        };
        Ok(builder.build())
    }
}

fn windows_client_provided_data(cpd: &mut Dictionary) {
    for key in ["bootstrap", "ckgen", "icscrec", "pbe", "winprot", "wpbe"] {
        cpd.insert(key.into(), plist::Value::Boolean(true));
    }
    cpd.remove("prkgen");
    cpd.insert("cou".into(), plist::Value::String("US".into()));
    cpd.insert("X-Apple-I-Device-Configuration-Mode".into(), plist::Value::String("0".into()));
}

#[cfg(test)]
mod windows_cpd_tests {
    use super::*;
    #[test]
    fn windows_and_mac_requests_both_carry_the_xcode_headers() {
        let info = |platform: &str| AnisetteClientInfo { client_info: platform.into(), user_agent: "test".into() };
        let windows = GrandSlam::base_headers(&info("<PC> <Windows;10>"), false).unwrap();
        assert!(windows.contains_key("X-Xcode-Version"));
        assert!(windows.contains_key("X-Apple-App-Info"));
        let mac = GrandSlam::base_headers(&info("<Mac15,7> <macOS;27>"), false).unwrap();
        assert!(mac.contains_key("X-Xcode-Version"));
        assert!(mac.contains_key("X-Apple-App-Info"));
    }

    #[test]
    fn changing_request_metadata_preserves_windows_cpd_profile() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let original = GrandSlam {
            client: MwClientBuilder::new(reqwest::Client::new()).build(),
            client_info: AnisetteClientInfo { client_info: "<PC> <Windows;10>".into(), user_agent: "test".into() },
            url_bag: Dictionary::new(),
            windows_adi: true,
        };
        let mac = original.claiming(AnisetteClientInfo { client_info: "<Mac15,7> <macOS;27>".into(), user_agent: "test".into() });
        assert!(mac.windows_adi);
        assert!(original.client_info.client_info.contains("<Windows;"));
        assert!(mac.client_info.client_info.contains("<macOS;"));
    }

    #[test]
    fn windows_flags_match_the_observed_authkit_types_without_replacing_identity() {
        let mut cpd = Dictionary::new();
        cpd.insert("pbe".into(), plist::Value::String("false".into()));
        cpd.insert("prkgen".into(), plist::Value::String("true".into()));
        cpd.insert("X-Apple-I-MD".into(), plist::Value::String("test-otp".into()));
        windows_client_provided_data(&mut cpd);
        for key in ["bootstrap", "ckgen", "icscrec", "pbe", "winprot", "wpbe"] {
            assert_eq!(cpd.get(key).and_then(plist::Value::as_boolean), Some(true));
        }
        assert!(!cpd.contains_key("prkgen"));
        assert_eq!(cpd.get("X-Apple-I-MD").and_then(plist::Value::as_string), Some("test-otp"));
    }
}

pub trait GrandSlamErrorChecker {
    fn check_grandslam_error(self) -> Result<Dictionary, Report<SideloadError>>;
}

impl GrandSlamErrorChecker for Dictionary {
    fn check_grandslam_error(self) -> Result<Self, Report<SideloadError>> {
        let result = match self.get("Status") {
            Some(plist::Value::Dictionary(d)) => d,
            _ => &self,
        };

        if result.get_signed_integer("ec").unwrap_or(0) != 0 {
            bail!(SideloadError::AuthWithMessage(
                result.get_signed_integer("ec").unwrap_or(-1),
                result.get_str("em").unwrap_or("Unknown error").to_string(),
            ))
        }

        Ok(self)
    }
}
