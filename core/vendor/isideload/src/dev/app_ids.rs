use crate::{
    dev::{
        developer_session::DeveloperSession,
        device_type::{DeveloperDeviceType, dev_url},
        normalize_app_names,
        teams::DeveloperTeam,
    },
    util::plist::{PlistDataExtract, SensitivePlistAttachment},
};
use plist::{Data, Date, Dictionary, Value};
use plist_macro::plist;
use reqwest::header::HeaderValue;
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppId {
    pub app_id_id: String,
    pub identifier: String,
    pub name: String,
    pub features: Dictionary,
    pub expiration_date: Option<Date>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAppIdsResponse {
    pub app_ids: Vec<AppId>,
    pub max_quantity: Option<u64>,
    pub available_quantity: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub encoded_profile: Data,
    pub filename: String,
    pub provisioning_profile_id: String,
    pub name: String,
    pub status: String,
    pub r#type: String,
    pub distribution_method: String,
    pub pro_pro_platorm: Option<String>,
    #[serde(rename = "UUID")]
    pub uuid: String,
    pub date_expire: Date,
    pub managing_app: Option<String>,
    pub app_id_id: String,
    pub is_template_profile: bool,
    pub is_team_profile: Option<bool>,
    pub is_free_provisioning_profile: Option<bool>,
}

#[cfg_attr(feature = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait::async_trait)]
pub trait AppIdsApi {
    fn developer_session(&mut self) -> &mut DeveloperSession;

    async fn add_app_id(
        &mut self,
        team: &DeveloperTeam,
        name: &str,
        identifier: &str,
        device_type: impl Into<Option<DeveloperDeviceType>> + Send,
    ) -> Result<AppId, Report> {
        let body = plist!(dict {
            "teamId": &team.team_id,
            "identifier": identifier,
            "name": normalize_app_names(name),
        });

        let app_id: AppId = self
            .developer_session()
            .send_dev_request(&dev_url("addAppId", device_type), body, "appId")
            .await
            .context("Failed to add developer app ID")?;

        Ok(app_id)
    }

    async fn list_app_ids(
        &mut self,
        team: &DeveloperTeam,
        device_type: impl Into<Option<DeveloperDeviceType>> + Send,
    ) -> Result<ListAppIdsResponse, Report> {
        let body = plist!(dict {
            "teamId": &team.team_id,
        });

        let response: Value = self
            .developer_session()
            .send_dev_request_no_response(&dev_url("listAppIds", device_type), body)
            .await
            .context("Failed to list developer app IDs")?
            .into();

        let app_ids: ListAppIdsResponse = plist::from_value(&response).map_err(|e| {
            report!("Failed to deserialize app id response: {:?}", e).attach(
                SensitivePlistAttachment::new(
                    response
                        .as_dictionary()
                        .unwrap_or(&Dictionary::new())
                        .clone(),
                ),
            )
        })?;

        Ok(app_ids)
    }

    async fn update_app_id(
        &mut self,
        team: &DeveloperTeam,
        app_id: &AppId,
        features: Dictionary,
        device_type: impl Into<Option<DeveloperDeviceType>> + Send,
    ) -> Result<AppId, Report> {
        let mut body = plist!(dict {
            "teamId": &team.team_id,
            "appIdId": &app_id.app_id_id
        });

        for (key, value) in features {
            body.insert(key.clone(), value.clone());
        }

        Ok(self
            .developer_session()
            .send_dev_request(&dev_url("updateAppId", device_type), body, "appId")
            .await
            .context("Failed to update developer app ID")?)
    }

    async fn delete_app_id(
        &mut self,
        team: &DeveloperTeam,
        app_id_id: &str,
        device_type: impl Into<Option<DeveloperDeviceType>> + Send,
    ) -> Result<(), Report> {
        let body = plist!(dict {
            "teamId": &team.team_id,
            "appIdId": app_id_id,
        });

        self.developer_session()
            .send_dev_request_no_response(&dev_url("deleteAppId", device_type), body)
            .await
            .context("Failed to delete developer app ID")?;

        Ok(())
    }

    async fn download_team_provisioning_profile(
        &mut self,
        team: &DeveloperTeam,
        app_id: &AppId,
        device_type: impl Into<Option<DeveloperDeviceType>> + Send,
    ) -> Result<Profile, Report> {
        let requested_platform = device_type.into();
        let tvos = matches!(requested_platform.as_ref(), Some(DeveloperDeviceType::Tvos));
        let mut body = plist!(dict {
            "teamId": &team.team_id,
            "appIdId": &app_id.app_id_id,
        });
        // Apple's legacy /ios/ endpoint uses a subPlatform field to select
        // a tvOS provisioning profile; switching to /tvos/ returns HTTP 404.
        if tvos {
            body.insert("subPlatform".into(), Value::String("tvOS".into()));
        }

        let response: Profile = self
            .developer_session()
            .send_dev_request(
                &dev_url(
                    "downloadTeamProvisioningProfile",
                    if tvos {
                        Some(DeveloperDeviceType::Ios)
                    } else {
                        requested_platform
                    },
                ),
                body,
                "provisioningProfile",
            )
            .await
            .context("Failed to download provisioning profile")?;

        Ok(response)
    }

    /// Turns on `capabilities` (the portal's IDs, such as `INCREASED_MEMORY_LIMIT`) for the App
    /// ID in one request, through the JSON API that has capabilities the plist API's feature
    /// codes don't. Apple's reply is kept in the error, since it names what it refused.
    async fn enable_capabilities(
        &mut self,
        team: &DeveloperTeam,
        app_id: &AppId,
        capabilities: &[String],
    ) -> Result<(), Report> {
        let body = serde_json::json!({ "data": {
            "type": "bundleIds",
            "id": app_id.app_id_id,
            "attributes": {
                "hasExclusiveManagedCapabilities": false,
                "teamId": team.team_id,
                "bundleType": "bundle",
                "identifier": app_id.identifier,
                "seedId": team.team_id,
                "name": app_id.name,
            },
            "relationships": { "bundleIdCapabilities": { "data": capabilities.iter().map(|id| serde_json::json!({
                "type": "bundleIdCapabilities",
                "attributes": { "enabled": true, "settings": [] },
                "relationships": { "capability": { "data": { "type": "capabilities", "id": id } } },
            })).collect::<Vec<_>>() } },
        } });
        let dev_session = self.developer_session();
        let headers = json_api_headers(dev_session).await?;
        let response = dev_session
            .get_grandslam_client()
            .patch(&format!("{BUNDLE_IDS_URL}/{}", app_id.app_id_id))?
            .headers(headers)
            .body(body.to_string())
            .send()
            .await
            .context("Failed to request capabilities")?;
        let status = response.status();
        if !status.is_success() {
            let reply = response.text().await.unwrap_or_default();
            // JSON:API errors: the `detail` says why ("…not eligible for this feature").
            let details: Vec<String> = serde_json::from_str::<serde_json::Value>(&reply)
                .ok()
                .and_then(|r| r["errors"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|e| e["detail"].as_str().map(|d| d.split_whitespace().collect::<Vec<_>>().join(" ")))
                .collect();
            let why = if details.is_empty() { reply } else { details.join(" ") };
            bail!("Apple refused {} ({status}): {why}", capabilities.join(", "));
        }
        Ok(())
    }
}

const BUNDLE_IDS_URL: &str = "https://developerservices2.apple.com/services/v1/bundleIds";

async fn json_api_headers(dev_session: &mut DeveloperSession) -> Result<reqwest::header::HeaderMap, Report> {
    let mut headers = dev_session
        .get_headers()
        .await
        .context("Failed to get anisette headers")?;
    headers.insert("Content-Type", HeaderValue::from_static("application/vnd.api+json"));
    headers.insert("Accept", HeaderValue::from_static("application/vnd.api+json"));
    Ok(headers)
}

impl AppIdsApi for DeveloperSession {
    fn developer_session(&mut self) -> &mut DeveloperSession {
        self
    }
}

impl AppId {
    pub async fn ensure_group_feature(
        &mut self,
        dev_session: &mut DeveloperSession,
        team: &DeveloperTeam,
    ) -> Result<(), Report> {
        let app_group_feature_enabled = self.features.get_bool("APG3427HIY")?;

        if !app_group_feature_enabled {
            let body = plist!(dict {
                "APG3427HIY": true,
            });
            let new_features = dev_session
                .update_app_id(team, self, body, None)
                .await?
                .features;
            self.features = new_features;
        }

        Ok(())
    }
}
