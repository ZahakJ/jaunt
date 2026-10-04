//! What the mesh view and `ls` draw: the daemon's status plus the event stream folded
//! into peers, a log, and pulses in flight. Time is passed in (wall-clock ms), so the
//! demo can drive the same model with simulated time.

use std::collections::VecDeque;

use super::canvas::clean;
use super::theme::{self, Col};
use crate::proto::{Event, EventKind, PairStep, PathKind, PeerState, RttSample, StatusSnapshot};
use crate::roster::MachineId;

const HISTORY: usize = 60;
const LOG: usize = 200;
/// Pulses older than this many ms are dropped.
const PULSE_LIFE: u64 = 2000;

#[derive(Debug, Clone)]
pub struct PeerView {
    pub id: MachineId,
    pub name: String,
    pub state: PeerState,
    pub rtt_ms: Option<f64>,
    pub hist: VecDeque<f64>,
    pub addr: Option<String>,
    pub addr_kind: Option<String>,
    pub host: Option<String>,
    pub sessions: Option<u32>,
    pub since_ms: Option<u64>,
    pub seen_ms: Option<u64>,
    pub version: Option<String>,
    pub error: Option<String>,
}

impl PeerView {
    pub fn up(&self) -> bool {
        matches!(self.state, PeerState::Direct | PeerState::Relay)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Relay,
    Bad,
    Accent,
    Quiet,
}

impl Tone {
    pub fn col(self) -> Col {
        match self {
            Tone::Good => theme::GREEN,
            Tone::Relay => theme::AMBER,
            Tone::Bad => theme::RED,
            Tone::Accent => theme::CYAN,
            Tone::Quiet => theme::DIM,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub t_ms: u64,
    pub tone: Tone,
    pub verb: String,
    pub peer: Option<String>,
    /// Detail text; `(text, tone)` pieces so a path change can show both styles.
    pub detail: Vec<(String, Tone)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
    /// Out and back (a ping).
    Bounce,
}

#[derive(Debug, Clone)]
pub struct Pulse {
    pub peer: MachineId,
    pub start_ms: u64,
    pub dur_ms: u64,
    pub dir: Dir,
    pub strong: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Mesh {
    pub me: String,
    pub me_id: Option<MachineId>,
    pub version: String,
    pub started_at_ms: u64,
    pub relays: Vec<String>,
    pub relay_connected: bool,
    /// This machine's host mode, when it is on.
    pub me_host: Option<String>,
    pub peers: Vec<PeerView>,
    pub revoked: usize,
    pub log: VecDeque<LogLine>,
    pub pulses: Vec<Pulse>,
}

fn path_state(p: PathKind) -> PeerState {
    match p {
        PathKind::Direct => PeerState::Direct,
        PathKind::Relay => PeerState::Relay,
        PathKind::Unknown => PeerState::Connecting,
    }
}

fn path_tone(p: PathKind) -> Tone {
    match p {
        PathKind::Direct => Tone::Good,
        PathKind::Relay => Tone::Relay,
        PathKind::Unknown => Tone::Quiet,
    }
}

impl Mesh {
    pub fn apply_status(&mut self, s: &StatusSnapshot) {
        self.me = clean(&s.me.name);
        self.me_id = Some(s.me.id);
        self.version = clean(&s.version);
        self.started_at_ms = s.started_at_ms;
        self.relays = s.relays.iter().map(|r| clean(r)).collect();
        self.relay_connected = s.relay_connected;
        self.revoked = s.revoked.len();
        self.me_host = s.host.as_ref().filter(|h| h.state != crate::proto::HostState::Off).map(|h| h.state.label().to_string());
        let mut peers: Vec<PeerView> = s
            .peers
            .iter()
            .map(|p| PeerView {
                id: p.id,
                name: clean(&p.name),
                state: p.state,
                rtt_ms: p.rtt_ms.filter(|v| v.is_finite() && *v >= 0.0),
                hist: p.rtt_history.iter().rev().take(HISTORY).rev().map(|r| r.rtt_ms).filter(|v| v.is_finite()).collect(),
                addr: p.remote_addr.as_deref().map(clean),
                addr_kind: p.addr_kind.as_deref().map(clean),
                host: p.host.as_deref().map(clean).filter(|h| !h.is_empty() && h != "off"),
                sessions: p.sessions,
                since_ms: p.connected_since_ms,
                seen_ms: p.last_seen_ms,
                version: p.version.as_deref().map(clean),
                error: p.last_error.as_deref().map(clean),
            })
            .collect();
        // Stable order: by name, so rows do not jump when a path changes.
        peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.0.cmp(&b.id.0)));
        self.peers = peers;
    }

    fn peer_mut(&mut self, id: &MachineId) -> Option<&mut PeerView> {
        self.peers.iter_mut().find(|p| p.id.0 == id.0)
    }

    pub fn apply_rtt(&mut self, peer: &MachineId, s: &RttSample, now_ms: u64) {
        if !s.rtt_ms.is_finite() || s.rtt_ms < 0.0 {
            return;
        }
        let Some(p) = self.peer_mut(peer) else { return };
        p.rtt_ms = Some(s.rtt_ms);
        if p.hist.len() >= HISTORY {
            p.hist.pop_front();
        }
        p.hist.push_back(s.rtt_ms);
        if s.path != PathKind::Unknown {
            p.state = path_state(s.path);
        }
        p.seen_ms = Some(s.t_ms);
        // A heartbeat: each peer every 4 s, staggered by id, out and back.
        let slot = (peer.0[0] % 4) as u64;
        if (s.t_ms / 1000) % 4 == slot {
            self.pulse(*peer, now_ms, Dir::Bounce, false);
        }
    }

    pub fn pulse(&mut self, peer: MachineId, now_ms: u64, dir: Dir, strong: bool) {
        let dur_ms = if dir == Dir::Bounce { 1100 } else { 650 };
        self.pulses.push(Pulse { peer, start_ms: now_ms, dur_ms, dir, strong });
    }

    pub fn prune(&mut self, now_ms: u64) {
        self.pulses.retain(|p| now_ms < p.start_ms + p.dur_ms.max(PULSE_LIFE));
    }

    /// Anything moving right now (the caller ticks faster while true).
    pub fn animating(&self, now_ms: u64) -> bool {
        self.pulses.iter().any(|p| now_ms < p.start_ms + p.dur_ms + 200)
            || self.peers.iter().any(|p| p.state == PeerState::Connecting)
    }

    fn log(&mut self, t_ms: u64, tone: Tone, verb: &str, peer: Option<String>, detail: Vec<(String, Tone)>) {
        if self.log.len() >= LOG {
            self.log.pop_back();
        }
        // Newest first; history arrives in order, so insert by time.
        let at = self.log.iter().position(|l| l.t_ms <= t_ms).unwrap_or(self.log.len());
        self.log.insert(at, LogLine { t_ms, tone, verb: verb.into(), peer, detail });
    }

    /// Fold one event in. `live` = it just happened (pulses are drawn only for those).
    pub fn apply_event(&mut self, e: &Event, now_ms: u64, live: bool) {
        let name = e.peer.as_ref().map(|p| clean(&p.name));
        let pid = e.peer.as_ref().map(|p| p.id);
        let q = |s: String| (s, Tone::Quiet);
        match &e.kind {
            EventKind::DaemonStarted { version } => {
                self.log(e.t_ms, Tone::Accent, "daemon up", None, vec![q(format!("v{}", clean(version)))]);
            }
            EventKind::Handshake { incoming, path } => {
                let dir = if *incoming { "incoming" } else { "outgoing" };
                let d = match path {
                    PathKind::Unknown => vec![q(dir.into())],
                    p => vec![(p.as_str().into(), path_tone(*p)), q(format!(" {} {dir}", theme::glyphs().dot))],
                };
                self.log(e.t_ms, if *path == PathKind::Unknown { Tone::Accent } else { path_tone(*path) }, "link up", name, d);
                if let Some(id) = pid {
                    if let Some(p) = self.peer_mut(&id) {
                        p.state = path_state(*path);
                        p.since_ms.get_or_insert(e.t_ms);
                    }
                    if live {
                        self.pulse(id, now_ms, if *incoming { Dir::In } else { Dir::Out }, true);
                    }
                }
            }
            EventKind::PathChanged { from, to, remote_addr } => {
                let addr = remote_addr.as_deref().map(clean);
                let (verb, tone) = match (from, to) {
                    (PathKind::Relay, PathKind::Direct) => ("upgraded", Tone::Good),
                    (PathKind::Direct, PathKind::Relay) => ("via relay", Tone::Relay),
                    (a, b) if a == b => ("roamed", Tone::Accent),
                    _ => ("path", path_tone(*to)),
                };
                let arrow = format!(" {} ", theme::glyphs().arrow);
                let mut d = if from == to {
                    vec![q("new address".into())]
                } else if *from == PathKind::Unknown {
                    vec![(to.as_str().into(), path_tone(*to))]
                } else {
                    vec![(from.as_str().into(), path_tone(*from)), q(arrow), (to.as_str().into(), path_tone(*to))]
                };
                if let Some(a) = &addr {
                    d.push(q(format!("  {a}")));
                }
                self.log(e.t_ms, tone, verb, name, d);
                if let Some(id) = pid {
                    if let Some(p) = self.peer_mut(&id) {
                        p.state = path_state(*to);
                        if addr.is_some() {
                            p.addr = addr;
                        }
                    }
                    if live {
                        self.pulse(id, now_ms, Dir::Out, true);
                    }
                }
            }
            EventKind::Disconnected { reason } => {
                self.log(e.t_ms, Tone::Bad, "link down", name, vec![q(clean(reason))]);
                if let Some(id) = pid
                    && let Some(p) = self.peer_mut(&id)
                {
                    p.state = PeerState::Offline;
                    p.seen_ms = Some(e.t_ms);
                    p.since_ms = None;
                }
            }
            EventKind::DialFailed { error } => {
                self.log(e.t_ms, Tone::Quiet, "no answer", name, vec![q(clean(error))]);
            }
            EventKind::Sync { changed, outgoing } => {
                let what = if *changed { "roster updated" } else { "in step" };
                self.log(e.t_ms, Tone::Accent, "sync", name, vec![q(what.into())]);
                if live && let Some(id) = pid {
                    self.pulse(id, now_ms, if *outgoing { Dir::Out } else { Dir::In }, false);
                }
            }
            EventKind::RosterChanged { what } => {
                self.log(e.t_ms, Tone::Accent, "roster", name, vec![q(clean(what))]);
            }
            EventKind::Served { service } => {
                self.log(e.t_ms, Tone::Accent, "served", name, vec![q(clean(service))]);
                if live && let Some(id) = pid {
                    self.pulse(id, now_ms, Dir::In, false);
                }
            }
            EventKind::Refused { reason } => {
                self.log(e.t_ms, Tone::Bad, "refused", name, vec![q(clean(reason))]);
            }
            EventKind::Pair { step: PairStep::Linked { other, members }, .. } => {
                self.log(e.t_ms, Tone::Good, "linked", Some(clean(&other.name)), vec![q(format!("mesh of {members}"))]);
            }
            EventKind::Pair { .. } | EventKind::Told { .. } => {}
            EventKind::Revoked { by, sessions_killed, .. } => {
                let mut d = vec![q(format!("by {}", clean(&by.name)))];
                if *sessions_killed > 0 {
                    d.push(q(format!(" · {sessions_killed} ended")));
                }
                self.log(e.t_ms, Tone::Bad, "unlinked", name, d);
                if let Some(id) = pid {
                    self.peers.retain(|p| p.id.0 != id.0);
                    self.revoked += 1;
                }
            }
        }
    }

    pub fn up_count(&self) -> usize {
        self.peers.iter().filter(|p| p.up()).count()
    }

    pub fn relay_host(&self) -> Option<String> {
        let r = self.relays.first()?;
        let rest = r.split_once("://").map(|(_, r)| r).unwrap_or(r);
        Some(rest.split(['/', '?']).next().unwrap_or(rest).to_string())
    }
}

/// "0.8 ms", "12 ms", "1.2 s".
pub fn fmt_rtt(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

/// A short age: "now", "12s", "3m", "2h", "4d".
pub fn fmt_age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=2 => "now".into(),
        3..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=172_799 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{PeerRef, PeerStatus};

    fn snap() -> StatusSnapshot {
        let peer = |b: u8, n: &str, st| PeerStatus {
            id: MachineId([b; 32]),
            name: n.into(),
            state: st,
            remote_addr: Some("10.0.0.2:1\x1b[2J".into()),
            addr_kind: None,
            rtt_ms: Some(f64::NAN),
            rtt_history: vec![],
            connected_since_ms: None,
            last_seen_ms: None,
            version: None,
            sessions: None,
            host: Some(String::new()),
            last_error: None,
        };
        StatusSnapshot {
            me: PeerRef { id: MachineId([9; 32]), name: "forge".into() },
            version: "0.1.0".into(),
            daemon_pid: 1,
            started_at_ms: 0,
            relays: vec!["https://relay.example.net/x".into()],
            relay_connected: true,
            bound: vec![],
            peers: vec![peer(2, "moss", PeerState::Relay), peer(1, "fern", PeerState::Direct)],
            revoked: vec![],
            host: None,
        }
    }

    #[test]
    fn status_is_cleaned_and_sorted() {
        let mut m = Mesh::default();
        m.apply_status(&snap());
        assert_eq!(m.peers[0].name, "fern");
        assert_eq!(m.peers[0].rtt_ms, None);
        assert!(!m.peers[0].addr.as_deref().unwrap_or("").contains('\x1b'));
        assert_eq!(m.peers[0].host, None);
        assert_eq!(m.relay_host().as_deref(), Some("relay.example.net"));
    }

    #[test]
    fn events_move_state_and_log_newest_first() {
        let mut m = Mesh::default();
        m.apply_status(&snap());
        let moss = PeerRef { id: MachineId([2; 32]), name: "moss".into() };
        let ev = |seq, t_ms, kind| Event { seq, t_ms, peer: Some(moss.clone()), kind };
        m.apply_event(&ev(1, 100, EventKind::PathChanged { from: PathKind::Relay, to: PathKind::Direct, remote_addr: None }), 100, true);
        m.apply_event(&ev(2, 200, EventKind::Disconnected { reason: "idle".into() }), 200, true);
        assert_eq!(m.peers[1].state, PeerState::Offline);
        assert_eq!(m.log[0].verb, "link down");
        assert_eq!(m.log[1].verb, "upgraded");
        assert_eq!(m.pulses.len(), 1);
        m.prune(100 + 5000);
        assert!(m.pulses.is_empty());
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_rtt(0.84), "0.8 ms");
        assert_eq!(fmt_rtt(48.2), "48 ms");
        assert_eq!(fmt_age(1000), "now");
        assert_eq!(fmt_age(7_200_000), "2h");
    }
}
