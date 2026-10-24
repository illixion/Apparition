//! Signs app bundles for Apparition: a thin layer over apple-codesign (indygreg's
//! apple-platform-rs, MPL-2.0, used unmodified from crates.io) that knows how sideloading signs
//! an app. One certificate and team for everything, entitlements chosen per bundle ID, each
//! bundle's provisioning profile embedded as embedded.mobileprovision, and nothing kept from the
//! signatures the app had before.
//!
//! apple-codesign does the signing itself: every Mach-O (fat or thin, the bundles' main
//! executables and any other code in them), nested bundles before the bundles holding them
//! (Frameworks/, PlugIns/ and ExtensionKit's Extensions/ alike), CodeResources, the
//! CodeDirectory with its special slots, requirements, entitlements as XML and DER, and the CMS
//! signature.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use apple_codesign::{BundleSigner, CodeSignatureFlags, SettingsScope, SigningSettings};
use cryptographic_message_syntax::SignedData;
use plist::{Dictionary, Value};
use rsa::RsaPrivateKey;
use rsa::pkcs8::EncodePrivateKey;
use x509_cert::der::Encode;
use x509_certificate::{CapturedX509Certificate, InMemorySigningKeyPair};

/// Why signing, or reading a profile or certificate, failed.
#[derive(Debug, Clone)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn error(context: &str, cause: impl fmt::Display) -> Error {
    Error(format!("{context}: {cause}"))
}

pub type Result<T> = std::result::Result<T, Error>;

/// A provisioning profile (.mobileprovision): the plist inside Apple's CMS envelope, and the
/// certificates the envelope carries.
#[derive(Clone, Debug)]
pub struct ProvisioningProfile {
    plist: Dictionary,
    team_id: String,
    entitlements: Dictionary,
    certificate_chain: Vec<Vec<u8>>,
}

impl ProvisioningProfile {
    pub fn parse(data: &[u8]) -> Result<Self> {
        let signed = SignedData::parse_ber(data).map_err(|e| error("not a CMS signed profile", e))?;
        let content = signed.signed_content().ok_or_else(|| Error("the profile holds no plist".into()))?;
        let plist: Dictionary = plist::from_bytes(content).map_err(|e| error("the profile's plist", e))?;
        let first = |key: &str| {
            plist.get(key).and_then(Value::as_array).and_then(|a| a.first()).and_then(Value::as_string).map(str::to_string)
        };
        let team_id = first("TeamIdentifier")
            .or_else(|| first("ApplicationIdentifierPrefix"))
            .ok_or_else(|| Error("the profile names no team".into()))?;
        let entitlements = plist
            .get("Entitlements")
            .and_then(Value::as_dictionary)
            .cloned()
            .ok_or_else(|| Error("the profile has no entitlements".into()))?;
        let certificate_chain = signed.certificates().map(|c| c.constructed_data().to_vec()).collect();
        Ok(Self { plist, team_id, entitlements, certificate_chain })
    }

    pub fn plist(&self) -> &Dictionary {
        &self.plist
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn entitlements(&self) -> &Dictionary {
        &self.entitlements
    }

    /// The DER certificates of the profile's CMS envelope (Apple's signing chain).
    pub fn certificate_chain_der(&self) -> &[Vec<u8>] {
        &self.certificate_chain
    }
}

/// The certificate and key to sign with, and the certificates between it and Apple's root,
/// which go into every signature.
pub struct Signer {
    key: InMemorySigningKeyPair,
    certificate: CapturedX509Certificate,
    chain: Vec<CapturedX509Certificate>,
}

impl Signer {
    pub fn new(key: RsaPrivateKey, certificate: x509_cert::Certificate, chain: Vec<x509_cert::Certificate>) -> Result<Self> {
        let pkcs8 = key.to_pkcs8_der().map_err(|e| error("the private key", e))?;
        let key = InMemorySigningKeyPair::from_pkcs8_der(pkcs8.as_bytes()).map_err(|e| error("the private key", e))?;
        let capture = |c: &x509_cert::Certificate| -> Result<CapturedX509Certificate> {
            let der = c.to_der().map_err(|e| error("a certificate", e))?;
            CapturedX509Certificate::from_der(der).map_err(|e| error("a certificate", e))
        };
        Ok(Self { key, certificate: capture(&certificate)?, chain: chain.iter().map(capture).collect::<Result<_>>()? })
    }
}

/// How to sign an app and the bundles in it.
pub struct BundleSigningSettings<'a> {
    pub team_id: String,
    /// The main app's entitlements, unless `entitlements_by_bundle_id` names its ID.
    pub main_entitlements: Dictionary,
    /// Each nested bundle's entitlements; a bundle not listed is signed with none.
    pub entitlements_by_bundle_id: BTreeMap<String, Dictionary>,
    /// None signs ad hoc.
    pub signer: Option<&'a Signer>,
    /// The main app's profile, unless `embedded_mobileprovisions_by_bundle_id` names its ID.
    pub embedded_mobileprovision: Option<&'a [u8]>,
    /// Each nested bundle's profile; a bundle not listed keeps whatever it has.
    pub embedded_mobileprovisions_by_bundle_id: BTreeMap<String, &'a [u8]>,
}

impl<'a> BundleSigningSettings<'a> {
    pub fn new(team_id: impl Into<String>, main_entitlements: Dictionary, signer: Option<&'a Signer>) -> Self {
        Self {
            team_id: team_id.into(),
            main_entitlements,
            entitlements_by_bundle_id: BTreeMap::new(),
            signer,
            embedded_mobileprovision: None,
            embedded_mobileprovisions_by_bundle_id: BTreeMap::new(),
        }
    }
}

/// A bundle inside the app: its folder relative to the app, ID and main executable.
struct Nested {
    relative: String,
    identifier: String,
    executable: Option<String>,
}

/// What's in the app: its nested bundles, and every Mach-O file (relative paths, `/`-separated).
#[derive(Default)]
struct Contents {
    bundles: Vec<Nested>,
    machos: Vec<String>,
    /// Mach-O files with no code to sign: object files, core dumps, dSYM companions.
    unsignable: Vec<String>,
}

/// Signs the app (or other bundle) at `path` in place, nested bundles and code included.
pub fn sign_bundle(path: impl Into<PathBuf>, settings: &BundleSigningSettings<'_>) -> Result<()> {
    let root: PathBuf = path.into();
    let info = info_plist(&root).ok_or_else(|| Error(format!("{} has no Info.plist", root.display())))?;
    let root_id = info
        .get("CFBundleIdentifier")
        .and_then(Value::as_string)
        .ok_or_else(|| Error("the app's Info.plist has no CFBundleIdentifier".into()))?
        .to_string();

    let mut contents = Contents::default();
    walk(&root, &root, &mut contents)?;

    // App Store encryption leftovers (SC_Info) make installd refuse a re-signed app.
    for dir in std::iter::once(root.clone()).chain(contents.bundles.iter().map(|b| root.join(&b.relative))) {
        let fairplay = dir.join("SC_Info");
        if fairplay.is_dir() {
            std::fs::remove_dir_all(&fairplay).map_err(|e| error("removing SC_Info", e))?;
        }
    }
    contents.machos.retain(|m| !m.split('/').any(|part| part == "SC_Info"));

    // Each bundle's profile, before signing so the parent's CodeResources seals it.
    let root_profile = settings.embedded_mobileprovisions_by_bundle_id.get(&root_id).copied().or(settings.embedded_mobileprovision);
    if let Some(profile) = root_profile {
        write(&root.join("embedded.mobileprovision"), profile)?;
    }
    for bundle in &contents.bundles {
        if let Some(profile) = settings.embedded_mobileprovisions_by_bundle_id.get(&bundle.identifier) {
            write(&root.join(&bundle.relative).join("embedded.mobileprovision"), profile)?;
        }
    }

    let mut signing = SigningSettings::default();
    if let Some(signer) = settings.signer {
        signing.set_signing_key(&signer.key, signer.certificate.clone());
        for certificate in &signer.chain {
            signing.chain_certificate(certificate.clone());
        }
    }
    signing.set_team_id(&settings.team_id);
    // apple-codesign fails on these (an object file has no __LINKEDIT); sealed as plain files.
    for path in &contents.unsignable {
        signing.add_path_exclusion(&glob_escape(path)).map_err(|e| error("excluding a file that can't be signed", e))?;
    }

    // Every scope gets its entitlements and flags explicitly: apple-codesign otherwise keeps
    // what an already signed binary had, and a re-signed app must claim only what its profiles
    // allow.
    let empty = Dictionary::new();
    let main = settings.entitlements_by_bundle_id.get(&root_id).unwrap_or(&settings.main_entitlements);
    let mut scopes = vec![(SettingsScope::Main, main)];
    let mut main_executables = Vec::new();
    for bundle in &contents.bundles {
        let entitlements = settings.entitlements_by_bundle_id.get(&bundle.identifier).unwrap_or(&empty);
        scopes.push((SettingsScope::Path(bundle.relative.clone()), entitlements));
        if let Some(executable) = &bundle.executable {
            main_executables.push(format!("{}/{executable}", bundle.relative));
        }
    }
    if let Some(executable) = info.get("CFBundleExecutable").and_then(Value::as_string) {
        main_executables.push(executable.to_string());
    }
    for macho in &contents.machos {
        if !main_executables.contains(macho) {
            scopes.push((SettingsScope::Path(macho.clone()), &empty));
        }
    }
    for (scope, entitlements) in scopes {
        signing
            .set_entitlements_xml(scope.clone(), entitlements_xml(entitlements)?)
            .map_err(|e| error("entitlements", e))?;
        signing.set_code_signature_flags(scope, CodeSignatureFlags::empty());
    }

    let mut signer = BundleSigner::new_from_path(&root).map_err(|e| error("reading the bundle", e))?;
    signer.collect_nested_bundles().map_err(|e| error("finding nested bundles", e))?;
    signer.write_signed_bundle(&root, &signing).map_err(|e| error("signing", e))?;
    Ok(())
}

/// Entitlements as XML, keys sorted: the DER form must list keys in order or the OS ignores
/// all of them ("invalid entitlements blob"), and paid profiles list theirs unsorted.
fn entitlements_xml(entitlements: &Dictionary) -> Result<String> {
    let mut xml = Vec::new();
    plist::to_writer_xml(&mut xml, &sorted(Value::Dictionary(entitlements.clone())))
        .map_err(|e| error("entitlements", e))?;
    String::from_utf8(xml).map_err(|e| error("entitlements", e))
}

fn sorted(value: Value) -> Value {
    match value {
        Value::Dictionary(dictionary) => {
            let mut entries: Vec<(String, Value)> = dictionary.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Dictionary(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// A bundle's Info.plist: at its top on iOS and its relatives, in Contents/ on macOS.
fn info_plist(dir: &Path) -> Option<Dictionary> {
    ["Info.plist", "Contents/Info.plist"].iter().find_map(|p| plist::from_file(dir.join(p)).ok())
}

fn relative(root: &Path, path: &Path) -> String {
    let parts: Vec<String> = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}

fn walk(root: &Path, dir: &Path, contents: &mut Contents) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| error(&format!("reading {}", dir.display()), e))?
        .filter_map(std::result::Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if let Some(info) = info_plist(&path)
                && let Some(identifier) = info.get("CFBundleIdentifier").and_then(Value::as_string)
            {
                let executable = info.get("CFBundleExecutable").and_then(Value::as_string).map(|exe| {
                    // macOS-style bundles keep the executable in Contents/MacOS.
                    if path.join("Contents/MacOS").join(exe).is_file() { format!("Contents/MacOS/{exe}") } else { exe.to_string() }
                });
                contents.bundles.push(Nested { relative: relative(root, &path), identifier: identifier.to_string(), executable });
            }
            walk(root, &path, contents)?;
        } else if kind.is_file()
            && let Some(file_type) = macho_file_type(&path)
        {
            let relative = relative(root, &path);
            if matches!(file_type, MH_OBJECT | MH_CORE | MH_DSYM) {
                contents.unsignable.push(relative);
            } else {
                contents.machos.push(relative);
            }
        }
    }
    Ok(())
}

// Mach-O file types (mach-o/loader.h) that hold no code.
const MH_OBJECT: u32 = 0x1;
const MH_CORE: u32 = 0x4;
const MH_DSYM: u32 = 0xa;

/// The file type of a Mach-O file (of its first slice, if fat), or `None` if it isn't one.
fn macho_file_type(path: &Path) -> Option<u32> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header[..16]).ok()?;
    let be = |bytes: &[u8]| u32::from_be_bytes(bytes.try_into().unwrap());
    match be(&header[..4]) {
        0xfeed_face | 0xfeed_facf => Some(be(&header[12..16])),
        0xcefa_edfe | 0xcffa_edfe => Some(u32::from_le_bytes(header[12..16].try_into().unwrap())),
        // Fat, but Java class files share the magic: they have a version where the arch
        // count would be, and no real fat binary has that many slices.
        magic @ (0xcafe_babe | 0xcafe_babf) => {
            if be(&header[4..8]) >= 32 {
                return None;
            }
            // Unknown (so signed) if the first slice can't be read.
            let unknown = Some(0);
            // The first slice's offset follows its cputype and cpusubtype, 64 bits wide in fat64.
            let Ok(()) = file.read_exact(&mut header[16..24]) else { return unknown };
            let offset = if magic == 0xcafe_babe { u64::from(be(&header[16..20])) } else { u64::from_be_bytes(header[16..24].try_into().unwrap()) };
            let mut slice = [0u8; 16];
            let Ok(_) = file.seek(SeekFrom::Start(offset)) else { return unknown };
            let Ok(()) = file.read_exact(&mut slice) else { return unknown };
            match be(&slice[..4]) {
                0xfeed_face | 0xfeed_facf => Some(be(&slice[12..16])),
                0xcefa_edfe | 0xcffa_edfe => Some(u32::from_le_bytes(slice[12..16].try_into().unwrap())),
                _ => unknown,
            }
        }
        _ => None,
    }
}

/// `path` as a glob that matches only itself.
fn glob_escape(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '*' | '?' | '[' => format!("[{c}]"),
            _ => c.to_string(),
        })
        .collect()
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::write(path, data).map_err(|e| error(&format!("writing {}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entitlements_are_sorted_deeply() {
        let mut inner = Dictionary::new();
        inner.insert("z".into(), Value::Boolean(true));
        inner.insert("a".into(), Value::Boolean(false));
        let mut outer = Dictionary::new();
        outer.insert("keychain-access-groups".into(), Value::Array(vec![Value::String("T.*".into())]));
        outer.insert("com.apple.security.application-groups".into(), Value::Dictionary(inner.clone()));
        outer.insert("application-identifier".into(), Value::Array(vec![Value::Dictionary(inner)]));
        let xml = entitlements_xml(&outer).unwrap();
        let at = |s: &str| xml.find(s).unwrap();
        assert!(at("application-identifier") < at("com.apple.security") && at("com.apple.security") < at("keychain"));
        let first_a = at("<key>a</key>");
        assert!(first_a < xml[first_a..].find("<key>z</key>").unwrap() + first_a);
    }

    /// A Mach-O header: 64-bit little-endian arm64 with the given file type.
    fn thin(file_type: u32) -> Vec<u8> {
        let mut bytes = vec![0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 0x01, 0, 0, 0, 0];
        bytes.extend(file_type.to_le_bytes());
        bytes.extend([0; 16]);
        bytes
    }

    #[test]
    fn recognises_macho() {
        let dir = std::env::temp_dir().join(format!("apparition-signer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut fat = vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        fat.extend(32u32.to_be_bytes());
        fat.extend([0; 12]);
        fat.extend(thin(MH_OBJECT));
        for (name, bytes, expected) in [
            ("thin", thin(2), Some(2)),
            ("dylib", thin(6), Some(6)),
            ("object", thin(MH_OBJECT), Some(MH_OBJECT)),
            ("fat", fat, Some(MH_OBJECT)),
            ("fat-unreadable", vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0, 0, 0, 0], Some(0)),
            ("class", vec![0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52, 0, 0, 0, 0, 0, 0, 0, 0], None),
            ("text", b"hello world, hello world".to_vec(), None),
        ] {
            std::fs::write(dir.join(name), bytes).unwrap();
            assert_eq!(macho_file_type(&dir.join(name)), expected, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn globs_match_only_their_path() {
        let path = "Frameworks/lib [x]/a*b?.o";
        let pattern = glob_escape(path);
        assert_eq!(pattern, "Frameworks/lib [[]x]/a[*]b[?].o");
        let mut settings = apple_codesign::SigningSettings::default();
        settings.add_path_exclusion(&pattern).unwrap();
        assert!(settings.path_exclusion_pattern_matches(path));
        assert!(!settings.path_exclusion_pattern_matches("Frameworks/lib x/aXbY.o"));
    }

    /// An app carrying an object file (Kodi's CPython ships python.o) signs, the object sealed as a plain file.
    #[test]
    fn signs_an_app_holding_an_object_file() {
        let root = std::env::temp_dir().join(format!("apparition-signer-obj-{}", std::process::id()));
        let app = root.join("Test.app");
        std::fs::create_dir_all(app.join("Frameworks/config")).unwrap();
        let mut info = Dictionary::new();
        info.insert("CFBundleIdentifier".into(), Value::String("com.example.test".into()));
        info.insert("CFBundleExecutable".into(), Value::String("Test".into()));
        plist::to_file_xml(app.join("Info.plist"), &Value::Dictionary(info)).unwrap();
        // The only segment is unnamed and there is no __LINKEDIT, as in a compiler's .o.
        let mut object = thin(MH_OBJECT);
        object.truncate(16);
        // One load command of 72 bytes, no flags, then the reserved word.
        object.extend([1, 0, 0, 0, 72, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        object.extend([0x19, 0, 0, 0, 72, 0, 0, 0]);
        object.extend([0; 16]);
        object.extend([0; 8]);
        object.extend(8u64.to_le_bytes());
        object.extend(104u64.to_le_bytes());
        object.extend(8u64.to_le_bytes());
        object.extend([7, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        object.extend([0; 8]);
        std::fs::write(app.join("Frameworks/config/python.o"), object).unwrap();

        let settings = BundleSigningSettings::new("TEAMID1234", Dictionary::new(), None);
        let result = sign_bundle(&app, &settings);
        let sealed = std::fs::read_to_string(app.join("_CodeSignature/CodeResources")).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);
        result.unwrap();
        assert!(sealed.contains("Frameworks/config/python.o"));
    }
}
