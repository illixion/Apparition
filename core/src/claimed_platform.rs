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
//! The claim follows our own companion's `apparition_anisette` field. A single explicitly
//! allowlisted HTTPS endpoint, mac-ani.jkcoxson.com, may additionally claim macOS, but only
//! if its /v3/client_info response actually reports macOS AuthKit. Other third-party
//! servers remain treated as Android; an advertised platform is not proof of provenance.

use std::sync::Arc;

use isideload::anisette::{AnisetteClientInfo, AnisetteDataResponse, AnisetteProvider};
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

/// Identifies the deliberately allowlisted, HTTPS-only test server. Other origins,
/// credentials, paths and query parameters must never inherit this exception.
pub fn is_allowlisted_mac_anisette(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else { return false };
    parsed.scheme() == "https"
        && parsed.host_str().is_some_and(|host| host.eq_ignore_ascii_case("mac-ani.jkcoxson.com"))
        && parsed.port_or_known_default() == Some(443)
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.path() == "/"
        && parsed.query().is_none()
        && parsed.fragment().is_none()
}

#[derive(Debug, Clone)]
pub struct ServerIdentity {
    pub served: Served,
    pub public_mac: bool,
    info: AnisetteClientInfo,
}

impl ServerIdentity {
    fn conventional(served: Served) -> Self {
        Self {
            served,
            public_mac: false,
            info: AnisetteClientInfo {
                client_info: served.client_info().to_owned(),
                user_agent: USER_AGENT.to_owned(),
            },
        }
    }
}

/// Strictly validate the remote service's public metadata before using it as a macOS claim.
/// This is a compatibility check, not an attestation that the server runs on a genuine Mac.
fn inspected_identity(url: &str, reply: &serde_json::Value) -> Result<ServerIdentity, Report> {
    if !is_allowlisted_mac_anisette(url) {
        return Ok(ServerIdentity::conventional(Served::from_client_info_reply(reply)));
    }
    let mac_info = reply.get("client_info").and_then(serde_json::Value::as_str)
        .ok_or_else(|| report!("The public Mac anisette server returned no client_info"))?;
    let user_agent = reply.get("user_agent").and_then(serde_json::Value::as_str)
        .ok_or_else(|| report!("The public Mac anisette server returned no user_agent"))?;
    if !mac_info.starts_with("<Mac")
        || !(mac_info.contains("<Mac OS X;") || mac_info.contains("<macOS;"))
        || !mac_info.contains("com.apple.akd/")
        || !user_agent.starts_with("akd/")
        || mac_info.len() > 256
        || user_agent.len() > 256
        || mac_info.contains(['\r', '\n'])
        || user_agent.contains(['\r', '\n'])
        || reply.get("apparition_anisette").and_then(serde_json::Value::as_str) == Some("android")
    {
        bail!("The allowlisted public anisette does not currently advertise a compatible macOS AuthKit identity; refusing to impersonate macOS");
    }
    Ok(ServerIdentity {
        served: Served::Mac,
        public_mac: true,
        info: AnisetteClientInfo {
            client_info: mac_info.to_owned(),
            user_agent: user_agent.to_owned(),
        },
    })
}

/// Inspect once, before any Apple ID login. Fail closed for our explicit public Mac
/// endpoint; preserve the historical Android fallback for other servers.
pub async fn inspect_server(url: &str) -> Result<ServerIdentity, Report> {
    let reply = async {
        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build().ok()?
            .get(format!("{}/v3/client_info", url.trim_end_matches('/')))
            .timeout(std::time::Duration::from_secs(20))
            .send().await.ok()?
            .error_for_status().ok()?;
        response.json::<serde_json::Value>().await.ok()
    }.await;
    if is_allowlisted_mac_anisette(url) {
        let reply = reply.ok_or_else(|| report!("Public Mac anisette metadata is unavailable: check its HTTPS URL or try later, without signing in"))?;
        inspected_identity(url, &reply)
    } else {
        Ok(reply.as_ref()
            .map(|reply| ServerIdentity::conventional(Served::from_client_info_reply(reply)))
            .unwrap_or_else(|| ServerIdentity::conventional(Served::Android)))
    }
}

/// Wrap the anisette provider with the accurately matched claim.
pub struct ClaimedPlatform<P> {
    inner: P,
    identity: ServerIdentity,
}

impl<P> ClaimedPlatform<P> {
    pub fn new(inner: P, identity: ServerIdentity) -> Self {
        ClaimedPlatform { inner, identity }
    }
}

#[async_trait::async_trait]
impl<P: AnisetteProvider + Send + Sync> AnisetteProvider for ClaimedPlatform<P> {
    async fn get_anisette_data(&self) -> Result<AnisetteDataResponse, Report> {
        self.inner.get_anisette_data().await
    }

    async fn resync(&self, sim: &str) -> Result<AnisetteDataResponse, Report> {
        self.inner.resync(sim).await
    }

    async fn get_client_info(&self) -> Result<AnisetteClientInfo, Report> {
        Ok(self.identity.info.clone())
    }

    async fn provision(&mut self, gs: &Arc<GrandSlam>, force_recheck: bool) -> Result<(), Report> {
        self.inner.provision(gs, force_recheck).await
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

    #[test]
    fn only_an_exact_https_origin_is_allowlisted() {
        for valid in ["https://mac-ani.jkcoxson.com", "https://mac-ani.jkcoxson.com/", "https://MAC-ANI.JKCOXSON.COM:443/"] {
            assert!(is_allowlisted_mac_anisette(valid), "{valid}");
        }
        for invalid in ["http://mac-ani.jkcoxson.com", "https://mac-ani.jkcoxson.com.evil.test", "https://mac-ani.jkcoxson.com:444/", "https://mac-ani.jkcoxson.com/redirect", "https://mac-ani.jkcoxson.com/?a=1", "https://user@mac-ani.jkcoxson.com", "https://mac-ani.jkcoxson.com@evil.test"] {
            assert!(!is_allowlisted_mac_anisette(invalid), "{invalid}");
        }
    }

    #[test]
    fn public_mac_must_advertise_macos_authkit_and_use_matching_headers() {
        let reply = serde_json::json!({
            "client_info": "<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.akd/1.0)>",
            "user_agent": "akd/1.0 CFNetwork/811.5.4 Darwin/16.7.0"
        });
        let identity = inspected_identity("https://mac-ani.jkcoxson.com", &reply).unwrap();
        assert_eq!(identity.served, Served::Mac);
        assert!(identity.public_mac);
        assert_eq!(identity.info.client_info, reply["client_info"].as_str().unwrap());
        assert_eq!(identity.info.user_agent, reply["user_agent"].as_str().unwrap());
        assert_eq!(inspected_identity("https://untrusted.example", &reply).unwrap().served, Served::Android);
    }

    #[test]
    fn public_mac_rejects_misleading_android_or_missing_metadata() {
        for reply in [
            serde_json::json!({"client_info": "<Pixel 8><Android; 14; abc>", "user_agent": "akd/1.0"}),
            serde_json::json!({"client_info": "<MacBookPro> <Mac OS X;10> <com.apple.akd/1>", "user_agent": "akd/1.0", "apparition_anisette": "android"}),
            serde_json::json!({"client_info": "<MacBookPro> <Mac OS X;10> <com.apple.akd/1>"})
        ] {
            assert!(inspected_identity("https://mac-ani.jkcoxson.com", &reply).is_err());
        }
    }
}
