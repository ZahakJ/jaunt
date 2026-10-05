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
/// How long a path change takes to redraw its line (the relay waypoint lifting off).
pub const MORPH_MS: u64 = 1400;
/// Routine lines within this window fold into one ("sync in step x4").
const FOLD_MS: u64 = 120_000;
/// A refusal and the link-up it cut short are folded when this close together.
const SETTLE_MS: u64 = 20_000;

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
    /// The path of each `hist` sample (same length, same order).
    pub hist_path: VecDeque<PathKind>,
    /// The wall-clock second of the newest `hist` sample (samples are one a second).
    pub hist_sec: u64,
    /// The last handshake seen: (when, incoming, path).
    pub handshake: Option<(u64, bool, PathKind)>,
    /// Recent path changes, newest last: (when, from, to).
    pub changes: VecDeque<(u64, PathKind, PathKind)>,
    /// A path change being drawn right now.
    pub morph: Option<Morph>,
}

/// A link redrawing itself from one path style to another.
#[derive(Debug, Clone, Copy)]
pub struct Morph {
    pub start_ms: u64,
    pub from: PeerState,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Other,
    /// Routine and quiet ("sync in step"): folded across peers, drawn dim.
    Routine,
    LinkUp,
    /// The other side refused us because it does not know this machine yet (a link
    /// being formed); folded and dim, not an alarm.
    Waiting,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub t_ms: u64,
    pub tone: Tone,
    pub verb: String,
    pub peer: Option<String>,
    /// Detail text; `(text, tone)` pieces so a path change can show both styles.
    pub detail: Vec<(String, Tone)>,
    /// How many identical events this line stands for.
    pub count: u32,
    /// For folded routine lines: every peer involved (display names).
    pub peers: Vec<String>,
    pub kind: LineKind,
    pub peer_id: Option<MachineId>,
    /// Refusals folded into this line (Waiting) or before it (LinkUp).
    pub tries: u32,
}

impl LogLine {
    fn new(t_ms: u64, tone: Tone, verb: &str, peer: Option<String>, detail: Vec<(String, Tone)>) -> LogLine {
        LogLine { t_ms, tone, verb: verb.into(), peers: peer.iter().cloned().collect(), peer, detail, count: 1, kind: LineKind::Other, peer_id: None, tries: 0 }
    }

    fn same_text(&self, o: &LogLine) -> bool {
        self.verb == o.verb && self.detail == o.detail && self.tone == o.tone
    }

    /// The peer column: one name, or the folded names.
    pub fn who(&self) -> String {
        if self.peers.len() > 1 { self.peers.join(",") } else { self.peer.clone().unwrap_or_default() }
    }
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
    /// The top of the shared latency scale last drawn: kept unless the data moves well
    /// away from it, so the sparklines do not jump every second.
    pub spark_hi: std::cell::Cell<f64>,
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
                addr: p.remote_addr.as_deref().map(clean).map(|a| addr_display(&a)),
                addr_kind: p.addr_kind.as_deref().map(clean),
                host: p.host.as_deref().map(clean).filter(|h| !h.is_empty() && h != "off"),
                sessions: p.sessions,
                since_ms: p.connected_since_ms,
                seen_ms: p.last_seen_ms,
                version: p.version.as_deref().map(clean),
                error: p.last_error.as_deref().map(clean),
                hist_path: p.rtt_history.iter().rev().take(HISTORY).rev().filter(|r| r.rtt_ms.is_finite()).map(|r| r.path).collect(),
                hist_sec: p.rtt_history.last().map(|r| r.t_ms / 1000).unwrap_or(0),
                handshake: None,
                changes: VecDeque::new(),
                morph: None,
            })
            .collect();
        // What only the event stream told us survives a fresh snapshot.
        for p in &mut peers {
            if let Some(old) = self.peers.iter().find(|o| o.id.0 == p.id.0) {
                p.handshake = old.handshake;
                p.changes = old.changes.clone();
                p.morph = old.morph;
            }
        }
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
        while p.hist_path.len() >= p.hist.len() {
            p.hist_path.pop_front();
        }
        p.hist_path.push_back(s.path);
        p.hist_sec = s.t_ms / 1000;
        // A sample older than the last path change says nothing about the path now.
        let stale = p.changes.back().is_some_and(|(t, _, _)| s.t_ms < *t);
        if s.path != PathKind::Unknown && !stale {
            let to = path_state(s.path);
            if p.up() && p.state != to && p.morph.is_none() {
                p.morph = Some(Morph { start_ms: now_ms, from: p.state });
            }
            p.state = to;
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
        for p in &mut self.peers {
            if p.morph.is_some_and(|m| now_ms >= m.start_ms + MORPH_MS || now_ms + 5000 < m.start_ms) {
                p.morph = None;
            }
        }
    }

    /// Anything moving right now (the caller ticks faster while true).
    pub fn animating(&self, now_ms: u64) -> bool {
        self.pulses.iter().any(|p| now_ms < p.start_ms + p.dur_ms + 200)
            || self.peers.iter().any(|p| p.state == PeerState::Connecting || p.morph.is_some_and(|m| now_ms < m.start_ms + MORPH_MS))
    }

    fn log(&mut self, t_ms: u64, tone: Tone, verb: &str, peer: Option<String>, detail: Vec<(String, Tone)>) {
        self.push(LogLine::new(t_ms, tone, verb, peer, detail));
    }

    /// Newest first; history arrives in order, so insert by time. Repeats fold.
    fn push(&mut self, mut line: LogLine) {
        let at = self.log.iter().position(|l| l.t_ms <= line.t_ms).unwrap_or(self.log.len());
        if at == 0 {
            // The same thing again (same peer): count it.
            if let Some(top) = self.log.front_mut()
                && line.kind != LineKind::Waiting
                && top.same_text(&line)
                && top.peer == line.peer
                && line.t_ms.saturating_sub(top.t_ms) < FOLD_MS
            {
                top.count += 1;
                top.t_ms = line.t_ms;
                return;
            }
            // Routine lines fold across peers and hop over a few lines in between.
            if line.kind == LineKind::Routine
                && let Some(k) = self.log.iter().take(4).position(|l| {
                    l.kind == LineKind::Routine && l.same_text(&line) && line.t_ms.saturating_sub(l.t_ms) < FOLD_MS
                })
                && let Some(old) = self.log.remove(k)
            {
                line.count += old.count;
                let mut peers = old.peers;
                for p in line.peers.drain(..) {
                    if !peers.contains(&p) {
                        peers.push(p);
                    }
                }
                peers.sort();
                line.peers = peers;
                line.peer = line.peers.first().cloned();
            }
        }
        if self.log.len() >= LOG {
            self.log.pop_back();
        }
        let at = at.min(self.log.len());
        self.log.insert(at, line);
    }

    /// Take a recent line of `kind` about `id` out of the log (it is being folded).
    fn take_recent(&mut self, kind: LineKind, id: &MachineId, t_ms: u64) -> Option<LogLine> {
        let k = self.log.iter().take(8).position(|l| {
            l.kind == kind && l.peer_id.is_some_and(|p| p.0 == id.0) && t_ms.saturating_sub(l.t_ms) < SETTLE_MS
        })?;
        self.log.remove(k)
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
                let mut d = match path {
                    PathKind::Unknown => vec![q(dir.into())],
                    p => vec![(p.as_str().into(), path_tone(*p)), q(format!(" {} {dir}", theme::glyphs().dot))],
                };
                // Refusals while the other side was still learning of us end here.
                let tries = pid.and_then(|id| self.take_recent(LineKind::Waiting, &id, e.t_ms)).map(|l| l.tries).unwrap_or(0);
                if tries > 0 {
                    d.push(q(format!(" {} after {tries} {}", theme::glyphs().dot, if tries == 1 { "retry" } else { "retries" })));
                }
                let mut line = LogLine::new(e.t_ms, if *path == PathKind::Unknown { Tone::Accent } else { path_tone(*path) }, "link up", name, d);
                line.kind = LineKind::LinkUp;
                line.peer_id = pid;
                line.tries = tries;
                self.push(line);
                if let Some(id) = pid {
                    if let Some(p) = self.peer_mut(&id) {
                        p.state = path_state(*path);
                        p.since_ms.get_or_insert(e.t_ms);
                        p.handshake = Some((e.t_ms, *incoming, *path));
                    }
                    if live {
                        self.pulse(id, now_ms, if *incoming { Dir::In } else { Dir::Out }, true);
                    }
                }
            }
            EventKind::PathChanged { from, to, remote_addr } => {
                // A relay path's address is the relay's URL: its host says enough.
                let addr = remote_addr.as_deref().map(clean).map(|a| addr_display(&a));
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
                    let mut morphed = false;
                    if let Some(p) = self.peer_mut(&id) {
                        let to_state = path_state(*to);
                        // The line redraws itself (relay waypoint lifts off or drops in).
                        if live && from != to && p.up() && p.state != to_state {
                            p.morph = Some(Morph { start_ms: now_ms, from: p.state });
                            morphed = true;
                        }
                        p.state = to_state;
                        if addr.is_some() {
                            p.addr = addr;
                        }
                        if from != to {
                            if p.changes.len() >= 4 {
                                p.changes.pop_front();
                            }
                            p.changes.push_back((e.t_ms, *from, *to));
                        }
                    }
                    if live && !morphed {
                        self.pulse(id, now_ms, Dir::Out, true);
                    }
                }
            }
            // The daemon now reports such a refusal once per streak as `dial_failed`, with
            // no handshake before it; older daemons sent handshake + disconnected each time.
            EventKind::Disconnected { reason } | EventKind::DialFailed { error: reason }
                if reason.contains("not a member") && pid.is_some() =>
            {
                // The other side does not know this machine yet: a link being formed (the
                // roster reaches it a moment later). Fold the false start into one quiet line.
                let Some(id) = pid else { return };
                let up = self.take_recent(LineKind::LinkUp, &id, e.t_ms).map(|l| l.tries).unwrap_or(0);
                let before = self.take_recent(LineKind::Waiting, &id, e.t_ms).map(|l| l.tries).unwrap_or(0);
                let tries = up.max(before) + 1;
                // Many refusals in a row are no longer "settling": say so in amber.
                let (tone, text) = if tries >= 4 {
                    (Tone::Relay, "refused: not a member there".to_string())
                } else {
                    (Tone::Quiet, "does not know this machine yet".to_string())
                };
                let mut line = LogLine::new(e.t_ms, tone, "settling", name, vec![q(text)]);
                line.kind = LineKind::Waiting;
                line.peer_id = pid;
                line.tries = tries;
                line.count = tries;
                self.push(line);
                if let Some(p) = self.peer_mut(&id) {
                    p.state = PeerState::Connecting;
                    p.seen_ms = Some(e.t_ms);
                    p.since_ms = None;
                    p.morph = None;
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
                    p.morph = None;
                }
            }
            EventKind::DialFailed { error } => {
                self.log(e.t_ms, Tone::Quiet, "no answer", name, vec![q(clean(error))]);
            }
            EventKind::Sync { changed, outgoing } => {
                let what = if *changed { "roster updated" } else { "in step" };
                let mut line = LogLine::new(e.t_ms, if *changed { Tone::Accent } else { Tone::Quiet }, "sync", name, vec![q(what.into())]);
                if !*changed {
                    line.kind = LineKind::Routine;
                }
                self.push(line);
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

/// A remote address for display: `ip:port` as it is; a relay's URL as its host (the
/// scheme and path say nothing new and make it too long for a narrow pane).
pub fn addr_display(a: &str) -> String {
    match a.split_once("://") {
        Some((_, rest)) => rest.split(['/', '?']).next().unwrap_or(rest).to_string(),
        None => a.to_string(),
    }
}

/// "5s ago", or "just now".
pub fn fmt_ago(ms: u64) -> String {
    match fmt_age(ms) {
        n if n == "now" => "just now".into(),
        a => format!("{a} ago"),
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
            me: PeerRef { id: MachineId([9; 32]), name: "benzene".into() },
            version: "0.1.0".into(),
            daemon_pid: 1,
            started_at_ms: 0,
            relays: vec!["https://relay.example.net/x".into()],
            relay_connected: true,
            bound: vec![],
            peers: vec![peer(2, "xylene", PeerState::Relay), peer(1, "toluene", PeerState::Direct)],
            revoked: vec![],
            host: None,
        }
    }

    #[test]
    fn status_is_cleaned_and_sorted() {
        let mut m = Mesh::default();
        m.apply_status(&snap());
        assert_eq!(m.peers[0].name, "toluene");
        assert_eq!(m.peers[0].rtt_ms, None);
        assert!(!m.peers[0].addr.as_deref().unwrap_or("").contains('\x1b'));
        assert_eq!(m.peers[0].host, None);
        assert_eq!(m.relay_host().as_deref(), Some("relay.example.net"));
    }

    #[test]
    fn events_move_state_and_log_newest_first() {
        let mut m = Mesh::default();
        m.apply_status(&snap());
        let xylene = PeerRef { id: MachineId([2; 32]), name: "xylene".into() };
        let ev = |seq, t_ms, kind| Event { seq, t_ms, peer: Some(xylene.clone()), kind };
        m.apply_event(&ev(1, 100, EventKind::PathChanged { from: PathKind::Relay, to: PathKind::Direct, remote_addr: None }), 100, true);
        // The upgrade redraws the line (the relay waypoint lifts off) instead of a pulse.
        assert!(m.peers[1].morph.is_some() && m.pulses.is_empty());
        assert!(m.animating(500));
        assert_eq!(m.peers[1].changes.len(), 1);
        m.apply_event(&ev(2, 200, EventKind::Sync { changed: false, outgoing: true }), 200, true);
        assert_eq!(m.pulses.len(), 1);
        m.apply_event(&ev(3, 300, EventKind::Disconnected { reason: "idle".into() }), 300, true);
        assert_eq!(m.peers[1].state, PeerState::Offline);
        assert!(m.peers[1].morph.is_none());
        assert_eq!(m.log[0].verb, "link down");
        assert_eq!(m.log[2].verb, "upgraded");
        m.prune(100 + 5000);
        assert!(m.pulses.is_empty());
    }

    #[test]
    fn routine_lines_fold_and_settling_is_not_an_alarm() {
        let mut m = Mesh::default();
        m.apply_status(&snap());
        let toluene = PeerRef { id: MachineId([1; 32]), name: "toluene".into() };
        let xylene = PeerRef { id: MachineId([2; 32]), name: "xylene".into() };
        let ev = |t_ms, p: &PeerRef, kind| Event { seq: 0, t_ms, peer: Some(p.clone()), kind };
        let sync = || EventKind::Sync { changed: false, outgoing: true };
        m.apply_event(&ev(1000, &toluene, sync()), 1000, false);
        m.apply_event(&ev(2000, &xylene, sync()), 2000, false);
        m.apply_event(&ev(3000, &toluene, EventKind::Served { service: "exec".into() }), 3000, false);
        m.apply_event(&ev(4000, &toluene, sync()), 4000, false);
        assert_eq!(m.log.len(), 2, "{:?}", m.log);
        assert_eq!((m.log[0].count, m.log[0].who()), (3, "toluene,xylene".to_string()));
        assert_eq!(m.log[0].tone, Tone::Quiet);

        // A link being formed: up, refused (the other side does not know us yet), up again.
        let mut m = Mesh::default();
        m.apply_status(&snap());
        let up = |incoming| EventKind::Handshake { incoming, path: PathKind::Relay };
        let refused = || EventKind::Disconnected { reason: "closed by the other side: not a member of this mesh".into() };
        m.apply_event(&ev(1000, &xylene, up(false)), 1000, false);
        m.apply_event(&ev(1100, &xylene, refused()), 1100, false);
        m.apply_event(&ev(1500, &xylene, up(false)), 1500, false);
        m.apply_event(&ev(1600, &xylene, refused()), 1600, false);
        assert_eq!(m.log.len(), 1);
        assert_eq!((m.log[0].verb.as_str(), m.log[0].tone, m.log[0].count), ("settling", Tone::Quiet, 2));
        m.apply_event(&ev(2000, &xylene, up(true)), 2000, false);
        assert_eq!(m.log.len(), 1);
        assert_eq!(m.log[0].verb, "link up");
        assert!(m.log[0].detail.iter().any(|(t, _)| t.contains("after 2 retries")));
        assert!(!m.log.iter().any(|l| l.tone == Tone::Bad));

        // H8: what the daemon sends now: one quiet refusal, then the link once it synced.
        let mut m = Mesh::default();
        m.apply_status(&snap());
        let refused_once = EventKind::DialFailed { error: "xylene refuses this machine as not a member there (it may not know of it yet)".into() };
        m.apply_event(&ev(1000, &xylene, refused_once), 1000, false);
        assert_eq!((m.log.len(), m.log[0].verb.as_str(), m.log[0].tone), (1, "settling", Tone::Quiet));
        m.apply_event(&ev(3000, &xylene, up(false)), 3000, false);
        assert_eq!(m.log.len(), 1);
        assert_eq!(m.log[0].verb, "link up");
        assert!(m.log[0].detail.iter().any(|(t, _)| t.contains("after 1 retry")));
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_rtt(0.84), "0.8 ms");
        assert_eq!(fmt_rtt(48.2), "48 ms");
        assert_eq!(fmt_age(1000), "now");
        assert_eq!(fmt_age(7_200_000), "2h");
    }
}
