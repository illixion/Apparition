//! Tweaks injected into an app before it's signed, the way jailed injectors (cyan, Feather,
//! Sideloadly) do it: the tweak's code goes into the app's Frameworks/, the app's executable
//! gets a weak load command for it, and the paths a jailbreak would have provided are pointed
//! inside the app. Signing then signs it all like any other code in the app.
//!
//! What goes in, from a `.deb` package, a `.dylib`, a `.framework` or `.bundle` folder, or a
//! `.zip` of those:
//! - dylibs (`Library/MobileSubstrate/DynamicLibraries/`, `usr/lib/TweakInject/`, `usr/lib/`)
//!   into `Frameworks/`; each one no other copied code links is loaded by the app;
//! - frameworks (`Library/Frameworks/`) into `Frameworks/`, loaded only when given on their own
//!   (a package's frameworks are what its tweak links);
//! - bundles (`Library/Application Support/`) into the app's root. Code that looks them up at
//!   their jailbreak path won't find them there, which is said in a warning.
//!
//! The rootless layout (`var/jb/…`) is read the same way. Substrate's filter plists are
//! ignored (the tweak is loaded into this one app regardless), and so are Settings panes
//! (PreferenceLoader isn't there), with a warning.
//!
//! Substrate itself, which most tweaks link, is ElleKit (BSD-3-Clause, `assets/ellekit/`): any
//! copied code that links Substrate, libhooker or blackjack is pointed at
//! `@rpath/CydiaSubstrate.framework/CydiaSubstrate`, and that framework is added from ElleKit.
//!
//! Refused: an app still encrypted by the App Store (its code can't be read, so nothing would
//! run), code built for another platform than the app's (an iOS tweak in a visionOS app), a
//! name the app already has, and an executable with no room for another load command.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
};

use rootcause::prelude::*;
use serde::Serialize;

use crate::macho;

/// ElleKit 1.1.3's libellekit.dylib, as `scripts/build-ellekit.sh` builds it, and its licence.
const ELLEKIT: &[u8] = include_bytes!("../assets/ellekit/libellekit.dylib");
const ELLEKIT_LICENSE: &str = include_str!("../assets/ellekit/LICENSE");
/// The build `scripts/build-ellekit.sh` makes (and checks) from ElleKit's v1.1.3 tag.
pub const ELLEKIT_SHA256: &str = "aae8eaf6eac08cfea6acece35426da6221198749cd4b625a4e38163aa6212dc8";
const ELLEKIT_VERSION: &str = "1.1.3";

const SUBSTRATE: &str = "@rpath/CydiaSubstrate.framework/CydiaSubstrate";
const RPATH: &str = "@executable_path/Frameworks";

/// What [`inject_app`] did.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Injection {
    /// What the app now loads, as its load commands name it (`@rpath/Foo.dylib`).
    pub loads: Vec<String>,
    /// What was added to the app, relative to it (`Frameworks/Foo.dylib`, `Foo.bundle`).
    pub added: Vec<String>,
    /// Whether ElleKit was added as Substrate.
    pub substrate: bool,
    /// Library paths pointed inside the app.
    pub rewritten: usize,
    /// What may not work, in words.
    pub warnings: Vec<String>,
}

/// One thing to copy into the app, gathered from the tweaks.
#[derive(Debug)]
enum Item {
    /// A file for `Frameworks/<name>`.
    Dylib { name: String, source: PathBuf, given: bool },
    /// A folder for `Frameworks/<name>`.
    Framework { name: String, source: PathBuf, given: bool },
    /// A folder for the app's root.
    Bundle { name: String, source: PathBuf },
}

impl Item {
    fn destination(&self) -> String {
        match self {
            Item::Dylib { name, .. } | Item::Framework { name, .. } => format!("Frameworks/{name}"),
            Item::Bundle { name, .. } => name.clone(),
        }
    }
}

/// Injects `tweaks` into the app inside the IPA `ipa`, writing the result to `out` (another IPA,
/// unsigned like the first). `scratch` holds the unpacked app meanwhile and is emptied after.
pub fn inject_ipa(ipa: &Path, tweaks: &[PathBuf], out: &Path, scratch: &Path) -> Result<Injection, Report> {
    let work = scratch.join("tweak-injection");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let result = (|| {
        let unpacked = work.join("app");
        let mut archive = zip::ZipArchive::new(std::fs::File::open(ipa).context("Opening the app failed")?).context("This app isn't a readable IPA")?;
        archive.extract(&unpacked).context("Unpacking the app failed")?;
        let app = std::fs::read_dir(unpacked.join("Payload"))
            .context("This IPA has no Payload folder")?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "app") && path.is_dir())
            .ok_or_else(|| report!("This IPA has no app in its Payload folder"))?;
        let injection = inject_app(&app, tweaks, &work.join("tweaks"))?;
        let _ = std::fs::remove_file(out);
        // Unchanged files are copied from the original IPA as they are, compressed already.
        crate::ipa::pack(&app, Some(ipa), out).context("Packing the app again failed")?;
        Ok(injection)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// Injects `tweaks` into the unpacked, unsigned `app` in place. `scratch` holds unpacked
/// packages meanwhile.
pub fn inject_app(app: &Path, tweaks: &[PathBuf], scratch: &Path) -> Result<Injection, Report> {
    if tweaks.is_empty() {
        return Ok(Injection::default());
    }
    let mut injection = Injection::default();
    let executable = main_executable(app)?;
    let mut main = std::fs::read(&executable).context("Reading the app's executable failed")?;
    let main_slices = macho::read_all(&main).context("The app's executable isn't one Apparition can change")?;
    if main_slices.iter().any(macho::Slice::encrypted) {
        bail!(
            "This app is still encrypted by the App Store, so a tweak can't be added to it (and its code wouldn't run \
             re-signed anyway). Tweaks go into an app that isn't encrypted."
        );
    }
    let platform = main_slices.iter().find_map(macho::Slice::platform).unwrap_or(macho::PLATFORM_IOS);

    // Gather everything first, so one package's framework can satisfy another's tweak.
    let mut items = Vec::new();
    for (index, tweak) in tweaks.iter().enumerate() {
        gather(tweak, &scratch.join(format!("{index}")), &mut items, &mut injection.warnings)
            .context(format!("Reading {} failed", display_name(tweak)))?;
    }
    if items.is_empty() {
        bail!("There's nothing to inject in {}: no dylib, framework or bundle", names(tweaks));
    }
    let mut taken = BTreeSet::new();
    for item in &items {
        let destination = item.destination();
        if !taken.insert(destination.clone()) {
            bail!("Two of the tweaks hold {destination}: inject one of them");
        }
        if app.join(&destination).exists() {
            bail!("The app already has {destination}; it may already have this tweak in it");
        }
    }

    // Copy the items in, then fix the libraries their code links.
    let copied_dylibs: BTreeSet<String> = items.iter().filter_map(|i| if let Item::Dylib { name, .. } = i { Some(name.clone()) } else { None }).collect();
    let copied_frameworks: BTreeSet<String> = items.iter().filter_map(|i| if let Item::Framework { name, .. } = i { Some(name.clone()) } else { None }).collect();
    std::fs::create_dir_all(app.join("Frameworks"))?;
    let mut code = Vec::new();
    for item in &items {
        let target = app.join(item.destination());
        match item {
            Item::Dylib { source, .. } => {
                std::fs::copy(source, &target)?;
                code.push(target.clone());
            }
            Item::Framework { source, .. } | Item::Bundle { source, .. } => {
                copy_dir(source, &target)?;
                code.extend(machos_in(&target));
            }
        }
        injection.added.push(item.destination());
    }
    // Every library any copied code links, as the app will now find it, by the file that links it.
    let mut linked: BTreeSet<String> = BTreeSet::new();
    let mut needs_substrate = false;
    for file in &code {
        let mut data = std::fs::read(file)?;
        let mut unresolved = BTreeSet::new();
        let platform_ok = macho::read_all(&data)
            .context(format!("{} isn't code Apparition can read", relative(app, file)))?
            .iter()
            .all(|s| s.platform().is_none_or(|p| compatible(p, platform)));
        if !platform_ok {
            bail!("{} is built for another platform than this app ({}), so it can't run in it", relative(app, file), platform_name(platform));
        }
        macho::edit(&mut data, |slice| {
            let mut changed = false;
            for (index, path) in slice.dylibs() {
                if let Some(new) = rewrite(&path, &copied_dylibs, &copied_frameworks) {
                    if new == SUBSTRATE {
                        needs_substrate = true;
                    }
                    if new != path {
                        slice.set_dylib_path(index, &new);
                        injection.rewritten += 1;
                        changed = true;
                    }
                    linked.insert(new);
                } else if let Some(warning) = missing(&path) {
                    unresolved.insert(warning);
                }
            }
            Ok(changed)
        })
        .context(format!("Changing {} failed", relative(app, file)))?;
        std::fs::write(file, &data)?;
        for warning in unresolved {
            injection.warnings.push(format!("{} {warning}", relative(app, file)));
        }
    }
    dedup(&mut injection.warnings);

    if needs_substrate && !app.join("Frameworks/CydiaSubstrate.framework").exists() {
        if !compatible(macho::PLATFORM_IOS, platform) {
            bail!("The tweak needs Substrate, and the ElleKit Apparition has is built for iOS, not {}", platform_name(platform));
        }
        add_ellekit(app)?;
        injection.substrate = true;
        injection.added.push("Frameworks/CydiaSubstrate.framework".into());
    }

    // What the app loads: every given or tweak dylib, and every given framework, that no other
    // copied code already links.
    let mut loads = Vec::new();
    for item in &items {
        let load = match item {
            Item::Dylib { name, given, .. } if *given || !linked.contains(&format!("@rpath/{name}")) => format!("@rpath/{name}"),
            Item::Framework { name, source, given: true } => {
                let executable = framework_executable(source).unwrap_or_else(|| name.trim_end_matches(".framework").to_string());
                let load = format!("@rpath/{name}/{executable}");
                if linked.contains(&load) {
                    continue;
                }
                load
            }
            _ => continue,
        };
        loads.push(load);
    }
    if loads.is_empty() {
        bail!("Nothing in {} is code the app can load", names(tweaks));
    }
    macho::edit(&mut main, |slice| {
        slice.add_rpath(RPATH);
        for load in &loads {
            slice.add_dylib(load, true);
        }
        Ok(true)
    })
    .map_err(|error| report!("The app's executable can't take the tweak: {}", crate::errors::describe(&error)))?;
    std::fs::write(&executable, &main)?;
    // A weak load that names nothing fails silently: check each one names a file now.
    for load in &loads {
        let relative = load.trim_start_matches("@rpath/");
        if !app.join("Frameworks").join(relative).is_file() {
            bail!("{load} doesn't name a file in the app's Frameworks folder");
        }
    }
    injection.loads = loads;
    Ok(injection)
}

fn display_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
}

fn names(tweaks: &[PathBuf]) -> String {
    tweaks.iter().map(|t| display_name(t)).collect::<Vec<_>>().join(", ")
}

fn relative(app: &Path, file: &Path) -> String {
    file.strip_prefix(app).unwrap_or(file).to_string_lossy().to_string()
}

fn dedup(lines: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    lines.retain(|line| seen.insert(line.clone()));
}

/// Code built for `code` can run in an app built for `app`: the same platform (an iPhone app on
/// a headset runs as iOS, so an iOS tweak suits it).
fn compatible(code: u32, app: u32) -> bool {
    code == app
}

fn platform_name(platform: u32) -> &'static str {
    match platform {
        macho::PLATFORM_IOS => "iOS",
        macho::PLATFORM_TVOS => "tvOS",
        macho::PLATFORM_VISIONOS => "visionOS",
        _ => "another platform",
    }
}

/// The app's main executable, from its Info.plist.
fn main_executable(app: &Path) -> Result<PathBuf, Report> {
    let info: plist::Dictionary = plist::from_file(app.join("Info.plist")).context("The app has no readable Info.plist")?;
    let name = info.get("CFBundleExecutable").and_then(plist::Value::as_string).ok_or_else(|| report!("The app's Info.plist names no executable"))?;
    let path = app.join(name);
    if !path.is_file() {
        bail!("The app's executable {name} is missing");
    }
    Ok(path)
}

fn framework_executable(framework: &Path) -> Option<String> {
    let info: plist::Dictionary = plist::from_file(framework.join("Info.plist")).ok()?;
    info.get("CFBundleExecutable").and_then(plist::Value::as_string).map(str::to_string)
}

/// Whether `path` names Substrate or one of the hooking libraries ElleKit stands in for.
fn is_substrate(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("substrate.") || lower.ends_with("/libhooker.dylib") || lower.ends_with("/libblackjack.dylib") || lower.ends_with("/libellekit.dylib")
}

/// Where a library a copied file links is in the app now: Substrate, or a copied dylib or
/// framework, under `@rpath`. `None` for anything else (the system's own libraries).
fn rewrite(path: &str, dylibs: &BTreeSet<String>, frameworks: &BTreeSet<String>) -> Option<String> {
    if is_substrate(path) {
        return Some(SUBSTRATE.into());
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    if dylibs.contains(file) {
        return Some(format!("@rpath/{file}"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    let at = parts.iter().position(|part| part.ends_with(".framework") && frameworks.contains(*part))?;
    Some(format!("@rpath/{}", parts[at..].join("/")))
}

/// What a library path the device won't have means, when it's one only a jailbreak provides.
fn missing(path: &str) -> Option<String> {
    if path.to_ascii_lowercase().contains("libsubstitute") {
        return Some(format!("links {path}: ElleKit stands in for Substrate, libhooker and blackjack, but not Substitute"));
    }
    let jailbreak_only = path.starts_with("/var/jb/") || path.starts_with("/Library/") || path.starts_with("/usr/local/") || path.starts_with("@loader_path/.jbroot");
    jailbreak_only.then(|| {
        format!("links {path}, which isn't on a device without a jailbreak: add the package that provides it as a tweak too, or this one won't load")
    })
}

// MARK: Gathering

/// What `tweak` holds, added to `items`.
fn gather(tweak: &Path, scratch: &Path, items: &mut Vec<Item>, warnings: &mut Vec<String>) -> Result<(), Report> {
    let name = display_name(tweak);
    if tweak.is_dir() {
        let kind = tweak.extension().and_then(|e| e.to_str()).unwrap_or("");
        match kind {
            "framework" => items.push(Item::Framework { name, source: tweak.to_path_buf(), given: true }),
            "bundle" => items.push(Item::Bundle { name, source: tweak.to_path_buf() }),
            _ => bail!("{name} is a folder that isn't a .framework or a .bundle"),
        }
        return Ok(());
    }
    let mut head = [0u8; 8];
    let read = std::fs::File::open(tweak)?.read(&mut head)?;
    let head = &head[..read];
    if macho::is_macho(head) {
        let data = std::fs::read(tweak)?;
        if macho::read_all(&data)?.first().map(|s| s.filetype) != Some(macho::MH_DYLIB) {
            bail!("{name} is code, but not a dynamic library (.dylib)");
        }
        if is_substrate(&format!("/{name}")) {
            warnings.push(format!("{name} is Substrate itself: Apparition adds ElleKit in its place"));
            return Ok(());
        }
        let name = if name.ends_with(".dylib") { name } else { format!("{name}.dylib") };
        items.push(Item::Dylib { name, source: tweak.to_path_buf(), given: true });
    } else if head.starts_with(b"!<arch>\n") {
        gather_deb(tweak, scratch, items, warnings)?;
    } else if head.starts_with(b"PK") {
        gather_zip(tweak, scratch, items)?;
    } else {
        bail!("{name} isn't a tweak Apparition can inject: that's a .deb package, a .dylib, a .framework or .bundle folder, or a .zip of those");
    }
    Ok(())
}

/// A zip's top-level (or once-nested) .framework and .bundle folders and .dylib files.
fn gather_zip(zip: &Path, scratch: &Path, items: &mut Vec<Item>) -> Result<(), Report> {
    std::fs::create_dir_all(scratch)?;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip)?).context("The zip is damaged")?;
    archive.extract(scratch).context("Unpacking the zip failed")?;
    let mut found = false;
    let mut look = vec![scratch.to_path_buf()];
    for depth in 0..2 {
        let mut next = Vec::new();
        for dir in look {
            for entry in std::fs::read_dir(&dir)?.filter_map(Result::ok) {
                let path = entry.path();
                let name = display_name(&path);
                if name.starts_with('.') || name == "__MACOSX" {
                    continue;
                }
                let kind = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
                if path.is_dir() && kind == "framework" {
                    items.push(Item::Framework { name, source: path, given: true });
                    found = true;
                } else if path.is_dir() && kind == "bundle" {
                    items.push(Item::Bundle { name, source: path });
                    found = true;
                } else if path.is_file() && kind == "dylib" && !is_substrate(&format!("/{name}")) {
                    items.push(Item::Dylib { name, source: path, given: true });
                    found = true;
                } else if path.is_dir() && depth == 0 {
                    next.push(path);
                }
            }
        }
        look = next;
    }
    if !found {
        bail!("The zip holds no .framework, .bundle or .dylib");
    }
    Ok(())
}

/// Where a file of a tweak package goes. `parts` is its path in the package, with a
/// rootless `var/jb/` prefix removed.
#[derive(Debug, PartialEq)]
enum Place {
    /// A tweak dylib (loaded by the app unless other code links it).
    Tweak(String),
    /// A library in usr/lib.
    Library(String),
    /// A file inside a framework: (the framework's folder name, the path inside it).
    Framework(String, String),
    /// A file inside a bundle in Application Support: (bundle, path inside).
    Bundle(String, String),
    Skip(Option<&'static str>),
}

fn place(parts: &[&str]) -> Place {
    match parts {
        ["Library", "MobileSubstrate", "DynamicLibraries", file] | ["usr", "lib", "TweakInject", file] => {
            if file.ends_with(".dylib") { Place::Tweak(file.to_string()) } else { Place::Skip(None) }
        }
        ["usr", "lib", file] if file.ends_with(".dylib") => Place::Library(file.to_string()),
        ["Library", "Frameworks", framework, rest @ ..] if framework.ends_with(".framework") && !rest.is_empty() => {
            Place::Framework(framework.to_string(), rest.join("/"))
        }
        ["Library", "Application Support", bundle, rest @ ..] if bundle.ends_with(".bundle") && !rest.is_empty() => {
            Place::Bundle(bundle.to_string(), rest.join("/"))
        }
        ["Library", "PreferenceBundles", ..] | ["Library", "PreferenceLoader", ..] => {
            Place::Skip(Some("Its Settings pane is left out: Settings panes need PreferenceLoader, which a device without a jailbreak doesn't have"))
        }
        ["Applications", ..] => Place::Skip(Some("It holds an app: install that as an app instead (Apparition takes .deb apps too)")),
        _ => Place::Skip(None),
    }
}

/// A .deb's tweaks, libraries, frameworks and bundles, unpacked into `scratch`.
fn gather_deb(deb: &Path, scratch: &Path, items: &mut Vec<Item>, warnings: &mut Vec<String>) -> Result<(), Report> {
    let package = display_name(deb);
    std::fs::create_dir_all(scratch)?;
    let mut tweaks = BTreeMap::new();
    let mut frameworks = BTreeSet::new();
    let mut bundles = BTreeSet::new();
    for entry in crate::package::data_archive(deb)?.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let mut parts: Vec<String> = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
                Component::CurDir | Component::RootDir => {}
                // A path that climbs out of the package is no file of it.
                Component::ParentDir | Component::Prefix(_) => {
                    parts.clear();
                    break;
                }
            }
        }
        // A Mac's tar adds `._name` files for extended attributes, and Finder `.DS_Store`.
        if parts.last().is_some_and(|name| name.starts_with("._") || name == ".DS_Store") {
            continue;
        }
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let parts = match parts.as_slice() {
            ["var", "jb", rest @ ..] => rest,
            all => all,
        };
        let target = match place(parts) {
            Place::Tweak(file) | Place::Library(file) => {
                if is_substrate(&format!("/{file}")) {
                    continue;
                }
                let given = matches!(place(parts), Place::Tweak(_));
                tweaks.insert(file.clone(), given);
                scratch.join("dylibs").join(file)
            }
            Place::Framework(framework, rest) => {
                if is_substrate(&format!("/{framework}/")) || framework == "CydiaSubstrate.framework" {
                    continue;
                }
                frameworks.insert(framework.clone());
                scratch.join("frameworks").join(framework).join(rest)
            }
            Place::Bundle(bundle, rest) => {
                bundles.insert(bundle.clone());
                scratch.join("bundles").join(bundle).join(rest)
            }
            Place::Skip(Some(why)) => {
                warnings.push(format!("{package}: {why}"));
                continue;
            }
            Place::Skip(None) => continue,
        };
        std::fs::create_dir_all(target.parent().unwrap())?;
        let mut file = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut file)?;
    }
    if tweaks.is_empty() && frameworks.is_empty() && bundles.is_empty() {
        bail!("This package has no tweak in it (nothing in Library/MobileSubstrate/DynamicLibraries, usr/lib, Library/Frameworks or Library/Application Support)");
    }
    for (file, given) in tweaks {
        let source = scratch.join("dylibs").join(&file);
        let mut head = [0u8; 4];
        if std::fs::File::open(&source).and_then(|mut f| f.read_exact(&mut head)).is_err() || !macho::is_macho(&head) {
            warnings.push(format!("{package}: {file} isn't code, so it's left out"));
            continue;
        }
        // A library from usr/lib is loaded only when nothing links it; a tweak always is.
        items.push(Item::Dylib { source, name: file, given });
    }
    for framework in frameworks {
        items.push(Item::Framework { source: scratch.join("frameworks").join(&framework), name: framework, given: false });
    }
    for bundle in bundles {
        warnings.push(format!(
            "{package}: {bundle} goes into the app's own folder; a tweak that looks for it at /Library/Application Support may not find it there"
        ));
        items.push(Item::Bundle { source: scratch.join("bundles").join(&bundle), name: bundle });
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), Report> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Every Mach-O file under `dir`.
fn machos_in(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let mut head = [0u8; 4];
                if std::fs::File::open(&path).and_then(|mut f| f.read_exact(&mut head)).is_ok() && macho::is_macho(&head) {
                    found.push(path);
                }
            }
        }
    }
    found.sort();
    found
}

/// ElleKit as `Frameworks/CydiaSubstrate.framework`, with its licence beside it.
fn add_ellekit(app: &Path) -> Result<(), Report> {
    let framework = app.join("Frameworks/CydiaSubstrate.framework");
    std::fs::create_dir_all(&framework)?;
    std::fs::write(framework.join("CydiaSubstrate"), ELLEKIT)?;
    std::fs::write(framework.join("LICENSE"), ELLEKIT_LICENSE)?;
    let info = plist::Dictionary::from_iter([
        ("CFBundleExecutable".to_string(), plist::Value::String("CydiaSubstrate".into())),
        ("CFBundleIdentifier".to_string(), plist::Value::String("com.illixion.apparition.ellekit".into())),
        ("CFBundleName".to_string(), plist::Value::String("CydiaSubstrate".into())),
        ("CFBundlePackageType".to_string(), plist::Value::String("FMWK".into())),
        ("CFBundleShortVersionString".to_string(), plist::Value::String(ELLEKIT_VERSION.into())),
        ("CFBundleVersion".to_string(), plist::Value::String(ELLEKIT_VERSION.into())),
        ("CFBundleInfoDictionaryVersion".to_string(), plist::Value::String("6.0".into())),
        ("CFBundleSupportedPlatforms".to_string(), plist::Value::Array(vec![plist::Value::String("iPhoneOS".into())])),
        ("MinimumOSVersion".to_string(), plist::Value::String("15.0".into())),
    ]);
    plist::Value::Dictionary(info).to_file_xml(framework.join("Info.plist"))?;
    Ok(())
}

// MARK: C ABI

/// Injects the tweaks in `tweaks_json` (a JSON array of paths: .deb, .dylib, .framework,
/// .bundle, .zip) into the IPA at `ipa`, writing the result to the IPA `out`; `scratch` is a
/// folder for the work. Returns `{ok, loads, added, substrate, rewritten, warnings}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_inject_tweaks(
    ipa: *const std::ffi::c_char,
    tweaks_json: *const std::ffi::c_char,
    out: *const std::ffi::c_char,
    scratch: *const std::ffi::c_char,
) -> *mut std::ffi::c_char {
    crate::reply((|| {
        let ipa = PathBuf::from(unsafe { crate::text(ipa) }?);
        let tweaks: Vec<PathBuf> = serde_json::from_str(&unsafe { crate::text(tweaks_json) }?).context("The tweak list isn't a JSON array of paths")?;
        let out = PathBuf::from(unsafe { crate::text(out) }?);
        let scratch = PathBuf::from(unsafe { crate::text(scratch) }?);
        let injection = inject_ipa(&ipa, &tweaks, &out, &scratch)?;
        Ok(serde_json::to_value(injection)?)
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macho::tests::{build, build_version, encryption, load};

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apparition-tweaks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An app folder with an executable that loads libSystem and has room for more.
    fn app(dir: &Path, extra: &[macho::Command]) -> PathBuf {
        let app = dir.join("Test.app");
        std::fs::create_dir_all(&app).unwrap();
        let info = plist::Dictionary::from_iter([("CFBundleExecutable".to_string(), plist::Value::String("Test".into()))]);
        plist::Value::Dictionary(info).to_file_xml(app.join("Info.plist")).unwrap();
        let mut commands = vec![load("/usr/lib/libSystem.B.dylib"), build_version(macho::PLATFORM_IOS)];
        commands.extend_from_slice(extra);
        std::fs::write(app.join("Test"), build(macho::MH_EXECUTE, 0x2000, &commands)).unwrap();
        app
    }

    fn tweak(dir: &Path, name: &str, links: &[&str]) -> PathBuf {
        let mut commands: Vec<_> = links.iter().map(|l| load(l)).collect();
        commands.push(build_version(macho::PLATFORM_IOS));
        let path = dir.join(name);
        std::fs::write(&path, build(macho::MH_DYLIB, 0x2000, &commands)).unwrap();
        path
    }

    fn loads_of(file: &Path) -> Vec<String> {
        macho::read_all(&std::fs::read(file).unwrap()).unwrap()[0].dylibs().into_iter().map(|(_, p)| p).collect()
    }

    #[test]
    fn a_dylib_is_loaded_with_ellekit_for_substrate() {
        let dir = temp("dylib");
        let app = app(&dir, &[]);
        let tweak = tweak(&dir, "NoAds.dylib", &["/Library/Frameworks/CydiaSubstrate.framework/CydiaSubstrate", "/usr/lib/libobjc.A.dylib"]);
        let injection = inject_app(&app, &[tweak], &dir.join("scratch")).unwrap();
        assert_eq!(injection.loads, ["@rpath/NoAds.dylib"]);
        assert!(injection.substrate);
        assert_eq!(injection.rewritten, 1);
        let main = macho::read_all(&std::fs::read(app.join("Test")).unwrap()).unwrap();
        assert!(main[0].rpaths().contains(&RPATH.to_string()));
        assert!(main[0].dylibs().iter().any(|(_, p)| p == "@rpath/NoAds.dylib"));
        assert_eq!(loads_of(&app.join("Frameworks/NoAds.dylib")), [SUBSTRATE, "/usr/lib/libobjc.A.dylib"]);
        let ellekit = std::fs::read(app.join("Frameworks/CydiaSubstrate.framework/CydiaSubstrate")).unwrap();
        assert_eq!(ellekit, ELLEKIT);
        assert!(app.join("Frameworks/CydiaSubstrate.framework/LICENSE").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_encrypted_app_is_refused() {
        let dir = temp("encrypted");
        let app = app(&dir, &[encryption(1)]);
        let tweak = tweak(&dir, "NoAds.dylib", &[]);
        let error = inject_app(&app, &[tweak], &dir.join("scratch")).unwrap_err();
        assert!(crate::errors::describe(&error).contains("encrypted"));
        assert!(!app.join("Frameworks").exists(), "nothing was copied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tweak_for_another_platform_is_refused() {
        let dir = temp("platform");
        let app = app(&dir, &[]);
        let path = dir.join("Vision.dylib");
        std::fs::write(&path, build(macho::MH_DYLIB, 0x2000, &[build_version(macho::PLATFORM_VISIONOS)])).unwrap();
        let error = inject_app(&app, &[path], &dir.join("scratch")).unwrap_err();
        assert!(crate::errors::describe(&error).contains("another platform"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_tweak_twice_is_refused() {
        let dir = temp("twice");
        let app = app(&dir, &[]);
        let tweak = tweak(&dir, "NoAds.dylib", &[]);
        inject_app(&app, &[tweak.clone()], &dir.join("scratch")).unwrap();
        let error = inject_app(&app, &[tweak], &dir.join("scratch")).unwrap_err();
        assert!(crate::errors::describe(&error).contains("already has Frameworks/NoAds.dylib"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_paths_go_where_injectors_put_them() {
        assert_eq!(place(&["Library", "MobileSubstrate", "DynamicLibraries", "X.dylib"]), Place::Tweak("X.dylib".into()));
        assert_eq!(place(&["Library", "MobileSubstrate", "DynamicLibraries", "X.plist"]), Place::Skip(None));
        assert_eq!(place(&["usr", "lib", "TweakInject", "X.dylib"]), Place::Tweak("X.dylib".into()));
        assert_eq!(place(&["usr", "lib", "libhelper.dylib"]), Place::Library("libhelper.dylib".into()));
        assert_eq!(place(&["Library", "Frameworks", "Orion.framework", "Orion"]), Place::Framework("Orion.framework".into(), "Orion".into()));
        assert_eq!(
            place(&["Library", "Application Support", "X.bundle", "en.lproj", "Strings.strings"]),
            Place::Bundle("X.bundle".into(), "en.lproj/Strings.strings".into())
        );
        assert!(matches!(place(&["Library", "PreferenceBundles", "X.bundle", "X"]), Place::Skip(Some(_))));
        assert_eq!(place(&["DEBIAN", "control"]), Place::Skip(None));
    }

    #[test]
    fn libraries_are_pointed_inside_the_app() {
        let dylibs = BTreeSet::from(["libhelper.dylib".to_string()]);
        let frameworks = BTreeSet::from(["Orion.framework".to_string()]);
        assert_eq!(rewrite("/usr/lib/libsubstrate.dylib", &dylibs, &frameworks).as_deref(), Some(SUBSTRATE));
        assert_eq!(rewrite("/var/jb/usr/lib/libhooker.dylib", &dylibs, &frameworks).as_deref(), Some(SUBSTRATE));
        assert_eq!(rewrite("@rpath/CydiaSubstrate.framework/CydiaSubstrate", &dylibs, &frameworks).as_deref(), Some(SUBSTRATE));
        assert_eq!(rewrite("/var/jb/usr/lib/libhelper.dylib", &dylibs, &frameworks).as_deref(), Some("@rpath/libhelper.dylib"));
        assert_eq!(rewrite("/Library/Frameworks/Orion.framework/Orion", &dylibs, &frameworks).as_deref(), Some("@rpath/Orion.framework/Orion"));
        assert_eq!(rewrite("/usr/lib/libobjc.A.dylib", &dylibs, &frameworks), None);
        assert!(missing("/Library/Frameworks/Cephei.framework/Cephei").is_some());
        assert!(missing("/usr/lib/libsubstitute.dylib").unwrap().contains("Substitute"));
        assert!(missing("/usr/lib/libobjc.A.dylib").is_none());
    }

    #[test]
    fn a_package_brings_its_framework_and_the_tweak_links_it() {
        let dir = temp("framework");
        let app = app(&dir, &[]);
        let mut items = Vec::new();
        // As gather_deb leaves them: a tweak linking a framework from the same package.
        let tweak_path = tweak(&dir, "Swifty.dylib", &["/Library/Frameworks/Orion.framework/Orion", "/usr/lib/libsubstrate.dylib"]);
        items.push(tweak_path);
        let framework = dir.join("Orion.framework");
        std::fs::create_dir_all(&framework).unwrap();
        tweak(&framework, "Orion", &[]);
        items.push(framework);
        let injection = inject_app(&app, &items, &dir.join("scratch")).unwrap();
        // Given on its own, the framework is loaded too, unless the tweak already links it.
        assert_eq!(injection.loads, ["@rpath/Swifty.dylib"]);
        assert_eq!(loads_of(&app.join("Frameworks/Swifty.dylib")), ["@rpath/Orion.framework/Orion", SUBSTRATE]);
        assert!(app.join("Frameworks/Orion.framework/Orion").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bundled_ellekit_is_the_reproducible_build() {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(ELLEKIT);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, ELLEKIT_SHA256, "assets/ellekit/libellekit.dylib changed: rebuild it with scripts/build-ellekit.sh");
        assert!(ELLEKIT_LICENSE.contains("Redistribution and use in source and binary forms"));
    }
}
