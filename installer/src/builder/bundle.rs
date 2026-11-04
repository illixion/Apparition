//! What happens to the built app before it is signed: its bundle IDs and build number stamped,
//! whether it links DebugTrace, and for a DebugTrace app its own trace-signing key (and for one
//! with a debug server, the server's token), as `build-and-sign` Step 4.5 to 4.7 do.

use std::{fs, path::{Path, PathBuf}};

use plist::{Dictionary, Value as Plist};
use serde_json::json;

use super::{random_bytes, utc_now};

/// The app's Info.plist: `Info.plist`, or a Mac app's `Contents/Info.plist`.
pub fn info_path(app: &Path) -> PathBuf {
    let mac = app.join("Contents/Info.plist");
    if mac.is_file() { mac } else { app.join("Info.plist") }
}

pub fn read_info(path: &Path) -> Result<Dictionary, String> {
    Plist::from_file(path).ok().and_then(Plist::into_dictionary)
        .ok_or_else(|| format!("{} isn't a readable property list", path.display()))
}

fn write_info(path: &Path, info: &Dictionary) -> Result<(), String> {
    plist::to_file_binary(path, &Plist::Dictionary(info.clone())).map_err(|e| format!("couldn't write {}: {e}", path.display()))
}

pub fn string(info: &Dictionary, key: &str) -> Option<String> {
    info.get(key).and_then(Plist::as_string).map(str::to_string)
}

/// Sets one string in a bundle's Info.plist.
pub fn set_string(plist: &Path, key: &str, value: &str) -> Result<(), String> {
    let mut info = read_info(plist)?;
    info.insert(key.into(), Plist::String(value.into()));
    write_info(plist, &info)
}

/// The first bundle of `extension` directly in `dir`, if any.
fn first_bundle(dir: &Path, extension: &str) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir).ok()?.filter_map(Result::ok).map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().is_some_and(|e| e == extension)).collect();
    found.sort();
    found.into_iter().next()
}

/// The app's one extension (`PlugIns/` or ExtensionKit's `Extensions/`) and its watch app.
pub fn extension(app: &Path) -> Option<PathBuf> {
    ["PlugIns", "Extensions"].iter().find_map(|d| first_bundle(&app.join(d), "appex"))
}

pub fn watch_app(app: &Path) -> Option<PathBuf> {
    first_bundle(&app.join("Watch"), "app")
}

/// Whether the app links DebugTrace, and its debug server: their marker strings in the main
/// binary, an embedded framework, or a debug build's `<name>.debug.dylib`.
pub fn links_debugtrace(app: &Path, info: &Dictionary) -> (bool, bool) {
    let mut candidates = Vec::new();
    if let Some(executable) = string(info, "CFBundleExecutable") {
        candidates.push(app.join(&executable));
        candidates.push(app.join("Contents/MacOS").join(&executable));
    }
    let children = |dir: PathBuf| -> Vec<PathBuf> {
        fs::read_dir(dir).map(|r| r.filter_map(Result::ok).map(|e| e.path()).collect()).unwrap_or_default()
    };
    candidates.extend(children(app.to_path_buf()).into_iter().filter(|p| p.to_string_lossy().ends_with(".debug.dylib")));
    for frameworks in [app.join("Frameworks"), app.join("Contents/Frameworks")] {
        for framework in children(frameworks).into_iter().filter(|p| p.extension().is_some_and(|e| e == "framework")) {
            candidates.extend(children(framework));
        }
    }
    let (mut debugtrace, mut server) = (false, false);
    for candidate in candidates.iter().filter(|c| c.is_file()) {
        let Ok(bytes) = fs::read(candidate) else { continue };
        debugtrace |= contains(&bytes, b"DEBUGTRACE_STDERR");
        server |= contains(&bytes, b"DEBUGTRACE_SERVER");
    }
    (debugtrace, server)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// What a DebugTrace build carries: the trace key's ID, and the debug server's token.
pub struct Credential {
    pub key_id: String,
    pub command_token: Option<String>,
}

pub struct LedgerEntry<'a> {
    pub bundle_id: &'a str,
    pub platform: &'a str,
    pub configuration: &'a str,
    pub project: &'a Path,
    pub branch: &'a str,
    pub device: &'a str,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Step 4.7: a fresh Ed25519 key for this build. The private half goes into
/// `DebugTraceCredential.plist` in the bundle (sealed by the signature that follows), the public
/// half into the key ledger the Store verifies uploads against (one JSON file per key, mode
/// 600). With a debug server, a per-build `CommandToken` too, in both. Neither is printed.
pub fn embed_credential(app: &Path, key_id: &str, with_server: bool, upload_url: Option<&str>, ledger: &Path, entry: LedgerEntry) -> Result<Credential, String> {
    use ring::signature::KeyPair;
    let seed = random_bytes(32);
    let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| "couldn't make a trace-signing key")?;
    let b64 = |bytes: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
    let command_token = with_server.then(|| random_bytes(24).iter().map(|b| format!("{b:02x}")).collect::<String>());

    let resources = if app.join("Contents").is_dir() { app.join("Contents/Resources") } else { app.to_path_buf() };
    fs::create_dir_all(&resources).map_err(|e| e.to_string())?;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n");
    xml += "\t<key>Version</key>\n\t<integer>1</integer>\n";
    xml += &format!("\t<key>KeyID</key>\n\t<string>{}</string>\n", escape(key_id));
    xml += &format!("\t<key>SigningKey</key>\n\t<string>{}</string>\n", b64(&seed));
    if let Some(url) = upload_url {
        xml += &format!("\t<key>UploadURL</key>\n\t<string>{}</string>\n", escape(url));
    }
    if let Some(token) = &command_token {
        xml += &format!("\t<key>CommandToken</key>\n\t<string>{token}</string>\n");
    }
    xml += "</dict>\n</plist>\n";
    fs::write(resources.join("DebugTraceCredential.plist"), xml).map_err(|e| e.to_string())?;

    let build = string(&read_info(&info_path(app))?, "CFBundleVersion").unwrap_or_default();
    let mut record = json!({
        "keyId": key_id, "buildId": key_id, "algorithm": "ed25519", "publicKey": b64(pair.public_key().as_ref()),
        "bundleId": entry.bundle_id, "build": build, "platform": entry.platform,
        "configuration": entry.configuration, "project": entry.project, "branch": entry.branch,
        "device": entry.device, "createdAt": utc_now().seconds,
    });
    if let Some(token) = &command_token {
        record["commandToken"] = token.clone().into();
    }
    fs::create_dir_all(ledger).map_err(|e| format!("couldn't create the key ledger {}: {e}", ledger.display()))?;
    let mut file = crate::builder::private_file(&ledger.join(format!("{key_id}.json")))?;
    std::io::Write::write_all(&mut file, serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Credential { key_id: key_id.to_string(), command_token })
}

/// The keys the always-on debug server needs, added to this signed build only, never to the
/// app's own Info.plist (an App Store build doesn't carry them).
pub fn add_debug_server_keys(app: &Path) -> Result<(), String> {
    let path = info_path(app);
    let mut info = read_info(&path)?;
    if !info.contains_key("NSLocalNetworkUsageDescription") {
        info.insert("NSLocalNetworkUsageDescription".into(), Plist::String("Development builds serve a debug API on your local network.".into()));
    }
    let mut services = info.get("NSBonjourServices").and_then(Plist::as_array).cloned().unwrap_or_default();
    if !services.iter().any(|s| s.as_string() == Some("_debugtrace._tcp")) {
        services.push(Plist::String("_debugtrace._tcp".into()));
    }
    info.insert("NSBonjourServices".into(), Plist::Array(services));
    write_info(&path, &info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(dir: &Path, executable_bytes: &[u8]) -> PathBuf {
        let app = dir.join("Demo.app");
        fs::create_dir_all(app.join("Frameworks/Kit.framework")).unwrap();
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), "com.example.demo".into());
        info.insert("CFBundleExecutable".into(), "Demo".into());
        info.insert("CFBundleVersion".into(), "7".into());
        info.insert("NSBonjourServices".into(), Plist::Array(vec!["_other._tcp".into()]));
        plist::to_file_binary(app.join("Info.plist"), &Plist::Dictionary(info)).unwrap();
        fs::write(app.join("Demo"), executable_bytes).unwrap();
        app
    }

    #[test]
    fn debugtrace_is_found_in_the_binary_or_a_framework() {
        let dir = tempfile::tempdir().unwrap();
        let plain = app(dir.path(), b"\xcf\xfa\xed\xfe nothing");
        let info = read_info(&info_path(&plain)).unwrap();
        assert_eq!(links_debugtrace(&plain, &info), (false, false));
        fs::write(plain.join("Frameworks/Kit.framework/Kit"), b"..DEBUGTRACE_STDERR..").unwrap();
        assert_eq!(links_debugtrace(&plain, &info), (true, false));
        fs::write(plain.join("Demo.debug.dylib"), b"DEBUGTRACE_SERVER").unwrap();
        assert_eq!(links_debugtrace(&plain, &info), (true, true));
    }

    #[test]
    fn the_credential_s_private_half_stays_in_the_bundle_and_the_ledger_gets_the_public_half() {
        use ring::signature::{ED25519, UnparsedPublicKey};
        let dir = tempfile::tempdir().unwrap();
        let demo = app(dir.path(), b"bin");
        let ledger = dir.path().join("keys");
        let entry = LedgerEntry { bundle_id: "com.example.demo", platform: "visionOS", configuration: "Release", project: dir.path(), branch: "main", device: "AVP" };
        let credential = embed_credential(&demo, "abc123", true, Some("https://store.example/api/traces?a=1&b=2"), &ledger, entry).unwrap();
        let token = credential.command_token.clone().unwrap();
        assert_eq!(token.len(), 48);

        let embedded = read_info(&demo.join("DebugTraceCredential.plist")).unwrap();
        assert_eq!(string(&embedded, "KeyID").as_deref(), Some("abc123"));
        assert_eq!(string(&embedded, "UploadURL").as_deref(), Some("https://store.example/api/traces?a=1&b=2"));
        assert_eq!(string(&embedded, "CommandToken"), Some(token.clone()));
        assert_eq!(embedded.get("Version").and_then(Plist::as_signed_integer), Some(1));
        let seed = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, string(&embedded, "SigningKey").unwrap()).unwrap();

        let record: serde_json::Value = serde_json::from_slice(&fs::read(ledger.join("abc123.json")).unwrap()).unwrap();
        assert_eq!(record["build"], "7");
        assert_eq!(record["commandToken"], token.as_str());
        assert_eq!(record["device"], "AVP");
        let public = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, record["publicKey"].as_str().unwrap()).unwrap();
        // What the app signs with the embedded half verifies against the ledger's half.
        let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        UnparsedPublicKey::new(&ED25519, &public).verify(b"trace", pair.sign(b"trace").as_ref()).unwrap();
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(ledger.join("abc123.json")).unwrap().permissions()) & 0o777, 0o600);

        let without = embed_credential(&demo, "def456", false, None, &ledger, LedgerEntry { bundle_id: "x", platform: "iOS", configuration: "Debug", project: dir.path(), branch: "", device: "" }).unwrap();
        assert!(without.command_token.is_none());
        let record: serde_json::Value = serde_json::from_slice(&fs::read(ledger.join("def456.json")).unwrap()).unwrap();
        assert!(record.get("commandToken").is_none());
    }

    #[test]
    fn the_debug_server_keys_are_added_once() {
        let dir = tempfile::tempdir().unwrap();
        let demo = app(dir.path(), b"bin");
        add_debug_server_keys(&demo).unwrap();
        add_debug_server_keys(&demo).unwrap();
        let info = read_info(&info_path(&demo)).unwrap();
        let services: Vec<&str> = info["NSBonjourServices"].as_array().unwrap().iter().filter_map(Plist::as_string).collect();
        assert_eq!(services, ["_other._tcp", "_debugtrace._tcp"]);
        assert!(info.contains_key("NSLocalNetworkUsageDescription"));
    }
}
