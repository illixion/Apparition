//! The commands behind each step of the wizard. Each long one runs on a blocking thread and
//! drives the core's async API there, so a PIN or 2FA prompt can wait for the page without
//! holding up anything else.

use std::{
    net::{IpAddr, SocketAddr},
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
    package, release,
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
    install_lock: Mutex<()>,
    prompt: Mutex<Option<mpsc::Sender<Answer>>>,
    /// Ends `nearby_receive`'s wait.
    nearby_stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
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
            install_lock: Mutex::new(()),
            prompt: Mutex::new(None),
            nearby_stop: Mutex::new(None),
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
/// offer to free one; `shared_certificate`, that signing stopped rather than make a second
/// certificate while Apparition elsewhere has one, so it can offer to get that one's key.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    message: String,
    slot: Option<Slot>,
    shared_certificate: bool,
    cancelled: bool,
    /// What the page can offer for this failure (`api::failure_kind`): a retry for `"network"`,
    /// files of the person's own for `"libraries"`, going on anyway for `"checksum"`.
    kind: Option<&'static str>,
}

impl Failure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        let message = message.into();
        Self { slot: Slot::of_error(&message), message, shared_certificate: false, cancelled: false, kind: None }
    }

    fn cancelled(message: &str) -> Self {
        Self { message: message.into(), slot: None, shared_certificate: false, cancelled: true, kind: None }
    }
}

impl From<Report> for Failure {
    fn from(error: Report) -> Self {
        Self { shared_certificate: api::is_shared_certificate(&error), kind: api::failure_kind(&error), ..Self::new(api::describe(&error)) }
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
    /// Whether the other device shows the same code (nearby signing).
    Match { matches: bool },
    InstallPlan { target: String, extensions: Vec<String>, #[serde(rename = "dataMode")] data_mode: String },
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

/// Takes the built-in anisette's libraries from files the person got themselves, for when Apple's
/// CDN can't be reached: the Apple Music APK, or `libstoreservicescore.so` and `libCoreADI.so`
/// from it. A file that isn't the version this build was tested with fails (kind `"checksum"`)
/// unless `accept_unverified` says to go on. Returns `{services, core, unverified}`.
#[tauri::command]
pub async fn import_anisette_libraries(state: Shared<'_>, paths: Vec<String>, accept_unverified: bool) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let paths: Vec<std::path::PathBuf> = paths.into_iter().map(Into::into).collect();
        let libraries = block_on(api::import_anisette_libraries(&state.store.apple_id_dir(), &paths, accept_unverified))?;
        Ok(json!(libraries))
    })
    .await
}

/// Whether the built-in anisette runs libraries the person supplied that didn't match the
/// checksum of the version Apparition was tested with: an unsupported setup the page warns about.
#[tauri::command]
pub fn anisette_unverified(state: Shared<'_>) -> bool {
    api::anisette_libraries_unverified(&state.store.apple_id_dir())
}

/// Forgets the cached anisette libraries, supplied or fetched, so Apple's own are fetched again.
#[tauri::command]
pub async fn reset_anisette_libraries(state: Shared<'_>) -> Result<(), Failure> {
    let state = state.inner().clone();
    blocking(move || Ok(api::reset_anisette_libraries(&state.store.apple_id_dir())?)).await
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
/// it showed, for a sealed one), so this computer signs with the same certificate. A signed-in
/// session stays and signs with it: the storage reads through to the file. Returns
/// `{appleId, from, replaced}`.
#[tauri::command]
pub async fn import_signing_key(state: Shared<'_>, path: String, words: Option<String>) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let data = std::fs::read(&path)?;
        Ok(api::import_any_signing_file(&state.store.apple_id_dir(), &data, words.as_deref())?)
    })
    .await
}

// MARK: The certificate key to or from Apparition close by

/// Asks the page whether the other device shows `code` (a `prompt` of kind "nearbyCode").
fn nearby_compare(state: Arc<State>, app: AppHandle) -> api::nearby::Compare {
    Arc::new(move |code: &str, peer: &str| {
        matches!(state.ask(&app, json!({ "kind": "nearbyCode", "code": code, "peer": peer })), Answer::Match { matches: true })
    })
}

/// Waits for Apparition on a nearby device or computer to send this computer its certificate
/// key (Share Signing there), asks the page to compare the code, and imports the key. Where it
/// listens arrives first, as a `nearby-listening` event. A signed-in session keeps going and
/// signs with the key next. Returns `{appleId, from, replaced}`, or null once stopped.
#[tauri::command]
pub async fn nearby_receive(app: AppHandle, state: Shared<'_>) -> Result<Option<Value>, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        *state.nearby_stop.lock().unwrap() = Some(stop);
        let name = api::hostname();
        let listening = |at: &api::nearby::Listening| {
            let payload = json!({ "name": name, "address": at.address.map(|a| a.to_string()), "port": at.port, "advertised": at.advertised });
            let _ = app.emit("nearby-listening", payload);
        };
        let waiting = api::nearby::receive(state.store.apple_id_dir(), &name, listening, nearby_compare(state.clone(), app.clone()), async {
            let _ = stopped.await;
        });
        let result = block_on(waiting);
        *state.nearby_stop.lock().unwrap() = None;
        Ok(result?)
    })
    .await
}

/// Stops `nearby_receive` waiting, answering its code prompt "different" if one is open. Does
/// nothing when it isn't waiting.
#[tauri::command]
pub fn nearby_stop(state: Shared<'_>) {
    let Some(stop) = state.nearby_stop.lock().unwrap().take() else { return };
    let _ = stop.send(());
    if let Some(sender) = state.prompt.lock().unwrap().take() {
        let _ = sender.send(Answer::Cancel);
    }
}

/// Receivers waiting nearby (Get Signing in Apparition on a device, `apparition signing
/// receive`): `[{name, address}]`, address being "ip:port".
#[tauri::command]
pub async fn nearby_browse() -> Result<Vec<Value>, Failure> {
    blocking(|| {
        let found = block_on(api::nearby::browse(4))?;
        Ok(found
            .iter()
            .filter(|found| found.name != api::hostname())
            .map(|found| json!({ "name": found.name, "address": found.address().map(|a| a.to_string()) }))
            .collect())
    })
    .await
}

/// Hands the signed-in Apple ID's certificate key to the receiver at `address` ("ip:port"),
/// asking the page to compare the code. Returns the receiver's name once it has the key.
#[tauri::command]
pub async fn nearby_send(app: AppHandle, state: Shared<'_>, address: String) -> Result<String, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let to: SocketAddr = address
            .trim()
            .parse()
            .map_err(|_| Failure::new(format!("{} isn't an address and port, like 192.168.1.20:52144", address.trim())))?;
        let email = state.session.lock().unwrap().as_ref().map(|s| s.email().to_string()).ok_or("Sign in with the Apple ID whose certificate to share first.")?;
        let name = api::hostname();
        let payload = api::export_apple_id_key(&state.store.apple_id_dir(), &email, None, &name)?;
        Ok(block_on(api::nearby::send(to, &name, payload, nearby_compare(state.clone(), app)))?)
    })
    .await
}

/// Lets the next signing make a certificate here, although Apparition elsewhere has one (the
/// install stopped with `sharedCertificate`).
#[tauri::command]
pub fn allow_new_certificate(state: Shared<'_>) -> Result<(), Failure> {
    state.session.lock().unwrap().as_mut().ok_or("Sign in with your Apple ID first.")?.allow_new_certificate();
    Ok(())
}

/// A file picker for "ipa", "p12", "key", "profile" or "libraries" (several at once). Empty when cancelled.
#[tauri::command]
pub async fn pick_file(app: AppHandle, kind: String) -> Result<Vec<String>, Failure> {
    blocking(move || {
        let dialog = app.dialog().file();
        let paths = match kind.as_str() {
            "ipa" => dialog.set_title("Choose an app").add_filter("iOS and visionOS apps", &["ipa", "deb"]).blocking_pick_file().into_iter().collect(),
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
            "libraries" => dialog
                .set_title("Choose the Apple Music APK, or its two libraries")
                .add_filter("Apple Music APK or libraries", &["apk", "so"])
                .blocking_pick_files()
                .unwrap_or_default(),
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

/// Uses a local IPA instead of the official release. A `.deb` package (how Kodi and many other
/// apps are published) is repacked as an IPA first. Returns `{name, size, converted}`: the
/// chosen file, the IPA's size, and the app's name when the file was a package.
#[tauri::command]
pub async fn prepare_local(state: Shared<'_>, path: String) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err("Choose an .ipa or .deb file.".into());
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let (ipa, converted) = match package::format(&path)? {
            package::Format::Zip => (path, None),
            package::Format::Deb => {
                let scratch = std::env::temp_dir().join("apparition-converted");
                let _ = std::fs::remove_dir_all(&scratch);
                std::fs::create_dir_all(&scratch)?;
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "app".into());
                let out = scratch.join(format!("{stem}.ipa"));
                let done = package::convert(&path, &out)?;
                (out, Some(done.app))
            }
            package::Format::Unknown => return Err(Failure::new(format!("{name} isn't an .ipa or a .deb package."))),
        };
        let size = std::fs::metadata(&ipa)?.len();
        *state.app.lock().unwrap() = Some(Prepared { path: ipa });
        Ok(json!({ "name": name, "size": size, "converted": converted }))
    })
    .await
}

#[derive(Clone, Serialize, Deserialize)]
struct Assignment {
    team: String,
    device: String,
    source: String,
    target: String,
    extensions: Vec<String>,
    #[serde(default)]
    move_from: Option<String>,
}

impl Assignment {
    fn claims(&self) -> Vec<String> {
        let mut ids = vec![self.target.clone()];
        ids.extend(self.extensions.iter().filter_map(|id| id.strip_prefix(&self.source).map(|suffix| format!("{}{suffix}", self.target))));
        if let Some(from) = &self.move_from { if from != &self.target { ids.push(from.clone()); } }
        ids
    }
}

fn read_assignments(path: &std::path::Path) -> Result<Vec<Assignment>, Failure> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let records: Vec<Assignment> = serde_json::from_slice(&bytes).map_err(|e| Failure::new(format!("Cannot read App ID assignments: {e}")))?;
            for (i, a) in records.iter().enumerate() {
                if records[..i].iter().any(|b| a.team == b.team && a.device == b.device && (a.source == b.source || a.claims().iter().any(|id| b.claims().contains(id)))) {
                    return Err(Failure::new("Conflicting App ID assignments; repair the database before installing"));
                }
            }
            Ok(records)
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_assignments(path: &std::path::Path, assignments: &[Assignment]) -> Result<(), Failure> {
    std::fs::create_dir_all(path.parent().ok_or("No assignments directory")?)?;
    let temporary = path.with_extension("pending");
    std::fs::write(&temporary, serde_json::to_vec_pretty(assignments).map_err(|e| Failure::new(e.to_string()))?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

// MARK: Install

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallOptions {
    udid: String,
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
/// slot fails with `slot` set. Data removal follows the explicit install-review choice.
#[tauri::command]
pub async fn install(app: AppHandle, state: Shared<'_>, options: InstallOptions) -> Result<Value, Failure> {
    let state = state.inner().clone();
    blocking(move || {
        let _install_guard = state.install_lock.try_lock().map_err(|_| Failure::new("An installation is already running"))?;
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
            let inventory = apparition_core::install_plan::inspect(&prepared.path)?;
            let is_apparition = inventory.id == APPARITION_ID;
            let assignment_path = state.store.dir.join("app-id-assignments.json");
            let mut assignments = read_assignments(&assignment_path).map_err(|e| rootcause::report!("{}", e.message))?;
            let (team, ids) = {
                let mut guard = state.session.lock().unwrap();
                match guard.as_mut().filter(|_| certificate.is_none()) {
                    Some(session) => (session.team_id().to_string(), api::list_app_ids(session).await?),
                    None => (String::new(), json!({"appIds": []})),
                }
            };
            let previous = assignments.iter().find(|a| a.team == team && a.device == info.udid && a.source == inventory.id).cloned();
            let certificate_plan = match &certificate {
                Some(cert) => api::offline_install_plan(&cert.dir, prepared.path.clone(), &cert.id, &info.udid, api::Platform::of_product(&info.product_type))?,
                None => Value::Null,
            };
            let default_target = certificate_plan["target"].as_str().map(str::to_owned)
                .unwrap_or_else(|| previous.as_ref().map(|a| a.target.clone()).unwrap_or_else(|| format!("{}.{}", inventory.id, team)));
            let supported: Vec<String> = certificate_plan["supportedExtensions"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
            let unsupported: Vec<String> = inventory.extensions.iter().filter(|e| if certificate.is_none() { e.extension_kit } else { !supported.contains(&e.id) }).map(|e| e.id.clone()).collect();
            let mut candidates = Vec::new();
            for id in ids["appIds"].as_array().into_iter().flatten() {
                let Some(identifier) = id["identifier"].as_str() else { continue };
                let installed = tunnel.installed_app(identifier).await?;
                let owner = assignments.iter().find(|a| a.team == team && a.device == info.udid && a.claims().contains(&identifier.to_string()));
                let reserved_child = owner.is_some_and(|a| a.source != inventory.id && a.target != identifier);
                candidates.push(json!({"id": identifier, "name": id["name"], "installed": installed, "owner": owner.map(|a| &a.source), "reservedChild": reserved_child}));
            }
            let usual_id = format!("{}.{}", inventory.id, team);
            let data_source = previous.as_ref().and_then(|a| a.move_from.clone().or_else(|| Some(a.target.clone())));
            let data_source = match data_source {
                Some(id) => Some(id),
                None if certificate.is_none()
                    && !assignments.iter().any(|a| a.team == team && a.device == info.udid && a.source != inventory.id && a.claims().contains(&usual_id))
                    && tunnel.installed_app(&usual_id).await?.is_some() => Some(usual_id.clone()),
                None => None,
            };
            let (target, extensions, data_mode) = match state.ask(&app, json!({
                "kind": "installPlan", "inventory": inventory, "appleId": certificate.is_none(),
                "candidates": candidates, "defaultTarget": default_target, "unsupported": unsupported,
                "destination": tunnel.installed_app(&default_target).await?,
                "previous": previous.as_ref().map(|a| &a.target),
                "selected": previous.as_ref().map(|a| a.extensions.clone()).unwrap_or_default(),
                "available": ids["availableQuantity"], "dataSource": data_source
            })) {
                Answer::InstallPlan { target, extensions, data_mode } => (target, extensions, data_mode),
                _ => return Err(rootcause::report!("Installation cancelled before signing")),
            };
            if !["keep", "discard"].contains(&data_mode.as_str()) { return Err(rootcause::report!("Choose whether to keep or discard app data")); }
            if certificate.is_none() && target != default_target && !candidates.iter().any(|c| c["id"].as_str() == Some(&target)) {
                return Err(rootcause::report!("Choose an App ID from this account"));
            }
            inventory.validate(&extensions, certificate.is_none())?;
            if extensions.iter().any(|e| unsupported.contains(e)) { return Err(rootcause::report!("A selected extension has no compatible provisioning profile")); }
            if certificate.is_none() {
                let registered: Vec<String> = candidates.iter().filter_map(|c| c["id"].as_str().map(str::to_owned)).collect();
                let needed = inventory.new_ids(&target, &extensions, &registered)?;
                if ids["availableQuantity"].as_u64().is_some_and(|available| needed as u64 > available) {
                    return Err(rootcause::report!("Not enough available App IDs for this selection. Remove extensions or reuse registered IDs"));
                }
            }
            if assignments.iter().any(|a| a.team == team && a.device == info.udid && a.source != inventory.id && a.target != target && a.claims().contains(&target)) {
                return Err(rootcause::report!("This identifier belongs to another app's extension or pending data transfer"));
            }
            let move_from = data_source.filter(|id| id != &target);
            for extension in inventory.extensions.iter().filter(|e| extensions.contains(&e.id)) {
                let suffix = extension.id.strip_prefix(&inventory.id).filter(|s| s.starts_with('.')).ok_or_else(|| rootcause::report!("Extension {} does not belong to this app", extension.name))?;
                let child = format!("{target}{suffix}");
                if assignments.iter().any(|a| a.team == team && a.device == info.udid && a.source != inventory.id && a.target != target && a.claims().contains(&child)) {
                    return Err(rootcause::report!("Extension {child} is assigned to another app. Deselect it before proceeding"));
                }
            }
            let selected_app = apparition_core::install_plan::select(&prepared.path, &extensions)?;
            let stripped: Vec<_> = inventory.extensions.iter().filter(|e| !extensions.contains(&e.id)).map(|e| &e.name).collect();
            // Reserve before signing: a failed install can still have registered portal IDs.
            if certificate.is_none() {
                assignments.retain(|a| !(a.team == team && a.device == info.udid && (a.source == inventory.id || a.target == target)));
                assignments.push(Assignment { team: team.clone(), device: info.udid.clone(), source: inventory.id.clone(), target: target.clone(), extensions: extensions.clone(), move_from: move_from.clone() });
                save_assignments(&assignment_path, &assignments).map_err(|e| rootcause::report!("{}", e.message))?;
            }
            stage(&app, "signing", json!(info.name));
            let mut apple_id: Option<(String, String)> = None;
            let signed = match &certificate {
                Some(cert) => api::sign_offline(
                    &cert.dir,
                    selected_app.clone(),
                    &cert.id,
                    None,
                    &info.udid,
                    false,
                    None,
                    api::Platform::of_product(&info.product_type),
                )?,
                None => {
                    let mut guard = state.session.lock().unwrap();
                    let session = guard.as_mut().ok_or_else(|| rootcause::report!("Sign in with your Apple ID first"))?;
                    apple_id = Some((session.email().to_string(), session.team_id().to_string()));
                    api::sign_with_apple_id_as(session, selected_app.clone(), &info.udid, &info.name, false, api::Platform::of_product(&info.product_type), Some(&target)).await?
                }
            };
            let signed_path = PathBuf::from(signed["signedPath"].as_str().ok_or_else(|| rootcause::report!("Signing returned no app"))?);
            let bundle_id = api::bundle_id(&signed_path).ok_or_else(|| rootcause::report!("The signed app has no bundle ID"))?;

            if bundle_id != target { return Err(rootcause::report!("Signer changed the reviewed App ID. Review the install again")); }
            let existing = tunnel.installed_app(&bundle_id).await?;
            if data_mode == "discard" && existing.is_some() {
                tunnel.uninstall(&bundle_id).await?;
            }
            stage(&app, "installing", json!({ "device": info.name, "update": existing.is_some() && data_mode != "discard" }));
            let source = prepared.path.is_file().then_some(prepared.path.as_path());
            // An update of an app that's open waits in installd until it closes, so only an
            // update stops waiting after 20 s; a new install, however large, is waited for.
            let reply = tunnel.install(&signed_path, source, if existing.is_some() && data_mode != "discard" { 20_000 } else { 0 }).await?;

            let mut migration = Value::Null;
            let mut migration_pending = false;
            if data_mode == "keep" && let Some(from) = &move_from && from != &bundle_id
                && tunnel.installed_app(from).await?.is_some() {
                migration_pending = true;
                if reply["queued"].as_bool() != Some(true) {
                    match tunnel.migrate_data(from, &bundle_id).await {
                        Ok(copied) => {
                            let complete = copied["failed"].as_array().is_some_and(|f| f.is_empty());
                            migration = copied;
                            if complete {
                                tunnel.uninstall(from).await?;
                                migration_pending = false;
                            }
                        }
                        Err(error) => migration = json!({"error": error.to_string()}),
                    }
                }
            }
            if !migration_pending && certificate.is_none() {
                if let Some(record) = assignments.iter_mut().find(|a| a.team == team && a.device == info.udid && a.source == inventory.id) { record.move_from = None; }
                save_assignments(&assignment_path, &assignments).map_err(|e| rootcause::report!("{}", e.message))?;
            }
            let mut handoff = Value::Null;
            let mut handoff_error = Value::Null;
            let mut pairing_handed_over = false;
            if options.hand_off && is_apparition {
                stage(&app, "handoff", Value::Null);
                // An iPhone or iPad paired over USB: Apparition gets its own pairing record.
                match state.store.hand_over_pairing(&mut tunnel, &bundle_id, &info.udid).await {
                    Ok(handed) => pairing_handed_over = handed,
                    Err(error) => handoff_error = json!(error.to_string()),
                }
                let apple_id_dir = state.store.apple_id_dir();
                let (kind, what) = match (&certificate, &apple_id) {
                    (Some(cert), _) => ("certificate", Handoff::Certificate { p12: &cert.p12, profiles: &cert.profiles }),
                    (None, Some((email, team_id))) => ("appleId", Handoff::AppleId { data_dir: &apple_id_dir, email, team_id: Some(team_id) }),
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
                "update": existing.is_some() && data_mode != "discard",
                "seconds": reply["elapsedMs"].as_u64().unwrap_or(0) as f64 / 1000.0,
                "queued": reply["queued"].as_bool() == Some(true),
                "strippedExtensions": stripped,
                "isApparition": is_apparition,
                "migration": migration,
                "migrationPending": migration_pending,
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

#[cfg(test)]
mod assignment_tests {
    use super::*;

    #[test]
    fn database_roundtrip_preserves_pending_migrations_and_blocks_overlapping_ids() {
        let dir = std::env::temp_dir().join(format!("apparition-assignment-test-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let path = dir.join("assignments.json");
        assert!(read_assignments(&path).unwrap().is_empty());
        let a = Assignment { team: "TEAM".into(), device: "device".into(), source: "org.app".into(), target: "org.pool.one".into(), extensions: vec!["org.app.share".into()], move_from: Some("org.old".into()) };
        save_assignments(&path, &[a.clone()]).unwrap();
        let read = read_assignments(&path).unwrap();
        assert_eq!(read[0].move_from.as_deref(), Some("org.old"));
        assert!(read[0].claims().contains(&"org.pool.one.share".to_string()));
        let mut b = a.clone(); b.source = "org.other".into(); b.target = "org.pool.one.share".into(); b.move_from = None; b.extensions.clear();
        save_assignments(&path, &[a.clone(), b.clone()]).unwrap();
        assert!(read_assignments(&path).is_err());
        b.device = "another device".into();
        save_assignments(&path, &[a,b]).unwrap();
        assert_eq!(read_assignments(&path).unwrap().len(), 2);
        std::fs::write(&path, b"broken database").unwrap();
        assert!(read_assignments(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
