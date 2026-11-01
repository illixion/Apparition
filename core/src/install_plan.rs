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
    /// A watchOS app in `Watch/`, which iOS copies to the paired Watch. Its own extensions are
    /// in `nested`, kept or left out with it, each needing an App ID too.
    #[serde(default)]
    pub watch: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub id: String,
    pub name: String,
    pub extensions: Vec<Extension>,
}

impl Inventory {
    /// Every signing frontend uses the same supported-extension and identifier rules.
    pub fn validate(&self, selected: &[String], apple_id: bool) -> Result<(), Report> {
        let mut seen = std::collections::HashSet::new();
        for id in selected {
            let extension = self.extensions.iter().find(|e| &e.id == id)
                .ok_or_else(|| report!("The selected extension is not in this app"))?;
            if !seen.insert(id) { bail!("An extension was selected more than once"); }
            for id in std::iter::once(id).chain(&extension.nested) {
                if !id.strip_prefix(&self.id).is_some_and(|suffix| suffix.starts_with('.')) {
                    bail!("Extension {} does not belong to this app", extension.name);
                }
            }
            if apple_id && extension.extension_kit {
                bail!("ExtensionKit helpers require imported provisioning profiles; deselect them for Apple ID signing");
            }
        }
        Ok(())
    }

    pub fn new_ids(&self, target: &str, selected: &[String], registered: &[String]) -> Result<usize, Report> {
        self.validate(selected, false)?;
        Ok(self.app_ids(target, selected).into_iter().filter(|id| !registered.contains(id)).count())
    }

    /// The App IDs signing registers under `target` with `selected` kept: the app's, each kept
    /// extension's and each kept watch app's own extensions'.
    pub fn app_ids(&self, target: &str, selected: &[String]) -> Vec<String> {
        let suffix = |id: &str| id.strip_prefix(&self.id).map(str::to_owned).unwrap_or_default();
        let mut ids = vec![target.to_owned()];
        for extension in self.extensions.iter().filter(|e| selected.contains(&e.id)) {
            ids.push(format!("{target}{}", suffix(&extension.id)));
            ids.extend(extension.nested.iter().map(|id| format!("{target}{}", suffix(id))));
        }
        ids
    }

    /// Whether the app carries a watchOS app.
    pub fn has_watch_app(&self) -> bool {
        self.extensions.iter().any(|e| e.watch)
    }
}

fn display_name<'a>(info: &'a plist::Dictionary, id: &'a str) -> &'a str {
    info.get("CFBundleDisplayName").or_else(|| info.get("CFBundleName")).and_then(plist::Value::as_string).unwrap_or(id)
}

fn bundle_id(info: &plist::Dictionary, kind: &str) -> Result<String, Report> {
    Ok(info.get("CFBundleIdentifier").and_then(plist::Value::as_string).ok_or_else(|| report!("{kind} has no bundle ID"))?.to_owned())
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
            let id = bundle_id(&info, "Extension")?;
            let name = display_name(&info, &id).to_owned();
            extensions.push(Extension { name, path: format!("{folder}/{}", path.file_name().unwrap().to_string_lossy()), id, extension_kit: folder == "Extensions", watch: false, nested: Vec::new() });
        }
    }
    let watch_dir = app.bundle.bundle_dir.join("Watch");
    if watch_dir.is_dir() {
        for entry in std::fs::read_dir(watch_dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "app") || !path.join("Info.plist").is_file() { continue; }
            let info: plist::Dictionary = plist::from_file(path.join("Info.plist"))?;
            let id = bundle_id(&info, "Watch app")?;
            let name = display_name(&info, &id).to_owned();
            let mut nested = Vec::new();
            if let Ok(plugins) = std::fs::read_dir(path.join("PlugIns")) {
                for plugin in plugins {
                    let plugin = plugin?.path();
                    if plugin.extension().is_none_or(|e| e != "appex") { continue; }
                    nested.push(bundle_id(&plist::from_file(plugin.join("Info.plist"))?, "Watch extension")?);
                }
            }
            nested.sort();
            extensions.push(Extension { name, path: format!("Watch/{}", path.file_name().unwrap().to_string_lossy()), id, extension_kit: false, watch: true, nested });
        }
    }
    extensions.sort_by(|a,b| a.id.cmp(&b.id));
    Ok(Inventory { id: app.main_bundle_id()?, name: app.main_app_name()?, extensions })
}

/// Copies the IPA `src` to `dst` without its watch app (`Payload/X.app/Watch/`), for an install
/// that keeps the Watch free of it. Returns false, writing nothing, when there is none.
pub fn without_watch(src: &Path, dst: &Path) -> Result<bool, Report> {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src)?)?;
    let is_watch = |name: &str| name.split('/').nth(2) == Some("Watch") && name.starts_with("Payload/");
    if !archive.file_names().any(|name| name.is_ok_and(|n| is_watch(&n))) {
        return Ok(false);
    }
    let mut out = zip::ZipWriter::new(std::fs::File::create(dst)?);
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index)?;
        if !entry.name().is_ok_and(|n| is_watch(&n)) {
            out.raw_copy_file(entry)?;
        }
    }
    out.finish()?;
    Ok(true)
}

/// Always starts from the original archive; selections are IDs, never filesystem paths from the UI.
pub fn select(path: &Path, selected: &[String]) -> Result<PathBuf, Report> {
    let inventory = inspect(path)?;
    inventory.validate(selected, false)?;
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
    // An empty Watch/ would still tell iOS there's a watch app.
    let watch = app.bundle.bundle_dir.join("Watch");
    if watch.is_dir() && std::fs::read_dir(&watch)?.next().is_none() {
        std::fs::remove_dir(&watch)?;
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
        for (path, id) in [("Payload/Test.app/Info.plist", "org.test"), ("Payload/Test.app/PlugIns/Share.appex/Info.plist", "org.test.share"), ("Payload/Test.app/PlugIns/Worker.appex/Info.plist", "org.test.worker"), ("Payload/Test.app/Extensions/Helper.appex/Info.plist", "org.test.helper"), ("Payload/Test.app/Watch/Test Watch.app/Info.plist", "org.test.watchkitapp"), ("Payload/Test.app/Watch/Test Watch.app/PlugIns/Complication.appex/Info.plist", "org.test.watchkitapp.complication")] {
            zip.start_file(path, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>Test</string></dict></plist>").as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        let bare = dir.join("bare.ipa");
        assert!(without_watch(&archive, &bare).unwrap());
        let inventory = inspect(&bare).unwrap();
        assert!(!inventory.has_watch_app());
        assert_eq!(inventory.extensions.len(), 3);
        assert!(!without_watch(&bare, &dir.join("again.ipa")).unwrap());
        let inventory = inspect(&archive).unwrap();
        assert_eq!(inventory.extensions.len(), 4);
        let watch = inventory.extensions.iter().find(|e| e.watch).unwrap();
        assert_eq!((watch.id.as_str(), watch.path.as_str()), ("org.test.watchkitapp", "Watch/Test Watch.app"));
        assert_eq!(watch.nested, ["org.test.watchkitapp.complication"]);
        // A kept watch app costs its own App ID and its extension's.
        assert_eq!(inventory.new_ids("pool.id", &["org.test.watchkitapp".into()], &["pool.id".into()]).unwrap(), 2);
        assert!(inventory.extensions.iter().any(|e| e.extension_kit));
        assert!(select(&archive, &["../outside".into()]).is_err());
        assert!(inventory.validate(&["org.test.worker".into(), "org.test.worker".into()], true).is_err());
        assert!(inventory.validate(&["org.test.helper".into()], true).is_err());
        assert!(inventory.validate(&["org.test.helper".into()], false).is_ok());
        assert_eq!(inventory.new_ids("pool.id", &["org.test.worker".into()], &["pool.id".into()]).unwrap(), 1);
        assert_eq!(inventory.new_ids("pool.id", &["org.test.worker".into()], &["pool.id".into(), "pool.id.worker".into()]).unwrap(), 0);
        let app = select(&archive, &["org.test.worker".into()]).unwrap();
        assert!(app.join("PlugIns/Worker.appex").is_dir());
        assert!(!app.join("PlugIns/Share.appex").exists());
        assert!(!app.join("Extensions/Helper.appex").exists());
        assert!(!app.join("Watch").exists());
        // Packing with the original IPA as a compression source must not restore omitted plugins.
        let packed = dir.join("selected.ipa");
        crate::ipa::pack(&app, Some(&archive), &packed).unwrap();
        let mut selected_zip = zip::ZipArchive::new(std::fs::File::open(packed).unwrap()).unwrap();
        assert!(selected_zip.by_name("Payload/Test.app/PlugIns/Worker.appex/Info.plist").is_ok());
        assert!(selected_zip.by_name("Payload/Test.app/PlugIns/Share.appex/Info.plist").is_err());
        assert!(selected_zip.by_name("Payload/Test.app/Extensions/Helper.appex/Info.plist").is_err());
        assert_eq!(inspect(&archive).unwrap().extensions.len(), 4);
        let kept = select(&archive, &["org.test.watchkitapp".into()]).unwrap();
        assert!(kept.join("Watch/Test Watch.app/PlugIns/Complication.appex").is_dir());
        // Delete only our fixture directory and the exact extracted test bundle.
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
