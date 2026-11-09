//! Every request an update check makes, with redirects followed here instead of by the HTTP client
//! so each hop picks its own way of connecting: a concealed host under [`Privacy::Ech`] goes through
//! DNS-over-HTTPS and Encrypted Client Hello, anything else (GitHub) the ordinary way. That is what
//! lets apps.illixion.com redirect to github.com without the first hop showing its name.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result, fail};
use crate::source::{Config, Privacy, normal_host};

const MAX_REDIRECTS: usize = 6;

#[derive(Debug, Clone)]
pub(crate) struct Net {
    user_agent: String,
    privacy: Privacy,
    doh: Vec<String>,
    /// Hosts that [`Privacy::Ech`] reaches only privately, normalised ([`normal_host`]).
    concealed: Vec<String>,
    plain: reqwest::Client,
    /// Private clients by host, so a check's three requests share one lookup.
    private: Arc<Mutex<HashMap<String, reqwest::Client>>>,
}

impl Net {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        let plain = reqwest::Client::builder()
            .user_agent(&config.user_agent)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::new(e.to_string()))?;
        Ok(Self {
            user_agent: config.user_agent.clone(),
            privacy: config.privacy,
            doh: config.doh.clone(),
            concealed: config.sources.iter().filter(|s| s.is_concealed()).filter_map(|s| s.host()).collect(),
            plain,
            private: Arc::default(),
        })
    }

    pub(crate) fn set_privacy(&mut self, privacy: Privacy) {
        self.privacy = privacy;
    }

    /// The client for `url`: the private one for a concealed host under [`Privacy::Ech`], whatever
    /// its port or spelling (`host` is normalised), and only over https, since a plain-http request
    /// would carry the name in the clear however it was routed.
    async fn client_for(&self, url: &reqwest::Url, host: &str) -> Result<reqwest::Client> {
        if self.privacy != Privacy::Ech || !self.concealed.iter().any(|h| h == host) {
            return Ok(self.plain.clone());
        }
        if url.scheme() != "https" {
            fail!("can only be reached privately, and {} isn't https", url.scheme());
        }
        if let Some(client) = self.private.lock().unwrap().get(host) {
            return Ok(client.clone());
        }
        let client = private_client(host, &self.doh, &self.user_agent).await?;
        self.private.lock().unwrap().insert(host.to_string(), client.clone());
        Ok(client)
    }

    /// GETs `url`, following up to a few redirects, each over the right kind of connection. Any
    /// status but a redirect is returned for the caller to judge.
    pub(crate) async fn get(&self, url: &str) -> Result<reqwest::Response> {
        let mut url = reqwest::Url::parse(url).map_err(|e| format!("{url}: {e}"))?;
        for _ in 0..MAX_REDIRECTS {
            let host = normal_host(url.host_str().ok_or_else(|| format!("{url} has no host"))?);
            let client = self.client_for(&url, &host).await.map_err(|e| format!("{host}: {e}"))?;
            let response = client.get(url.clone()).send().await.map_err(|e| format!("can't reach {host}: {e}"))?;
            if response.status().is_redirection() {
                let to = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| format!("{url} redirects nowhere"))?;
                url = url.join(to).map_err(|e| format!("{url} redirects to {to}: {e}"))?;
                continue;
            }
            return Ok(response);
        }
        fail!("{url}: too many redirects")
    }
}

#[cfg(feature = "ech")]
async fn private_client(host: &str, doh: &[String], user_agent: &str) -> Result<reqwest::Client> {
    crate::ech::client(host, doh, user_agent).await
}

/// Without the `ech` feature a concealed host can't be reached privately, and is never reached in
/// the clear instead.
#[cfg(not(feature = "ech"))]
async fn private_client(host: &str, _doh: &[String], _user_agent: &str) -> Result<reqwest::Client> {
    fail!("{host} can only be reached privately, which needs the signed-update `ech` feature")
}
