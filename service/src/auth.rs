//! Password login, sessions and signed links, wire-compatible with `store/lib/auth.js`: the same
//! scrypt string in `config.json`, the same `data/sessions.json` and `data/secret`, the same
//! `?t=<expiry>.<hmac>` links. A Mac that switches from the Node store keeps its password, its
//! signed-in browsers and every link already handed out.

use base64::{Engine, engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}};
use ring::{digest, hmac, rand::{SecureRandom, SystemRandom}};
use subtle::ConstantTimeEq;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

pub const COOKIE: &str = "store_session";
const SESSION_DAYS: u64 = 30;
const DAY_MS: u64 = 86_400_000;
const LOG_N: u8 = 15; // 1 << 15, as the Node store writes it
const R: u32 = 8;
const P: u32 = 1;

fn sha_hex(input: &[u8]) -> String {
    digest::digest(&digest::SHA256, input).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 random bytes, base64url: a session or one-time code.
pub fn new_token() -> String {
    URL_SAFE_NO_PAD.encode(random(32))
}

fn random(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    SystemRandom::new().fill(&mut bytes).expect("system randomness");
    bytes
}

fn scrypt_bytes(password: &str, salt: &[u8], log_n: u8, r: u32, p: u32, len: usize) -> Option<Vec<u8>> {
    let params = scrypt::Params::new(log_n, r, p).ok()?;
    let mut out = vec![0u8; len];
    scrypt::scrypt(password.as_bytes(), salt, &params, &mut out).ok()?;
    Some(out)
}

/// `scrypt$N$r$p$salt$hash`, standard base64: what `set-password` writes and `login` checks.
pub fn hash_password(password: &str) -> Result<String, &'static str> {
    if password.chars().count() < 8 {
        return Err("use a password of at least 8 characters");
    }
    let salt = random(16);
    let hash = scrypt_bytes(password, &salt, LOG_N, R, P, 32).ok_or("scrypt failed")?;
    Ok(format!("scrypt${}${R}${P}${}${}", 1u32 << LOG_N, STANDARD.encode(salt), STANDARD.encode(hash)))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 6 || parts[0] != "scrypt" {
        return false;
    }
    let (Ok(n), Ok(r), Ok(p)) = (parts[1].parse::<u64>(), parts[2].parse::<u32>(), parts[3].parse::<u32>()) else {
        return false;
    };
    if n < 2 || !n.is_power_of_two() {
        return false;
    }
    let (Ok(salt), Ok(expected)) = (STANDARD.decode(parts[4]), STANDARD.decode(parts[5])) else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    match scrypt_bytes(password, &salt, n.trailing_zeros() as u8, r, p, expected.len()) {
        Some(actual) => actual.len() == expected.len() && bool::from(actual.ct_eq(&expected)),
        None => false,
    }
}

/// The value of one cookie in a `Cookie` header.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    created: u64,
    last_seen: u64,
    pw: String,
    /// `user` and anything a newer store adds: kept as written.
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, PartialEq)]
pub enum Login {
    Ok(String),
    Refused { status: u16, error: String },
}

#[derive(Default)]
struct Throttle {
    failures: Vec<u64>,
    locked_until: u64,
}

/// One per data directory: sessions, the link-signing secret and the failed-login throttle.
pub struct Auth {
    sessions_file: PathBuf,
    key: hmac::Key,
    sessions: Mutex<HashMap<String, Session>>,
    throttle: Mutex<Throttle>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

pub fn system_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("tmp-{}", new_token()));
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(bytes)?;
    fs::rename(tmp, path)
}

impl Auth {
    pub fn open(data_dir: &Path) -> std::io::Result<Auth> {
        Auth::open_with_clock(data_dir, Box::new(system_ms))
    }

    pub fn open_with_clock(data_dir: &Path, clock: Box<dyn Fn() -> u64 + Send + Sync>) -> std::io::Result<Auth> {
        fs::create_dir_all(data_dir)?;
        let secret_file = data_dir.join("secret");
        let secret = match fs::read(&secret_file) {
            Ok(bytes) if bytes.len() >= 32 => bytes,
            _ => {
                let bytes = random(32);
                write_private(&secret_file, &bytes)?;
                bytes
            }
        };
        let sessions_file = data_dir.join("sessions.json");
        let sessions = fs::read_to_string(&sessions_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Ok(Auth {
            sessions_file,
            key: hmac::Key::new(hmac::HMAC_SHA256, &secret),
            sessions: Mutex::new(sessions),
            throttle: Mutex::new(Throttle::default()),
            clock,
        })
    }

    fn save(&self, sessions: &HashMap<String, Session>) {
        if let Ok(json) = serde_json::to_vec(sessions) {
            let _ = write_private(&self.sessions_file, &json);
        }
    }

    /// Five wrong passwords lock logins for a minute, for everyone: behind a proxy every request
    /// comes from 127.0.0.1.
    pub fn login(&self, password: &str, password_hash: &str, user: Option<&str>) -> Login {
        let now = (self.clock)();
        let mut throttle = self.throttle.lock_ignoring_poison();
        if now < throttle.locked_until {
            let secs = (throttle.locked_until - now).div_ceil(1000);
            return Login::Refused { status: 429, error: format!("too many wrong passwords; try again in {secs} s") };
        }
        if !verify_password(password, password_hash) {
            throttle.failures.push(now);
            throttle.failures.retain(|t| *t >= now.saturating_sub(10 * 60_000));
            if throttle.failures.len() >= 5 {
                throttle.locked_until = now + 60_000;
                throttle.failures.clear();
            }
            return Login::Refused { status: 401, error: "wrong password".into() };
        }
        throttle.failures.clear();
        drop(throttle);
        let token = URL_SAFE_NO_PAD.encode(random(32));
        let mut sessions = self.sessions.lock_ignoring_poison();
        let cutoff = now.saturating_sub(SESSION_DAYS * DAY_MS);
        sessions.retain(|_, s| s.last_seen >= cutoff);
        let mut extra = Map::new();
        extra.insert("user".into(), user.map_or(Value::Null, |u| Value::String(u.into())));
        sessions.insert(
            sha_hex(token.as_bytes()),
            Session { created: now, last_seen: now, pw: sha_hex(password_hash.as_bytes())[..16].into(), extra },
        );
        self.save(&sessions);
        Login::Ok(token)
    }

    /// True for a live session made with the current password; slides its expiry at most hourly.
    pub fn check(&self, token: &str, password_hash: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let key = sha_hex(token.as_bytes());
        let mut sessions = self.sessions.lock_ignoring_poison();
        let Some(session) = sessions.get_mut(&key) else { return false };
        if session.pw != sha_hex(password_hash.as_bytes())[..16] {
            return false;
        }
        let now = (self.clock)();
        if session.last_seen < now.saturating_sub(SESSION_DAYS * DAY_MS) {
            sessions.remove(&key);
            self.save(&sessions);
            return false;
        }
        if now.saturating_sub(session.last_seen) > 3_600_000 {
            session.last_seen = now;
            self.save(&sessions);
        }
        true
    }

    pub fn logout(&self, token: &str) {
        let mut sessions = self.sessions.lock_ignoring_poison();
        if sessions.remove(&sha_hex(token.as_bytes())).is_some() {
            self.save(&sessions);
        }
    }

    pub fn logout_everywhere(&self) {
        let mut sessions = self.sessions.lock_ignoring_poison();
        sessions.clear();
        self.save(&sessions);
    }

    pub fn cookie(token: &str, secure: bool) -> String {
        format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}", SESSION_DAYS * 86400, if secure { "; Secure" } else { "" })
    }

    pub fn clear_cookie(secure: bool) -> String {
        format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}", if secure { "; Secure" } else { "" })
    }

    fn mac(&self, message: &str) -> Vec<u8> {
        hmac::sign(&self.key, message.as_bytes()).as_ref().to_vec()
    }

    /// `?t=` for a path: valid for `ttl_seconds`, for that path only.
    pub fn sign_path(&self, pathname: &str, ttl_seconds: u64) -> String {
        let exp = (self.clock)() / 1000 + ttl_seconds;
        format!("{exp}.{}", URL_SAFE_NO_PAD.encode(self.mac(&format!("{exp}\n{pathname}"))))
    }

    /// A lasting token for `label` (the AltSource feed URL), stable until `data/secret` changes.
    pub fn derive(&self, label: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.mac(&format!("derive\n{label}")))
    }

    pub fn verify_path(&self, pathname: &str, token: &str) -> bool {
        let Some((exp, mac)) = token.split_once('.') else { return false };
        if exp.is_empty() || exp.len() > 12 || !exp.bytes().all(|b| b.is_ascii_digit()) || mac.len() != 43 {
            return false;
        }
        let Ok(exp_secs) = exp.parse::<u64>() else { return false };
        if exp_secs < (self.clock)() / 1000 {
            return false;
        }
        let Ok(given) = URL_SAFE_NO_PAD.decode(mac) else { return false };
        hmac::verify(&self.key, format!("{exp}\n{pathname}").as_bytes(), &given).is_ok()
    }
}

trait IgnorePoison<T> {
    fn lock_ignoring_poison(&self) -> std::sync::MutexGuard<'_, T>;
}
impl<T> IgnorePoison<T> for Mutex<T> {
    /// A poisoned lock still holds consistent session data; keep serving.
    fn lock_ignoring_poison(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}
