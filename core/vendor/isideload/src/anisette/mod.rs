pub mod remote_v3;

use crate::auth::grandslam::GrandSlam;
use plist::Dictionary;
use plist_macro::plist;
use reqwest::header::HeaderMap;
use rootcause::prelude::*;
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use tracing::warn;
use web_time::SystemTime;

#[derive(Deserialize, Debug, Clone)]
pub struct AnisetteClientInfo {
    pub client_info: String,
    pub user_agent: String,
}

#[derive(Debug, Clone)]
pub struct AnisetteData {
    machine_id: String,
    one_time_password: String,
    pub routing_info: String,
    _device_description: String,
    device_unique_identifier: String,
    local_user_id: String,
    /// Stable per-install stand-in for the Windows `X-Mme-Legacy-Device-Id` hardware anchor,
    /// derived from our own seed (collision-safe; never a real serial). See `state.rs`.
    legacy_device_id: String,
    /// Headers beyond these that the server gave (see remote_v3's `extra`).
    pub extra: HashMap<String, String>,
    generated_at: SystemTime,
}

// Some headers don't seem to be required. I guess not including them is technically more efficient soooo
impl AnisetteData {
    pub fn get_headers(&self) -> HashMap<String, String> {
        //let dt: DateTime<Utc> = Utc::now().round_subsecs(0);

        let mut headers = self.extra.clone();
        // Routing info is something every provider must give (remote_v3 refuses an answer without
        // it), and both genuine clients we have captures of, Apple's macOS and Windows ones, send
        // it in every sign-in request. Upstream sent it only when the provider also gave other
        // headers, so a plain server's sign-ins went without it.
        if !self.routing_info.trim().is_empty() {
            headers.insert("X-Apple-I-MD-RINFO".to_string(), self.routing_info.clone());
        }
        headers.extend([
            // ("X-Apple-I-SRL-NO".to_string(), serial),
            // ("X-Apple-I-TimeZone".to_string(), "UTC".to_string()),
            // ("X-Apple-Locale".to_string(), "en_US".to_string()),
            // ("X-Apple-I-MD-LU".to_string(), self.local_user_id.clone()),
            (
                "X-Mme-Device-Id".to_string(),
                self.device_unique_identifier.clone(),
            ),
            ("X-Apple-I-MD".to_string(), self.one_time_password.clone()),
            ("X-Apple-I-MD-M".to_string(), self.machine_id.clone()),
            // (
            //     "X-Mme-Client-Info".to_string(),
            //     self.device_description.clone(),
            // ),
        ]);
        // Identity fields Apple's own Windows client (AuthKitWin) always sends but Apparition
        // historically omitted: the local-user hash and the local-derived hardware anchor. Apple
        // appears to have begun requiring a hardware identity (Oct 2026). Fill only if the provider
        // didn't already supply them (a native/macOS provider might), so a real value isn't
        // clobbered by our derived one.
        headers
            .entry("X-Apple-I-MD-LU".to_string())
            .or_insert_with(|| self.local_user_id.clone());
        headers
            .entry("X-Mme-Legacy-Device-Id".to_string())
            .or_insert_with(|| self.legacy_device_id.clone());
        // The time of the request, which is what Apple's clients put here (second precision, UTC).
        // A provider that already gave one (the anisette VM's guest does) keeps its own. Taken
        // when the headers are built, not when the provider answered, so a cached answer isn't
        // minutes old by the time it goes out.
        headers
            .entry("X-Apple-I-Client-Time".to_string())
            .or_insert_with(|| client_time(web_time::SystemTime::now()));
        headers
    }

    pub fn get_header_map(&self) -> Result<HeaderMap, Report> {
        let headers_map = self.get_headers();
        let mut header_map = HeaderMap::new();

        for (key, value) in headers_map {
            header_map.insert(
                reqwest::header::HeaderName::from_bytes(key.as_bytes())?,
                reqwest::header::HeaderValue::from_str(&value)?,
            );
        }

        Ok(header_map)
    }

    pub fn get_client_provided_data(&self) -> Dictionary {
        let headers = self.get_headers();

        let mut cpd = plist!(dict {
            "bootstrap": "true",
            "icscrec": "true",
            "loc": "en_US",
            "pbe": "false",
            "prkgen": "true",
            "svct": "iCloud"
        });

        for (key, value) in headers {
            cpd.insert(key.to_string(), plist::Value::String(value));
        }

        cpd
    }

    pub fn needs_refresh(&self) -> bool {
        let elapsed = self.generated_at.elapsed();
        match elapsed {
            Ok(elapsed) => elapsed.as_secs() > 60,
            Err(_) => {
                warn!("Unable to determine anisette data age, treating as expired");
                true
            }
        }
    }
}

/// `2026-11-12T09:30:05Z`: an instant as Apple's clients write `X-Apple-I-Client-Time`.
fn client_time(at: SystemTime) -> String {
    let seconds = at.duration_since(web_time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0) as i64;
    let (days, of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days since 1970-01-01 to a calendar date (Hinnant's civil_from_days).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", of_day / 3_600, of_day % 3_600 / 60, of_day % 60)
}

#[derive(Debug, Clone)]
pub enum AnisetteDataResponse {
    Data(AnisetteData),
    NeedsProvisioning,
    IncorrectlyProvisioned,
}

#[cfg_attr(feature = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait::async_trait)]
pub trait AnisetteProvider {
    async fn get_anisette_data(&self) -> Result<AnisetteDataResponse, Report>;

    async fn resync(&self, _sim: &str) -> Result<AnisetteDataResponse, Report> {
        Err(report!("anisette resync not supported by this provider"))
    }

    async fn get_client_info(&self) -> Result<AnisetteClientInfo, Report>;

    async fn provision(&mut self, gs: &Arc<GrandSlam>, force_recheck: bool) -> Result<(), Report>;
}

#[derive(Clone)]
pub struct AnisetteDataGenerator {
    provider: Arc<RwLock<dyn AnisetteProvider + Send + Sync>>,
    data: Option<Arc<AnisetteData>>,
}

impl AnisetteDataGenerator {
    pub fn new(provider: Arc<RwLock<dyn AnisetteProvider + Send + Sync>>) -> Self {
        AnisetteDataGenerator {
            provider,
            data: None,
        }
    }

    pub async fn get_anisette_data(
        &mut self,
        gs: Arc<GrandSlam>,
    ) -> Result<Arc<AnisetteData>, Report> {
        if let Some(data) = &self.data
            && !data.needs_refresh()
        {
            return Ok(data.clone());
        }

        // 3 attempts for the worst case: load anisette from storage, server says -45061 -> reprovision anisette -> get data
        let data = self.get_data_or_provision(&gs, 3).await?;
        let arc_data = Arc::new(data);
        self.data = Some(arc_data.clone());
        Ok(arc_data)
    }

    async fn get_data_or_provision(
        &mut self,
        gs: &Arc<GrandSlam>,
        max_iters: usize,
    ) -> Result<AnisetteData, Report> {
        if max_iters == 0 {
            bail!("Max anisette provisioning attempts reached");
        }
        // trying to avoid locking as write unless necessary for concurrency later on
        let provider = self.provider.read().await;
        match provider.get_anisette_data().await? {
            AnisetteDataResponse::Data(data) => Ok(data),
            status @ (AnisetteDataResponse::NeedsProvisioning
            | AnisetteDataResponse::IncorrectlyProvisioned) => {
                drop(provider);
                let mut provider_write = self.provider.write().await;
                provider_write
                    .provision(
                        gs,
                        matches!(status, AnisetteDataResponse::IncorrectlyProvisioned),
                    )
                    .await?;
                drop(provider_write);
                Box::pin(self.get_data_or_provision(gs, max_iters - 1)).await
            }
        }
    }

    pub async fn get_client_info(&self) -> Result<AnisetteClientInfo, Report> {
        let provider = self.provider.read().await;
        provider.get_client_info().await
    }

    /// Perform a login anisette resync with the server, returning corrected data.
    pub async fn resync(&mut self, sim: &str) -> Result<Arc<AnisetteData>, Report> {
        let provider = self.provider.read().await;
        let data = match provider.resync(sim).await? {
            AnisetteDataResponse::Data(data) => data,
            _ => {
                bail!("Anisette provider failed to provide data for resync, needs provisioning");
            }
        };
        let arc_data = Arc::new(data);
        self.data = Some(arc_data.clone());
        Ok(arc_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(extra: &[(&str, &str)]) -> AnisetteData {
        AnisetteData {
            machine_id: "MID".into(),
            one_time_password: "OTP".into(),
            routing_info: "17106176".into(),
            _device_description: "desc".into(),
            device_unique_identifier: "DEVICE-UUID".into(),
            local_user_id: "MDLU-HEX".into(),
            legacy_device_id: "LEGACY-HEX".into(),
            extra: extra.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            generated_at: SystemTime::now(),
        }
    }

    fn get<'a>(cpd: &'a Dictionary, key: &str) -> Option<&'a str> {
        cpd.get(key).and_then(|v| v.as_string())
    }

    #[test]
    fn cpd_carries_the_identity_fields_apple_wants() {
        let cpd = sample(&[]).get_client_provided_data();
        // The anchor Apple's Windows client sends and we used to omit entirely.
        assert_eq!(get(&cpd, "X-Mme-Legacy-Device-Id"), Some("LEGACY-HEX"));
        assert_eq!(get(&cpd, "X-Apple-I-MD-LU"), Some("MDLU-HEX"));
        // And the fields we already sent are still there.
        assert_eq!(get(&cpd, "X-Mme-Device-Id"), Some("DEVICE-UUID"));
        assert_eq!(get(&cpd, "X-Apple-I-MD"), Some("OTP"));
        assert_eq!(get(&cpd, "X-Apple-I-MD-M"), Some("MID"));
        // We deliberately never fabricate a Mac serial.
        assert!(cpd.get("X-Apple-I-SRL-NO").is_none());
        assert!(cpd.get("X-Apple-I-MLB").is_none());
        assert!(cpd.get("X-Apple-I-ROM").is_none());
    }

    #[test]
    fn routing_info_and_the_request_time_are_sent_whatever_the_provider_adds() {
        // A plain server gives the three required headers and nothing else, so `extra` is empty:
        // both genuine clients still send these two.
        let cpd = sample(&[]).get_client_provided_data();
        assert_eq!(get(&cpd, "X-Apple-I-MD-RINFO"), Some("17106176"));
        let time = get(&cpd, "X-Apple-I-Client-Time").expect("a request time");
        assert_eq!(time.len(), 20, "{time}");
        assert!(time.ends_with('Z') && time.as_bytes()[10] == b'T', "{time}");
        // Not the others the genuine clients carry that nothing here can honestly supply.
        for absent in ["X-Apple-I-SRL-NO", "X-Apple-I-TimeZone", "X-Apple-Locale"] {
            assert!(cpd.get(absent).is_none(), "{absent}");
        }
    }

    #[test]
    fn a_provider_that_gave_a_request_time_keeps_it() {
        let cpd = sample(&[("X-Apple-I-Client-Time", "2026-01-02T03:04:05Z"), ("X-Apple-I-TimeZone", "UTC")]).get_client_provided_data();
        assert_eq!(get(&cpd, "X-Apple-I-Client-Time"), Some("2026-01-02T03:04:05Z"));
        assert_eq!(get(&cpd, "X-Apple-I-TimeZone"), Some("UTC"));
    }

    #[test]
    fn the_request_time_is_written_the_way_apples_clients_write_it() {
        let at = |seconds: u64| client_time(web_time::UNIX_EPOCH + std::time::Duration::from_secs(seconds));
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(1_700_000_000), "2023-11-14T22:13:20Z");
        // Leap days, in a year divisible by four and in one divisible by 400.
        assert_eq!(at(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_794_475_805), "2026-11-12T09:30:05Z");
        assert_eq!(at(4_102_444_799), "2099-12-31T23:59:59Z");
    }

    #[test]
    fn a_providers_own_identity_values_are_not_clobbered() {
        // A native/macOS provider that already supplies these keeps its real values.
        let cpd = sample(&[("X-Apple-I-MD-LU", "REAL-LU"), ("X-Mme-Legacy-Device-Id", "REAL-LEGACY")])
            .get_client_provided_data();
        assert_eq!(get(&cpd, "X-Apple-I-MD-LU"), Some("REAL-LU"));
        assert_eq!(get(&cpd, "X-Mme-Legacy-Device-Id"), Some("REAL-LEGACY"));
    }
}
