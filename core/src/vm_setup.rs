//! Serves the VM setup to a macOS VM in recoveryOS, from this Mac, over the local network.
//!
//! recoveryOS can't paste, so what the person types there has to be short:
//! `curl -s <this Mac's address>:4949|bash`. That fetches [`SCRIPT`] (selfhost/macos-vm/setup.sh,
//! with `@@BASE@@` replaced by the address the VM used to reach this Mac), which then fetches the
//! `apparition` program and its anisette helper from the same place and installs them. So the VM
//! gets exactly this build, with no download from the internet. The helper is sent byte for byte:
//! its signature carries the entitlements, so it must never be re-signed.
//!
//! There is no token: anything on the network that can reach the port could fetch the same two
//! files that are in every release. The server stops by itself once the guest confirms setup completed, or after
//! [`TIMEOUT`], so it listens only while someone is setting a VM up.
//!
//! The app's own anisette VM (vm.rs) is set up through the same server, by the host typing into the VM's recoveryOS
//! itself ([`serve_appliance`]). That serves more: the first-boot payload, the VM account's password among it. So
//! everything is served only under an unguessable token in the path, which the host types and nothing else sees.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use rootcause::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

/// The script the VM runs.
pub const SCRIPT: &str = include_str!("../../selfhost/macos-vm/setup.sh");

/// The appliance trim the script fetches beside itself (selfhost/macos-vm/appliance-trim.sh and its two lists).
pub const TRIM_SCRIPT: &str = include_str!("../../selfhost/macos-vm/appliance-trim.sh");
pub const TRIM_USER: &str = include_str!("../../selfhost/macos-vm/appliance-disabled-user.txt");
pub const TRIM_SYSTEM: &str = include_str!("../../selfhost/macos-vm/appliance-disabled-system.txt");

/// The anisette VM's first boot and its status reporter (written into the guest by the provision stage).
pub const FIRST_BOOT: &str = include_str!("../../selfhost/macos-vm/first-boot.sh");
pub const REPORT_STATUS: &str = include_str!("../../selfhost/macos-vm/report-status.sh");

/// The port the person types. Short, fixed, and away from the companion's 49160-49162.
pub const PORT: u16 = 4949;

/// How long the server waits for the VM before giving up.
pub const TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Where the setup is, and what the VM has fetched so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub script: bool,
    pub program: bool,
    pub helper: bool,
    /// The guest confirmed that installation and LaunchAgent creation succeeded.
    pub done: bool,
    /// Nobody finished in [`TIMEOUT`].
    pub timed_out: bool,
}

/// A running server. Dropping it stops it.
pub struct Session {
    pub port: u16,
    /// This Mac's private IPv4 addresses: the one the VM can reach is among them, but which
    /// one depends on the VM's network (UTM's, Docker's, the LAN), so the person picks.
    pub addresses: Vec<Ipv4Addr>,
    pub progress: watch::Receiver<Progress>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What the app's anisette VM gets on top of the program and its helper, all under `token` (see the module comment).
pub struct Appliance {
    /// Lowercase hex, typed into recoveryOS: `/<token>/provision.sh` or `/<token>/setup.sh`.
    pub token: String,
    /// The VM account's password (first-boot.sh makes the account with it).
    pub password: String,
    /// guest-status and guest-bridge, built for the guest (selfhost/macos-vm/build.sh).
    pub guest_status: Vec<u8>,
    pub guest_bridge: Vec<u8>,
    /// Development only: an SSH key to authorize, which keeps Remote Login on in the guest.
    pub authorized_keys: Option<Vec<u8>>,
}

struct Files {
    program: Vec<u8>,
    helper: Vec<u8>,
    appliance: Option<Appliance>,
}

/// Starts serving `program` (the `apparition` command line) and `helper`.
pub async fn serve(program: PathBuf, helper: PathBuf) -> Result<Session, Report> {
    serve_files(program, helper, None).await
}

/// Starts serving the app's anisette VM its setup, under `appliance.token`.
pub async fn serve_appliance(program: PathBuf, helper: PathBuf, appliance: Appliance) -> Result<Session, Report> {
    serve_files(program, helper, Some(appliance)).await
}

async fn serve_files(program: PathBuf, helper: PathBuf, appliance: Option<Appliance>) -> Result<Session, Report> {
    let program = std::fs::read(&program).context(format!("reading {}", program.display()))?;
    let helper = std::fs::read(&helper).context(format!("reading {}", helper.display()))?;
    let listener = match TcpListener::bind(("0.0.0.0", PORT)).await {
        Ok(listener) => listener,
        // Busy (a second copy, say): any free port, shown to the person.
        Err(_) => TcpListener::bind(("0.0.0.0", 0)).await.context("binding a port")?,
    };
    let port = listener.local_addr().context("reading the port")?.port();
    let (tx, progress) = watch::channel(Progress::default());
    let files = std::sync::Arc::new(Files { program, helper, appliance });
    let task = tokio::spawn(async move {
        let _busy = crate::stay_awake::busy();
        let deadline = tokio::time::sleep(TIMEOUT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    tx.send_modify(|p| p.timed_out = true);
                    return;
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let files = files.clone();
                    let tx = tx.clone();
                    tokio::spawn(async move { handle(stream, &files, &tx).await });
                }
                // Guest confirmed installation: let the response finish, then stop listening.
                _ = async {
                    loop {
                        if tx.borrow().done { break; }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } => return,
            }
        }
    });
    Ok(Session { port, addresses: crate::relay::lan_addresses(), progress, task })
}

async fn handle(mut stream: TcpStream, files: &Files, tx: &watch::Sender<Progress>) {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => head.extend_from_slice(&chunk[..n]),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.lines();
    let path = lines.next().and_then(|l| l.strip_prefix("GET ")).and_then(|l| l.split(' ').next()).unwrap_or("");
    let host = lines
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("host")).map(|(_, v)| v.trim().to_string()))
        .filter(|h| h.chars().all(|c| c.is_ascii_alphanumeric() || ".:-[]".contains(c)));
    let not_found = async |mut stream: TcpStream| {
        let _ = stream.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
    };
    // The appliance's session serves nothing outside its token.
    let (path, prefix) = match &files.appliance {
        Some(appliance) => match path.strip_prefix('/').and_then(|p| p.strip_prefix(appliance.token.as_str())) {
            Some(rest) if rest.starts_with('/') => (rest, format!("/{}", appliance.token)),
            _ => return not_found(stream).await,
        },
        None => (path, String::new()),
    };
    let appliance = files.appliance.as_ref();
    let script = |stage: &str| {
        let automated = appliance.is_some() || std::env::var("APPARITION_VM_AUTOMATED").as_deref() == Ok("1");
        // Remote Login only for development: a key handed in, or the older automated flow that asked for it.
        let keep_ssh = appliance.map_or(automated, |a| a.authorized_keys.is_some());
        let flag = |on: bool| if on { "1" } else { "0" };
        host.as_ref().map(|host| {
            SCRIPT.replace("@@BASE@@", &format!("http://{host}{prefix}"))
                .replace("@@AUTOMATED@@", flag(automated))
                .replace("@@STAGE@@", stage)
                .replace("@@APPLIANCE@@", flag(appliance.is_some()))
                .replace("@@KEEP_SSH@@", flag(keep_ssh))
                .into_bytes()
        })
    };
    let (body, kind): (Vec<u8>, u8) = match (path, appliance) {
        ("/" | "/setup.sh", _) => match script("setup") {
            Some(body) => (body, 0),
            None => return,
        },
        ("/provision.sh", Some(_)) => match script("provision") {
            Some(body) => (body, 0),
            None => return,
        },
        ("/appliance-trim.sh", _) => (TRIM_SCRIPT.as_bytes().to_vec(), 4),
        ("/appliance-disabled-user.txt", _) => (TRIM_USER.as_bytes().to_vec(), 4),
        ("/appliance-disabled-system.txt", _) => (TRIM_SYSTEM.as_bytes().to_vec(), 4),
        ("/apparition", _) => (files.program.clone(), 1),
        ("/apparition-anisette-helper", _) => (files.helper.clone(), 2),
        ("/password", Some(a)) => (a.password.as_bytes().to_vec(), 4),
        ("/first-boot.sh", Some(_)) => (FIRST_BOOT.as_bytes().to_vec(), 4),
        ("/report-status.sh", Some(_)) => (REPORT_STATUS.as_bytes().to_vec(), 4),
        ("/guest-status", Some(a)) => (a.guest_status.clone(), 4),
        ("/guest-bridge", Some(a)) => (a.guest_bridge.clone(), 4),
        ("/authorized_keys", Some(Appliance { authorized_keys: Some(keys), .. })) => (keys.clone(), 4),
        // The provision stage confirms without the program: it comes in the setup stage.
        ("/complete", Some(_)) => (b"OK\n".to_vec(), 3),
        ("/complete", None) if tx.borrow().program && tx.borrow().helper => (b"OK\n".to_vec(), 3),
        _ => return not_found(stream).await,
    };
    let header = format!("HTTP/1.0 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n", body.len());
    if stream.write_all(header.as_bytes()).await.is_err() || stream.write_all(&body).await.is_err() {
        return;
    }
    if stream.shutdown().await.is_err() {
        return;
    }
    tx.send_modify(|p| {
        match kind {
            0 => p.script = true,
            1 => p.program = true,
            2 => p.helper = true,
            3 => p.done = true,
            _ => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, path: &str) -> Vec<u8> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: 10.0.0.5:{port}\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn serves_the_script_with_its_own_address_and_the_files_whole() {
        let dir = std::env::temp_dir().join(format!("vm-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), b"program").unwrap();
        std::fs::write(dir.join("h"), b"helper").unwrap();
        let mut session = serve(dir.join("a"), dir.join("h")).await.unwrap();
        let script = String::from_utf8(get(session.port, "/").await).unwrap();
        assert!(script.contains("http://10.0.0.5:"), "base is the address the VM used");
        assert!(!script.contains("@@BASE@@"));
        assert!(get(session.port, "/nope").await.starts_with(b"HTTP/1.0 404"));
        // The trim travels with the script, and serving it is not progress.
        assert!(get(session.port, "/appliance-trim.sh").await.ends_with(TRIM_SCRIPT.as_bytes()));
        assert!(get(session.port, "/appliance-disabled-user.txt").await.ends_with(TRIM_USER.as_bytes()));
        assert!(!session.progress.borrow().program && !session.progress.borrow().done);
        assert!(get(session.port, "/apparition").await.ends_with(b"program"));
        assert!(!session.progress.borrow().done);
        assert!(get(session.port, "/apparition-anisette-helper").await.ends_with(b"helper"));
        session.progress.changed().await.ok();
        assert!(!session.progress.borrow().done, "downloads do not prove installation");
        assert!(get(session.port, "/complete").await.ends_with(b"OK\n"));
        session.progress.changed().await.ok();
        assert!(session.progress.borrow().done);
        std::fs::remove_file(dir.join("a")).unwrap();
        std::fs::remove_file(dir.join("h")).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }

    /// What loginwindow waits for, what the guest's own daemons are, and what was found to break the VM when off.
    const NEVER_DISABLE: &[&str] = &[
        "com.apple.Finder", "com.apple.Dock.agent", "com.apple.SystemUIServer.agent", "com.apple.biometrickitd",
        "com.apple.bluetoothd", "com.apple.storagekitd", "com.apple.analyticsd", "com.apple.symptomsd", "com.apple.sysmond",
        "com.apple.loginwindow", "com.apple.WindowServer", "com.apple.opendirectoryd", "com.apple.securityd",
        "com.apple.cfprefsd.xpc.daemon", "com.apple.mobile.keybagd", "com.illixion.apparition.anisette",
        "com.illixion.apparition.vm-anisette", "com.illixion.apparition.vm-bridge", "com.illixion.apparition.vm-status",
    ];

    fn labels(list: &str) -> Vec<&str> {
        list.lines().filter(|line| !line.is_empty() && !line.starts_with('#')).collect()
    }

    #[test]
    fn the_trim_lists_are_plain_unique_and_keep_what_login_needs() {
        for list in [TRIM_USER, TRIM_SYSTEM] {
            let found = labels(list);
            assert!(found.len() > 50);
            let unique: std::collections::HashSet<_> = found.iter().collect();
            assert_eq!(unique.len(), found.len(), "duplicates");
            for label in &found {
                assert!(label.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)), "{label}");
                // A UUID-like piece is one machine's, not a service's.
                assert!(!label.split(['.', '-']).any(|part| part.len() == 8 && part.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())), "{label}");
                assert!(!NEVER_DISABLE.contains(label), "{label} must never be disabled");
            }
        }
    }

    /// Runs appliance-trim.sh on `data` with the given lists; returns (system, user) overrides.
    fn trim(dir: &std::path::Path, data: &std::path::Path, user: &str, system: &str, keep_ssh: bool) -> (plist::Dictionary, plist::Dictionary) {
        std::fs::write(dir.join("appliance-trim.sh"), TRIM_SCRIPT).unwrap();
        std::fs::write(dir.join("appliance-disabled-user.txt"), user).unwrap();
        std::fs::write(dir.join("appliance-disabled-system.txt"), system).unwrap();
        let mut command = std::process::Command::new("/bin/bash");
        command.arg(dir.join("appliance-trim.sh")).arg(data).env("APPARITION_TRIM_TEST", "1");
        if keep_ssh {
            command.env("APPARITION_KEEP_SSH", "1");
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let launchd = data.join("private/var/db/com.apple.xpc.launchd");
        let read = |name: &str| plist::Value::from_file(launchd.join(name)).unwrap().into_dictionary().unwrap();
        (read("disabled.plist"), read("disabled.501.plist"))
    }

    fn disabled(overrides: &plist::Dictionary) -> std::collections::BTreeSet<String> {
        overrides.iter().filter(|(_, v)| v.as_boolean() == Some(true)).map(|(k, _)| k.clone()).collect()
    }

    #[test]
    fn the_trim_writes_overrides_merges_and_keeps_ssh_only_when_asked() {
        if !std::path::Path::new("/usr/libexec/PlistBuddy").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let launchd = data.join("private/var/db/com.apple.xpc.launchd");
        std::fs::create_dir_all(&launchd).unwrap();
        let mut mine = plist::Dictionary::new();
        mine.insert("com.example.mine".into(), true.into());
        mine.insert("com.apple.softwareupdated".into(), false.into());
        plist::Value::Dictionary(mine).to_file_xml(launchd.join("disabled.plist")).unwrap();
        let (system, user) = trim(dir.path(), &data, TRIM_USER, TRIM_SYSTEM, true);
        assert_eq!(disabled(&user), labels(TRIM_USER).into_iter().map(String::from).collect());
        assert!(labels(TRIM_SYSTEM).iter().all(|label| disabled(&system).contains(*label)));
        assert_eq!(system["com.example.mine"].as_boolean(), Some(true), "an override the person set survives");
        assert_eq!(system["com.apple.softwareupdated"].as_boolean(), Some(true), "the trim wins for its own labels");
        assert_eq!(system["com.openssh.sshd"].as_boolean(), Some(false), "development keeps SSH");
        assert!(data.join(".metadata_never_index").exists());
    }

    #[test]
    fn a_label_taken_off_a_trim_list_is_put_back_where_it_was_applied() {
        if !std::path::Path::new("/usr/libexec/PlistBuddy").exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("private/var/db/com.apple.xpc.launchd")).unwrap();
        let (system, _) = trim(dir.path(), &data, "com.example.one\ncom.example.two\n", "com.example.sys\ncom.example.badidea\n", false);
        assert_eq!(disabled(&system), ["com.example.badidea", "com.example.sys"].map(String::from).into());
        assert!(!system.contains_key("com.openssh.sshd"), "no SSH unless asked");
        let (system, user) = trim(dir.path(), &data, "com.example.one\n", "com.example.sys\n", false);
        assert_eq!(disabled(&system), ["com.example.sys".to_string()].into());
        assert!(!system.contains_key("com.example.badidea"));
        assert_eq!(disabled(&user), ["com.example.one".to_string()].into());
    }

    #[test]
    fn the_trim_refuses_to_run_as_anyone_but_root() {
        let uid = std::process::Command::new("/usr/bin/id").arg("-u").output().unwrap().stdout;
        if String::from_utf8_lossy(&uid).trim() == "0" {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("appliance-trim.sh"), TRIM_SCRIPT).unwrap();
        let output = std::process::Command::new("/bin/bash").arg(dir.path().join("appliance-trim.sh")).arg(dir.path()).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("run as root"));
    }

    #[tokio::test]
    async fn the_appliance_session_serves_only_under_its_token() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"program").unwrap();
        std::fs::write(dir.path().join("h"), b"helper").unwrap();
        let appliance = Appliance {
            token: "0123456789abcdef".into(),
            password: "secret".into(),
            guest_status: b"status-binary".to_vec(),
            guest_bridge: b"bridge-binary".to_vec(),
            authorized_keys: None,
        };
        let mut session = serve_appliance(dir.path().join("a"), dir.path().join("h"), appliance).await.unwrap();
        for path in ["/", "/setup.sh", "/password", "/apparition", "/0123456789abcdee/password", "/0123456789abcdefpassword"] {
            assert!(get(session.port, path).await.starts_with(b"HTTP/1.0 404"), "{path} without the token");
        }
        let provision = String::from_utf8(get(session.port, "/0123456789abcdef/provision.sh").await).unwrap();
        assert!(provision.contains("BASE='http://10.0.0.5:") && provision.contains("/0123456789abcdef'"), "base keeps the token");
        assert!(provision.contains("STAGE='provision'") && provision.contains("APPLIANCE='1'") && provision.contains("AUTOMATED='1'"));
        assert!(provision.contains("KEEP_SSH='0'"), "no SSH without a key");
        assert!(String::from_utf8(get(session.port, "/0123456789abcdef/setup.sh").await).unwrap().contains("STAGE='setup'"));
        assert!(get(session.port, "/0123456789abcdef/password").await.ends_with(b"\r\n\r\nsecret"));
        assert!(get(session.port, "/0123456789abcdef/guest-bridge").await.ends_with(b"bridge-binary"));
        assert!(get(session.port, "/0123456789abcdef/first-boot.sh").await.ends_with(FIRST_BOOT.as_bytes()));
        assert!(get(session.port, "/0123456789abcdef/authorized_keys").await.starts_with(b"HTTP/1.0 404"), "no key, no file");
        // The provision stage confirms before the program has been fetched (it comes in the setup stage).
        assert!(get(session.port, "/0123456789abcdef/complete").await.ends_with(b"OK\n"));
        session.progress.changed().await.ok();
        assert!(session.progress.borrow().done);
    }
}
