//! Which device the sign-in says it comes from (`X-Mme-Client-Info`), matched to the anisette.
//!
//! Since 2026-10-12 Apple refuses sign-in (`-80045`) when the anisette's platform and the
//! claimed one differ. isideload always claims a Mac (`<Mac15,7> <macOS;27.0;…>`), while nearly
//! every anisette server, the built-in one off a Mac included, makes Android anisette with Apple
//! Music's libraries. Tested 2026-10-13 with a made-up account, same anisette and request: the
//! Android platform in the claim (`<…><Android; …>`) is accepted, macOS gets `-80045`; the
//! program named at the end (`com.apple.akd`, `com.apple.android.music`) and the User-Agent
//! make no difference. A Mac's own anisette (anisette::native) is accepted claiming macOS.
//!
//! So the claim follows the server: Apparition's own says what it serves (`/v3/client_info`
//! carries `apparition_anisette`), and any other is taken to run the Android libraries.

use std::sync::Arc;

use isideload::anisette::{AnisetteClientInfo, AnisetteData, AnisetteProvider};
use isideload::auth::grandslam::GrandSlam;
use rootcause::prelude::*;

/// An Android phone running Apple Music, for anisette made by Apple Music's Android libraries.
pub const ANDROID_CLIENT_INFO: &str = "<Pixel 8><Android; 14; UQ1A.240205.004><com.apple.AuthKit/1 (com.apple.android.music/4.9.6)>";
/// A Mac's AuthKit daemon, for a Mac's own anisette.
pub const MAC_CLIENT_INFO: &str = "<Mac15,7> <macOS;27.0;26A5378j> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
pub const USER_AGENT: &str = "akd/1.0 CFNetwork/808.1.4";

/// What Apparition's own anisette server says it serves, in `/v3/client_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served {
    /// A Mac's own anisette.
    Mac,
    /// Made by Apple Music's Android libraries.
    Android,
}

impl Served {
    pub fn name(self) -> &'static str {
        match self {
            Served::Mac => "mac",
            Served::Android => "android",
        }
    }

    pub fn client_info(self) -> &'static str {
        match self {
            Served::Mac => MAC_CLIENT_INFO,
            Served::Android => ANDROID_CLIENT_INFO,
        }
    }

    fn from_client_info_reply(reply: &serde_json::Value) -> Served {
        match reply.get("apparition_anisette").and_then(|value| value.as_str()) {
            Some("mac") => Served::Mac,
            // Another make of server, or ours serving the Android libraries' anisette.
            _ => Served::Android,
        }
    }
}

/// What the server at `url` serves. A server that doesn't answer is taken to be an Android one;
/// the sign-in then fails on the server, not here.
pub async fn served_by(url: &str) -> Served {
    let reply = async {
        let response = reqwest::Client::new()
            .get(format!("{}/v3/client_info", url.trim_end_matches('/')))
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .ok()?;
        response.json::<serde_json::Value>().await.ok()
    };
    reply.await.map(|reply| Served::from_client_info_reply(&reply)).unwrap_or(Served::Android)
}

/// `inner`, claiming the device that matches what its server serves.
pub struct ClaimedPlatform<P> {
    inner: P,
    served: Served,
}

impl<P> ClaimedPlatform<P> {
    pub fn new(inner: P, served: Served) -> Self {
        ClaimedPlatform { inner, served }
    }
}

#[async_trait::async_trait]
impl<P: AnisetteProvider + Send + Sync> AnisetteProvider for ClaimedPlatform<P> {
    async fn get_anisette_data(&self) -> Result<AnisetteData, Report> {
        self.inner.get_anisette_data().await
    }

    async fn get_client_info(&self) -> Result<AnisetteClientInfo, Report> {
        Ok(AnisetteClientInfo { client_info: self.served.client_info().to_string(), user_agent: USER_AGENT.to_string() })
    }

    async fn provision(&mut self, gs: Arc<GrandSlam>) -> Result<(), Report> {
        self.inner.provision(gs).await
    }

    fn needs_provisioning(&self) -> Result<bool, Report> {
        self.inner.needs_provisioning()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_mac_server_claims_a_mac() {
        let reply = |value: serde_json::Value| Served::from_client_info_reply(&value);
        assert_eq!(reply(serde_json::json!({ "client_info": "x", "apparition_anisette": "mac" })), Served::Mac);
        assert_eq!(reply(serde_json::json!({ "client_info": "x", "apparition_anisette": "android" })), Served::Android);
        // Dadoum's server and others: the Android libraries.
        assert_eq!(reply(serde_json::json!({ "client_info": "<MacBookPro13,2> <macOS;13.1;22C65> <…>" })), Served::Android);
        assert!(ANDROID_CLIENT_INFO.contains("<Android; "));
        assert!(MAC_CLIENT_INFO.contains("<macOS;"));
    }
}
