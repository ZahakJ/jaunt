//! Small daemon state kept across restarts, in the state directory (0600 files):
//!
//! - `addrs.json`: each peer's last working direct addresses and this machine's own
//!   last UDP port. They are only ever dial *hints*: iroh still authenticates the peer by
//!   its key, so a stale or wrong address costs a failed attempt and nothing else. With
//!   them a restarted daemon can find a peer on the same direct path while the relay is down.
//! - `tmux-owners.json`: which machine created which tmux session here, so that
//!   `jaunt unlink --kill-sessions` can end the sessions a revoked machine started.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::paths::{Paths, read_capped, write_private_atomic};
use crate::roster::MachineId;

/// Addresses kept per peer (most recent first).
const PER_PEER: usize = 4;
const MAX_PEERS: usize = 256;
const MAX_FILE: u64 = 256 * 1024;
const MAX_OWNERS: usize = 1024;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddrBook {
    #[serde(default)]
    own_port: Option<u16>,
    #[serde(default)]
    peers: BTreeMap<MachineId, Vec<SocketAddr>>,
}

fn addrs_file(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir.join("addrs.json")
}

fn owners_file(paths: &Paths) -> std::path::PathBuf {
    paths.state_dir.join("tmux-owners.json")
}

impl AddrBook {
    /// A missing or unreadable file is an empty book: hints are never required.
    pub fn load(paths: &Paths) -> AddrBook {
        let mut b: AddrBook = match read_capped(&addrs_file(paths), MAX_FILE) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
            _ => AddrBook::default(),
        };
        for v in b.peers.values_mut() {
            v.truncate(PER_PEER);
        }
        while b.peers.len() > MAX_PEERS {
            let Some(k) = b.peers.keys().next().copied() else { break };
            b.peers.remove(&k);
        }
        b
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        crate::paths::private_dir(&paths.state_dir)?;
        write_private_atomic(&addrs_file(paths), &serde_json::to_vec_pretty(self)?)
    }

    /// Remember a working direct address of `id`. Returns true when the book changed.
    pub fn note(&mut self, id: MachineId, addr: SocketAddr) -> bool {
        let v = self.peers.entry(id).or_default();
        if v.first() == Some(&addr) {
            return false;
        }
        v.retain(|a| *a != addr);
        v.insert(0, addr);
        v.truncate(PER_PEER);
        true
    }

    pub fn hints(&self, id: &MachineId) -> Vec<SocketAddr> {
        self.peers.get(id).cloned().unwrap_or_default()
    }

    pub fn retain_peers(&mut self, keep: impl Fn(&MachineId) -> bool) {
        self.peers.retain(|id, _| keep(id));
    }

    pub fn own_port(&self) -> Option<u16> {
        self.own_port.filter(|p| *p != 0)
    }

    pub fn set_own_port(&mut self, port: u16) -> bool {
        let changed = self.own_port != Some(port);
        self.own_port = Some(port);
        changed
    }
}

pub fn load_tmux_owners(paths: &Paths) -> BTreeMap<String, MachineId> {
    match read_capped(&owners_file(paths), MAX_FILE) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
        _ => BTreeMap::new(),
    }
}

pub fn save_tmux_owners(paths: &Paths, owners: &BTreeMap<String, MachineId>) -> Result<()> {
    crate::paths::private_dir(&paths.state_dir)?;
    // Oldest names first would be arbitrary; keep the file bounded by dropping by name order.
    let bounded: BTreeMap<&String, &MachineId> = owners.iter().take(MAX_OWNERS).collect();
    write_private_atomic(&owners_file(paths), &serde_json::to_vec_pretty(&bounded)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_are_most_recent_first_bounded_and_persisted() {
        let dir = std::env::temp_dir().join(format!("jaunt-addrs-{}", std::process::id()));
        let paths = Paths::under(&dir);
        let id = MachineId([7; 32]);
        let mut b = AddrBook::load(&paths);
        assert!(b.hints(&id).is_empty());
        for i in 0..6u16 {
            assert!(b.note(id, SocketAddr::from(([198, 51, 100, 12], 40000 + i))));
        }
        assert!(!b.note(id, SocketAddr::from(([198, 51, 100, 12], 40005))), "same first address: no change");
        assert!(b.note(id, SocketAddr::from(([198, 51, 100, 12], 40003))));
        let h = b.hints(&id);
        assert_eq!(h.len(), PER_PEER);
        assert_eq!(h[0].port(), 40003);
        assert_eq!(h[1].port(), 40005);
        assert!(b.set_own_port(41641));
        b.save(&paths).unwrap();
        let mode = std::fs::metadata(dir.join("state/addrs.json")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        let again = AddrBook::load(&paths);
        assert_eq!(again, b);
        assert_eq!(again.own_port(), Some(41641));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbled_file_is_an_empty_book() {
        let dir = std::env::temp_dir().join(format!("jaunt-addrs-bad-{}", std::process::id()));
        let paths = Paths::under(&dir);
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        std::fs::write(paths.state_dir.join("addrs.json"), b"{not json").unwrap();
        assert_eq!(AddrBook::load(&paths), AddrBook::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
