//! A real Node store to compare against, for the parity tests. `NodeStore::start` returns None
//! when `node` is not installed, and the test returns early.
#![allow(dead_code)]

use std::{io::{Read, Write}, net::TcpStream, path::Path, process::{Child, Command, Stdio}, time::{Duration, Instant}};

pub struct NodeStore {
    child: Child,
    pub port: u16,
}

impl Drop for NodeStore {
    fn drop(&mut self) {
        let _ = self.child.kill(); // the Node store this test started: its own pid
        let _ = self.child.wait();
    }
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

impl NodeStore {
    /// Starts `store/server.js` on `config`/`data` (the config's `port` must be `port`).
    pub fn start(config: &Path, data: &Path, port: u16, env: &[(&str, &Path)]) -> Option<NodeStore> {
        if Command::new("node").arg("--version").stdout(Stdio::null()).status().is_err() {
            return None;
        }
        let store = Path::new(env!("CARGO_MANIFEST_DIR")).join("../store");
        let mut cmd = Command::new("node");
        cmd.arg("server.js").current_dir(&store).env("STORE_CONFIG", config).env("STORE_DATA", data)
            .stdout(Stdio::null()).stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let node = NodeStore { child: cmd.spawn().unwrap(), port };
        let start = Instant::now();
        while request(port, "GET", "/api/auth", &[], &[]).is_none() {
            assert!(start.elapsed() < Duration::from_secs(20), "the Node store did not start");
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(node)
    }

    pub fn get(&self, path: &str) -> (u16, String) {
        request(self.port, "GET", path, &[], &[]).unwrap()
    }
}

/// (status, body) of one HTTP/1.0 request, so the body comes back unframed.
pub fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let mut head = format!("{method} {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head += &format!("{k}: {v}\r\n");
    }
    stream.write_all(head.as_bytes()).ok()?;
    stream.write_all(b"\r\n").ok()?;
    stream.write_all(body).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    Some((head.split(' ').nth(1)?.parse().ok()?, body.to_string()))
}
