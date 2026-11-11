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

fn utc_iso8601_now() -> String {
    let secs = SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yr = if mo <= 2 { y + 1 } else { y };
    format!("{yr:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// Some headers don't seem to be required. I guess not including them is technically more efficient soooo
impl AnisetteData {
    pub fn get_headers(&self) -> HashMap<String, String> {
        let client_time = utc_iso8601_now();

        let mut headers = self.extra.clone();
        headers.extend([
            ("X-Apple-I-MD-RINFO".to_string(), self.routing_info.clone()),
            (
                "X-Mme-Device-Id".to_string(),
                self.device_unique_identifier.clone(),
            ),
            ("X-Apple-I-MD".to_string(), self.one_time_password.clone()),
            ("X-Apple-I-MD-M".to_string(), self.machine_id.clone()),
        ]);
        headers
            .entry("X-Apple-I-Client-Time".to_string())
            .or_insert(client_time);
        headers
            .entry("X-Apple-I-TimeZone".to_string())
            .or_insert_with(|| "UTC".to_string());
        headers
            .entry("X-Apple-Locale".to_string())
            .or_insert_with(|| "en_US".to_string());
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
        headers
    }

    pub fn validate(&self) -> Result<(), Report> {
        let headers = self.get_headers();
        const CRITICAL_HEADERS: &[&str] = &[
            "X-Apple-I-MD",
            "X-Apple-I-MD-M",
            "X-Apple-I-MD-RINFO",
            "X-Apple-I-MD-LU",
            "X-Mme-Device-Id",
            "X-Apple-I-Client-Time",
            "X-Apple-Locale",
            "X-Apple-I-TimeZone",
        ];
        for &key in CRITICAL_HEADERS {
            match headers.get(key) {
                Some(val) if !val.trim().is_empty() => {}
                _ => {
                    tracing::error!(header = key, "Critical Anisette header is missing or empty");
                    bail!("Critical Anisette header is missing or empty: {}", key);
                }
            }
        }
        if let Some(srl) = headers.get("X-Apple-I-SRL-NO")
            && srl.trim().is_empty()
        {
            tracing::error!(header = "X-Apple-I-SRL-NO", "Anisette header X-Apple-I-SRL-NO is empty");
            bail!("Critical Anisette header is missing or empty: X-Apple-I-SRL-NO");
        }
        Ok(())
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
        data.validate()?;
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
        data.validate()?;
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
        let data = sample(&[]);
        assert!(data.validate().is_ok());
        let cpd = data.get_client_provided_data();
        // The anchor Apple's Windows client sends and we used to omit entirely.
        assert_eq!(get(&cpd, "X-Mme-Legacy-Device-Id"), Some("LEGACY-HEX"));
        assert_eq!(get(&cpd, "X-Apple-I-MD-LU"), Some("MDLU-HEX"));
        // And the fields we already sent are still there.
        assert_eq!(get(&cpd, "X-Mme-Device-Id"), Some("DEVICE-UUID"));
        assert_eq!(get(&cpd, "X-Apple-I-MD"), Some("OTP"));
        assert_eq!(get(&cpd, "X-Apple-I-MD-M"), Some("MID"));
        assert_eq!(get(&cpd, "X-Apple-I-MD-RINFO"), Some("17106176"));
        assert!(get(&cpd, "X-Apple-I-Client-Time").is_some());
        assert_eq!(get(&cpd, "X-Apple-I-TimeZone"), Some("UTC"));
        assert_eq!(get(&cpd, "X-Apple-Locale"), Some("en_US"));
        // We deliberately never fabricate a Mac serial.
        assert!(cpd.get("X-Apple-I-SRL-NO").is_none());
        assert!(cpd.get("X-Apple-I-MLB").is_none());
        assert!(cpd.get("X-Apple-I-ROM").is_none());
    }

    #[test]
    fn validate_rejects_missing_or_empty_critical_headers() {
        let mut bad = sample(&[]);
        bad.one_time_password = String::new();
        assert!(bad.validate().is_err());
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
