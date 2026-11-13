//! "Report a problem": the desktop app's bug form on GitHub, opened with what the app already
//! knows filled in. The field ids are the ones in `.github/ISSUE_TEMPLATE/desktop_bug.yml`
//! (`where`, `version`, `os`); the `where` text is one of that dropdown's options, word for word.
//! The phone and headset app does the same for `bug_report.yml` (Probe/ProblemReport.swift).

use tauri::AppHandle;
use tauri_plugin_opener::OpenerExt;

use crate::flow::Failure;

const ISSUES: &str = "https://github.com/illixion/Apparition/issues/new";
const TEMPLATE: &str = "desktop_bug.yml";

/// The form's "Where does it happen?" option for this computer.
fn place() -> &'static str {
    if cfg!(target_os = "macos") {
        "Desktop app (macOS)"
    } else if cfg!(target_os = "windows") {
        "Desktop app (Windows)"
    } else {
        "Desktop app (Linux)"
    }
}

/// The form with the three fields filled in. Nothing else about this computer goes in the link:
/// no name, address or account.
fn issue_url(place: &str, version: &str, system: &str) -> String {
    tauri::Url::parse_with_params(ISSUES, [("template", TEMPLATE), ("where", place), ("version", version), ("os", system)])
        .map_or_else(|_| ISSUES.to_string(), Into::into)
}

/// This computer's OS and architecture the way a person would say it: "Windows 11 (build
/// 26100.2605), arm64".
fn system() -> String {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    };
    format!("{}, {arch}", os_version())
}

#[cfg(target_os = "macos")]
fn os_version() -> String {
    let version = std::process::Command::new("sw_vers").arg("-productVersion").output().ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|version| !version.is_empty());
    version.map_or_else(|| "macOS".to_string(), |version| format!("macOS {version}"))
}

#[cfg(target_os = "windows")]
fn os_version() -> String {
    // ProductName still says "Windows 10" on Windows 11, so the build number decides.
    let Ok(key) = windows_registry::LOCAL_MACHINE.open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion") else {
        return "Windows".to_string();
    };
    let build = key.get_string("CurrentBuild").unwrap_or_default();
    let name = if build.parse::<u32>().is_ok_and(|build| build >= 22000) { "Windows 11" } else { "Windows 10" };
    match (build.is_empty(), key.get_u32("UBR")) {
        (true, _) => name.to_string(),
        (false, Ok(revision)) => format!("{name} (build {build}.{revision})"),
        (false, Err(_)) => format!("{name} (build {build})"),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_version() -> String {
    std::fs::read_to_string("/etc/os-release").ok()
        .and_then(|text| pretty_name(&text))
        .unwrap_or_else(|| "Linux".to_string())
}

/// `PRETTY_NAME` from an os-release file, without its quotes.
#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
fn pretty_name(text: &str) -> Option<String> {
    let value = text.lines().find_map(|line| line.strip_prefix("PRETTY_NAME="))?;
    let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
    (!value.is_empty()).then(|| value.to_string())
}

#[tauri::command]
pub fn open_issue(app: AppHandle) -> Result<(), Failure> {
    let url = issue_url(place(), env!("CARGO_PKG_VERSION"), &system());
    app.opener().open_url(url, None::<&str>).map_err(|error| Failure::new(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_opens_the_desktop_form_with_its_three_fields() {
        let url = tauri::Url::parse(&issue_url("Desktop app (Windows)", "1.0.0", "Windows 11 (build 26100.2605), arm64")).unwrap();
        assert_eq!(url.path(), "/illixion/Apparition/issues/new");
        let fields: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(fields["template"], "desktop_bug.yml");
        assert_eq!(fields["where"], "Desktop app (Windows)");
        assert_eq!(fields["version"], "1.0.0");
        assert_eq!(fields["os"], "Windows 11 (build 26100.2605), arm64");
        assert_eq!(fields.len(), 4);
    }

    /// The form's dropdown has to offer exactly the text the link sends, or GitHub ignores it.
    #[test]
    fn the_place_is_one_of_the_forms_options() {
        let form = include_str!("../../.github/ISSUE_TEMPLATE/desktop_bug.yml");
        assert!(form.contains(&format!("- {}\n", place())), "{} is not an option of desktop_bug.yml", place());
    }

    #[test]
    fn this_computer_is_described() {
        let system = system();
        assert!(system.contains(", "), "{system}");
        let name = apparition_core::api::hostname();
        assert!(name.is_empty() || !system.contains(&name), "the computer's name is in {system}");
    }

    #[test]
    fn reads_the_pretty_name() {
        assert_eq!(pretty_name("NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\n").as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(pretty_name("NAME=Arch\n"), None);
    }
}
