//! `config.json`, read exactly as the Node store reads it, so a Mac switches without migrating.
//! Unknown keys are kept out of the model but never rejected: the Node server's own options
//! (and ones from newer versions) must not stop this one from starting.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub port: Option<u16>,
    /// One prefix or a list; the Node store accepts both.
    #[serde(default, deserialize_with = "one_or_many")]
    pub bundle_prefix: Vec<String>,
    pub password_hash: Option<String>,
    #[serde(default)]
    pub web_ui: bool,
    #[serde(default)]
    pub repos: Vec<Repo>,
    pub branding: Option<Branding>,
}

/// `{ org, title, icon }`: `title` is the full name, else "<org>'s Apparition Store"; `icon` is
/// an image path relative to config.json, or any other text shown as is (an emoji, a letter).
#[derive(Debug, Default, Deserialize)]
pub struct Branding {
    pub org: Option<String>,
    pub title: Option<String>,
    pub icon: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<PlatformRow>,
    #[serde(default)]
    pub clone_exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformRow {
    pub platform: String,
    pub bundle_id: String,
    pub device: Option<String>,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Either::deserialize(d)? {
        Either::One(s) => vec![s],
        Either::Many(v) => v,
    })
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The Node store's rule: the web pages are on unless hosted; hosted they need both the switch
    /// and a password.
    pub fn web_ui_on(&self, hosted: bool) -> bool {
        !hosted || (self.web_ui && self.password_hash.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_example_config_the_node_store_ships() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../store/config.example.json");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.port, Some(8484));
        assert_eq!(config.bundle_prefix, ["com.example"]);
        let repo = &config.repos[0];
        assert_eq!(repo.name, "MyApp");
        assert_eq!(repo.platforms.len(), 2);
        assert_eq!(repo.platforms[1].platform, "visionOS");
    }

    #[test]
    fn a_single_bundle_prefix_string_is_a_list() {
        let config: Config = serde_json::from_str(r#"{"bundlePrefix":"com.illixion","somethingNew":1}"#).unwrap();
        assert_eq!(config.bundle_prefix, ["com.illixion"]);
    }

    #[test]
    fn hosted_web_pages_need_the_switch_and_a_password() {
        let mut config = Config::default();
        assert!(config.web_ui_on(false));
        assert!(!config.web_ui_on(true));
        config.web_ui = true;
        assert!(!config.web_ui_on(true));
        config.password_hash = Some("scrypt$...".into());
        assert!(config.web_ui_on(true));
    }
}
