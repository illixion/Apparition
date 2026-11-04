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

/// Starts serving `program` (the `apparition` command line) and `helper`.
pub async fn serve(program: PathBuf, helper: PathBuf) -> Result<Session, Report> {
    let program = std::fs::read(&program).context(format!("reading {}", program.display()))?;
    let helper = std::fs::read(&helper).context(format!("reading {}", helper.display()))?;
    let listener = match TcpListener::bind(("0.0.0.0", PORT)).await {
        Ok(listener) => listener,
        // Busy (a second copy, say): any free port, shown to the person.
        Err(_) => TcpListener::bind(("0.0.0.0", 0)).await.context("binding a port")?,
    };
    let port = listener.local_addr().context("reading the port")?.port();
    let (tx, progress) = watch::channel(Progress::default());
    let files = std::sync::Arc::new((program, helper));
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

async fn handle(mut stream: TcpStream, files: &(Vec<u8>, Vec<u8>), tx: &watch::Sender<Progress>) {
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
    let (body, kind): (Vec<u8>, u8) = match path {
        "/" | "/setup.sh" => match host {
            Some(host) => (SCRIPT.replace("@@BASE@@", &format!("http://{host}"))
                .replace("@@AUTOMATED@@", if std::env::var("APPARITION_VM_AUTOMATED").as_deref() == Ok("1") { "1" } else { "0" })
                .into_bytes(), 0),
            None => return,
        },
        "/appliance-trim.sh" => (TRIM_SCRIPT.as_bytes().to_vec(), 4),
        "/appliance-disabled-user.txt" => (TRIM_USER.as_bytes().to_vec(), 4),
        "/appliance-disabled-system.txt" => (TRIM_SYSTEM.as_bytes().to_vec(), 4),
        "/apparition" => (files.0.clone(), 1),
        "/apparition-anisette-helper" => (files.1.clone(), 2),
        "/complete" if tx.borrow().program && tx.borrow().helper => (b"OK\n".to_vec(), 3),
        _ => {
            let _ = stream.write_all(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            return;
        }
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
}
