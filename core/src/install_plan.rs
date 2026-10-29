//! Inventory and filter an IPA before any portal registrations are made.
use std::path::{Path, PathBuf};
use isideload::sideload::application::Application;
use rootcause::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    pub id: String,
    pub name: String,
    pub path: String,
    pub extension_kit: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub id: String,
    pub name: String,
    pub extensions: Vec<Extension>,
}

pub fn inspect(path: &Path) -> Result<Inventory, Report> {
    let app = Application::new(path.to_path_buf())?;
    let mut extensions = Vec::new();
    for folder in ["PlugIns", "Extensions"] {
        let dir = app.bundle.bundle_dir.join(folder);
        if !dir.is_dir() { continue; }
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "appex") { continue; }
            let info: plist::Dictionary = plist::from_file(path.join("Info.plist"))?;
            let id = info.get("CFBundleIdentifier").and_then(plist::Value::as_string).ok_or_else(|| report!("Extension has no bundle ID"))?;
            let name = info.get("CFBundleDisplayName").or_else(|| info.get("CFBundleName")).and_then(plist::Value::as_string).unwrap_or(id);
            extensions.push(Extension { id: id.into(), name: name.into(), path: format!("{folder}/{}", path.file_name().unwrap().to_string_lossy()), extension_kit: folder == "Extensions" });
        }
    }
    extensions.sort_by(|a,b| a.id.cmp(&b.id));
    Ok(Inventory { id: app.main_bundle_id()?, name: app.main_app_name()?, extensions })
}

/// Always starts from the original archive; selections are IDs, never filesystem paths from the UI.
pub fn select(path: &Path, selected: &[String]) -> Result<PathBuf, Report> {
    let inventory = inspect(path)?;
    if selected.iter().any(|id| !inventory.extensions.iter().any(|e| &e.id == id)) {
        bail!("The selected extension is not in this app");
    }
    let app = Application::new(path.to_path_buf())?;
    let keep_extension_kit = inventory.extensions.iter().any(|e| e.extension_kit && selected.contains(&e.id));
    for extension in inventory.extensions {
        if !selected.contains(&extension.id) {
            std::fs::remove_dir_all(app.bundle.bundle_dir.join(extension.path))?;
        }
    }
    if !keep_extension_kit && app.bundle.bundle_dir.join("Extensions").is_dir() {
        std::fs::remove_dir_all(app.bundle.bundle_dir.join("Extensions"))?;
    }
    Ok(app.bundle.bundle_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn selections_remove_plugins_before_signing_and_leave_archive_intact() {
        let dir = std::env::temp_dir().join(format!("apparition-selection-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("fixture.ipa");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        for (path, id) in [("Payload/Test.app/Info.plist", "org.test"), ("Payload/Test.app/PlugIns/Share.appex/Info.plist", "org.test.share"), ("Payload/Test.app/PlugIns/Worker.appex/Info.plist", "org.test.worker"), ("Payload/Test.app/Extensions/Helper.appex/Info.plist", "org.test.helper")] {
            zip.start_file(path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>Test</string></dict></plist>").as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let inventory = inspect(&archive).unwrap();
        assert_eq!(inventory.extensions.len(), 3);
        assert!(inventory.extensions.iter().any(|e| e.extension_kit));
        assert!(select(&archive, &["../outside".into()]).is_err());
        let app = select(&archive, &["org.test.worker".into()]).unwrap();
        assert!(app.join("PlugIns/Worker.appex").is_dir());
        assert!(!app.join("PlugIns/Share.appex").exists());
        assert!(!app.join("Extensions/Helper.appex").exists());
        assert_eq!(inspect(&archive).unwrap().extensions.len(), 3);
        // Delete only our fixture directory and the exact extracted test bundle.
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
