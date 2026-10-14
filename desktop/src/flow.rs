//! The commands behind each step of the wizard. Each long one runs on a blocking thread and
//! drives the core's async API there, so a PIN or 2FA prompt can wait for the page without
//! holding up anything else.

use std::{
    net::IpAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

use apparition_core::{
    api::{self, APPARITION_ID, DeviceStore, Handoff, PairedDevice, Slot, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    release,
};
use rootcause::Report;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DOWNLOADS: &str = "https://apparition.illixion.com/#download";
const QUESTIONS: &str = "https://apparition.illixion.com/#faq";
/// How long a PIN or 2FA prompt waits for an answer.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// What the app holds between steps. Nothing secret: the Apple ID session holds tokens (in
/// memory, as the command line does), and the certificate's key is in the scratch folder,
/// removed when the app quits.
pub struct State {
    store: DeviceStore,
    /// Per run: the imported certificate and the downloaded release.
    scratch: PathBuf,
    session: Mutex<Option<api::Session>>,
    certificate: Mutex<Option<Certificate>>,
    app: Mutex<Option<Prepared>>,
    prompt: Mutex<Option<mpsc::Sender<Answer>>>,
}

#[derive(Clone)]
struct Certificate {
    dir: PathBuf,
    id: String,
    p12: PathBuf,
    profiles: Vec<PathBuf>,
}

#[derive(Clone)]
struct Prepared {
    path: PathBuf,
}

impl State {
    pub fn new() -> Self {
        Self {
            store: DeviceStore::default(),
            scratch: std::env::temp_dir().join(format!("apparition-installer-app-{}", std::process::id())),
            session: Mutex::new(None),
            certificate: Mutex::new(None),
            app: Mutex::new(None),
            prompt: Mutex::new(None),
        }
    }

    /// Drops the session and removes the scratch folder (with an imported certificate's key).
    pub fn clean_up(&self) {
        *self.session.lock().unwrap() = None;
        *self.certificate.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&self.scratch);
    }

    /// Asks the page (event `prompt`) and waits for `answer_prompt` or `cancel_prompt`.
    fn ask(&self, app: &AppHandle, payload: Value) -> Answer {
        let (sender, receiver) = mpsc::channel();
        *self.prompt.lock().unwrap() = Some(sender);
        let _ = app.emit("prompt", payload);
        let answer = receiver.recv_timeout(PROMPT_TIMEOUT).unwrap_or(Answer::Cancel);
        *self.prompt.lock().unwrap() = None;
        let _ = app.emit("prompt-done", ());
        answer
    }

    fn settings_path(&self) -> PathBuf {
        self.store.dir.join("desktop.json")
    }
}

type Shared<'a> = tauri::State<'a, Arc<State>>;

/// An error as the page gets it. `slot` says a free account's limit was hit, so the page can
/// offer to free one.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    message: String,
    slot: Option<Slot>,
    cancelled: bool,
}

impl Failure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self { slot: Slot::of_error(&message), message, cancelled: false }
    }

    fn cancelled(message: &str) -> Self {
        Self { message: message.into(), slot: None, cancelled: true }
    }
}

impl From<Report> for Failure {
    fn from(error: Report) -> Self {
        Self::new(api::describe(&error))
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Runs `job` on a blocking thread; it drives async core calls with [`block_on`].
async fn blocking<T: Send + 'static>(job: impl FnOnce() -> Result<T, Failure> + Send + 'static) -> Result<T, Failure> {
    tauri::async_runtime::spawn_blocking(job).await.map_err(|error| Failure::new(error.to_string()))?
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tauri::async_runtime::block_on(future)
}

/// An answer to a prompt, from the page.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Answer {
    Pin { pin: String },
    Code { code: String },
    Sms { id: u32 },
    Resend,
    Cancel,
}

fn digits(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).collect()
}

// MARK: The app and its updates

/// The asset platform of this build, as the release site names installers.
fn installer_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos-universal"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-arm64"
    } else if cfg!(target_os = "windows") {
        "windows-x86_64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64"
    } else {
        "linux-x86_64"
    }
}

/// The official site, checked against the compiled-in release keys. A debug build may point
/// at a scratch site and its own keys (APPARITION_RELEASE_SITE, APPARITION_RELEASE_SIGNERS);
/// a release build can't.
async fn latest_release() -> Result<release::Verified, Report> {
    #[cfg(debug_assertions)]
    if let Ok(site) = std::env::var("APPARITION_RELEASE_SITE") {
        return match std::env::var("APPARITION_RELEASE_SIGNERS").ok().and_then(|path| std::fs::read_to_string(path).ok()) {
            Some(signers) => release::latest_signed_by(&site, &signers).await,
            None => release::latest(&site).await,
        };
    }
    release::latest(release::SITE).await
}

#[tauri::command]
pub fn app_info() -> Value {
    // A debug build can open on any step, to look at it without a headset.
    let preview = if cfg!(debug_assertions) { std::env::var("APPARITION_DESKTOP_PREVIEW").ok() } else { None };
    json!({
        "version": VERSION,
        "platform": installer_platform(),
        "debug": cfg!(debug_assertions),
        "computerName": api::hostname(),
        "preview": preview,
    })
}

/// A newer installer for this platform on the release site, verified, or null. Never
/// downloads or replaces anything: the page links to the site.
#[tauri::command]
pub async fn check_update() -> Result<Option<Value>, Failure> {
    blocking(|| {
        let verified = block_on(latest_release())?;
        let newer = verified.asset("installer", installer_platform()).is_some()
            && release::is_newer(&verified.release.version, VERSION);
        Ok(newer.then(|| json!({ "version": verified.release.version, "date": verified.release.date })))
    })
    .await
}

#[tauri::command]
pub fn open_downloads(app: AppHandle) -> Result<(), Failure> {
    app.opener().open_url(DOWNLOADS, None::<&str>).map_err(|error| Failure::new(error.to_string()))
}

#[tauri::command]
pub fn open_help(app: AppHandle) -> Result<(), Failure> {
    app.opener().open_url(QUESTIONS, None::<&str>).map_err(|error| Failure::new(error.to_string()))
}

// MARK: Devices and pairing

/// A paired device as the page sees it (no pairing file).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    udid: String,
    name: String,
    address: IpAddr,
    product_type: Option<String>,
    /// "visionOS", "iOS" (iPhone and iPad) or "tvOS".
    platform: &'static str,
    os_version: Option<String>,
}

impl DeviceView {
    fn of(device: &PairedDevice, info: Option<&api::DeviceInfo>) -> Self {
        Self {
            udid: device.udid.clone(),
            name: device.name.clone(),
            address: device.address,
            product_type: device.product_type.clone(),
            platform: device.platform().os_name(),
            os_version: info.map(|i| i.os_version.clone()),
        }
    }
}

#[tauri::command]
pub fn paired_devices(state: Shared<'_>) -> Vec<DeviceView> {
    state.store.devices().iter().map(|d| DeviceView::of(d, None)).collect()
}

/// Devices on this network: paired or waiting to pair (Remote Devices open).
#[tauri::command]
pub async fn discover() -> Result<Vec<api::Found>, Failure> {
    blocking(|| Ok(block_on(api::browse(3))?)).await
}

#[tauri::command]
pub fn forget_device(state: Shared<'_>, udid: String) -> Result<(), Failure> {
    Ok(state.store.forget(&udid)?)
}

/// Pairs with a device showing its pairing screen (Remote Devices on Vision Pro, Remote App
/// and Devices on Apple TV). The PIN is asked for with a `prompt` event of kind "pin", once the
/// device shows it; later asks carry `second`, since an Apple TV shows a second PIN for
/// Apparition's own record.
#[tauri::command]
pub async fn pair_device(app: AppHandle, state: Shared<'_>, address: String, port: u16) -> Result<DeviceView, Failure> {
    let address: IpAddr = address.trim().parse().map_err(|_| Failure::new(format!("{} isn't an IP address", address.trim())))?;
    let state = state.inner().clone();
    blocking(move || {
        let cancelled = AtomicBool::new(false);
        let asked = AtomicBool::new(false);
        let pin = || match state.ask(&app, json!({ "kind": "pin", "second": asked.swap(true, Ordering::Relaxed) })) {
            Answer::Pin { pin } => digits(&pin),
            _ => {
                cancelled.store(true, Ordering::Relaxed);
                String::new()
            }
        };
        match block_on(state.store.pair(address, port, pin)) {
            Ok((device, info)) => Ok(DeviceView::of(&device, Some(&info))),
            Err(_) if cancelled.load(Ordering::Relaxed) => Err(Failure::cancelled("Pairing was cancelled.")),
            Err(error) => Err(error.into()),
        }
    })
    .await
}

/// iPhones and iPads usbmuxd sees (plugged in, or on the network once trusted). They have
/// no PIN screen, so they pair this way. `paired` marks ones this computer already has.
#[tauri::command]
pub async fn usb_devices(state: Shared<'_>) -> Result<Vec<Value>, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let paired = state.store.devices();
        let devices = block_on(api::usb_devices())?;
        Ok(devices
            .into_iter()
            .map(|d| {
                let known = paired.iter().find(|p| p.udid == d.udid);
                json!({ "udid": d.udid, "usb": d.usb, "paired": known.is_some(), "name": known.map(|p| p.name.clone()) })
            })
            .collect())
    })
    .await
}

/// Pairs with an iPhone or iPad over usbmuxd: no PIN, but it must trust this computer (if it
/// doesn't yet, it asks, and `pair-status` says what to do on it), and then be found on the
/// network.
#[tauri::command]
pub async fn pair_usb_device(app: AppHandle, state: Shared<'_>, udid: String) -> Result<DeviceView, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let waiting = |what: &str| {
            let _ = app.emit("pair-status", what);
        };
        let (device, info) = block_on(state.store.pair_usb(&udid, &waiting))?;
        Ok(DeviceView::of(&device, Some(&info)))
    })
    .await
}

#[tauri::command]
pub fn answer_prompt(state: Shared<'_>, answer: Answer) -> Result<(), Failure> {
    let sender = state.prompt.lock().unwrap().take().ok_or("Nothing is waiting for that any more.")?;
    let _ = sender.send(answer);
    Ok(())
}

#[tauri::command]
pub fn cancel_prompt(state: Shared<'_>) {
    if let Some(sender) = state.prompt.lock().unwrap().take() {
        let _ = sender.send(Answer::Cancel);
    }
}

// MARK: Signing

/// What the page remembers between runs: never a password.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    email: Option<String>,
    anisette: Option<String>,
}

/// Whether signing in can make its own anisette data (see `api::BUILTIN_ANISETTE`).
#[tauri::command]
pub fn builtin_anisette() -> bool {
    api::builtin_anisette_available()
}

#[tauri::command]
pub fn saved_settings(state: Shared<'_>) -> Settings {
    std::fs::read(state.settings_path()).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default()
}

/// Signs in to Apple. 2FA is asked for with a `prompt` event of kind "twoFactor". The
/// password goes into the core's sign-in and no further.
#[tauri::command]
pub async fn apple_id_sign_in(
    app: AppHandle,
    state: Shared<'_>,
    email: String,
    password: String,
    anisette: String,
) -> Result<Value, Failure> {
    let email = email.trim().to_string();
    let anisette = anisette.trim().to_string();
    if !email.contains('@') {
        return Err("Enter the email address of your Apple ID.".into());
    }
    if anisette == api::BUILTIN_ANISETTE {
        if !api::builtin_anisette_available() {
            return Err("This computer can't make its own anisette data; give the address of an anisette server.".into());
        }
    } else if !(anisette.starts_with("http://") || anisette.starts_with("https://")) {
        return Err("The anisette server's address starts with http:// or https://.".into());
    }
    let state = state.inner().clone();
    blocking(move || {
        let cancelled = AtomicBool::new(false);
        let two_factor = |params: &TwoFactorCallbackParams| {
            let numbers: Vec<Value> =
                params.numbers.iter().map(|n| json!({ "id": n.id, "number": n.number_with_dial_code })).collect();
            let payload = json!({ "kind": "twoFactor", "lastError": params.last_error, "numbers": numbers });
            match state.ask(&app, payload) {
                Answer::Code { code } => TwoFactorCallbackResponse::SubmitCode(digits(&code)),
                Answer::Sms { id } => TwoFactorCallbackResponse::SendSms(id),
                Answer::Resend => TwoFactorCallbackResponse::ResendCode,
                _ => {
                    cancelled.store(true, Ordering::Relaxed);
                    TwoFactorCallbackResponse::Abort
                }
            }
        };
        let (session, info) = match block_on(api::sign_in(&email, password, &anisette, &state.store.apple_id_dir(), two_factor)) {
            Ok(signed_in) => signed_in,
            Err(_) if cancelled.load(Ordering::Relaxed) => return Err(Failure::cancelled("Sign-in was cancelled.")),
            Err(error) => return Err(error.into()),
        };
        let settings = Settings { email: Some(email.clone()), anisette: Some(anisette) };
        let _ = std::fs::create_dir_all(&state.store.dir)
            .and_then(|_| std::fs::write(state.settings_path(), serde_json::to_vec_pretty(&settings).unwrap_or_default()));
        let team_id = session.team_id().to_string();
        *state.session.lock().unwrap() = Some(session);
        // Signing with the Apple ID from now on: drop an imported certificate and its key.
        if state.certificate.lock().unwrap().take().is_some() {
            let _ = std::fs::remove_dir_all(state.scratch.join("certificate"));
        }
        Ok(json!({ "email": email, "teamId": team_id, "team": info["team"] }))
    })
    .await
}

#[tauri::command]
pub fn sign_out(state: Shared<'_>) {
    *state.session.lock().unwrap() = None;
}

/// Who sealed a key file (`.apparitionkey`), to ask for the words it showed; null for a plain
/// `.apparitionsigning`, which needs none.
#[tauri::command]
pub async fn signing_key_from(path: String) -> Result<Option<String>, Failure> {
    blocking(move || {
        let data = std::fs::read(&path)?;
        Ok(api::sealed_from(&data))
    })
    .await
}

/// Imports the Apple ID's certificate key from another device's key file (with the five words
/// it showed, for a sealed one), so this computer signs with the same certificate. Ends a
/// signed-in session, whose copy of the storage would otherwise keep the old key. Returns
/// `{appleId, from, replaced}`.
#[tauri::command]
pub async fn import_signing_key(state: Shared<'_>, path: String, words: Option<String>) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let data = std::fs::read(&path)?;
        let imported = api::import_any_signing_file(&state.store.apple_id_dir(), &data, words.as_deref())?;
        *state.session.lock().unwrap() = None;
        Ok(imported)
    })
    .await
}

/// A file picker for "ipa", "p12", "key" or "profile" (several at once). Empty when cancelled.
#[tauri::command]
pub async fn pick_file(app: AppHandle, kind: String) -> Result<Vec<String>, Failure> {
    blocking(move || {
        let dialog = app.dialog().file();
        let paths = match kind.as_str() {
            "ipa" => dialog.set_title("Choose an app").add_filter("iOS and visionOS apps", &["ipa"]).blocking_pick_file().into_iter().collect(),
            "p12" => dialog
                .set_title("Choose your certificate")
                .add_filter("Certificate with its key", &["p12", "pfx"])
                .blocking_pick_file()
                .into_iter()
                .collect(),
            "key" => dialog
                .set_title("Choose the key file from your other device")
                .add_filter("Apparition key files", &[api::SEALED_EXTENSION, "apparitionsigning"])
                .blocking_pick_file()
                .into_iter()
                .collect(),
            "profile" => dialog
                .set_title("Choose provisioning profiles")
                .add_filter("Provisioning profiles", &["mobileprovision"])
                .blocking_pick_files()
                .unwrap_or_default(),
            _ => return Err("unknown kind of file".into()),
        };
        Ok(paths.into_iter().filter_map(|p| p.into_path().ok()).map(|p| p.to_string_lossy().to_string()).collect())
    })
    .await
}

/// Opens a .p12 with its password (dropped once the file is open) and imports profiles,
/// checked against the device `udid`. Returns `{identity, profiles}` for the page to show.
#[tauri::command]
pub async fn import_certificate(
    state: Shared<'_>,
    p12: String,
    password: String,
    profiles: Vec<String>,
    udid: String,
) -> Result<Value, Failure> {
    if profiles.is_empty() {
        return Err("Add at least one provisioning profile that lists the headset.".into());
    }
    let state = state.inner().clone();
    blocking(move || {
        let dir = state.scratch.join("certificate");
        let _ = std::fs::remove_dir_all(&dir);
        let data = std::fs::read(&p12)?;
        let identity = api::import_identity(&dir, &data, password)?;
        let id = identity["identity"]["id"].as_str().ok_or("The certificate has no id")?.to_string();
        let mut views = Vec::new();
        for profile in &profiles {
            let imported = api::import_profile(&dir, std::fs::read(profile)?, Some(&udid))?;
            views.push(imported["profile"].clone());
        }
        *state.certificate.lock().unwrap() =
            Some(Certificate { dir, id, p12: p12.into(), profiles: profiles.into_iter().map(PathBuf::from).collect() });
        *state.session.lock().unwrap() = None;
        Ok(json!({ "identity": identity["identity"], "profiles": views }))
    })
    .await
}

// MARK: The app to install

/// Finds the latest official release, checks its signature against the compiled-in keys,
/// and downloads its app for the device `udid` (events `release` and `download`). Returns `{version,
/// signer, date, notes, size}`.
#[tauri::command]
pub async fn prepare_latest(app: AppHandle, state: Shared<'_>, udid: String) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        // The build for the device's platform (records from before iPhone and iPad are headsets).
        let platform = state.store.device(&udid).map(|d| d.platform()).unwrap_or(api::Platform::VisionOS);
        block_on(async {
            *state.app.lock().unwrap() = None;
            let verified = latest_release().await?;
            let release = &verified.release;
            let size = verified.asset("app", api::release_platform(platform)).map(|a| a.size);
            let summary = json!({
                "version": release.version, "signer": verified.signer, "date": release.date,
                "notes": release.notes, "size": size, "platform": platform.os_name(),
            });
            let _ = app.emit("release", &summary);
            let dir = state.scratch.join("download");
            let _ = std::fs::remove_dir_all(&dir);
            let shown = AtomicU64::new(u64::MAX);
            let path = api::download_apparition_for(&verified, platform, &dir, |received, total| {
                let percent = received * 100 / total.max(1);
                if shown.swap(percent, Ordering::Relaxed) != percent {
                    let _ = app.emit("download", json!({ "received": received, "total": total }));
                }
            })
            .await?;
            *state.app.lock().unwrap() = Some(Prepared { path });
            Ok(summary)
        })
    })
    .await
}

/// Uses a local IPA instead of the official release.
#[tauri::command]
pub fn prepare_local(state: Shared<'_>, path: String) -> Result<Value, Failure> {
    let path = PathBuf::from(path);
    if !path.is_file() || !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ipa")) {
        return Err("Choose an .ipa file.".into());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let size = std::fs::metadata(&path)?.len();
    *state.app.lock().unwrap() = Some(Prepared { path });
    Ok(json!({ "name": name, "size": size }))
}

// MARK: Install

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOptions {
    udid: String,
    #[serde(default)]
    keep_extensions: bool,
    #[serde(default = "yes")]
    hand_off: bool,
}

fn yes() -> bool {
    true
}

fn stage(app: &AppHandle, name: &str, detail: Value) {
    let _ = app.emit("stage", json!({ "stage": name, "detail": detail }));
}

/// Connects, signs the prepared app for the device and installs it, then (for Apparition)
/// hands it what signed it. Stages arrive as `stage` events. A full certificate or App ID
/// slot fails with `slot` set; nothing is revoked or deleted here.
#[tauri::command]
pub async fn install(app: AppHandle, state: Shared<'_>, options: InstallOptions) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let device = state.store.device(&options.udid).ok_or("That device isn't paired any more. Pair it again.")?;
        let prepared = state.app.lock().unwrap().clone().ok_or("Choose what to install first.")?;
        let certificate = state.certificate.lock().unwrap().clone();
        if certificate.is_none() && state.session.lock().unwrap().is_none() {
            return Err("Sign in with your Apple ID, or choose your certificate, first.".into());
        }
        let cancelled = AtomicBool::new(false);
        let pin = || match state.ask(&app, json!({ "kind": "pin", "again": true })) {
            Answer::Pin { pin } => digits(&pin),
            _ => {
                cancelled.store(true, Ordering::Relaxed);
                String::new()
            }
        };
        stage(&app, "connecting", json!(device.name));
        let result = block_on(async {
            let mut tunnel = state.store.connect(&device, pin).await?;
            let info = tunnel.device_info().await?;
            let strip = !options.keep_extensions;

            stage(&app, "signing", json!(info.name));
            let mut apple_id: Option<(String, String)> = None;
            let signed = match &certificate {
                Some(cert) => api::sign_offline(
                    &cert.dir,
                    prepared.path.clone(),
                    &cert.id,
                    None,
                    &info.udid,
                    strip,
                    None,
                    api::Platform::of_product(&info.product_type),
                )?,
                None => {
                    let mut guard = state.session.lock().unwrap();
                    let session = guard.as_mut().ok_or_else(|| rootcause::report!("Sign in with your Apple ID first"))?;
                    apple_id = Some((session.email().to_string(), session.team_id().to_string()));
                    api::sign_with_apple_id(session, prepared.path.clone(), &info.udid, &info.name, strip).await?
                }
            };
            let signed_path = PathBuf::from(signed["signedPath"].as_str().ok_or_else(|| rootcause::report!("Signing returned no app"))?);
            let bundle_id = api::bundle_id(&signed_path).ok_or_else(|| rootcause::report!("The signed app has no bundle ID"))?;

            let existing = tunnel.installed_app(&bundle_id).await?;
            stage(&app, "installing", json!({ "device": info.name, "update": existing.is_some() }));
            let source = prepared.path.is_file().then_some(prepared.path.as_path());
            // An update of an app that's open waits in installd until it closes.
            let reply = tunnel.install(&signed_path, source, 20_000).await?;

            let mut handoff = Value::Null;
            let mut handoff_error = Value::Null;
            let mut pairing_handed_over = false;
            if options.hand_off && bundle_id.starts_with(APPARITION_ID) {
                stage(&app, "handoff", Value::Null);
                // An iPhone or iPad paired over USB: Apparition gets its own pairing record.
                match state.store.hand_over_pairing(&mut tunnel, &bundle_id, &info.udid).await {
                    Ok(handed) => pairing_handed_over = handed,
                    Err(error) => handoff_error = json!(error.to_string()),
                }
                let apple_id_dir = state.store.apple_id_dir();
                let (kind, what) = match (&certificate, &apple_id) {
                    (Some(cert), _) => ("certificate", Handoff::Certificate { p12: &cert.p12, profiles: &cert.profiles }),
                    (None, Some((email, team_id))) => ("appleId", Handoff::AppleId { data_dir: &apple_id_dir, email, team_id }),
                    (None, None) => unreachable!("signed with neither"),
                };
                // The app is installed either way; a failed handoff is reported, not fatal.
                match tunnel.hand_off_signing(&bundle_id, what).await {
                    Ok(()) => handoff = json!(kind),
                    Err(error) if handoff_error.is_null() => handoff_error = json!(error.to_string()),
                    Err(_) => {}
                }
            }
            Ok::<_, Report>(json!({
                "bundleId": bundle_id,
                "deviceName": info.name,
                "update": existing.is_some(),
                "seconds": reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0,
                "queued": reply["queued"].as_bool() == Some(true),
                "strippedExtensions": signed["strippedExtensions"],
                "handoff": handoff,
                "handoffError": handoff_error,
                "pairingHandedOver": pairing_handed_over,
                "platform": api::Platform::of_product(&info.product_type).os_name(),
                "appleId": apple_id.map(|(email, _)| email),
            }))
        });
        match result {
            Ok(value) => Ok(value),
            Err(_) if cancelled.load(Ordering::Relaxed) => Err(Failure::cancelled("Cancelled: the device wanted to pair again.")),
            Err(error) => Err(error.into()),
        }
    })
    .await
}

// MARK: The account's slots

fn with_session<T: Send + 'static>(
    state: Arc<State>,
    job: impl AsyncFnOnce(&mut api::Session) -> Result<T, Report> + Send + 'static,
) -> impl std::future::Future<Output = Result<T, Failure>> {
    blocking(move || {
        let mut guard = state.session.lock().unwrap();
        let session = guard.as_mut().ok_or("Sign in with your Apple ID first.")?;
        Ok(block_on(job(session))?)
    })
}

#[tauri::command]
pub async fn list_certificates(state: Shared<'_>) -> Result<Value, Failure> {
    with_session(state.inner().clone(), async |session: &mut api::Session| api::list_certificates(session).await).await
}

/// Revokes a certificate, only with the word the page asked the user to type.
#[tauri::command]
pub async fn revoke_certificate(state: Shared<'_>, serial: String, confirmation: String) -> Result<(), Failure> {
    if confirmation.trim() != "revoke" {
        return Err("Type revoke to confirm.".into());
    }
    with_session(state.inner().clone(), async move |session: &mut api::Session| {
        api::revoke_certificate(session, &serial).await.map(|_| ())
    })
    .await
}

#[tauri::command]
pub async fn list_app_ids(state: Shared<'_>) -> Result<Value, Failure> {
    with_session(state.inner().clone(), async |session: &mut api::Session| api::list_app_ids(session).await).await
}

/// Deletes an App ID, only with the word the page asked the user to type.
#[tauri::command]
pub async fn delete_app_id(state: Shared<'_>, app_id_id: String, confirmation: String) -> Result<(), Failure> {
    if confirmation.trim() != "delete" {
        return Err("Type delete to confirm.".into());
    }
    with_session(state.inner().clone(), async move |session: &mut api::Session| {
        api::delete_app_id(session, &app_id_id).await.map(|_| ())
    })
    .await
}
