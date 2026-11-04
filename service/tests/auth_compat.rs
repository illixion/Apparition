//! The Rust auth against the Node store's own `store/lib/auth.js`, on a shared data directory:
//! whatever one writes, the other must accept. Skipped where `node` is not installed.

use apparition_service::auth::{Auth, Login, hash_password, verify_password};
use std::{path::Path, process::Command};

fn auth_js() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../store/lib/auth.js").display().to_string()
}

/// Runs `script` in node with `auth.js` loaded as `A`; None when node is missing.
fn node(script: &str, args: &[&str]) -> Option<String> {
    let prelude = format!("const A=require({:?});", auth_js());
    let out = Command::new("node").arg("-e").arg(format!("{prelude}{script}")).arg("--").args(args).output().ok()?;
    assert!(out.status.success(), "node failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8(out.stdout).unwrap().trim().to_string())
}

#[test]
fn a_node_password_hash_verifies_in_rust() {
    let Some(hash) = node("console.log(A.hashPassword(process.argv[1]))", &["correct horse"]) else { return };
    assert!(verify_password("correct horse", &hash));
    assert!(!verify_password("wrong horse!", &hash));
}

#[test]
fn a_rust_password_hash_verifies_in_node() {
    let hash = hash_password("correct horse").unwrap();
    let Some(out) = node("console.log(A.verifyPassword(process.argv[1], process.argv[2]) + ',' + A.verifyPassword('nope nope', process.argv[2]))", &["correct horse", &hash]) else { return };
    assert_eq!(out, "true,false");
}

#[test]
fn short_passwords_and_malformed_hashes_are_refused() {
    assert!(hash_password("short").is_err());
    for bad in ["", "scrypt$1$2", "bcrypt$32768$8$1$AAAA$AAAA", "scrypt$30000$8$1$AAAA$AAAA", "scrypt$32768$8$1$AAAA$"] {
        assert!(!verify_password("whatever1", bad), "{bad}");
    }
}

#[test]
fn signed_links_cross_between_node_and_rust() {
    let dir = tempfile::tempdir().unwrap();
    let auth = Auth::open(dir.path()).unwrap(); // writes data/secret
    let dir_s = dir.path().to_str().unwrap();
    let from_rust = auth.sign_path("/ota/app.ipa", 3600);
    let Some(out) = node(
        "const a=A.createAuth(process.argv[1]);console.log(a.verifyPath('/ota/app.ipa',process.argv[2])+','+a.verifyPath('/ota/other.ipa',process.argv[2])+','+a.signPath('/mac/x.zip')+','+a.derive('altsource'))",
        &[dir_s, &from_rust],
    ) else { return };
    let parts: Vec<&str> = out.split(',').collect();
    assert_eq!(&parts[..2], ["true", "false"]);
    assert!(auth.verify_path("/mac/x.zip", parts[2]), "a link Node signed");
    assert!(!auth.verify_path("/mac/y.zip", parts[2]));
    assert_eq!(auth.derive("altsource"), parts[3]);
}

#[test]
fn a_node_session_is_valid_in_rust_and_a_rust_session_in_node() {
    let dir = tempfile::tempdir().unwrap();
    let dir_s = dir.path().to_str().unwrap();
    let hash = hash_password("correct horse").unwrap();

    let Some(node_token) = node("const a=A.createAuth(process.argv[1]);console.log(a.login('correct horse',process.argv[2],{user:'me'}).token)", &[dir_s, &hash]) else { return };
    let auth = Auth::open(dir.path()).unwrap(); // reads Node's sessions.json
    assert!(auth.check(&node_token, &hash));
    assert!(!auth.check(&node_token, &hash_password("another pass").unwrap()), "a new password signs everyone out");

    let Login::Ok(rust_token) = auth.login("correct horse", &hash, Some("me")) else { panic!("login refused") };
    let out = node("const a=A.createAuth(process.argv[1]);console.log(a.check(process.argv[2],process.argv[3])+','+a.check(process.argv[4],process.argv[3]))", &[dir_s, &rust_token, &hash, &node_token]).unwrap();
    assert_eq!(out, "true,true");
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
