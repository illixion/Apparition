//! Apparition's native core. One process-wide Apple ID session, driven through a small
//! JSON-returning C ABI. Every call blocks; call it off the main thread.
//!
//! Secrets: the password is used once for sign-in and never stored or logged. The
//! certificate's private key and the anisette state live in `<data_dir>/storage.json`.

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_void},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

use isideload::{
    anisette::remote_v3::RemoteV3AnisetteProvider,
    auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::{app_ids::AppIdsApi, developer_session::DeveloperSession, devices::DevicesApi, teams::DeveloperTeam},
    sideload::{SideloaderBuilder, TeamSelection, builder::MaxCertsBehavior, sideloader::Sideloader},
    util::{callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox}, storage::SideloadingStorage},
};
use rootcause::prelude::*;

pub mod api;
pub mod release;
pub(crate) mod account;
pub mod errors;
pub mod claimed_platform;
mod share;
mod nearby;
mod identity;
mod ipa;
pub mod package;
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
    sideloader: Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>,
    /// The next signing may make a certificate although Apparition elsewhere has one
    /// (`check_certificate`).
    allow_new_certificate: bool,
    /// When `check_certificate` last found the account's certificate for this install's key.
    certificate_checked: Option<std::time::Instant>,
}

impl Session {
    pub fn email(&self) -> &str {
        &self.email
    }

    pub fn team_id(&self) -> &str {
        &self.team.team_id
    }

    /// Lets the next signing make a certificate here, although Apparition on another device has
    /// one on the account (it stopped with [`SharedCertificate`] otherwise).
    pub fn allow_new_certificate(&mut self) {
        self.allow_new_certificate = true;
    }
}

/// Signing stopped before making a certificate: Apparition on another device or computer already
/// has one on the account, and this install has no key for it. Handing that install's key over
/// (share.rs, nearby.rs) lets both sign with one certificate, where a new one would take the
/// account's other slot of two. `Session::allow_new_certificate` (or
/// `apparition_allow_new_certificate`) makes one anyway.
#[derive(Debug)]
pub struct SharedCertificate;

impl std::fmt::Display for SharedCertificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Apparition on another device or computer already has a certificate for this Apple ID. Get its key from there, so both sign with that one, or make a new certificate here (a free account has room for two)."
        )
    }
}

impl std::error::Error for SharedCertificate {}

/// Whether `error` is signing stopping at [`SharedCertificate`].
pub fn is_shared_certificate(error: &Report) -> bool {
    error.iter_reports().any(|cause| cause.downcast_current_context::<SharedCertificate>().is_some())
}

/// The code of the developer services error behind `error` (7460: no room for a certificate).
fn developer_code(error: &Report) -> Option<i64> {
    error.iter_reports().find_map(|cause| match cause.downcast_current_context::<isideload::SideloadError>()? {
        isideload::SideloadError::DeveloperError(code, _) => Some(*code),
        _ => None,
    })
}

/// Apple IDs (lower case) whose next signing may make a certificate: what the app chose with
/// `apparition_allow_new_certificate`, kept across its sessions.
static NEW_CERTIFICATE_ALLOWED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// How long a found certificate is trusted to still be there: a refresh of every app signs many
/// times in a row, while one revoked from another device should be noticed.
const CERTIFICATE_RECHECK: std::time::Duration = std::time::Duration::from_secs(600);

/// Before signing with the Apple ID: stops with [`SharedCertificate`] where isideload would make
/// a certificate (no key here, or one that matches none of the account's) while the account
/// already has one Apparition made. isideload otherwise makes one on its own, so whichever
/// install signed first decided, and a third device found no room (7460). The first certificate,
/// with none of Apparition's on the account, is made as before.
async fn check_certificate(session: &mut Session) -> Result<(), Report> {
    if session.certificate_checked.is_some_and(|at| at.elapsed() < CERTIFICATE_RECHECK) {
        return Ok(());
    }
    share::adopt_key(&session.data_dir, &session.email);
    let ours = share::stored_public_key(&session.data_dir, &session.email);
    let team = session.team.clone();
    let certificates = match isideload::dev::certificates::CertificatesApi::list_ios_certs(session.sideloader.get_dev_session(), &team).await {
        Ok(certificates) => certificates,
        // isideload lists them again and reports what's wrong.
        Err(error) => {
            tracing::warn!("couldn't list the certificates before signing: {}", errors::describe(&error));
            return Ok(());
        }
    };
    let apparition: Vec<Vec<u8>> = certificates
        .iter()
        .filter(|cert| cert.machine_name.as_deref() == Some(account::MACHINE_NAME))
        .filter_map(|cert| account::public_key(cert.cert_content.as_ref()?.as_ref()))
        .collect();
    let check = certificate_check(ours.as_deref(), &apparition, || {
        let lower = session.email.to_lowercase();
        let mut allowed = NEW_CERTIFICATE_ALLOWED.lock().unwrap();
        let chosen = allowed.contains(&lower);
        allowed.retain(|email| *email != lower);
        std::mem::take(&mut session.allow_new_certificate) || chosen
    });
    match check {
        CertificateCheck::Ours => session.certificate_checked = Some(std::time::Instant::now()),
        CertificateCheck::First => {}
        CertificateCheck::Chosen => tracing::info!("making a certificate though Apparition elsewhere has one, as chosen"),
        CertificateCheck::Shared => {
            tracing::info!("not making a certificate: Apparition elsewhere has one ({} on the account)", apparition.len());
            return Err(report!(SharedCertificate).into_dynamic());
        }
    }
    Ok(())
}

/// What `check_certificate` makes of this install's key and the public keys of the account's
/// certificates Apparition made.
#[derive(Debug, PartialEq, Eq)]
enum CertificateCheck {
    /// One of them is this install's: signing uses it.
    Ours,
    /// Apparition has none: signing makes the first.
    First,
    /// Signing makes another, as the person chose (`chosen` is asked only then, and uses up the choice).
    Chosen,
    /// Stop and ask: getting the key from the install that has it costs no slot.
    Shared,
}

fn certificate_check(ours: Option<&[u8]>, apparition: &[Vec<u8>], chosen: impl FnOnce() -> bool) -> CertificateCheck {
    if ours.is_some_and(|ours| apparition.iter().any(|key| key == ours)) {
        CertificateCheck::Ours
    } else if apparition.is_empty() {
        CertificateCheck::First
    } else if chosen() {
        CertificateCheck::Chosen
    } else {
        CertificateCheck::Shared
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
    check_certificate(session).await?;
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
pub const REFUSED_ANISETTE: &str = "Apple refused the sign-in (-80045): since 12 October 2026 it turns away anisette that doesn't match the device the sign-in says it is. Apparition claims what the anisette server says it makes (Android for Dadoum's anisette-v3-server), and this server's anisette doesn't match that. Use the built-in anisette, Apparition's companion, or another anisette-v3 server.";
/// Apple took the password but refused the Xcode sign-in for anisette from the Android libraries.
pub const REFUSED_ANDROID_ANISETTE: &str = "Apple took the password but refused the Xcode sign-in (-80046) for anisette made by Apple's Android libraries, which the built-in anisette uses off a Mac and Dadoum's anisette-v3-server runs, even asked for as a Mac. Use a Mac's own anisette (Apparition on a Mac with macOS 26 or earlier, or its companion) or an anisette-v3 server that doesn't use those libraries.";

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

/// The code of the GrandSlam error behind `error` (-22411, -80045...), if one is.
fn auth_code(error: &Report) -> Option<i64> {
    error.iter_reports().find_map(|cause| match cause.downcast_current_context::<isideload::SideloadError>()? {
        isideload::SideloadError::AuthWithMessage(code, _) => Some(*code),
        _ => None,
    })
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
    let claim = claimed_platform::claim_for(&anisette_url).await;
    let served = claim.served;
    let after_sign_in = claim.after_sign_in();
    let anisette = claimed_platform::ClaimedPlatform::new(
        RemoteV3AnisetteProvider::new(&anisette_url, Box::new(FileStorage::open(storage_path.clone())), "0".to_string())?,
        claim,
    );
    let two_factor = &two_factor;
    let callback = |params: TwoFactorCallbackParams| {
        let response = two_factor(&params);
        async move { Ok(response) }
    };
    let refused = |error: Report| {
        if auth_code(&error) == Some(-80045) {
            error.context(REFUSED_ANISETTE).into_dynamic()
        } else {
            error.context("Apple ID sign-in failed").into_dynamic()
        }
    };
    let mut account =
        AppleAccount::builder(email).anisette_provider(anisette).login(&password, callback).await.map_err(refused)?;

    // The Xcode token, and the developer requests made with it, may need another claim than the
    // sign-in (claimed_platform, `after_sign_in`).
    let signing_in = account.grandslam_client.clone();
    let claim_after_sign_in = |account: &mut AppleAccount| {
        if let Some(client_info) = &after_sign_in {
            account.grandslam_client = Arc::new(signing_in.claiming(client_info.clone()));
        }
    };
    claim_after_sign_in(&mut account);
    let session = match DeveloperSession::from_account(&mut account).await {
        Ok(session) => session,
        // Claiming Android, a sign-in Apple doesn't trust yet got no 2FA step and no Xcode
        // token (-22411) until the code it pushes to the trusted devices was checked: sign in
        // again, which pushes one, and check it (vendor/isideload, code_after_login).
        Err(error) if served == claimed_platform::Served::Android && auth_code(&error) == Some(-22411) => {
            account.grandslam_client = signing_in.clone();
            account.code_after_login = true;
            account.login(&password, callback).await.map_err(refused)?;
            claim_after_sign_in(&mut account);
            DeveloperSession::from_account(&mut account).await.context("Failed to open a developer session")?
        }
        Err(error) if served == claimed_platform::Served::Android && auth_code(&error) == Some(-80046) => {
            return Err(error.context(REFUSED_ANDROID_ANISETTE).into_dynamic());
        }
        Err(error) => return Err(error.context("Failed to open a developer session").into_dynamic()),
    };
    drop(password);
    let mut sideloader: Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox> = SideloaderBuilder::new(session, email.to_string())
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
    let session = Session {
        email: email.to_string(),
        team,
        data_dir: data_dir.to_path_buf(),
        sideloader,
        allow_new_certificate: false,
        certificate_checked: None,
    };
    Ok((session, result))
}

/// Registers the device on the session's team (tolerating "already registered"), then signs
/// the app or IPA at `app_path` for it. Returns `{signedPath, devices, strippedExtensions}`.
pub async fn sign_with_apple_id(
    session: &mut Session,
    app_path: PathBuf,
    udid: &str,
    device_name: &str,
    strip_extensions: bool,
    platform: identity::Platform,
) -> Result<Value, Report> {
    identity::check_app_platform(&app_path, platform)?;
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

    // isideload stores a new key before asking Apple for its certificate: one Apple refused for
    // want of room (7460) would sit there unused, and an import later say it replaced one.
    check_certificate(session).await?;
    let had_key = share::stored_public_key(&session.data_dir, &session.email).is_some();
    let signed = session
        .sideloader
        .sign_app(app_path, Some(team.clone()), &capabilities, None::<fn(f32) -> std::future::Ready<()>>, None, Some(udid))
        .await;
    if let Err(error) = &signed
        && !had_key
        && developer_code(error) == Some(7460)
    {
        share::forget_key(&session.data_dir, &session.email);
    }
    let (signed, _) = signed.context("Signing failed")?;
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
///
/// Reads and writes go through to the file, so instances opened at different times agree: the
/// anisette provider's, the session's, and an import's (a key shared from another device is then
/// what the next signing uses, without signing in again).
struct FileStorage {
    path: PathBuf,
    /// What the file held when last read, for when it can't be read.
    values: Mutex<HashMap<String, String>>,
}

impl FileStorage {
    fn open(path: PathBuf) -> Self {
        let storage = Self { path, values: Mutex::default() };
        drop(storage.current());
        storage
    }

    /// The values now on disk (or the last ones read, if the file can't be read), locked.
    fn current(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        let mut values = self.values.lock().unwrap();
        if let Some(on_disk) = std::fs::read(&self.path).ok().and_then(|data| serde_json::from_slice(&data).ok()) {
            *values = on_disk;
        }
        values
    }

    fn get(&self, key: &str) -> Option<String> {
        self.current().get(key).cloned()
    }

    fn put(&self, key: &str, value: &str) -> Result<(), Report> {
        self.store(key, value)
    }
}

impl SideloadingStorage for FileStorage {
    /// Writes `key` into what's on disk now (an empty value removes it).
    fn store(&self, key: &str, value: &str) -> Result<(), Report> {
        let mut values = self.current();
        if value.is_empty() {
            values.remove(key);
        } else {
            values.insert(key.to_string(), value.to_string());
        }
        std::fs::write(&self.path, serde_json::to_vec(&*values)?)?;
        Ok(())
    }

    fn retrieve(&self, key: &str) -> Result<Option<String>, Report> {
        Ok(self.get(key))
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
            } else if is_shared_certificate(&error) {
                json!({"ok": false, "code": "sharedCertificate", "error": text})
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

/// Lets the next signing with `email` make a certificate, although Apparition on another device
/// has one on the account (signing stopped with code `sharedCertificate`). Returns `{ok}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_allow_new_certificate(email: *const c_char) -> *mut c_char {
    reply((|| {
        let email = unsafe { text(email) }?.to_lowercase();
        let mut allowed = NEW_CERTIFICATE_ALLOWED.lock().unwrap();
        if !allowed.contains(&email) {
            allowed.push(email);
        }
        Ok(json!({}))
    })())
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
        runtime().block_on(sign_with_apple_id(session, app_path, &udid, &device_name, strip_extensions, identity::Platform::THIS))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signing_stops_before_a_second_apparition_certificate() {
        let (mine, other) = (vec![1u8, 2, 3], vec![4u8, 5, 6]);
        let never = || panic!("asked for a choice it didn't need");
        assert_eq!(certificate_check(Some(&mine), &[other.clone(), mine.clone()], never), CertificateCheck::Ours);
        assert_eq!(certificate_check(None, &[], never), CertificateCheck::First);
        assert_eq!(certificate_check(Some(&mine), &[], never), CertificateCheck::First);
        // No key here, or one whose certificate was revoked: the other install's key would do.
        assert_eq!(certificate_check(None, &[other.clone()], || false), CertificateCheck::Shared);
        assert_eq!(certificate_check(Some(&mine), &[other.clone()], || false), CertificateCheck::Shared);
        assert_eq!(certificate_check(None, &[other], || true), CertificateCheck::Chosen);
        let error: Result<(), Report> = Err(report!(SharedCertificate).into_dynamic());
        let error = error.context("Signing failed").unwrap_err().into_dynamic();
        assert!(is_shared_certificate(&error));
        assert!(!is_shared_certificate(&report!("something else")));
    }

    #[test]
    fn storage_instances_see_each_others_writes() {
        let dir = std::env::temp_dir().join(format!("apparition-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("storage.json");
        // The session's, opened at sign-in, and an import's, opened later.
        let session = FileStorage::open(path.clone());
        session.put("anisette/state", "one").unwrap();
        let import = FileStorage::open(path.clone());
        import.put("someone/key", "shared").unwrap();
        assert_eq!(session.retrieve("someone/key").unwrap().as_deref(), Some("shared"));
        // A write from the session keeps the import's key.
        session.put("anisette/state", "two").unwrap();
        assert_eq!(FileStorage::open(path.clone()).get("someone/key").as_deref(), Some("shared"));
        import.put("someone/key", "").unwrap();
        assert_eq!(session.get("someone/key"), None);
        assert_eq!(session.get("anisette/state").as_deref(), Some("two"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_code_is_found_under_the_contexts() {
        let token = || -> Result<(), Report> {
            bail!(isideload::SideloadError::AuthWithMessage(-80046, "This action cannot be completed at this time.".into()))
        };
        let error = token()
            .context("GrandSlam error during app token request")
            .context("Failed to get xcode token from Apple account")
            .unwrap_err()
            .into_dynamic();
        assert_eq!(auth_code(&error), Some(-80046));
        assert_eq!(auth_code(&report!("no code here").into_dynamic()), None);
    }
}
