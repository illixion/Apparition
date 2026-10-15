//! Apparition's native core. One process-wide Apple ID session, driven through a small
//! JSON-returning C ABI. Every call blocks; call it off the main thread.
//!
//! Secrets: the password is used once for sign-in and never stored or logged. The
//! certificate's private key and the anisette state live in `<data_dir>/storage.json`.

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_void},
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use isideload::{
    anisette::remote_v3::RemoteV3AnisetteProvider,
    auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::{app_ids::AppIdsApi, developer_session::DeveloperSession, devices::DevicesApi, teams::DeveloperTeam},
    sideload::{SideloaderBuilder, TeamSelection, builder::MaxCertsBehavior, sideloader::Sideloader},
    util::{callbacks::MaxCertsCallbackBox, storage::SideloadingStorage},
};
use rootcause::prelude::*;

pub mod api;
pub mod release;
pub(crate) mod account;
pub mod errors;
pub mod claimed_platform;
mod share;
mod identity;
mod ipa;
mod jit;
mod tunnel;
#[cfg(all(feature = "anisette", unix))]
pub mod anisette;
#[cfg(feature = "desktop")]
pub mod companion;
#[cfg(feature = "desktop")]
pub mod relay;
use serde::Serialize;
use serde_json::{Value, json};

// Keep the idevice FFI symbols in the static library.
pub use idevice_ffi;

/// A signed-in Apple ID with its developer session: the app keeps one in `SESSION`, the
/// desktop installer holds its own.
pub struct Session {
    email: String,
    team: DeveloperTeam,
    /// Where its storage.json (anisette state, the certificate key) lives.
    data_dir: PathBuf,
    sideloader: Sideloader<MaxCertsCallbackBox>,
}

impl Session {
    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn team_id(&self) -> &str {
        &self.team.team_id
    }
}

/// Experiment (profile reuse): signs `app_path` with the free profile of an App ID the team
/// already has (`profile_app_id`, e.g. `com.utmapp.UTM.<team>`), keeping the app's own bundle
/// ID or `bundle_id`. Registers nothing, so it uses up no App ID; the certificate is the one
/// this install already signs with (none is made). See `identity::sign_keeping_bundle_id`.
pub async fn sign_with_existing_profile(
    session: &mut Session,
    app_path: PathBuf,
    profile_app_id: &str,
    bundle_id: Option<&str>,
    minimal: bool,
) -> Result<Value, Report> {
    use isideload::{
        dev::app_ids::AppIdsApi,
        sideload::cert_identity::CertificateIdentity,
    };
    let team = session.team.clone();
    let storage = FileStorage::open(session.data_dir.join("storage.json"));
    let dev = session.sideloader.get_dev_session();
    let identity = CertificateIdentity::retrieve(
        account::MACHINE_NAME,
        &session.email,
        dev,
        &team,
        &storage,
        &MaxCertsBehavior::<MaxCertsCallbackBox>::Error,
    )
    .await
    .context("No usable certificate")?;
    let app_ids = dev.list_app_ids(&team, None).await?.app_ids;
    let app_id = app_ids
        .iter()
        .find(|id| id.identifier == profile_app_id)
        .ok_or_else(|| report!("The team has no App ID {profile_app_id}"))?;
    let profile = dev.download_team_provisioning_profile(&team, app_id, None).await?;
    identity::sign_keeping_bundle_id(
        identity.certificate,
        identity.private_key,
        profile.encoded_profile.as_ref().to_vec(),
        app_path,
        bundle_id,
        minimal,
    )
}

/// Experiment: `sign_with_existing_profile` for the app's session. Returns `{ok, signedPath, ...}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_sign_with_existing_profile(
    app_path: *const c_char,
    profile_app_id: *const c_char,
    bundle_id: *const c_char,
    minimal: bool,
) -> *mut c_char {
    reply((|| {
        let app_path = PathBuf::from(unsafe { text(app_path) }?);
        let profile_app_id = unsafe { text(profile_app_id) }?;
        let bundle_id = if bundle_id.is_null() { None } else { Some(unsafe { text(bundle_id) }?) };
        let mut guard = SESSION.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| report!("Not signed in"))?;
        runtime().block_on(sign_with_existing_profile(session, app_path, &profile_app_id, bundle_id.as_deref(), minimal))
    })())
}

/// What to pass as the anisette address to use the built-in provider (Apple's own libraries,
/// loaded in this process) instead of a v3 server. See [`builtin_anisette_available`].
pub const BUILTIN_ANISETTE: &str = "builtin";

/// Whether this build and platform has the built-in anisette provider.
pub fn builtin_anisette_available() -> bool {
    cfg!(all(feature = "anisette", unix))
}

/// Whether the built-in anisette uses this Mac's own headers (macOS 26 and earlier, or macOS 27
/// with SIP and AMFI off and the helper beside this program) rather than Apple's Android
/// libraries. Both sign in; a Mac's own goes through the usual 2FA (see claimed_platform).
/// Found once; it can take a moment.
pub fn native_anisette() -> bool {
    #[cfg(all(feature = "anisette", target_os = "macos"))]
    {
        anisette::native::source().is_some()
    }
    #[cfg(not(all(feature = "anisette", target_os = "macos")))]
    {
        false
    }
}

/// What Apple's `-80045` ("This action cannot be completed at this time") means here.
pub const REFUSED_ANISETTE: &str = "Apple refused the sign-in (-80045): since 12 October 2026 it turns away anisette that doesn't match the device the sign-in says it is. Apparition says Android for anisette from Apple's Android libraries (what anisette servers run) and a Mac for a Mac's own, so this anisette server makes neither. Use the built-in anisette, Apparition's companion, or another anisette-v3 server.";

/// The v3 server address to use for `requested`: itself, or the built-in provider's, which is
/// started on first use and stays up for the life of the process.
async fn anisette_address(requested: &str, data_dir: &std::path::Path) -> Result<String, Report> {
    if requested != BUILTIN_ANISETTE {
        return Ok(requested.to_string());
    }
    #[cfg(all(feature = "anisette", unix))]
    {
        static SERVER: tokio::sync::OnceCell<anisette::Server> = tokio::sync::OnceCell::const_new();
        let server = SERVER
            .get_or_try_init(|| async {
                // Next to the Apple ID's folder, so the libraries are fetched once, not per Apple ID.
                let dir = data_dir.parent().unwrap_or(data_dir).join("anisette");
                anisette::start(&dir, std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await
            })
            .await
            .context("Couldn't start the built-in anisette provider")?;
        Ok(server.url())
    }
    #[cfg(not(all(feature = "anisette", unix)))]
    {
        let _ = data_dir;
        bail!("This build has no built-in anisette provider; give the address of an anisette v3 server")
    }
}

/// Signs in with an Apple ID (anisette from the v3 server at `anisette_url`, or the built-in
/// provider for [`BUILTIN_ANISETTE`]; state kept in `<data_dir>/storage.json`) and opens a
/// developer session. `two_factor` answers 2FA.
/// Returns the session and `{email, team, teams}`.
pub async fn sign_in(
    email: &str,
    password: String,
    anisette_url: &str,
    data_dir: &std::path::Path,
    two_factor: impl Fn(&TwoFactorCallbackParams) -> TwoFactorCallbackResponse + Send + Sync,
) -> Result<(Session, Value), Report> {
    std::fs::create_dir_all(data_dir)?;
    let storage_path = data_dir.join("storage.json");
    let anisette_url = anisette_address(anisette_url, data_dir).await?;
    // Claim the platform the anisette was made on, or Apple refuses the sign-in (claimed_platform).
    let served = claimed_platform::served_by(&anisette_url).await;
    let anisette = claimed_platform::ClaimedPlatform::new(
        RemoteV3AnisetteProvider::new(&anisette_url, Box::new(FileStorage::open(storage_path.clone())), "0".to_string())?,
        served,
    );
    let two_factor = &two_factor;
    let callback = |params: TwoFactorCallbackParams| {
        let response = two_factor(&params);
        async move { Ok(response) }
    };
    let refused = |error: Report| {
        if format!("{error:?}").contains("-80045") {
            error.context(REFUSED_ANISETTE).into_dynamic()
        } else {
            error.context("Apple ID sign-in failed").into_dynamic()
        }
    };
    let mut account =
        AppleAccount::builder(email).anisette_provider(anisette).login(&password, callback).await.map_err(refused)?;

    let session = match DeveloperSession::from_account(&mut account).await {
        Ok(session) => session,
        // Claiming Android, a sign-in Apple doesn't trust yet gets no 2FA step and no Xcode
        // token (-22411) until the code it pushes to the trusted devices is checked: sign in
        // again, which pushes one, and check it (vendor/isideload, code_after_login).
        Err(error) if served == claimed_platform::Served::Android && format!("{error:?}").contains("-22411") => {
            account.code_after_login = true;
            account.login(&password, callback).await.map_err(refused)?;
            DeveloperSession::from_account(&mut account).await.context("Failed to open a developer session")?
        }
        Err(error) => return Err(error.context("Failed to open a developer session").into_dynamic()),
    };
    drop(password);
    let mut sideloader: Sideloader<MaxCertsCallbackBox> = SideloaderBuilder::new(session, email.to_string())
        .team_selection(TeamSelection::First)
        // Never revoke someone's existing certificates on our own: report instead.
        .max_certs_behavior(MaxCertsBehavior::Error)
        .storage(Box::new(FileStorage::open(storage_path)))
        .machine_name(account::MACHINE_NAME.to_string())
        .delete_app_after_install(false)
        .build();
    let teams = isideload::dev::teams::TeamsApi::list_teams(sideloader.get_dev_session()).await?;
    let team = sideloader.get_team().await?;
    let result = json!({
        "email": email,
        "team": team_info(&team),
        "teams": teams.iter().map(team_info).collect::<Vec<_>>(),
    });
    Ok((Session { email: email.to_string(), team, data_dir: data_dir.to_path_buf(), sideloader }, result))
}

/// Registers the device on the session's team (tolerating "already registered"), then signs
/// the app or IPA at `app_path` for it. Returns `{signedPath, devices, strippedExtensions}`.
pub async fn sign_with_apple_id(
    session: &mut Session,
    app_path: PathBuf,
    udid: &str,
    device_name: &str,
    strip_extensions: bool,
) -> Result<Value, Report> {
    let mut app_path = app_path;
    let mut stripped = Vec::new();
    if strip_extensions {
        (app_path, stripped) = extract_without_extensions(&app_path)?;
    }
    let team = session.team.clone();
    // Entitlements the app asks for that only a capability on its App ID puts in the profile.
    let asked = identity::app_entitlements(&app_path).unwrap_or_default();
    let capabilities: Vec<String> = CAPABILITIES
        .iter()
        .filter(|(entitlement, _)| asked.get(entitlement).and_then(plist::Value::as_boolean) == Some(true))
        .map(|(_, capability)| capability.to_string())
        .collect();
    let dev = session.sideloader.get_dev_session();
    dev.ensure_device_registered(&team, device_name, udid, None)
        .await
        .context("Device registration failed")?;
    let devices = dev.list_devices(&team, None).await?;
    let registered: Vec<Value> = devices
        .iter()
        .map(|d| json!({"name": d.name, "udid": d.device_number, "status": d.status}))
        .collect();

    let (signed, _) = session
        .sideloader
        .sign_app(app_path, Some(team.clone()), &capabilities, None::<fn(f32) -> std::future::Ready<()>>, None, Some(udid))
        .await
        .context("Signing failed")?;
    let mut result = json!({"signedPath": signed.to_string_lossy(), "devices": registered, "strippedExtensions": stripped});
    if !capabilities.is_empty() {
        let signed_with: Vec<&str> = identity::app_entitlements(&signed)
            .map(|e| CAPABILITIES.iter().filter(|(key, _)| e.contains_key(key)).map(|(key, _)| *key).collect())
            .unwrap_or_default();
        result["capabilities"] = json!({"requested": capabilities, "signed": signed_with});
    }
    Ok(result)
}

/// App entitlements that need a capability turned on for the App ID, and its portal ID.
const CAPABILITIES: &[(&str, &str)] = &[
    ("com.apple.developer.kernel.increased-memory-limit", "INCREASED_MEMORY_LIMIT"),
    ("com.apple.developer.kernel.extended-virtual-addressing", "EXTENDED_VIRTUAL_ADDRESSING"),
];

/// Why each capability in `report` (`sign_with_apple_id`'s `capabilities`) didn't reach the
/// signed app at `signed`: asks Apple for it again, alone, and keeps the reply, since signing only
/// logs it. A free team gets "not eligible" for EXTENDED_VIRTUAL_ADDRESSING. Costs a request per
/// capability missing, so it's for showing a person, not for every refresh.
pub async fn capability_refusals(session: &mut Session, signed: &std::path::Path, report: &Value) -> serde_json::Map<String, Value> {
    let signed_with: Vec<&str> = report["signed"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let missing: Vec<String> = report["requested"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|capability| CAPABILITIES.iter().any(|(key, id)| id == capability && !signed_with.contains(key)))
        .map(str::to_string)
        .collect();
    let mut refused = serde_json::Map::new();
    if missing.is_empty() {
        return refused;
    }
    let team = session.team.clone();
    let dev = session.sideloader.get_dev_session();
    let app_id = async {
        let bundle_id = api::bundle_id(signed).ok_or_else(|| report!("the signed app has no bundle ID"))?;
        let ids = dev.list_app_ids(&team, None).await?;
        ids.app_ids.into_iter().find(|id| id.identifier == bundle_id).ok_or_else(|| report!("no App ID {bundle_id}"))
    }
    .await;
    for capability in missing {
        let reason = match &app_id {
            Ok(app_id) => match dev.enable_capabilities(&team, app_id, std::slice::from_ref(&capability)).await {
                Ok(()) => "accepted, but the profile doesn't grant it".to_string(),
                Err(error) => errors::describe(&error),
            },
            Err(error) => errors::describe(error),
        };
        refused.insert(capability, json!(reason));
    }
    refused
}

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static SESSION: Mutex<Option<Session>> = Mutex::new(None);

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
        errors::install_hooks();
        let _ = isideload::init();
        tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime")
    })
}

/// A key-value store in one JSON file. Holds the anisette state and the signing identity.
struct FileStorage {
    path: PathBuf,
    values: Mutex<HashMap<String, String>>,
}

impl FileStorage {
    fn open(path: PathBuf) -> Self {
        let values = std::fs::read(&path)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        Self { path, values: Mutex::new(values) }
    }
}

impl FileStorage {
    fn get(&self, key: &str) -> Option<String> {
        self.values.lock().unwrap().get(key).cloned()
    }

    fn put(&self, key: &str, value: &str) -> Result<(), Report> {
        self.store(key, value)
    }
}

impl SideloadingStorage for FileStorage {
    /// Merges into what's on disk: another instance (the anisette provider's, an imported
    /// shared key) may have written since this one was opened.
    fn store(&self, key: &str, value: &str) -> Result<(), Report> {
        let mut values = self.values.lock().unwrap();
        if let Some(on_disk) = std::fs::read(&self.path).ok().and_then(|d| serde_json::from_slice::<HashMap<String, String>>(&d).ok()) {
            for (name, stored) in on_disk {
                values.entry(name).or_insert(stored);
            }
        }
        if value.is_empty() {
            values.remove(key);
        } else {
            values.insert(key.to_string(), value.to_string());
        }
        std::fs::write(&self.path, serde_json::to_vec(&*values)?)?;
        Ok(())
    }

    fn retrieve(&self, key: &str) -> Result<Option<String>, Report> {
        Ok(self.values.lock().unwrap().get(key).cloned())
    }
}

/// Asked for a 2FA response. `params_json` describes the request; the callback returns a
/// string that stays valid until the next call: `code:<digits>`, `devices`, `sms:<id>`,
/// `resend` or `abort` (NULL also aborts).
pub type TwoFactorFn = extern "C" fn(context: *mut c_void, params_json: *const c_char) -> *const c_char;

struct Callback {
    function: TwoFactorFn,
    context: usize,
}

impl Callback {
    fn ask(&self, params: &TwoFactorCallbackParams) -> TwoFactorCallbackResponse {
        let numbers: Vec<Value> = params
            .numbers
            .iter()
            .map(|n| json!({"id": n.id, "number": n.number_with_dial_code}))
            .collect();
        let request = json!({
            "lastError": params.last_error,
            "unknown": params.unknown,
            "sms": params.sms,
            "numbers": numbers,
            "selectedNumberId": params.selected_number_id,
        });
        let request = CString::new(request.to_string()).unwrap_or_default();
        let reply = (self.function)(self.context as *mut c_void, request.as_ptr());
        if reply.is_null() {
            return TwoFactorCallbackResponse::Abort;
        }
        let reply = unsafe { CStr::from_ptr(reply) }.to_string_lossy().trim().to_string();
        match reply.split_once(':') {
            Some(("code", code)) => TwoFactorCallbackResponse::SubmitCode(code.trim().to_string()),
            Some(("sms", id)) => id
                .trim()
                .parse()
                .map(TwoFactorCallbackResponse::SendSms)
                .unwrap_or(TwoFactorCallbackResponse::Abort),
            _ => match reply.as_str() {
                "devices" => TwoFactorCallbackResponse::SendToDevices,
                "resend" => TwoFactorCallbackResponse::ResendCode,
                _ => TwoFactorCallbackResponse::Abort,
            },
        }
    }
}

fn reply(result: Result<Value, Report>) -> *mut c_char {
    let value = match result {
        Ok(Value::Object(mut map)) => {
            map.insert("ok".into(), true.into());
            Value::Object(map)
        }
        Ok(other) => json!({"ok": true, "value": other}),
        Err(error) => {
            let text = errors::describe(&error);
            // A security-key-only account answers the 2FA step with a WebAuthn challenge, and
            // isideload reports the raw response (account name, key names) as the error.
            if text.contains("fsaChallenge") {
                json!({
                    "ok": false,
                    "code": "securityKeysRequired",
                    "error": "This Apple ID uses security keys for two-factor authentication, which Apparition can't answer on the headset. Use an Apple ID without security keys.",
                })
            } else {
                json!({"ok": false, "error": text})
            }
        }
    };
    CString::new(value.to_string()).unwrap_or_default().into_raw()
}

unsafe fn text(pointer: *const c_char) -> Result<String, Report> {
    if pointer.is_null() {
        bail!("missing argument");
    }
    Ok(unsafe { CStr::from_ptr(pointer) }.to_str()?.to_string())
}

#[derive(Serialize)]
struct TeamInfo {
    id: String,
    name: Option<String>,
    kind: Option<String>,
}

fn team_info(team: &DeveloperTeam) -> TeamInfo {
    TeamInfo { id: team.team_id.clone(), name: team.name.clone(), kind: team.r#type.clone() }
}

/// Frees a string returned by any `apparition_*` call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_string_free(string: *mut c_char) {
    if !string.is_null() {
        drop(unsafe { CString::from_raw(string) });
    }
}

/// Sends isideload's tracing output (info and up) to `log_path`. Call once, before anything else.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_init_logging(log_path: *const c_char) {
    let Ok(path) = (unsafe { text(log_path) }) else { return };
    let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { return };
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
}

/// Signs in with an Apple ID and opens a developer session, replacing any previous one.
/// Returns `{ok, email, team, teams}` or `{ok: false, error}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_login(
    email: *const c_char,
    password: *const c_char,
    anisette_url: *const c_char,
    data_dir: *const c_char,
    two_factor: TwoFactorFn,
    context: *mut c_void,
) -> *mut c_char {
    let callback = Callback { function: two_factor, context: context as usize };
    reply((|| {
        let email = unsafe { text(email) }?;
        let password = unsafe { text(password) }?;
        let anisette_url = unsafe { text(anisette_url) }?;
        let data_dir = PathBuf::from(unsafe { text(data_dir) }?);
        runtime().block_on(async move {
            let (session, result) = sign_in(&email, password, &anisette_url, &data_dir, |params| callback.ask(params)).await?;
            *SESSION.lock().unwrap() = Some(session);
            Ok(result)
        })
    })())
}

/// The signed-in account, or `{ok, signedIn: false}`.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_session() -> *mut c_char {
    let guard = SESSION.lock().unwrap();
    reply(Ok(match guard.as_ref() {
        Some(session) => json!({"signedIn": true, "email": session.email, "team": team_info(&session.team)}),
        None => json!({"signedIn": false}),
    }))
}

/// Forgets the signed-in session (this process only; nothing is sent to Apple). Returns `{ok}`.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_logout() -> *mut c_char {
    *SESSION.lock().unwrap() = None;
    reply(Ok(json!({})))
}

/// Registers the device on the team (tolerating "already registered"), then signs the
/// app or IPA at `app_path` for it. Returns `{ok, signedPath, devices}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_sign(
    app_path: *const c_char,
    udid: *const c_char,
    device_name: *const c_char,
    strip_extensions: bool,
) -> *mut c_char {
    reply((|| {
        let app_path = PathBuf::from(unsafe { text(app_path) }?);
        let udid = unsafe { text(udid) }?;
        let device_name = unsafe { text(device_name) }?;
        let mut guard = SESSION.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| report!("Not signed in"))?;
        runtime().block_on(sign_with_apple_id(session, app_path, &udid, &device_name, strip_extensions))
    })())
}

/// Unpacks an IPA next to it and removes `Extensions/` (ExtensionKit) from the app, which a
/// re-signed app can't keep: installd rejects the extension under a renamed bundle ID.
/// Returns the unpacked .app and the names removed.
fn extract_without_extensions(ipa: &std::path::Path) -> Result<(PathBuf, Vec<String>), Report> {
    let target = std::env::temp_dir().join(format!(
        "{}_stripped",
        ipa.file_name().ok_or_else(|| report!("bad path"))?.to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target)?;
    zip::ZipArchive::new(std::fs::File::open(ipa)?)?.extract(&target)?;
    let app = std::fs::read_dir(target.join("Payload"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "app"))
        .ok_or_else(|| report!("no .app in Payload"))?;
    let extensions = app.join("Extensions");
    let mut removed = Vec::new();
    if extensions.is_dir() {
        for entry in std::fs::read_dir(&extensions)?.filter_map(Result::ok) {
            removed.push(entry.file_name().to_string_lossy().to_string());
        }
        std::fs::remove_dir_all(&extensions)?;
    }
    Ok((app, removed))
}

/// Installs a signed app over an open RSD tunnel (from idevice's `tunnel_create_rppairing`):
/// AFC upload to PublicStaging, then installation_proxy. The handles are borrowed.
/// Returns `{ok, elapsedMs}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_install(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    app_path: *const c_char,
) -> *mut c_char {
    reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let app_path = PathBuf::from(unsafe { text(app_path) }?);
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        let started = std::time::Instant::now();
        runtime().block_on(async {
            isideload::sideload::install::install_app_rsd(adapter, handshake, &app_path, |progress| {
                tracing::info!("install progress {progress}%");
            })
            .await
            .context("Install failed")?;
            Ok(json!({"elapsedMs": started.elapsed().as_millis() as u64}))
        })
    })())
}

/// Every user app on the device, whoever installed it, from installation_proxy's Lookup over
/// the tunnel: `{ok, apps: [{bundleId, name, version, debuggable, teamId, signer}]}`. An app is
/// `debuggable` (so it can be launched with JIT) when its signature grants get-task-allow:
/// development-signed apps (Xcode, build-and-sign, SideStore), not App Store ones.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_list_apps(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
) -> *mut c_char {
    reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        runtime().block_on(async {
            use idevice::RsdService;
            let mut installer = idevice::installation_proxy::InstallationProxyClient::connect_rsd(adapter, handshake)
                .await
                .context("installation_proxy connect failed")?;
            let apps = installer.get_apps(Some("User"), None).await.context("app lookup failed")?;
            let string = |info: &plist::Dictionary, key: &str| info.get(key).and_then(plist::Value::as_string).map(str::to_string);
            let mut list: Vec<Value> = apps
                .into_iter()
                .filter_map(|(bundle_id, info)| {
                    let info = info.into_dictionary()?;
                    let entitlements = info.get("Entitlements").and_then(plist::Value::as_dictionary);
                    let entitlement = |key: &str| entitlements.and_then(|e| e.get(key));
                    Some(json!({
                        "bundleId": bundle_id,
                        "name": string(&info, "CFBundleDisplayName").or_else(|| string(&info, "CFBundleName")).unwrap_or_else(|| bundle_id.clone()),
                        "version": string(&info, "CFBundleShortVersionString"),
                        "debuggable": entitlement("get-task-allow").and_then(plist::Value::as_boolean).unwrap_or(false),
                        "teamId": entitlement("com.apple.developer.team-identifier").and_then(plist::Value::as_string),
                        "signer": string(&info, "SignerIdentity"),
                    }))
                })
                .collect();
            list.sort_by_key(|app| app["name"].as_str().unwrap_or_default().to_lowercase());
            Ok(json!({ "apps": list }))
        })
    })())
}

/// Deletes `bundle_id` and its data from the device through installation_proxy over the
/// tunnel. Returns `{ok}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_uninstall(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    bundle_id: *const c_char,
) -> *mut c_char {
    reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = unsafe { text(bundle_id) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        runtime().block_on(async {
            use idevice::RsdService;
            let mut installer = idevice::installation_proxy::InstallationProxyClient::connect_rsd(adapter, handshake)
                .await
                .context("installation_proxy connect failed")?;
            installer.uninstall(bundle_id, None).await.context("Uninstall failed")?;
            Ok(json!({}))
        })
    })())
}

// MARK: JIT

/// Launches `bundle_id` suspended over the tunnel, attaches debugserver, and serves its JIT
/// requests (see `jit`). Waits up to `wait_seconds` (0: no limit) for the first request (UTM
/// asks when a VM starts). Returns `{ok, pid, events, preparedBytes, detached, ended}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_launch_jit(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    bundle_id: *const c_char,
    wait_seconds: u32,
) -> *mut c_char {
    reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = unsafe { text(bundle_id) }?;
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        runtime().block_on(jit::serve(adapter, handshake, jit::Target::Launch(&bundle_id), wait_seconds, &mut |_| {}))
    })())
}

/// Asks the JIT session serving `key` (a bundle ID or a pid) to detach now, leaving the app
/// running without a debugger; that session's call then returns with `ended: "stopped"`.
/// Returns `{ok, stopped}` (false: no session served it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_jit_stop(key: *const c_char) -> *mut c_char {
    reply((|| {
        let key = unsafe { text(key) }?;
        Ok(json!({ "stopped": jit::stop(&key) }))
    })())
}

/// Attaches debugserver to the running process `pid` (0: `bundle_id`'s running process) and
/// serves its JIT requests, as `apparition_launch_jit` does. With `bundle_id`, that app is
/// brought back to the front once attached: it asked from its own screen (Madeira's Enable
/// JIT, through an `enable-jit` link), and opening Apparition left it. `bundle_id` may be null
/// when `pid` is given. Returns the same reply.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_attach_jit(
    adapter: *mut idevice_ffi::core_device_proxy::AdapterHandle,
    handshake: *mut idevice_ffi::rsd::RsdHandshakeHandle,
    pid: u64,
    bundle_id: *const c_char,
    wait_seconds: u32,
) -> *mut c_char {
    reply((|| {
        if adapter.is_null() || handshake.is_null() {
            bail!("missing tunnel");
        }
        let bundle_id = if bundle_id.is_null() { None } else { Some(unsafe { text(bundle_id) }?) };
        let adapter = unsafe { &mut (*adapter).0 };
        let handshake = unsafe { &mut (*handshake).0 };
        runtime().block_on(async {
            let pid = match (pid, &bundle_id) {
                (0, Some(bundle_id)) => jit::running_pid(adapter, handshake, bundle_id)
                    .await?
                    .ok_or_else(|| report!("{bundle_id} isn't running"))?,
                (0, None) => bail!("no process to attach to"),
                (pid, _) => pid,
            };
            let target = jit::Target::Attach { pid, bring_back: bundle_id.as_deref() };
            jit::serve(adapter, handshake, target, wait_seconds, &mut |_| {}).await
        })
    })())
}

#[cfg(all(test, feature = "anisette", unix))]
mod builtin_anisette_tests {
    /// The built-in address starts a server next to the Apple ID's folder and answers. Reaches
    /// the network for the libraries: `APPARITION_NET=1`.
    #[tokio::test(flavor = "multi_thread")]
    async fn builtin_address_serves_client_info() {
        if std::env::var_os("APPARITION_NET").is_none() {
            return;
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = std::env::temp_dir().join(format!("apparition-builtin-{}", std::process::id()));
        let url = super::anisette_address(super::BUILTIN_ANISETTE, &root.join("apple-id")).await.unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        // (The libraries are loaded once per process, so another test may have fetched them
        // into its own folder first.)
        let info = reqwest::get(format!("{url}/v3/client_info")).await.unwrap().text().await.unwrap();
        assert!(info.contains("client_info"), "{info}");
        // Anything else is passed through untouched.
        assert_eq!(super::anisette_address("http://example.test:1", &root).await.unwrap(), "http://example.test:1");
        std::fs::remove_dir_all(&root).ok();
    }
}
