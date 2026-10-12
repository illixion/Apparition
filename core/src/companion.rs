//! The companion: what a computer runs for Apparition on a device. The relay (`relay.rs`), and
//! an anisette server the device can sign in to Apple with:
//!
//! - built in (the `anisette` feature): Apple's libraries in this process, served on the LAN
//!   on port 6969;
//! - or the self-hosted Docker image (selfhost/anisette/compose.yaml), started with the user's
//!   Docker, which the companion offers once it answers.
//!
//! The command line (`apparition relay`) and the desktop app both run this.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rootcause::prelude::*;

#[allow(unused_imports)]
use crate::relay::{ANISETTE_PORT, Log, Relay};

/// The compose file for the self-hosted anisette server.
pub const COMPOSE: &str = include_str!("../../selfhost/anisette/compose.yaml");

/// Where the anisette server the companion offers comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AnisetteSource {
    Builtin,
    /// Another server on this machine's port 6969, normally the Docker one.
    Local,
}

pub struct Companion {
    pub relay: Relay,
    #[cfg(all(feature = "anisette", unix))]
    builtin: tokio::sync::Mutex<Option<crate::anisette::Server>>,
    source: Mutex<Option<AnisetteSource>>,
    data_dir: PathBuf,
    anisette_port: u16,
}

impl Companion {
    /// Starts the relay on `port`. `data_dir` holds the built-in anisette's libraries, and
    /// `anisette_port` is where an anisette server is served or looked for (normally
    /// [`ANISETTE_PORT`]).
    pub async fn start(port: u16, anisette_port: u16, advertise: bool, data_dir: &Path, log: Log) -> Result<Companion, Report> {
        Ok(Companion {
            relay: Relay::start(port, advertise, log).await?,
            #[cfg(all(feature = "anisette", unix))]
            builtin: Default::default(),
            source: Mutex::new(None),
            data_dir: data_dir.to_owned(),
            anisette_port,
        })
    }

    /// What the companion offers now.
    pub fn anisette(&self) -> Option<AnisetteSource> {
        *self.source.lock().unwrap()
    }

    pub fn anisette_port(&self) -> u16 {
        self.anisette_port
    }

    /// Serves the built-in anisette on the LAN and offers it.
    pub async fn share_builtin_anisette(&self) -> Result<(), Report> {
        #[cfg(all(feature = "anisette", unix))]
        {
            let mut builtin = self.builtin.lock().await;
            if builtin.is_none() {
                let port = self.anisette_port;
                let bind = std::net::SocketAddr::from(([0, 0, 0, 0], port));
                let server = crate::anisette::start(&self.data_dir, bind).await.context(format!(
                    "Couldn't serve anisette on port {port}. If another program uses that port (the Docker server, say), stop it or offer that one instead."
                ))?;
                *builtin = Some(server);
            }
            *self.source.lock().unwrap() = Some(AnisetteSource::Builtin);
            self.relay.set_anisette(Some(self.anisette_port));
            Ok(())
        }
        #[cfg(not(all(feature = "anisette", unix)))]
        {
            let _ = &self.data_dir;
            bail!("This build has no built-in anisette; run the Docker server instead")
        }
    }

    /// Stops serving the built-in anisette.
    pub async fn stop_builtin_anisette(&self) {
        #[cfg(all(feature = "anisette", unix))]
        {
            self.builtin.lock().await.take();
        }
        if self.anisette() == Some(AnisetteSource::Builtin) {
            *self.source.lock().unwrap() = None;
            self.relay.set_anisette(None);
        }
    }

    /// When not serving the built-in one, offers a server on this machine's port 6969 if one
    /// answers (and stops offering it when it doesn't). Call every few seconds.
    pub async fn refresh(&self) {
        if self.anisette() == Some(AnisetteSource::Builtin) {
            return;
        }
        let answers = crate::relay::anisette_answers(self.anisette_port).await;
        let source = answers.then_some(AnisetteSource::Local);
        *self.source.lock().unwrap() = source;
        self.relay.set_anisette(answers.then_some(self.anisette_port));
    }
}

/// Runs `docker compose up -d` (or `down`) with the bundled compose file.
pub async fn docker_anisette(up: bool) -> Result<(), Report> {
    let file = std::env::temp_dir().join("apparition-anisette-compose.yaml");
    std::fs::write(&file, COMPOSE).context(format!("Can't write {}", file.display()))?;
    let mut command = tokio::process::Command::new("docker");
    command.arg("compose").arg("-f").arg(&file);
    if up {
        command.args(["up", "-d"]);
    } else {
        command.arg("down");
    }
    let output = command
        .output()
        .await
        .context("Can't run docker (Docker Desktop, OrbStack, Colima, or Docker Engine with the compose plugin)")?;
    if !output.status.success() {
        bail!("docker compose failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}
