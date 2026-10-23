#[derive(Debug, Clone)]
pub enum DeveloperDeviceType {
    Any,
    Ios,
    Tvos,
    Watchos,
}

impl DeveloperDeviceType {
    pub fn url_segment(&self) -> &'static str {
        match self {
            DeveloperDeviceType::Any => "",
            DeveloperDeviceType::Ios => "ios/",
            DeveloperDeviceType::Tvos => "tvos/",
            DeveloperDeviceType::Watchos => "watchos/",
        }
    }
}

pub fn dev_url(endpoint: &str, device_type: impl Into<Option<DeveloperDeviceType>>) -> String {
    // This legacy service uses its `ios/` namespace for App IDs, application
    // groups and team provisioning profiles, even when the requested app is tvOS.
    // The `tvos/` URL for these operations returns HTTP 404. Let callers that
    // require a device-specific route request it explicitly via device_type.
    let default_type = DeveloperDeviceType::Ios;
    format!(
        "https://developerservices2.apple.com/services/QH65B2/{}{}.action?clientId=XABBG36SBA",
        device_type
            .into()
            .unwrap_or(default_type)
            .url_segment(),
        endpoint,
    )
}
