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
//! So the claim follows the server. Apparition's own says what it serves (`/v3/client_info`
//! carries `apparition_anisette`). Dadoum's anisette-v3-server (`Implementation-Version:
//! anisette-v3-server v2.2.2`; ani.sidestore.io, our selfhost compose) runs the Android libraries
//! but reports a Mac's client info, so it gets the Android claim. Any other server is claimed as
//! it reports itself, as isideload does: jkcoxson's native-c one (ani.jkcoxson.com,
//! ani.iloader.app) reports a Mac and Apple refuses provisioning claiming Android (401). Checked
//! 2026-10-24 with a made-up account, each claim on each: only these choices got past anisette.

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

}

/// What a sign-in through one server claims to be (`X-Mme-Client-Info` and User-Agent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// Android or not, for what Apple does differently then (the Xcode token: `after_sign_in`).
    pub served: Served,
    pub client_info: String,
    pub user_agent: String,
}

impl Claim {
    fn of(served: Served) -> Claim {
        Claim { served, client_info: served.client_info().to_string(), user_agent: USER_AGENT.to_string() }
    }

    /// What to claim once Apple has taken the password, for the Xcode token and the developer
    /// requests made with it, when that differs from the sign-in's claim. Claiming Android, Apple
    /// refuses the Xcode token (-80046, since about 2026-10-22) but issues it claiming a Mac,
    /// whatever made the anisette. Checked 2026-10-24 on one account with
    /// examples/apptoken_probe.rs: an Android-claimed sign-in, then the token request claiming
    /// Android (with or without Xcode's headers, naming Xcode, `svct` Xcode) got -80046 and
    /// claiming a Mac got the token. An iCloud token was issued claiming Android.
    pub fn after_sign_in(&self) -> Option<AnisetteClientInfo> {
        (self.served == Served::Android)
            .then(|| AnisetteClientInfo { client_info: MAC_CLIENT_INFO.to_string(), user_agent: USER_AGENT.to_string() })
    }

    /// The claim for a server's `/v3/client_info` reply and its `Implementation-Version` header.
    fn from_reply(reply: &serde_json::Value, implementation: Option<&str>) -> Claim {
        let text = |key| reply.get(key).and_then(|value| value.as_str()).filter(|text| !text.is_empty() && !text.contains(['\r', '\n']));
        match reply.get("apparition_anisette").and_then(|value| value.as_str()) {
            // Our server on a Mac says which Mac it is: a VM's sign-in is refused claiming another.
            Some("mac") => {
                return match (text("client_info"), text("user_agent")) {
                    (Some(client_info), Some(user_agent)) if !client_info.contains("<Android") => {
                        Claim { served: Served::Mac, client_info: client_info.to_string(), user_agent: user_agent.to_string() }
                    }
                    _ => Claim::of(Served::Mac),
                };
            }
            Some(_) => return Claim::of(Served::Android),
            None => {}
        }
        if implementation.is_some_and(|name| name.starts_with("anisette-v3-server v")) {
            return Claim::of(Served::Android);
        }
        match (text("client_info"), text("user_agent")) {
            (Some(client_info), Some(user_agent)) => Claim {
                served: if client_info.contains("<Android") { Served::Android } else { Served::Mac },
                client_info: client_info.to_string(),
                user_agent: user_agent.to_string(),
            },
            _ => Claim::of(Served::Android),
        }
    }
}

/// The claim for the server at `url`. A server that doesn't answer is taken to be an Android one;
/// the sign-in then fails on the server, not here.
pub async fn claim_for(url: &str) -> Claim {
    let reply = async {
        let response = reqwest::Client::new()
            .get(format!("{}/v3/client_info", url.trim_end_matches('/')))
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .ok()?;
        let implementation = response.headers().get("implementation-version").and_then(|value| value.to_str().ok()).map(str::to_string);
        Some((response.json::<serde_json::Value>().await.ok()?, implementation))
    };
    reply.await.map(|(reply, implementation)| Claim::from_reply(&reply, implementation.as_deref())).unwrap_or(Claim::of(Served::Android))
}

/// `inner`, claiming the device that matches what its server serves.
pub struct ClaimedPlatform<P> {
    inner: P,
    claim: Claim,
}

impl<P> ClaimedPlatform<P> {
    pub fn new(inner: P, claim: Claim) -> Self {
        ClaimedPlatform { inner, claim }
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
        Ok(AnisetteClientInfo { client_info: self.claim.client_info.clone(), user_agent: self.claim.user_agent.clone() })
    }

    async fn provision(&mut self, gs: &Arc<GrandSlam>, force_recheck: bool) -> Result<(), Report> {
        self.inner.provision(gs, force_recheck).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DADOUM: &str = "<MacBookPro13,2> <macOS;13.1;22C65> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
    const NATIVE_C: &str = "<MacBookPro13,3> <Mac OS X;10.12.6;16G29> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";

    #[test]
    fn our_server_says_what_it_serves() {
        let claim = |value| Claim::from_reply(&value, None);
        let vm = "<VirtualMac2,1> <macOS;27.0;26A428> <com.apple.AuthKit/1 (com.apple.akd/1.0)>";
        let mac = claim(json!({ "client_info": vm, "user_agent": "akd/1.0 CFNetwork/3896.100.1.1.1 Darwin/27.0.0", "apparition_anisette": "mac" }));
        assert_eq!((mac.served, mac.client_info.as_str()), (Served::Mac, vm));
        assert_eq!(claim(json!({ "apparition_anisette": "mac" })), Claim::of(Served::Mac));
        assert_eq!(claim(json!({ "client_info": "x", "user_agent": "y", "apparition_anisette": "android" })), Claim::of(Served::Android));
        assert!(ANDROID_CLIENT_INFO.contains("<Android; "));
        assert!(MAC_CLIENT_INFO.contains("<macOS;"));
    }

    #[test]
    fn dadoums_server_runs_the_android_libraries_whatever_it_reports() {
        let reply = json!({ "client_info": DADOUM, "user_agent": "akd/1.0 CFNetwork/808.1.4" });
        assert_eq!(Claim::from_reply(&reply, Some("anisette-v3-server v2.2.2")), Claim::of(Served::Android));
    }

    #[test]
    fn other_servers_are_claimed_as_they_report_themselves() {
        let reply = json!({ "client_info": NATIVE_C, "user_agent": "akd/1.0 CFNetwork/811.5.4 Darwin/16.7.0" });
        let claim = Claim::from_reply(&reply, Some("anisette-v3-server (native-c 0.2.0)"));
        assert_eq!(claim.served, Served::Mac);
        assert_eq!((claim.client_info.as_str(), claim.user_agent.as_str()), (NATIVE_C, "akd/1.0 CFNetwork/811.5.4 Darwin/16.7.0"));
        let android = Claim::from_reply(&json!({ "client_info": "<Pixel 7> <Android;14;x> <…>", "user_agent": "akd/1.0" }), None);
        assert_eq!(android.served, Served::Android);
    }

    #[test]
    fn an_android_sign_in_asks_for_the_xcode_token_as_a_mac() {
        let after = Claim::of(Served::Android).after_sign_in().unwrap();
        assert_eq!((after.client_info.as_str(), after.user_agent.as_str()), (MAC_CLIENT_INFO, USER_AGENT));
        assert!(Claim::of(Served::Mac).after_sign_in().is_none());
        let native_c = Claim::from_reply(&json!({ "client_info": NATIVE_C, "user_agent": "akd/1.0" }), None);
        assert!(native_c.after_sign_in().is_none());
    }

    #[test]
    fn an_incomplete_or_odd_reply_is_taken_as_android() {
        assert_eq!(Claim::from_reply(&json!({ "client_info": NATIVE_C }), None), Claim::of(Served::Android));
        assert_eq!(Claim::from_reply(&json!({ "client_info": "a\r\nX-Evil: 1", "user_agent": "b" }), None), Claim::of(Served::Android));
    }
}
