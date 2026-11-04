//! The Rust auth against what the old Node store wrote. `tests/fixtures/node-data/` holds a `secret`
//! and a `sessions.json` as `store/lib/auth.js` produced them, and the constants below are the
//! values it computed, all captured from the Node store when it was removed: the scrypt string
//! for "correct horse", the session token behind the one entry in `sessions.json`, and the links
//! and `derive` token its `createAuth` made over that secret at `NODE_NOW`. Whatever the Node
//! store wrote, Rust must still accept, and sign byte for byte the same.

use apparition_service::auth::{Auth, Login, hash_password, verify_password};
use std::{path::Path, sync::{Arc, atomic::{AtomicU64, Ordering}}};

const NODE_HASH: &str = "scrypt$32768$8$1$ndy5ejy2IaC35eoDf/eXVg==$Ixz/y+IDQrLnyNv9WfnCZnGrQLNgD8f31rYHe+mu5oI=";
/// The token whose SHA-256 is the key in the fixture's `sessions.json`.
const NODE_TOKEN: &str = "M0nrhvRH2CamqnvFpVCVCJWD3Q8Lc_f9tIV838HLkQQ";
/// The clock the fixture was captured at (ms).
const NODE_NOW: u64 = 1_700_000_000_000;
/// `signPath('/mac/x.zip')` and `signPath('/ota/app.ipa', 3600)` at `NODE_NOW`, default ttl 3600.
const NODE_LINK_MAC: &str = "1700003600.eQbqJOOKVLF6BkXsgMJ4MhvkdT1KnhIoIa_oAGK3320";
const NODE_LINK_OTA: &str = "1700003600.OYxEKN5F2grsXxb-WwYeQEPuJqOGLI9FcNziZfHC4A0";
/// `derive('altsource')`.
const NODE_DERIVE_ALTSOURCE: &str = "FJNRiUwctaTumQ_XyFFCJ8vaCOfuLnyxCTIfjtJWITU";

/// A copy of the Node-written data directory, and an `Auth` on it whose clock the test moves.
fn node_data() -> (tempfile::TempDir, Auth, Arc<AtomicU64>) {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/node-data");
    for name in ["secret", "sessions.json"] {
        std::fs::copy(fixtures.join(name), dir.path().join(name)).unwrap();
    }
    let now = Arc::new(AtomicU64::new(NODE_NOW));
    let clock = now.clone();
    let auth = Auth::open_with_clock(dir.path(), Box::new(move || clock.load(Ordering::SeqCst))).unwrap();
    (dir, auth, now)
}

#[test]
fn a_node_password_hash_verifies_in_rust() {
    assert!(verify_password("correct horse", NODE_HASH));
    assert!(!verify_password("wrong horse!", NODE_HASH));
}

#[test]
fn a_rust_password_hash_has_the_shape_node_reads() {
    // Node's verifyPassword splits on `$`, base64-decodes salt and hash and reads N, r, p from it.
    let hash = hash_password("correct horse").unwrap();
    let parts: Vec<&str> = hash.split('$').collect();
    assert_eq!(parts.len(), 6, "{hash}");
    assert_eq!(&parts[..4], ["scrypt", "32768", "8", "1"]);
    assert_eq!((parts[4].len(), parts[5].len()), (24, 44), "standard base64 of 16 and 32 bytes: {hash}");
    assert!(verify_password("correct horse", &hash));
    assert_ne!(hash, NODE_HASH, "a fresh salt each time");
}

#[test]
fn short_passwords_and_malformed_hashes_are_refused() {
    assert!(hash_password("short").is_err());
    for bad in ["", "scrypt$1$2", "bcrypt$32768$8$1$AAAA$AAAA", "scrypt$30000$8$1$AAAA$AAAA", "scrypt$32768$8$1$AAAA$"] {
        assert!(!verify_password("whatever1", bad), "{bad}");
    }
}

#[test]
fn links_signed_by_node_verify_and_rust_signs_the_same_bytes() {
    let (_dir, auth, now) = node_data();
    assert!(auth.verify_path("/mac/x.zip", NODE_LINK_MAC), "a link Node signed");
    assert!(auth.verify_path("/ota/app.ipa", NODE_LINK_OTA));
    assert!(!auth.verify_path("/mac/y.zip", NODE_LINK_MAC), "only for its own path");
    assert!(!auth.verify_path("/mac/x.zip", NODE_LINK_OTA));
    assert_eq!(auth.sign_path("/mac/x.zip", 3600), NODE_LINK_MAC, "same secret, same clock, same link");
    assert_eq!(auth.sign_path("/ota/app.ipa", 3600), NODE_LINK_OTA);
    assert_eq!(auth.derive("altsource"), NODE_DERIVE_ALTSOURCE);
    now.fetch_add(3_601_000, Ordering::SeqCst);
    assert!(!auth.verify_path("/mac/x.zip", NODE_LINK_MAC), "expired");
}

#[test]
fn a_node_session_is_valid_in_rust_and_the_file_stays_in_node_s_shape() {
    let (dir, auth, _now) = node_data(); // reads Node's sessions.json
    assert!(auth.check(NODE_TOKEN, NODE_HASH));
    assert!(!auth.check(NODE_TOKEN, &hash_password("another pass").unwrap()), "a new password signs everyone out");
    assert!(!auth.check("not-a-token", NODE_HASH));

    let Login::Ok(rust_token) = auth.login("correct horse", NODE_HASH, Some("me")) else { panic!("login refused") };
    assert!(auth.check(&rust_token, NODE_HASH));
    // What Node's createAuth reads back: the SHA-256 of each token keys {created, lastSeen, pw, user}.
    let file: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.path().join("sessions.json")).unwrap()).unwrap();
    let sessions = file.as_object().unwrap();
    assert_eq!(sessions.len(), 2, "Node's session is kept beside the new one");
    let node_entry = &sessions["9ced1281a1463465d06116c66dc0748a4fd8cf6e324d0a27a543db286ab3adb7"];
    assert_eq!(node_entry["pw"], "d46e9d1b7a533de9", "sha256(passwordHash)[..16], the same in both");
    assert_eq!(node_entry["user"], "me");
    for (key, entry) in sessions {
        assert!(key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()), "{key}");
        let mut fields: Vec<&str> = entry.as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort();
        assert_eq!(fields, ["created", "lastSeen", "pw", "user"]);
    }
    assert_eq!(sessions.values().filter(|e| e["pw"] == "d46e9d1b7a533de9").count(), 2);
}

#[test]
fn five_wrong_passwords_lock_logins_for_a_minute() {
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_000_000));
    let clock = now.clone();
    let auth = Auth::open_with_clock(dir.path(), Box::new(move || clock.load(Ordering::SeqCst))).unwrap();
    let hash = hash_password("correct horse").unwrap();
    for _ in 0..5 {
        assert_eq!(auth.login("wrong one!", &hash, None), Login::Refused { status: 401, error: "wrong password".into() });
    }
    assert!(matches!(auth.login("correct horse", &hash, None), Login::Refused { status: 429, .. }));
    now.fetch_add(61_000, Ordering::SeqCst);
    assert!(matches!(auth.login("correct horse", &hash, None), Login::Ok(_)));
}

#[test]
fn links_expire_and_sessions_slide_out_after_thirty_days() {
    use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
    let dir = tempfile::tempdir().unwrap();
    let now = Arc::new(AtomicU64::new(1_700_000_000_000));
    let clock = now.clone();
    let auth = Auth::open_with_clock(dir.path(), Box::new(move || clock.load(Ordering::SeqCst))).unwrap();
    let hash = hash_password("correct horse").unwrap();
    let link = auth.sign_path("/p", 60);
    assert!(auth.verify_path("/p", &link));
    let Login::Ok(token) = auth.login("correct horse", &hash, None) else { panic!() };
    now.fetch_add(61_000, Ordering::SeqCst);
    assert!(!auth.verify_path("/p", &link));
    assert!(auth.check(&token, &hash));
    now.fetch_add(31 * 86_400_000, Ordering::SeqCst);
    assert!(!auth.check(&token, &hash));
}
