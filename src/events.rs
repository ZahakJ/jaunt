//! The daemon's event log: a ring of recent events for new subscribers, a broadcast
//! channel for live ones, and an append-only JSON-lines file in the state directory.

use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::proto::{Event, EventKind, IpcResponse, PeerRef, RttSample};
use crate::roster::{MachineId, now_ms};

const RING: usize = 500;
const MAX_FILE: u64 = 1 << 20;

pub struct EventLog {
    inner: Mutex<(u64, VecDeque<Event>)>,
    tx: broadcast::Sender<IpcResponse>,
    file: PathBuf,
}

impl EventLog {
    pub fn new(file: PathBuf) -> EventLog {
        let (tx, _) = broadcast::channel(1024);
        EventLog { inner: Mutex::new((0, VecDeque::new())), tx, file }
    }

    pub fn emit(&self, peer: Option<PeerRef>, kind: EventKind) {
        let ev = {
            let Ok(mut g) = self.inner.lock() else { return };
            g.0 += 1;
            let ev = Event { seq: g.0, t_ms: now_ms(), peer, kind };
            if g.1.len() >= RING {
                g.1.pop_front();
            }
            g.1.push_back(ev.clone());
            ev
        };
        tracing::info!(target: "jaunt::event", "{}", serde_json::to_string(&ev).unwrap_or_default());
        self.append(&ev);
        let _ = self.tx.send(IpcResponse::Event(ev));
    }

    /// RTT samples go to live subscribers only; they are not history.
    pub fn rtt(&self, peer: MachineId, sample: RttSample) {
        let _ = self.tx.send(IpcResponse::Rtt { peer, sample });
    }

    pub fn history(&self, n: usize) -> Vec<Event> {
        let Ok(g) = self.inner.lock() else { return Vec::new() };
        g.1.iter().skip(g.1.len().saturating_sub(n)).cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<IpcResponse> {
        self.tx.subscribe()
    }

    fn append(&self, ev: &Event) {
        if let Ok(meta) = std::fs::metadata(&self.file)
            && meta.len() > MAX_FILE
        {
            let _ = std::fs::rename(&self.file, self.file.with_extension("jsonl.1"));
        }
        let line = match serde_json::to_string(ev) {
            Ok(l) => l,
            Err(_) => return,
        };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&self.file) {
            let _ = writeln!(f, "{line}");
        }
    }
}
