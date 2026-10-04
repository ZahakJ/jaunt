//! `config.toml`: per-machine settings. Fleet-wide settings (the relay list) live in the
//! roster and are synced; the `relays` here bootstrap a machine that has no fleet settings yet.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, read_capped, write_private_atomic};

const MAX_CONFIG: u64 = 64 * 1024;
pub const DEFAULT_SESSION: &str = "jaunt";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// This machine's name in the mesh (default: hostname, lowercased).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Relay URLs used until the fleet settings in the roster say otherwise.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub relays: Vec<String>,
    /// Extra PEM CA to trust for the relay's TLS (self-hosted relay with its own CA, tests).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_ca: Option<PathBuf>,
    /// tmux session attached by a bare `jaunt <machine>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_session: Option<String>,
    /// tmux server socket name (`tmux -L <name>`); overridden by `JAUNT_TMUX_SOCKET`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    /// UPnP / NAT-PMP / PCP port mapping on the local gateway (default on; off when
    /// `JAUNT_HOME` is set, so tests never ask a real gateway for a mapping).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portmapper: Option<bool>,
    /// Fixed UDP port for the endpoint (default: any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        let path = paths.config_file();
        let Some(bytes) = read_capped(&path, MAX_CONFIG)? else {
            return Ok(Config::default());
        };
        let text = String::from_utf8(bytes).context("config.toml is not UTF-8")?;
        toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        write_private_atomic(&paths.config_file(), text.as_bytes())
    }

    pub fn tmux_socket(&self) -> Option<String> {
        match std::env::var("JAUNT_TMUX_SOCKET") {
            Ok(s) if !s.is_empty() => Some(s),
            _ => self.tmux_socket.clone(),
        }
    }

    pub fn default_session(&self) -> String {
        self.default_session.clone().unwrap_or_else(|| DEFAULT_SESSION.to_string())
    }

    /// Relays from the config, or from `JAUNT_RELAY` (comma-separated) when the config has none.
    pub fn bootstrap_relays(&self) -> Vec<String> {
        if !self.relays.is_empty() {
            return self.relays.clone();
        }
        std::env::var("JAUNT_RELAY")
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default()
    }
}

/// The default machine name: the hostname, lowercased and reduced to `[a-z0-9-]`.
pub fn default_name() -> String {
    let uname = rustix::system::uname();
    let raw = uname.nodename().to_string_lossy().to_lowercase();
    let host = raw.split('.').next().unwrap_or("");
    let cleaned: String = host
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(crate::roster::MAX_NAME)
        .collect();
    if crate::roster::valid_name(&cleaned) { cleaned } else { "machine".to_string() }
}
