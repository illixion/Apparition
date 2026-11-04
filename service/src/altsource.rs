//! An AltStore-format source of the Store's apps, built on demand (`store/lib/altsource.js`).
//!
//! `GET /altsource/source.json` lists every configured app at the head of its source branch:
//! `buildVersion` is that commit's short SHA, which is what build-and-sign stamps into
//! `CFBundleVersion`, so a client (AltStore, SideStore, Hermes) sees an update whenever the branch
//! moves. Each version's `downloadURL` names the commit (`/altsource/<slug>/<commit>.ipa`).
//! Requesting it serves the IPA if that commit is already built, else starts a build of exactly
//! that commit and answers with its job id for the server to wait on (the long poll itself,
//! `holdSeconds` and the 307 redirects, belongs to the server).
//!
//! AltStore refuses an IPA whose entitlements or privacy strings differ from the source's
//! `appPermissions`, and whose version or build differ from the listing, so both are read from
//! the built IPA (`inspect_ipa`). Before a commit is built, the listing carries the previous
//! build's values, which only change when the app's capabilities do.
//!
//! Everything that touches the build queue is behind [`Deps`], so the tests drive this without
//! Xcode and the module knows nothing of axum or the queue.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// IPAs kept per app.
pub const KEEP_PER_APP: usize = 2;
/// A failed build is reported to further requests for this long instead of starting another, so
/// a client that retries on its own doesn't queue a build each time.
pub const FAILURE_MEMORY_MS: u64 = 2 * 60_000;
/// Entitlements AltStore says a source need not list.
const IMPLICIT_ENTITLEMENTS: [&str; 2] = ["application-identifier", "com.apple.developer.team-identifier"];
/// The largest Info.plist read from an IPA.
const MAX_INFO_PLIST: u64 = 1 << 20;
/// The largest icon read from an IPA.
const MAX_ICON: u64 = 8 << 20;
/// The largest code signature read from an executable.
const MAX_SIGNATURE: u64 = 16 << 20;
/// The largest executable scanned for an entitlements plist when its Mach-O can't be parsed.
const MAX_SCAN: u64 = 256 << 20;

/// `<bundleId>-<platform>` and file names made of commits: runs of anything but
/// `A-Za-z0-9._-` become one `-`.
pub fn slug_of(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_run = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out
}

/// Short SHAs from two places may be abbreviated to different lengths.
pub fn same_commit(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

// ---- the listing: pure functions over config.json ----

/// One row a source lists: an app built for a platform of a repo.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SourceApp {
    pub repo: String,
    pub path: String,
    pub branch: String,
    /// `<bundleId>-<platform>`, the key in every URL and file name.
    pub slug: String,
    pub platform: String,
    pub device: Option<String>,
    pub bundle_id: String,
    /// The platform row's own `name`, when config.json gives one. Only a fallback for the
    /// listing's name before an IPA says what the app is called (the JS used the repo's).
    pub name: Option<String>,
    pub description: Option<String>,
    pub subtitle: Option<String>,
    pub icon_url: Option<String>,
    pub developer_name: Option<String>,
    pub category: Option<String>,
}

/// A non-empty string member (the JS `||` chains treat `""` as absent).
fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn branches(rc: &Value) -> Vec<&str> {
    rc.get("branches").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default()
}

/// The branch a repo's source entry follows: its own `altSource.branch`, the source-wide one
/// (`cfg` = `config.altSource`) when the repo offers it, else main/master when offered, else the
/// first branch offered.
pub fn source_branch(rc: &Value, cfg: &Value) -> String {
    if let Some(b) = rc.get("altSource").and_then(|a| a.get("branch")).and_then(Value::as_str) {
        return b.to_string();
    }
    let offered = branches(rc);
    if let Some(b) = cfg.get("branch").and_then(Value::as_str).filter(|b| offered.contains(b)) {
        return b.to_string();
    }
    if offered.contains(&"main") {
        "main".into()
    } else if offered.contains(&"master") {
        "master".into()
    } else {
        offered.first().copied().unwrap_or("main").to_string()
    }
}

/// The platform rows a repo offers to the feed: all of them, or those its `altSource.platforms`
/// names; none with `altSource: false`. A watchOS row never makes a feed app: a watch app is
/// installed only from inside its iPhone app's IPA, never alone.
fn feed_rows(rc: &Value) -> Vec<&Value> {
    if rc.get("altSource") == Some(&Value::Bool(false)) {
        return Vec::new();
    }
    let own = rc.get("altSource").filter(|a| a.is_object());
    let allowed = own.and_then(|o| o.get("platforms")).and_then(Value::as_array);
    rc.get("platforms").and_then(Value::as_array).into_iter().flatten()
        .filter(|p| p.get("platform").and_then(Value::as_str).is_some_and(|n| !n.eq_ignore_ascii_case("watchos")))
        .filter(|p| allowed.is_none_or(|list| list.iter().any(|n| n.as_str() == p.get("platform").and_then(Value::as_str))))
        .collect()
}

fn repos(config: &Value) -> impl Iterator<Item = &Value> {
    config.get("repos").and_then(Value::as_array).into_iter().flatten()
}

/// Every platform name some repo offers, in config order: one source each. These are
/// config.json's names, aliases included (`iPad`, a visionOS row that builds the iPad binary),
/// since that is the device the list is for.
pub fn source_platforms(cfg: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rc in repos(cfg) {
        for pc in feed_rows(rc) {
            let name = pc["platform"].as_str().unwrap_or_default();
            if !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// The apps of one platform's source (`platform` set) or of the combined one. A source can't
/// list a bundle id twice, so the first row naming it wins: in a platform source that only
/// matters for two rows of one platform (two iPhones). Each app is keyed by bundle id and
/// platform, since one bundle id can build differently per platform (an alias with its own
/// `DEVICE_NAME`).
pub fn source_apps(config: &Value, platform: Option<&str>) -> Vec<SourceApp> {
    let platform = platform.filter(|p| !p.is_empty());
    let cfg = config.get("altSource").cloned().unwrap_or(Value::Null);
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    for rc in repos(config) {
        let own = rc.get("altSource").filter(|a| a.is_object()).cloned().unwrap_or(Value::Null);
        for pc in feed_rows(rc) {
            let name = pc["platform"].as_str().unwrap_or_default();
            if platform.is_some_and(|p| p != name) {
                continue;
            }
            let Some(bundle_id) = pc.get("bundleId").and_then(Value::as_str) else { continue };
            if !seen.insert(bundle_id) {
                continue;
            }
            let owned = |v: &Value, k: &str| text(v, k).map(str::to_string);
            out.push(SourceApp {
                slug: slug_of(&format!("{bundle_id}-{name}")),
                repo: rc.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                path: rc.get("path").and_then(Value::as_str).unwrap_or_default().to_string(),
                bundle_id: bundle_id.to_string(),
                platform: name.to_string(),
                device: pc.get("device").and_then(Value::as_str).map(str::to_string),
                branch: source_branch(rc, &cfg),
                name: owned(pc, "name"),
                description: owned(&own, "description"),
                subtitle: owned(&own, "subtitle"),
                icon_url: owned(&own, "iconURL"),
                developer_name: owned(&own, "developerName").or_else(|| owned(&cfg, "developerName")),
                category: owned(&own, "category"),
            });
        }
    }
    out
}

/// Every app any source lists, once per slug: what a download URL can name.
pub fn all_source_apps(config: &Value) -> Vec<SourceApp> {
    let mut out = source_apps(config, None);
    for p in source_platforms(config) {
        for a in source_apps(config, Some(&p)) {
            if !out.iter().any(|b| b.slug == a.slug) {
                out.push(a);
            }
        }
    }
    out
}

// ---- what a built IPA says ----

/// The entitlement keys and privacy strings AltStore compares with the listing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub entitlements: Vec<String>,
    #[serde(default)]
    pub privacy: BTreeMap<String, String>,
}

/// What `inspect_ipa` reads from an IPA.
#[derive(Clone, Debug, Default)]
pub struct IpaInfo {
    pub name: String,
    pub bundle_id: Option<String>,
    pub version: String,
    pub build_no: String,
    pub min_os_version: Option<String>,
    pub permissions: Permissions,
    /// The largest loose `AppIcon*.png` (PNG bytes).
    pub icon: Option<Vec<u8>>,
}

type Zip = zip::ZipArchive<fs::File>;

fn read_plist(zip: &mut Zip, name: &str) -> Option<plist::Dictionary> {
    let entry = zip.by_name(name).ok()?;
    if entry.size() > MAX_INFO_PLIST {
        return None;
    }
    let mut data = Vec::new();
    entry.take(MAX_INFO_PLIST).read_to_end(&mut data).ok()?;
    plist::Value::from_reader(io::Cursor::new(data)).ok()?.into_dictionary()
}

fn string(info: &plist::Dictionary, key: &str) -> Option<String> {
    info.get(key).and_then(|v| v.as_string()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// `len` bytes of an entry from `offset` on (fewer at its end): a decompressing read that skips
/// what comes before, since a deflated entry has no random access.
fn read_range(zip: &mut Zip, index: usize, offset: u64, len: u64) -> Option<Vec<u8>> {
    let mut entry = zip.by_index(index).ok()?;
    if offset > entry.size() {
        return None;
    }
    io::copy(&mut (&mut entry).take(offset), &mut io::sink()).ok()?;
    let mut buf = Vec::new();
    (&mut entry).take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The slice (offset, size) of a Mach-O file whose first bytes are `head`: the file itself, or
/// the arm64 (else first) slice of a fat one.
fn macho_slice(head: &[u8], file_size: u64) -> Option<(u64, u64)> {
    match be32(head, 0)? {
        0xcafe_babe | 0xcafe_babf => {
            let wide = be32(head, 0)? == 0xcafe_babf;
            let (stride, count) = (if wide { 32 } else { 20 }, be32(head, 4)? as usize);
            let mut first = None;
            for i in 0..count.min(16) {
                let at = 8 + i * stride;
                let cpu = be32(head, at)?;
                let (offset, size) = if wide { (be64(head, at + 8)?, be64(head, at + 16)?) } else { (be32(head, at + 8)? as u64, be32(head, at + 12)? as u64) };
                if cpu == 0x0100_000c {
                    return Some((offset, size));
                }
                first.get_or_insert((offset, size));
            }
            first
        }
        _ => Some((0, file_size)),
    }
}

/// `LC_CODE_SIGNATURE`'s (dataoff, datasize) from a thin Mach-O's header.
fn code_signature_location(slice: &[u8]) -> Option<(u64, u64)> {
    let header = match le32(slice, 0)? {
        0xfeed_facf => 32,
        0xfeed_face => 28,
        _ => return None,
    };
    let (ncmds, mut at) = (le32(slice, 16)?, header);
    for _ in 0..ncmds {
        let (cmd, size) = (le32(slice, at)?, le32(slice, at + 4)? as usize);
        if cmd == 0x1d {
            return Some((le32(slice, at + 8)? as u64, le32(slice, at + 12)? as u64));
        }
        if size < 8 {
            return None;
        }
        at += size;
    }
    None
}

/// The XML entitlements blob (slot 5, magic `0xfade7171`) of a code signature's SuperBlob.
fn entitlements_blob(sig: &[u8]) -> Option<&[u8]> {
    if be32(sig, 0)? != 0xfade_0cc0 {
        return None;
    }
    for i in 0..be32(sig, 8)? as usize {
        let at = 12 + i * 8;
        if be32(sig, at)? != 5 {
            continue;
        }
        let offset = be32(sig, at + 4)? as usize;
        if be32(sig, offset)? != 0xfade_7171 {
            continue;
        }
        let len = be32(sig, offset + 4)? as usize;
        return sig.get(offset + 8..offset + len);
    }
    None
}

/// The first `<?xml` ... `</plist>` in `data`, which is how codesign stores entitlements.
fn scan_plist(data: &[u8]) -> Option<&[u8]> {
    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
    let start = find(data, b"<?xml")?;
    let end = find(&data[start..], b"</plist>")? + start + b"</plist>".len();
    Some(&data[start..end])
}

/// The entitlement keys an executable's embedded code signature carries. Best effort: an
/// unsigned or unreadable executable has none.
fn executable_entitlements(zip: &mut Zip, index: usize) -> Vec<String> {
    let size = zip.by_index_raw(index).map(|e| e.size()).unwrap_or(0);
    let from_signature = (|| {
        let head = read_range(zip, index, 0, 256 << 10)?;
        let (offset, slice_size) = macho_slice(&head, size)?;
        let slice = if offset == 0 { head } else { read_range(zip, index, offset, 256 << 10)? };
        let (data_off, data_size) = code_signature_location(&slice)?;
        if data_size == 0 || data_size > MAX_SIGNATURE || data_off + data_size > slice_size {
            return None;
        }
        let sig = read_range(zip, index, offset + data_off, data_size)?;
        Some(entitlements_blob(&sig)?.to_vec())
    })();
    let xml = from_signature.or_else(|| {
        if size > MAX_SCAN {
            return None;
        }
        Some(scan_plist(&read_range(zip, index, 0, MAX_SCAN)?)?.to_vec())
    });
    xml.and_then(|x| plist::Value::from_reader(io::Cursor::new(x)).ok()?.into_dictionary())
        .map(|d| d.keys().cloned().collect()).unwrap_or_default()
}

/// The pixel size of a loose `AppIcon<N>x<N>[@<k>x][~ipad].png` file name.
fn app_icon_pixels(name: &str) -> Option<f64> {
    fn number(s: &str) -> Option<(f64, &str)> {
        let digits = s.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let mut end = digits;
        if s.as_bytes().get(end) == Some(&b'.') {
            let frac = s[end + 1..].bytes().take_while(u8::is_ascii_digit).count();
            if frac > 0 {
                end += 1 + frac;
            }
        }
        Some((s[..end].parse().ok()?, &s[end..]))
    }
    let (points, rest) = number(name.strip_prefix("AppIcon")?)?;
    let (_, mut rest) = number(rest.strip_prefix('x')?)?;
    let mut scale = 1.0;
    if let Some(r) = rest.strip_prefix('@') {
        let b = r.as_bytes();
        if b.len() < 2 || !b[0].is_ascii_digit() || b[1] != b'x' {
            return None;
        }
        scale = f64::from(b[0] - b'0');
        rest = &r[2..];
    }
    let rest = rest.strip_prefix("~ipad").unwrap_or(rest);
    (rest == ".png").then_some(points * scale)
}

/// The `.app`/`.appex` bundles of an IPA under its main app: their zip prefixes, the main app
/// first. Bundle folders inside localisations, asset catalogs, resource bundles, frameworks and
/// dSYMs are not entered, and the search goes five levels down.
fn bundle_roots(names: &[String]) -> Vec<String> {
    let mut app: Option<String> = None;
    let mut found: BTreeSet<String> = BTreeSet::new();
    for name in names {
        let parts: Vec<&str> = name.split('/').collect();
        if parts.len() < 3 || parts[0] != "Payload" || !parts[1].ends_with(".app") || parts[1].len() <= 4 {
            continue;
        }
        if app.get_or_insert_with(|| parts[1].to_string()) != parts[1] {
            continue;
        }
        // Directory components below the app: all but the last, which is the file (or empty).
        for (i, c) in parts.iter().enumerate().take(parts.len() - 1).skip(2) {
            if i - 2 > 4 {
                break;
            }
            if c.ends_with(".app") || c.ends_with(".appex") {
                found.insert(parts[..=i].join("/"));
            }
            if [".lproj", ".car", ".bundle", ".framework", ".dSYM"].iter().any(|s| c.ends_with(s)) {
                break;
            }
        }
    }
    let Some(app) = app else { return Vec::new() };
    let mut out = vec![format!("Payload/{app}")];
    out.extend(found);
    out
}

/// Version, permissions and icon of an IPA, for the listing. Unpacks nothing: the zip is read in
/// place, so it works wherever the Store does (the JS shelled out to `ditto` and `codesign`).
pub fn inspect_ipa(ipa: &Path) -> Result<IpaInfo, String> {
    let file = fs::File::open(ipa).map_err(|e| format!("could not open {}: {e}", ipa.display()))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("could not unpack {}: {e}", ipa.file_name().and_then(|n| n.to_str()).unwrap_or("the ipa")))?;
    let mut names = Vec::with_capacity(zip.len());
    let mut index_of: BTreeMap<String, usize> = BTreeMap::new();
    for i in 0..zip.len() {
        let Ok(entry) = zip.by_index_raw(i) else { continue };
        let Ok(name) = entry.name().map(|n| n.into_owned()) else { continue };
        index_of.entry(name.clone()).or_insert(i);
        names.push(name);
    }
    let bundles = bundle_roots(&names);
    let app_dir = bundles.first().ok_or("no .app in Payload")?.clone();
    let info = read_plist(&mut zip, &format!("{app_dir}/Info.plist")).ok_or("could not read Info.plist")?;

    let mut entitlements: BTreeSet<String> = BTreeSet::new();
    let mut privacy: BTreeMap<String, String> = BTreeMap::new();
    for bundle in &bundles {
        let bundle_info = if *bundle == app_dir { Some(info.clone()) } else { read_plist(&mut zip, &format!("{bundle}/Info.plist")) };
        let executable = bundle_info.as_ref().and_then(|i| string(i, "CFBundleExecutable")).or_else(|| {
            let leaf = bundle.rsplit('/').next().unwrap_or_default();
            Some(leaf.rsplit_once('.').map_or(leaf, |(stem, _)| stem).to_string())
        });
        if let Some(index) = executable.and_then(|e| index_of.get(&format!("{bundle}/{e}")).copied()) {
            entitlements.extend(executable_entitlements(&mut zip, index).into_iter().filter(|k| !IMPLICIT_ENTITLEMENTS.contains(&k.as_str())));
        }
        let Some(bundle_info) = bundle_info else { continue };
        for (k, v) in &bundle_info {
            if let Some(s) = v.as_string().filter(|_| k.ends_with("UsageDescription")) {
                privacy.entry(k.clone()).or_insert_with(|| s.to_string());
            }
        }
    }

    let prefix = format!("{app_dir}/");
    let mut icon: Option<(f64, usize)> = None;
    for (name, &index) in &index_of {
        let Some(px) = name.strip_prefix(&prefix).filter(|r| !r.contains('/')).and_then(app_icon_pixels) else { continue };
        if icon.is_none_or(|(best, _)| px > best) {
            icon = Some((px, index));
        }
    }
    let icon = icon.and_then(|(_, index)| {
        let entry = zip.by_index(index).ok()?;
        if entry.size() > MAX_ICON {
            return None;
        }
        let mut png = Vec::new();
        entry.take(MAX_ICON).read_to_end(&mut png).ok()?;
        Some(png)
    });

    let stem = app_dir.rsplit('/').next().unwrap_or_default().trim_end_matches(".app").to_string();
    Ok(IpaInfo {
        name: string(&info, "CFBundleDisplayName").or_else(|| string(&info, "CFBundleName")).unwrap_or(stem),
        bundle_id: string(&info, "CFBundleIdentifier"),
        version: string(&info, "CFBundleShortVersionString").unwrap_or_else(|| "1.0".into()),
        build_no: string(&info, "CFBundleVersion").unwrap_or_default(),
        min_os_version: string(&info, "MinimumOSVersion"),
        permissions: Permissions { entitlements: entitlements.into_iter().collect(), privacy },
        icon,
    })
}

// ---- the feed document ----

/// A branch head: `git log --format=%h,%cI,%s` of the source branch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Head {
    pub commit: String,
    pub date: String,
    pub subject: String,
}

/// One kept IPA, as `index.json` records it (the same shape the Node Store wrote).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub commit: String,
    #[serde(default)]
    pub build_no: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "minOSVersion")]
    pub min_os_version: Option<String>,
    #[serde(default)]
    pub permissions: Permissions,
    /// File name inside the altsource folder.
    pub file: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub size: u64,
    /// Milliseconds since 1970 it was recorded.
    #[serde(default)]
    pub at: u64,
}

/// What the feed knows per app: the source row, the branch head (an app without one is left
/// out) and the newest recorded IPA.
pub struct FeedApp {
    pub app: SourceApp,
    pub head: Option<Head>,
    pub built: Option<Entry>,
}

/// The AltStore source document. The flat `version`/`downloadURL`/... app keys are the pre-2.0
/// layout older SideStore builds still read. `cfg` is `config.altSource`; `url` turns a path
/// (`/altsource/<slug>/<commit>.ipa`, `/altsource/<slug>/icon.png`, `/altsource/icon.png`) into a
/// signed absolute link.
pub fn build_source(cfg: &Value, title: &str, apps: &[FeedApp], url: &dyn Fn(&str) -> String, platform: Option<&str>) -> Value {
    let list: Vec<Value> = apps.iter().filter_map(|f| {
        let (a, head) = (&f.app, f.head.as_ref()?);
        let prev = f.built.as_ref();
        let b = prev.filter(|p| same_commit(&p.commit, &head.commit));
        let known = b.or(prev);
        let mut version = json!({
            "version": known.map(|k| k.version.as_str()).filter(|v| !v.is_empty()).unwrap_or("1.0"),
            // The real stamp when this commit is built (it could say -dirty), else the SHA it will get.
            "buildVersion": b.map_or(head.commit.as_str(), |b| b.build_no.as_str()),
            "date": head.date,
            "localizedDescription": head.subject,
            "downloadURL": url(&format!("/altsource/{}/{}.ipa", a.slug, head.commit)),
            "size": known.map_or(0, |k| k.size),
        });
        if let Some(min) = known.and_then(|k| k.min_os_version.as_deref()).filter(|m| !m.is_empty()) {
            version["minOSVersion"] = json!(min);
        }
        let developer = a.developer_name.as_deref().or_else(|| text(cfg, "developerName")).unwrap_or("Me");
        Some(json!({
            "name": known.map(|k| k.name.as_str()).filter(|n| !n.is_empty()).or(a.name.as_deref()).unwrap_or(&a.repo),
            "bundleIdentifier": a.bundle_id,
            "developerName": developer,
            "subtitle": a.subtitle.clone().unwrap_or_else(|| format!("{} \u{b7} {}", a.repo, a.branch)),
            "localizedDescription": a.description.clone().unwrap_or_else(|| format!("{}, built from {} on demand.", a.repo, a.branch)),
            "iconURL": a.icon_url.clone().unwrap_or_else(|| url(&format!("/altsource/{}/icon.png", a.slug))),
            "category": a.category.as_deref().unwrap_or("developer"),
            // Not part of the format: lets a client (Hermes) pick the apps for its platform.
            "platform": a.platform,
            "versions": [version.clone()],
            "appPermissions": known.map_or_else(|| json!({"entitlements": [], "privacy": {}}), |k| json!(k.permissions)),
            "screenshots": [],
            "version": version["version"], "versionDate": version["date"], "versionDescription": version["localizedDescription"],
            "downloadURL": version["downloadURL"], "size": version["size"],
        }))
    }).collect();
    let base_name = text(cfg, "name").unwrap_or(title);
    let mut doc = Map::new();
    // AltStore keys sources by identifier, so each platform's needs its own.
    doc.insert("name".into(), json!(match platform { Some(p) => format!("{base_name} ({p})"), None => base_name.to_string() }));
    doc.insert("identifier".into(), json!(format!("{}{}", text(cfg, "identifier").unwrap_or("local.apparition.store"),
        platform.map(|p| format!(".{}", slug_of(p).to_lowercase())).unwrap_or_default())));
    doc.insert("subtitle".into(), json!(text(cfg, "subtitle").unwrap_or("Dev builds, built when you ask for them")));
    doc.insert("iconURL".into(), json!(text(cfg, "iconURL").map_or_else(|| url("/altsource/icon.png"), str::to_string)));
    if let Some(site) = text(cfg, "website") {
        doc.insert("website".into(), json!(site));
    }
    doc.insert("tintColor".into(), json!(text(cfg, "tintColor").unwrap_or("#4f8cff")));
    doc.insert("apps".into(), Value::Array(list));
    doc.insert("news".into(), json!([]));
    Value::Object(doc)
}

// ---- the store of built IPAs and the join of requests ----

/// What a build job is doing, as far as this module cares.
#[derive(Clone, Debug, PartialEq)]
pub struct JobView {
    /// `queued`, `running`, `success`, `failed`, `cancelled`, ...: anything but the first two is over.
    pub status: String,
    pub error: Option<String>,
}

impl JobView {
    fn active(&self) -> bool {
        self.status == "queued" || self.status == "running"
    }
}

/// What the Store gives the feed: the live config's rows, a branch head and the build queue. The
/// methods are called while the feed holds its lock, so they must not call back into it.
pub trait Deps: Send + Sync {
    /// `source_apps` of the live config (`None`: the combined list).
    fn apps(&self, platform: Option<&str>) -> Vec<SourceApp>;
    /// `all_source_apps` of the live config.
    fn all_apps(&self) -> Vec<SourceApp>;
    /// The head of the app's source branch.
    fn head(&self, app: &SourceApp) -> Result<Head, String>;
    /// Queue a build of `app` pinned to `commit`; returns the job id.
    fn enqueue(&self, app: &SourceApp, commit: &str) -> u64;
    /// The job, if the queue still knows it.
    fn job(&self, id: u64) -> Option<JobView>;
}

/// What a download request for `slug`@`commit` gets right now.
#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    /// Built: serve `AltSource::file(&entry)`.
    Ready(Entry),
    /// A build is queued or running: wait on this job, then call `AltSource::outcome`.
    Building(u64),
    /// Unknown app (404) or a build that just failed (502).
    Failed { status: u16, error: String },
}

/// A build in flight, for the state view.
#[derive(Clone, Debug, PartialEq)]
pub struct Building {
    /// `<slug>@<commit>`
    pub key: String,
    pub job_id: u64,
}

/// An IPA built elsewhere (a LAN deploy) whose stamp is exactly this commit, so it needn't be
/// built again: `request`'s `adopt` hook.
pub type Adopt<'a> = &'a dyn Fn(&SourceApp, &str) -> Option<PathBuf>;

#[derive(Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    apps: BTreeMap<String, Vec<Entry>>,
}

#[derive(Default)]
struct Inner {
    index: Index,
    /// `<slug>@<commit>` -> build job id
    in_flight: BTreeMap<String, u64>,
    /// `<slug>@<commit>` -> (error, when)
    failed: BTreeMap<String, (String, u64)>,
}

type InspectFn = dyn Fn(&Path) -> Result<IpaInfo, String> + Send + Sync;

pub struct AltSource {
    dir: PathBuf,
    deps: Arc<dyn Deps>,
    inner: Mutex<Inner>,
    inspect: Box<InspectFn>,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

impl AltSource {
    /// The feed's store in `dir` (`<data>/altsource/`, created; `index.json` read if present).
    pub fn new(dir: impl Into<PathBuf>, deps: Arc<dyn Deps>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let index = fs::read_to_string(dir.join("index.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Ok(Self {
            dir,
            deps,
            inner: Mutex::new(Inner { index, ..Inner::default() }),
            inspect: Box::new(inspect_ipa),
            now: Box::new(crate::auth::system_ms),
            log: Box::new(|_| {}),
        })
    }

    pub fn with_log(mut self, log: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.log = Box::new(log);
        self
    }

    /// Replace the clock (milliseconds since 1970), for tests.
    pub fn with_clock(mut self, now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    /// Replace the IPA reader, for tests.
    pub fn with_inspect(mut self, inspect: impl Fn(&Path) -> Result<IpaInfo, String> + Send + Sync + 'static) -> Self {
        self.inspect = Box::new(inspect);
        self
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, inner: &Inner) -> io::Result<()> {
        let path = self.dir.join("index.json");
        let tmp = self.dir.join("index.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&inner.index).unwrap_or_default())?;
        fs::rename(tmp, path)
    }

    /// The app's kept IPAs that are still on disk, newest first.
    fn entries(&self, inner: &Inner, slug: &str) -> Vec<Entry> {
        inner.index.apps.get(slug).into_iter().flatten().filter(|e| self.dir.join(&e.file).exists()).cloned().collect()
    }

    /// The newest kept IPA of the app.
    pub fn latest(&self, slug: &str) -> Option<Entry> {
        self.entries(&self.lock(), slug).into_iter().next()
    }

    /// The kept IPA of that commit (abbreviations of either length match).
    pub fn cached(&self, slug: &str, commit: &str) -> Option<Entry> {
        self.cached_in(&self.lock(), slug, commit)
    }

    fn cached_in(&self, inner: &Inner, slug: &str, commit: &str) -> Option<Entry> {
        self.entries(inner, slug).into_iter().find(|e| same_commit(&e.commit, commit))
    }

    /// Any app a source lists, by slug.
    pub fn app_by_slug(&self, slug: &str) -> Option<SourceApp> {
        self.deps.all_apps().into_iter().find(|a| a.slug == slug)
    }

    /// Where a kept IPA is.
    pub fn file(&self, entry: &Entry) -> PathBuf {
        self.dir.join(&entry.file)
    }

    /// The icon of the newest kept IPA that has one.
    pub fn icon_file(&self, slug: &str) -> Option<PathBuf> {
        let entries = self.entries(&self.lock(), slug);
        entries.iter().find_map(|e| e.icon.as_ref()).map(|i| self.dir.join(i))
    }

    /// Builds in flight (`<slug>@<commit>` and job id). A job that ended is settled first.
    pub fn building(&self) -> Vec<Building> {
        let mut inner = self.lock();
        let keys: Vec<(String, u64)> = inner.in_flight.iter().map(|(k, v)| (k.clone(), *v)).collect();
        for (key, id) in keys {
            self.settle(&mut inner, &key, id);
        }
        inner.in_flight.iter().map(|(k, id)| Building { key: k.clone(), job_id: *id }).collect()
    }

    /// If job `id` of `key` has ended, forget it as in flight and remember why a failure failed.
    /// Returns the job's view. (The JS did this when `waitFor` settled; here the next look does.)
    fn settle(&self, inner: &mut Inner, key: &str, id: u64) -> Option<JobView> {
        let job = self.deps.job(id);
        if job.as_ref().is_some_and(JobView::active) {
            return job;
        }
        if inner.in_flight.get(key) == Some(&id) {
            inner.in_flight.remove(key);
            if let Some(done) = job.as_ref().filter(|j| j.status != "success") {
                let error = format!("build {}: {}", done.status, done.error.as_deref().filter(|e| !e.is_empty()).unwrap_or("no IPA"));
                inner.failed.insert(key.to_string(), (error, (self.now)()));
            }
        }
        job
    }

    /// Keep a copy of a signed IPA as this app's build of `commit`, newest first, `KEEP_PER_APP`
    /// per app. Reads the IPA for the listing: blocking, so run it off the async threads.
    pub fn record(&self, slug: &str, commit: &str, ipa: &Path) -> Result<Entry, String> {
        let meta = (self.inspect)(ipa)?;
        let base = format!("{slug}-{}", slug_of(commit));
        let file = format!("{base}.ipa");
        let target = self.dir.join(&file);
        let tmp = self.dir.join(format!("{file}.tmp"));
        let _ = fs::remove_file(&target);
        fs::copy(ipa, &tmp).and_then(|_| fs::rename(&tmp, &target)).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("could not keep {}: {e}", ipa.display())
        })?;
        let icon = match &meta.icon {
            Some(png) => {
                let name = format!("{base}.png");
                fs::write(self.dir.join(&name), png).map_err(|e| e.to_string())?;
                Some(name)
            }
            None => {
                let _ = fs::remove_file(self.dir.join(format!("{base}.png")));
                None
            }
        };
        let entry = Entry {
            commit: commit.to_string(),
            build_no: meta.build_no.clone(),
            version: meta.version,
            name: meta.name,
            min_os_version: meta.min_os_version,
            permissions: meta.permissions,
            icon,
            size: fs::metadata(&target).map_err(|e| e.to_string())?.len(),
            at: (self.now)(),
            file: file.clone(),
        };
        {
            let mut inner = self.lock();
            let mut list = vec![entry.clone()];
            list.extend(inner.index.apps.remove(slug).unwrap_or_default().into_iter().filter(|e| e.file != file));
            for old in list.iter().skip(KEEP_PER_APP) {
                for f in [Some(&old.file), old.icon.as_ref()].into_iter().flatten() {
                    let _ = fs::remove_file(self.dir.join(f));
                }
            }
            list.truncate(KEEP_PER_APP);
            inner.index.apps.insert(slug.to_string(), list);
            if let Err(e) = self.save(&inner) {
                (self.log)(&format!("altsource: could not write index.json: {e}"));
            }
        }
        let stamp = meta.build_no.strip_suffix("-dirty").unwrap_or(&meta.build_no);
        if !meta.build_no.is_empty() && !same_commit(stamp, commit) {
            (self.log)(&format!("altsource: {slug} built for {commit} is stamped {}", meta.build_no));
        }
        Ok(entry)
    }

    /// What `slug`@`commit` can be answered from what is known: a kept IPA, a build in flight, a
    /// recent failure.
    fn pending(&self, inner: &mut Inner, key: &str, slug: &str, commit: &str) -> Option<Outcome> {
        if let Some(entry) = self.cached_in(inner, slug, commit) {
            return Some(Outcome::Ready(entry));
        }
        if let Some(&id) = inner.in_flight.get(key)
            && self.settle(inner, key, id).is_some_and(|j| j.active())
        {
            return Some(Outcome::Building(id));
        }
        if let Some((error, at)) = inner.failed.get(key).cloned() {
            if (self.now)().saturating_sub(at) < FAILURE_MEMORY_MS {
                return Some(Outcome::Failed { status: 502, error });
            }
            inner.failed.remove(key);
        }
        None
    }

    /// What a download request gets right now: the IPA, the job to wait on (one build is shared by
    /// every request for the same `slug@commit`), a 404 for an unknown slug, or a 502 for a build
    /// that failed in the last two minutes. `adopt` may name an IPA built elsewhere whose stamp is
    /// exactly this commit. Blocking when it adopts (it reads the IPA); run it off the async threads.
    pub fn request(&self, slug: &str, commit: &str, adopt: Option<Adopt>) -> Outcome {
        let Some(app) = self.app_by_slug(slug) else {
            return Outcome::Failed { status: 404, error: "unknown app".into() };
        };
        let key = format!("{slug}@{commit}");
        if let Some(found) = self.pending(&mut self.lock(), &key, slug, commit) {
            return found;
        }
        if let Some(other) = adopt.and_then(|a| a(&app, commit)) {
            match self.record(slug, commit, &other) {
                Ok(entry) => return Outcome::Ready(entry),
                Err(e) => (self.log)(&format!("altsource: could not adopt {}: {e}", other.display())),
            }
        }
        let mut inner = self.lock();
        // Another request may have got here first while this one adopted.
        if let Some(found) = self.pending(&mut inner, &key, slug, commit) {
            return found;
        }
        let id = self.deps.enqueue(&app, commit);
        inner.in_flight.insert(key.clone(), id);
        inner.failed.remove(&key);
        Outcome::Building(id)
    }

    /// After waiting on a build: the IPA, the job again if it still runs, or why there isn't one.
    pub fn outcome(&self, slug: &str, commit: &str, job_id: u64) -> Outcome {
        let mut inner = self.lock();
        if let Some(entry) = self.cached_in(&inner, slug, commit) {
            return Outcome::Ready(entry);
        }
        let job = self.settle(&mut inner, &format!("{slug}@{commit}"), job_id);
        match job {
            Some(j) if j.active() => Outcome::Building(job_id),
            Some(j) => Outcome::Failed { status: 502, error: format!("build {}: {}", j.status, j.error.as_deref().filter(|e| !e.is_empty()).unwrap_or("it produced no IPA")) },
            None => Outcome::Failed { status: 502, error: "build lost".into() },
        }
    }

    /// The source document. `title` is the Store's name for itself (used when `cfg`, the
    /// `altSource` config, has no `name`); `url` signs a path into an absolute link; `platform`
    /// is a platform source's name (`None`: the combined one). The heads are read one after the
    /// other, so call it off the async threads; an app whose head can't be read is left out.
    pub fn source(&self, title: &str, cfg: &Value, platform: Option<&str>, url: &dyn Fn(&str) -> String) -> Value {
        let apps: Vec<FeedApp> = self.deps.apps(platform).into_iter().map(|app| {
            let head = match self.deps.head(&app) {
                Ok(h) => Some(h),
                Err(e) => {
                    (self.log)(&format!("altsource: no head for {}@{}: {e}", app.repo, app.branch));
                    None
                }
            };
            let built = self.latest(&app.slug);
            FeedApp { app, head, built }
        }).collect();
        build_source(cfg, title, &apps, url, platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, io::Write, sync::atomic::{AtomicU64, Ordering}};

    fn config() -> Value {
        json!({
            "altSource": { "enabled": true, "developerName": "Ixion" },
            "repos": [
                { "name": "A", "path": "/a", "branches": ["dev", "main"], "platforms": [
                    { "platform": "iOS", "bundleId": "com.example.a", "device": "Phone" },
                    { "platform": "iPad", "bundleId": "com.example.a", "device": "Pad" },
                    { "platform": "visionOS", "bundleId": "com.example.a.vision", "device": "AVP" },
                    { "platform": "watchOS", "bundleId": "com.example.a.watchkitapp", "device": "Watch" }] },
                { "name": "B", "path": "/b", "branches": ["feature"], "altSource": false, "platforms": [
                    { "platform": "iOS", "bundleId": "com.example.b", "device": "Phone" }] },
                { "name": "C", "path": "/c", "branches": ["x", "y"], "altSource": { "branch": "y", "platforms": ["tvOS"] }, "platforms": [
                    { "platform": "iOS", "bundleId": "com.example.c", "device": "Phone" },
                    { "platform": "tvOS", "bundleId": "com.example.c.tv", "device": "ATV" }] },
            ],
        })
    }

    #[test]
    fn one_app_per_bundle_id_opted_out_repos_left_out_platforms_filterable() {
        let apps = source_apps(&config(), None);
        let rows: Vec<(&str, &str, &str)> = apps.iter().map(|a| (a.bundle_id.as_str(), a.platform.as_str(), a.branch.as_str())).collect();
        assert_eq!(rows, [("com.example.a", "iOS", "main"), ("com.example.a.vision", "visionOS", "main"), ("com.example.c.tv", "tvOS", "y")]);
        assert_eq!(apps[0].developer_name.as_deref(), Some("Ixion"));
        assert_eq!(apps[0].slug, "com.example.a-iOS");
        assert_eq!((apps[0].repo.as_str(), apps[0].path.as_str(), apps[0].device.as_deref()), ("A", "/a", Some("Phone")));
    }

    #[test]
    fn a_watchos_row_is_never_a_feed_app() {
        assert!(!all_source_apps(&config()).iter().any(|a| a.platform == "watchOS"));
    }

    #[test]
    fn one_source_per_offered_platform_name_aliases_included() {
        assert_eq!(source_platforms(&config()), ["iOS", "iPad", "visionOS", "tvOS"]);
    }

    #[test]
    fn a_platform_source_lists_that_platforms_rows_keyed_per_platform() {
        let cfg = config();
        let ids = |p: &str| source_apps(&cfg, Some(p)).into_iter().map(|a| a.bundle_id).collect::<Vec<_>>();
        let pad = source_apps(&cfg, Some("iPad"));
        assert_eq!((pad[0].bundle_id.as_str(), pad[0].device.as_deref(), pad[0].slug.as_str()), ("com.example.a", Some("Pad"), "com.example.a-iPad"));
        assert_eq!(ids("visionOS"), ["com.example.a.vision"]);
        assert_eq!(ids("tvOS"), ["com.example.c.tv"]);
        // C's iOS row is filtered out by its altSource.platforms, B is opted out
        assert_eq!(ids("iOS"), ["com.example.a"]);
        assert!(ids("watchOS").is_empty());
    }

    #[test]
    fn every_slug_any_source_lists_can_be_downloaded() {
        let mut slugs: Vec<String> = all_source_apps(&config()).into_iter().map(|a| a.slug).collect();
        slugs.sort();
        assert_eq!(slugs, ["com.example.a-iOS", "com.example.a-iPad", "com.example.a.vision-visionOS", "com.example.c.tv-tvOS"]);
    }

    #[test]
    fn platform_sources_get_their_own_name_and_identifier() {
        let cfg = json!({ "name": "N", "identifier": "x.y" });
        let doc = build_source(&cfg, "T", &[], &|p| p.into(), Some("visionOS"));
        assert_eq!(doc["name"], "N (visionOS)");
        assert_eq!(doc["identifier"], "x.y.visionos");
        assert_eq!(build_source(&json!({ "identifier": "x.y" }), "T", &[], &|p| p.into(), None)["identifier"], "x.y");
    }

    #[test]
    fn branch_own_then_source_wide_if_offered_then_main_master_then_first() {
        assert_eq!(source_branch(&json!({ "branches": ["a", "b"], "altSource": { "branch": "b" } }), &Value::Null), "b");
        assert_eq!(source_branch(&json!({ "branches": ["a", "b"] }), &json!({ "branch": "b" })), "b");
        assert_eq!(source_branch(&json!({ "branches": ["a", "master"] }), &json!({ "branch": "zzz" })), "master");
        assert_eq!(source_branch(&json!({ "branches": ["a", "b"] }), &Value::Null), "a");
        assert_eq!(source_branch(&json!({}), &Value::Null), "main");
    }

    #[test]
    fn short_shas_of_different_lengths_match() {
        assert!(same_commit("abc1234", "abc12345"));
        assert!(!same_commit("abc1234", "abd1234"));
        assert!(!same_commit("", "abc"));
    }

    #[test]
    fn slugs_collapse_runs_of_other_characters() {
        assert_eq!(slug_of("com.example.a iOS"), "com.example.a-iOS");
        assert_eq!(slug_of("a/../b  c"), "a-..-b-c");
    }

    fn url(p: &str) -> String {
        format!("https://h{p}?t=x")
    }

    fn app() -> SourceApp {
        SourceApp { slug: "com.example.a".into(), repo: "A".into(), bundle_id: "com.example.a".into(), platform: "iOS".into(), branch: "main".into(), ..SourceApp::default() }
    }

    fn perms() -> Permissions {
        Permissions { entitlements: vec!["get-task-allow".into()], privacy: BTreeMap::from([("NSCameraUsageDescription".to_string(), "cam".to_string())]) }
    }

    fn built(commit: &str, build_no: &str) -> Entry {
        Entry { commit: commit.into(), build_no: build_no.into(), version: "1.2".into(), name: "Apple".into(), min_os_version: None, permissions: perms(), file: "f.ipa".into(), icon: None, size: 42, at: 0 }
    }

    #[test]
    fn a_built_head_lists_its_real_stamp_permissions_and_size() {
        let head = Head { commit: "abc1234".into(), date: "2026-10-11T10:00:00+02:00".into(), subject: "Fix it".into() };
        let doc = build_source(&Value::Null, "T", &[FeedApp { app: app(), head: Some(head), built: Some(built("abc1234", "abc1234")) }], &url, None);
        let a = &doc["apps"][0];
        assert_eq!(doc["name"], "T");
        assert_eq!(a["name"], "Apple");
        assert_eq!(a["versions"][0], json!({
            "version": "1.2", "buildVersion": "abc1234", "date": "2026-10-11T10:00:00+02:00",
            "localizedDescription": "Fix it", "downloadURL": "https://h/altsource/com.example.a/abc1234.ipa?t=x", "size": 42,
        }));
        assert_eq!(a["appPermissions"], json!({ "entitlements": ["get-task-allow"], "privacy": { "NSCameraUsageDescription": "cam" } }));
        assert_eq!(a["downloadURL"], a["versions"][0]["downloadURL"]); // legacy layout
        assert_eq!(a["iconURL"], "https://h/altsource/com.example.a/icon.png?t=x");
        assert_eq!(a["subtitle"], "A \u{b7} main");
        assert_eq!(a["category"], "developer");
        assert_eq!(a["developerName"], "Me");
    }

    #[test]
    fn an_unbuilt_head_offers_its_sha_with_the_previous_builds_version_and_permissions() {
        let head = Head { commit: "def5678".into(), date: "d".into(), subject: "s".into() };
        let mut prev = built("abc1234", "abc1234-dirty");
        prev.min_os_version = Some("17.0".into());
        let doc = build_source(&Value::Null, "T", &[FeedApp { app: app(), head: Some(head), built: Some(prev) }], &url, None);
        let v = &doc["apps"][0]["versions"][0];
        assert_eq!(v["buildVersion"], "def5678");
        assert_eq!(v["version"], "1.2");
        assert_eq!(v["minOSVersion"], "17.0");
        assert!(v["downloadURL"].as_str().unwrap().contains("def5678.ipa"));
        assert_eq!(doc["apps"][0]["appPermissions"]["entitlements"], json!(["get-task-allow"]));
    }

    #[test]
    fn never_built_is_version_1_0_with_empty_permissions_and_no_head_is_left_out() {
        let head = Head { commit: "a1b2c3d".into(), date: "d".into(), subject: String::new() };
        let other = SourceApp { slug: "x".into(), ..app() };
        let doc = build_source(&Value::Null, "T", &[FeedApp { app: app(), head: Some(head), built: None }, FeedApp { app: other, head: None, built: None }], &url, None);
        assert_eq!(doc["apps"].as_array().unwrap().len(), 1);
        assert_eq!(doc["apps"][0]["versions"][0]["version"], "1.0");
        assert_eq!(doc["apps"][0]["appPermissions"], json!({ "entitlements": [], "privacy": {} }));
    }

    #[test]
    fn per_repo_options_reach_the_listing() {
        let cfg = json!({ "name": "Src", "website": "https://w", "tintColor": "#000", "developerName": "Dev" });
        let a = SourceApp { description: Some("D".into()), subtitle: Some("S".into()), icon_url: Some("https://i".into()), category: Some("games".into()), ..app() };
        let head = Head { commit: "a1b2c3d".into(), ..Head::default() };
        let doc = build_source(&cfg, "T", &[FeedApp { app: a, head: Some(head), built: None }], &url, None);
        assert_eq!((doc["name"].as_str(), doc["website"].as_str(), doc["tintColor"].as_str()), (Some("Src"), Some("https://w"), Some("#000")));
        let a = &doc["apps"][0];
        assert_eq!((a["localizedDescription"].as_str(), a["subtitle"].as_str(), a["iconURL"].as_str(), a["category"].as_str(), a["developerName"].as_str()),
            (Some("D"), Some("S"), Some("https://i"), Some("games"), Some("Dev")));
        assert!(build_source(&Value::Null, "T", &[], &url, None).get("website").is_none());
    }

    // ---- requests, with a fake queue ----

    #[derive(Default)]
    struct Fake {
        jobs: Mutex<HashMap<u64, JobView>>,
        enqueued: Mutex<Vec<String>>,
        next: AtomicU64,
        head: Mutex<Option<Head>>,
    }

    impl Deps for Fake {
        fn apps(&self, platform: Option<&str>) -> Vec<SourceApp> {
            if platform.is_some_and(|p| p != "iOS") { Vec::new() } else { vec![app()] }
        }
        fn all_apps(&self) -> Vec<SourceApp> {
            vec![app()]
        }
        fn head(&self, _: &SourceApp) -> Result<Head, String> {
            self.head.lock().unwrap().clone().ok_or_else(|| "no such branch".into())
        }
        fn enqueue(&self, _: &SourceApp, commit: &str) -> u64 {
            let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
            self.jobs.lock().unwrap().insert(id, JobView { status: "queued".into(), error: None });
            self.enqueued.lock().unwrap().push(commit.into());
            id
        }
        fn job(&self, id: u64) -> Option<JobView> {
            self.jobs.lock().unwrap().get(&id).cloned()
        }
    }

    struct Harness {
        s: AltSource,
        fake: Arc<Fake>,
        ipa: PathBuf,
        clock: Arc<AtomicU64>,
        dir: PathBuf,
        _tmp: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dir = tmp.path().join("altsource");
            let fake = Arc::new(Fake::default());
            let clock = Arc::new(AtomicU64::new(1_000_000));
            let c = clock.clone();
            let s = AltSource::new(&dir, fake.clone()).unwrap()
                .with_clock(move || c.load(Ordering::SeqCst))
                .with_inspect(|_| Ok(IpaInfo {
                    name: "A".into(), version: "1.0".into(), build_no: "abc1234".into(),
                    icon: Some(b"png".to_vec()), ..IpaInfo::default()
                }));
            let ipa = tmp.path().join("in.ipa");
            fs::write(&ipa, "ipa").unwrap();
            Self { s, fake, ipa, clock, dir, _tmp: tmp }
        }

        /// The job ends, however it ends; a success has recorded its IPA first.
        fn finish(&self, id: u64, status: &str, record: bool) {
            if record {
                self.s.record("com.example.a", "abc1234", &self.ipa).unwrap();
            }
            let error = (status == "failed").then(|| "boom".to_string());
            self.fake.jobs.lock().unwrap().insert(id, JobView { status: status.into(), error });
        }

        fn enqueued(&self) -> Vec<String> {
            self.fake.enqueued.lock().unwrap().clone()
        }
    }

    #[test]
    fn unknown_apps_are_404() {
        let h = Harness::new();
        assert_eq!(h.s.request("nope", "abc1234", None), Outcome::Failed { status: 404, error: "unknown app".into() });
    }

    #[test]
    fn concurrent_requests_for_one_commit_share_one_build_then_get_the_ipa() {
        let h = Harness::new();
        assert_eq!(h.s.request("com.example.a", "abc1234", None), Outcome::Building(1));
        assert_eq!(h.s.request("com.example.a", "abc1234", None), Outcome::Building(1));
        assert_eq!(h.enqueued(), ["abc1234"]);
        assert_eq!(h.s.building(), [Building { key: "com.example.a@abc1234".into(), job_id: 1 }]);
        assert_eq!(h.s.outcome("com.example.a", "abc1234", 1), Outcome::Building(1));
        h.fake.jobs.lock().unwrap().get_mut(&1).unwrap().status = "running".into();
        h.finish(1, "success", true);
        let Outcome::Ready(entry) = h.s.outcome("com.example.a", "abc1234", 1) else { panic!("not ready") };
        assert_eq!(fs::read_to_string(h.s.file(&entry)).unwrap(), "ipa");
        assert!(matches!(h.s.request("com.example.a", "abc1234", None), Outcome::Ready(_)));
        assert_eq!(fs::read_to_string(h.s.icon_file("com.example.a").unwrap()).unwrap(), "png");
        assert!(h.s.building().is_empty());
    }

    #[test]
    fn a_different_abbreviation_of_a_built_commit_is_served() {
        let h = Harness::new();
        h.s.record("com.example.a", "abc1234", &h.ipa).unwrap();
        assert!(matches!(h.s.request("com.example.a", "abc12345", None), Outcome::Ready(_)));
    }

    #[test]
    fn a_failed_build_is_reported_not_retried_straight_away() {
        let h = Harness::new();
        h.s.request("com.example.a", "abc1234", None);
        h.finish(1, "failed", false);
        let Outcome::Failed { status, error } = h.s.outcome("com.example.a", "abc1234", 1) else { panic!("not failed") };
        assert_eq!(status, 502);
        assert!(error.contains("build failed: boom"), "{error}");
        assert_eq!(h.s.request("com.example.a", "abc1234", None), Outcome::Failed { status: 502, error: "build failed: boom".into() });
        assert_eq!(h.enqueued().len(), 1);
        // two minutes on, a request starts a new build
        h.clock.fetch_add(FAILURE_MEMORY_MS + 1, Ordering::SeqCst);
        assert_eq!(h.s.request("com.example.a", "abc1234", None), Outcome::Building(2));
        assert_eq!(h.enqueued().len(), 2);
    }

    #[test]
    fn a_failure_is_noticed_by_the_next_request_even_if_nobody_waited() {
        let h = Harness::new();
        h.s.request("com.example.a", "abc1234", None);
        h.finish(1, "cancelled", false);
        let Outcome::Failed { status: 502, error } = h.s.request("com.example.a", "abc1234", None) else { panic!("not failed") };
        assert!(error.starts_with("build cancelled"), "{error}");
        assert_eq!(h.enqueued().len(), 1);
    }

    #[test]
    fn a_build_the_queue_lost_is_reported_and_retried() {
        let h = Harness::new();
        h.s.request("com.example.a", "abc1234", None);
        h.fake.jobs.lock().unwrap().clear();
        assert_eq!(h.s.outcome("com.example.a", "abc1234", 1), Outcome::Failed { status: 502, error: "build lost".into() });
        assert_eq!(h.s.request("com.example.a", "abc1234", None), Outcome::Building(2));
    }

    #[test]
    fn an_ipa_already_built_for_the_commit_elsewhere_is_adopted() {
        let h = Harness::new();
        let ipa = h.ipa.clone();
        let adopt = move |_: &SourceApp, _: &str| Some(ipa.clone());
        assert!(matches!(h.s.request("com.example.a", "abc1234", Some(&adopt)), Outcome::Ready(_)));
        assert!(h.enqueued().is_empty());
    }

    #[test]
    fn an_adoption_that_cannot_be_read_falls_back_to_a_build() {
        let h = Harness::new();
        let s = AltSource::new(&h.dir, h.fake.clone()).unwrap().with_inspect(|_| Err("broken".into()));
        let adopt = |_: &SourceApp, _: &str| Some(PathBuf::from("/nonexistent.ipa"));
        assert_eq!(s.request("com.example.a", "abc1234", Some(&adopt)), Outcome::Building(1));
    }

    #[test]
    fn keeps_two_builds_per_app() {
        let h = Harness::new();
        for c in ["aaaa111", "bbbb222", "cccc333"] {
            h.s.record("com.example.a", c, &h.ipa).unwrap();
        }
        assert!(h.s.cached("com.example.a", "cccc333").is_some());
        assert!(h.s.cached("com.example.a", "bbbb222").is_some());
        assert!(h.s.cached("com.example.a", "aaaa111").is_none());
        assert!(!h.dir.join("com.example.a-aaaa111.ipa").exists());
        assert!(!h.dir.join("com.example.a-aaaa111.png").exists());
        assert_eq!(h.s.latest("com.example.a").unwrap().commit, "cccc333");
    }

    #[test]
    fn the_index_survives_a_restart_and_a_deleted_file_drops_out() {
        let h = Harness::new();
        let entry = h.s.record("com.example.a", "abc1234", &h.ipa).unwrap();
        let again = AltSource::new(&h.dir, h.fake.clone()).unwrap();
        assert_eq!(again.cached("com.example.a", "abc1234"), Some(entry.clone()));
        fs::remove_file(again.file(&entry)).unwrap();
        assert!(again.cached("com.example.a", "abc1234").is_none());
    }

    #[test]
    fn the_index_keeps_the_shape_the_node_store_wrote() {
        let h = Harness::new();
        fs::write(h.dir.join("com.example.a-abc1234.ipa"), "ipa").unwrap();
        fs::write(h.dir.join("index.json"), r#"{"apps":{"com.example.a":[{"commit":"abc1234","buildNo":"abc1234","version":"2.0","name":"A",
            "minOSVersion":"17.0","permissions":{"entitlements":["x"],"privacy":{"NSCameraUsageDescription":"c"}},
            "file":"com.example.a-abc1234.ipa","icon":null,"size":3,"at":1}]}}"#).unwrap();
        let s = AltSource::new(&h.dir, h.fake.clone()).unwrap().with_inspect(|_| Ok(IpaInfo::default()));
        let e = s.latest("com.example.a").unwrap();
        assert_eq!((e.version.as_str(), e.min_os_version.as_deref(), e.permissions.entitlements.len()), ("2.0", Some("17.0"), 1));
        s.record("com.example.a", "def5678", &h.ipa).unwrap();
        let written: Value = serde_json::from_str(&fs::read_to_string(h.dir.join("index.json")).unwrap()).unwrap();
        assert_eq!(written["apps"]["com.example.a"][0]["commit"], "def5678");
        assert_eq!(written["apps"]["com.example.a"][1]["buildNo"], "abc1234");
        assert_eq!(written["apps"]["com.example.a"][1]["minOSVersion"], "17.0");
    }

    #[test]
    fn source_lists_heads_and_leaves_out_apps_without_one() {
        let h = Harness::new();
        let cfg = json!({ "name": "Test Source" });
        assert!(h.s.source("T", &cfg, None, &url)["apps"].as_array().unwrap().is_empty());
        *h.fake.head.lock().unwrap() = Some(Head { commit: "abc1234".into(), date: "d".into(), subject: "s".into() });
        let doc = h.s.source("T", &cfg, None, &url);
        assert_eq!(doc["name"], "Test Source");
        assert_eq!(doc["apps"][0]["versions"][0]["buildVersion"], "abc1234");
        h.s.record("com.example.a", "abc1234", &h.ipa).unwrap();
        let doc = h.s.source("T", &cfg, None, &url);
        assert_eq!(doc["apps"][0]["name"], "A");
        assert_eq!(doc["apps"][0]["versions"][0]["size"], 3);
        assert_eq!(h.s.source("T", &cfg, Some("tvOS"), &url)["apps"].as_array().unwrap().len(), 0);
        assert_eq!(h.s.source("T", &cfg, Some("iOS"), &url)["identifier"], "local.apparition.store.ios");
    }

    // ---- reading a synthetic IPA ----

    fn entitlements_xml(keys: &[&str]) -> String {
        let body: String = keys.iter().map(|k| format!("<key>{k}</key><true/>")).collect();
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>{body}</dict></plist>\n")
    }

    /// A thin arm64 Mach-O whose code signature (after `pad` bytes of "code") holds `xml` as its
    /// entitlements, laid out as codesign does: a SuperBlob with the entitlements in slot 5.
    fn macho(xml: &str, pad: usize) -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend(0xfade_7171u32.to_be_bytes());
        blob.extend((8 + xml.len() as u32).to_be_bytes());
        blob.extend(xml.as_bytes());
        let mut sig = Vec::new();
        sig.extend(0xfade_0cc0u32.to_be_bytes());
        sig.extend((20 + blob.len() as u32).to_be_bytes());
        sig.extend(1u32.to_be_bytes());
        sig.extend(5u32.to_be_bytes());
        sig.extend(20u32.to_be_bytes());
        sig.extend(blob);
        let mut out = Vec::new();
        for word in [0xfeed_facfu32, 0x0100_000c, 0, 2, 1, 16, 0, 0] {
            out.extend(word.to_le_bytes());
        }
        for word in [0x1du32, 16, (32 + 16 + pad) as u32, sig.len() as u32] {
            out.extend(word.to_le_bytes());
        }
        out.resize(32 + 16 + pad, 0);
        out.extend(sig);
        out
    }

    /// `thin` as the arm64 slice of a fat file, after an x86_64 one.
    fn fat(thin: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(0xcafe_babeu32.to_be_bytes());
        out.extend(2u32.to_be_bytes());
        for (cpu, offset, size) in [(0x0100_0007u32, 4096u32, 8u32), (0x0100_000c, 8192, thin.len() as u32)] {
            for word in [cpu, 0, offset, size, 12] {
                out.extend(word.to_be_bytes());
            }
        }
        out.resize(4096, 0);
        out.extend([0u8; 8]);
        out.resize(8192, 0);
        out.extend(thin);
        out
    }

    fn info_plist(entries: &[(&str, &str)]) -> Vec<u8> {
        let body: String = entries.iter().map(|(k, v)| format!("<key>{k}</key><string>{v}</string>")).collect();
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>{body}</dict></plist>").into_bytes()
    }

    fn write_ipa(path: &Path, files: &[(&str, Vec<u8>)]) {
        let mut zip = zip::ZipWriter::new(fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in files {
            zip.start_file(*name, options).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }

    fn sample_files() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("Payload/Test.app/Info.plist", info_plist(&[
                ("CFBundleIdentifier", "com.example.test"), ("CFBundleName", "Test"), ("CFBundleExecutable", "Test"),
                ("CFBundleShortVersionString", "2.0"), ("CFBundleVersion", "abc1234-dirty"), ("MinimumOSVersion", "17.0"),
                ("NSCameraUsageDescription", "to see"), ("CFBundleDisplayName", "Test App")])),
            ("Payload/Test.app/Test", macho(&entitlements_xml(&["application-identifier", "get-task-allow", "com.apple.security.application-groups"]), 300_000)),
            ("Payload/Test.app/AppIcon60x60@2x.png", b"small".to_vec()),
            ("Payload/Test.app/AppIcon60x60@3x.png", b"largest".to_vec()),
            ("Payload/Test.app/AppIcon76x76~ipad.png", b"ipad".to_vec()),
            ("Payload/Test.app/AppIcon.notpng", b"x".to_vec()),
            ("Payload/Test.app/Assets.car", b"car".to_vec()),
            ("Payload/Test.app/PlugIns/Share.appex/Info.plist", info_plist(&[
                ("CFBundleExecutable", "Share"), ("NSPhotoLibraryUsageDescription", "to share"), ("NSCameraUsageDescription", "ignored: the app's wins")])),
            ("Payload/Test.app/PlugIns/Share.appex/Share", fat(&macho(&entitlements_xml(&["com.apple.developer.team-identifier", "com.apple.security.app-sandbox"]), 10))),
            // inside a framework: never entered
            ("Payload/Test.app/Frameworks/F.framework/Plugin.appex/Info.plist", info_plist(&[("NSMicrophoneUsageDescription", "no")])),
        ]
    }

    #[test]
    fn inspects_version_permissions_and_the_largest_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let ipa = tmp.path().join("t.ipa");
        write_ipa(&ipa, &sample_files());
        let info = inspect_ipa(&ipa).unwrap();
        assert_eq!((info.name.as_str(), info.version.as_str(), info.build_no.as_str()), ("Test App", "2.0", "abc1234-dirty"));
        assert_eq!(info.bundle_id.as_deref(), Some("com.example.test"));
        assert_eq!(info.min_os_version.as_deref(), Some("17.0"));
        assert_eq!(info.permissions.entitlements, ["com.apple.security.app-sandbox", "com.apple.security.application-groups", "get-task-allow"]);
        assert_eq!(info.permissions.privacy, BTreeMap::from([
            ("NSCameraUsageDescription".to_string(), "to see".to_string()),
            ("NSPhotoLibraryUsageDescription".to_string(), "to share".to_string()),
        ]));
        assert_eq!(info.icon.as_deref(), Some(&b"largest"[..]));
    }

    #[test]
    fn an_unsigned_app_with_a_bare_plist_still_lists_its_entitlements() {
        let tmp = tempfile::tempdir().unwrap();
        let ipa = tmp.path().join("t.ipa");
        let mut binary = vec![7u8; 5000];
        binary.extend(entitlements_xml(&["get-task-allow"]).into_bytes());
        binary.extend([9u8; 100]);
        write_ipa(&ipa, &[
            ("Payload/B.app/Info.plist", info_plist(&[("CFBundleExecutable", "B")])),
            ("Payload/B.app/B", binary),
        ]);
        let info = inspect_ipa(&ipa).unwrap();
        assert_eq!(info.permissions.entitlements, ["get-task-allow"]);
        // defaults: name from the folder, version 1.0, no build number, no icon
        assert_eq!((info.name.as_str(), info.version.as_str(), info.build_no.as_str()), ("B", "1.0", ""));
        assert!(info.icon.is_none() && info.min_os_version.is_none());
    }

    #[test]
    fn an_executable_without_a_signature_has_no_entitlements() {
        let tmp = tempfile::tempdir().unwrap();
        let ipa = tmp.path().join("t.ipa");
        write_ipa(&ipa, &[("Payload/B.app/Info.plist", info_plist(&[("CFBundleExecutable", "B")])), ("Payload/B.app/B", vec![0u8; 100])]);
        assert!(inspect_ipa(&ipa).unwrap().permissions.entitlements.is_empty());
    }

    #[test]
    fn a_zip_without_an_app_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let ipa = tmp.path().join("t.ipa");
        write_ipa(&ipa, &[("Payload/readme.txt", b"x".to_vec())]);
        assert!(inspect_ipa(&ipa).unwrap_err().contains("no .app"));
        fs::write(&ipa, "not a zip").unwrap();
        assert!(inspect_ipa(&ipa).unwrap_err().contains("could not unpack"));
    }

    #[test]
    fn record_reads_a_real_ipa_for_the_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let ipa = tmp.path().join("t.ipa");
        write_ipa(&ipa, &sample_files());
        let s = AltSource::new(tmp.path().join("altsource"), Arc::new(Fake::default())).unwrap();
        let entry = s.record("com.example.a", "abc1234", &ipa).unwrap();
        assert_eq!((entry.version.as_str(), entry.name.as_str(), entry.min_os_version.as_deref()), ("2.0", "Test App", Some("17.0")));
        assert_eq!(entry.size, fs::metadata(&ipa).unwrap().len());
        assert_eq!(fs::read(s.icon_file("com.example.a").unwrap()).unwrap(), b"largest");
        assert_eq!(fs::read(s.file(&entry)).unwrap(), fs::read(&ipa).unwrap());
    }

    #[test]
    fn icon_names() {
        for (name, px) in [("AppIcon60x60@2x.png", 120.0), ("AppIcon60x60@3x.png", 180.0), ("AppIcon76x76~ipad.png", 76.0),
            ("AppIcon83.5x83.5@2x~ipad.png", 167.0), ("AppIcon1024x1024.png", 1024.0)] {
            assert_eq!(app_icon_pixels(name), Some(px), "{name}");
        }
        for name in ["AppIcon.png", "AppIcon60x60@2x.jpg", "AppIcon60x60@x.png", "Icon60x60.png", "AppIcon60.png"] {
            assert_eq!(app_icon_pixels(name), None, "{name}");
        }
    }

    #[test]
    fn bundles_are_found_to_five_levels_and_not_inside_frameworks() {
        let names: Vec<String> = ["Payload/A.app/Info.plist", "Payload/A.app/PlugIns/S.appex/Info.plist", "Payload/A.app/Watch/W.app/Info.plist",
            "Payload/A.app/Watch/W.app/PlugIns/X.appex/Info.plist", "Payload/A.app/Frameworks/F.framework/Y.appex/Info.plist",
            "Payload/A.app/en.lproj/Z.appex/Info.plist", "Payload/Other.app/Info.plist"].iter().map(|s| s.to_string()).collect();
        assert_eq!(bundle_roots(&names), ["Payload/A.app", "Payload/A.app/PlugIns/S.appex", "Payload/A.app/Watch/W.app", "Payload/A.app/Watch/W.app/PlugIns/X.appex"]);
    }
}
