//! Processes started on behalf of a peer (shell pty clients, exec commands, later tcp
//! splices), so that revoking a machine can end everything it is running here at once.
//! Each process is the leader of its own process group; the whole group is signalled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};

use crate::roster::MachineId;

#[derive(Default)]
pub struct Children {
    next: AtomicU64,
    map: Mutex<HashMap<u64, (MachineId, Pid)>>,
}

/// Removes its process from the registry when dropped (the serving task is done with it).
pub struct Ticket {
    reg: Arc<Children>,
    key: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Ok(mut g) = self.reg.map.lock() {
            g.remove(&self.key);
        }
    }
}

impl Children {
    pub fn track(self: &Arc<Self>, peer: MachineId, pid: Option<Pid>) -> Option<Ticket> {
        let pid = pid?;
        let key = self.next.fetch_add(1, Ordering::Relaxed);
        self.map.lock().ok()?.insert(key, (peer, pid));
        Some(Ticket { reg: self.clone(), key })
    }

    /// Peers that have something running here.
    pub fn peers(&self) -> Vec<MachineId> {
        let mut v: Vec<MachineId> = self.map.lock().map(|g| g.values().map(|(p, _)| *p).collect()).unwrap_or_default();
        v.sort();
        v.dedup();
        v
    }

    pub fn count_for(&self, peer: &MachineId) -> usize {
        self.map.lock().map(|g| g.values().filter(|(p, _)| p == peer).count()).unwrap_or(0)
    }

    /// SIGHUP every process group started for `peer`, then SIGKILL whatever is still
    /// registered a second later. Returns how many were signalled.
    pub fn kill_peer(self: &Arc<Self>, peer: &MachineId) -> usize {
        let victims: Vec<(u64, Pid)> = match self.map.lock() {
            Ok(g) => g.iter().filter(|(_, (p, _))| p == peer).map(|(k, (_, pid))| (*k, *pid)).collect(),
            Err(_) => return 0,
        };
        for (_, pid) in &victims {
            let _ = kill_process_group(*pid, Signal::HUP);
        }
        let n = victims.len();
        if n > 0 {
            let reg = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Ok(g) = reg.map.lock() else { return };
                for (key, pid) in &victims {
                    // Still registered = the serving task has not seen it exit yet.
                    if g.contains_key(key) {
                        let _ = kill_process_group(*pid, Signal::KILL);
                    }
                }
            });
        }
        n
    }
}
