//! `jaunt demo [mesh|link|ls]`: the real views driven by a simulated mesh, for trying the
//! look and for captures. Deterministic: the same `--at` gives the same frame.
//!
//! The story: benzene (this desktop) with toluene (a laptop on the LAN that roams to a phone
//! hotspot), xylene (a server whose path upgrades from the relay to direct), phenol (behind a
//! hard NAT, stays relayed) and styrene (asleep).

use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use super::model::{Mesh, Tone};
use super::ritual::{self, State};
use super::tui::{Action, Tui, handle_key};
use super::view::ViewState;
use super::{input::Key, theme};
use crate::proto::{
    Event, EventKind, PairRole, PairStep, PairUpdate, PathKind, PeerRef, PeerState, PeerStatus, RttSample, StatusSnapshot,
};
use crate::roster::MachineId;

/// Simulated wall clock at demo time 0 (a fixed instant, so ages are reproducible).
const BASE: u64 = 1_790_000_000_000;
const SEC: u64 = 1000;
const MIN: u64 = 60 * SEC;
const HOUR: u64 = 60 * MIN;

fn id(seed: u8) -> MachineId {
    let mut b = [0u8; 32];
    let mut x = (seed as u32).wrapping_mul(2_654_435_761) | 1;
    for v in b.iter_mut() {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *v = (x >> 8) as u8;
    }
    b[0] = seed;
    MachineId(b)
}

fn r(seed: u8, name: &str) -> PeerRef {
    PeerRef { id: id(seed), name: name.into() }
}

const TOLUENE: u8 = 0x1a;
const XYLENE: u8 = 0x5d;
const PHENOL: u8 = 0x93;
const STYRENE: u8 = 0xc6;

/// Round-trip time of `peer` at demo time `t` (ms), with a little deterministic jitter.
fn rtt(peer: u8, t: i64) -> Option<(f64, PathKind)> {
    let s = t.div_euclid(1000);
    let j = (((s * 7919 + peer as i64 * 104_729) % 1000) as f64 / 1000.0 - 0.5).abs();
    let wave = ((s as f64) / 6.0 + peer as f64).sin();
    match peer {
        TOLUENE if t < 9 * SEC as i64 => Some((0.7 + 0.25 * j + 0.08 * wave, PathKind::Direct)),
        TOLUENE => Some((22.0 + 9.0 * j + 3.0 * wave, PathKind::Direct)),
        XYLENE if t < 3500 => Some((41.0 + 12.0 * j + 4.0 * wave, PathKind::Relay)),
        XYLENE => Some((8.6 + 2.5 * j + 0.8 * wave, PathKind::Direct)),
        PHENOL => Some((47.0 + 22.0 * j + 9.0 * wave, PathKind::Relay)),
        _ => None,
    }
}

fn peer(seed: u8, name: &str, state: PeerState, t: i64) -> PeerStatus {
    let hist = (0..60)
        .filter_map(|k| {
            let at = t - (59 - k) * 1000;
            rtt(seed, at).map(|(v, path)| RttSample { t_ms: (BASE as i64 + at) as u64, rtt_ms: v, path })
        })
        .collect();
    let (addr, kind, host, sessions, since, seen) = match seed {
        TOLUENE => ("192.168.1.23:41641", "lan", Some("awake"), Some(2), BASE - 2 * HOUR - 13 * MIN, None),
        XYLENE => ("https://relay.jaunt.example", "relay", None, Some(1), BASE - 40 * SEC, None),
        PHENOL => ("https://relay.jaunt.example", "relay", Some("on battery"), Some(0), BASE - 25 * MIN, None),
        _ => ("", "", Some("will sleep on lid close"), None, 0, Some(BASE - 2 * HOUR - 50 * MIN)),
    };
    let up = matches!(state, PeerState::Direct | PeerState::Relay);
    PeerStatus {
        id: id(seed),
        name: name.into(),
        state,
        remote_addr: up.then(|| addr.into()),
        addr_kind: up.then(|| kind.into()),
        rtt_ms: rtt(seed, t).map(|x| x.0).filter(|_| up),
        rtt_history: if up { hist } else { Vec::new() },
        connected_since_ms: up.then_some(since),
        last_seen_ms: seen,
        version: Some("0.1.0".into()),
        sessions,
        host: host.map(Into::into),
        last_error: None,
    }
}

fn snapshot() -> StatusSnapshot {
    StatusSnapshot {
        me: r(0x07, "benzene"),
        version: "0.1.0".into(),
        daemon_pid: 4242,
        started_at_ms: BASE - 3 * HOUR,
        relays: vec!["https://relay.jaunt.example".into()],
        relay_connected: true,
        bound: vec![],
        peers: vec![
            peer(TOLUENE, "toluene", PeerState::Direct, 0),
            peer(XYLENE, "xylene", PeerState::Relay, 0),
            peer(PHENOL, "phenol", PeerState::Relay, 0),
            peer(STYRENE, "styrene", PeerState::Offline, 0),
        ],
        revoked: vec![],
        host: None,
        forwards: vec![],
    }
}

/// The scripted events: (demo time ms, peer, event). Negative times are history.
fn script() -> Vec<(i64, Option<PeerRef>, EventKind)> {
    use EventKind::*;
    let h = |m: u64| -(m as i64);
    vec![
        (h(3 * HOUR), None, DaemonStarted { version: "0.1.0".into() }),
        (h(2 * HOUR + 50 * MIN), Some(r(STYRENE, "styrene")), Disconnected { reason: "idle timeout".into() }),
        (h(2 * HOUR + 13 * MIN), Some(r(TOLUENE, "toluene")), Handshake { incoming: true, path: PathKind::Direct }),
        (h(2 * HOUR + 12 * MIN), Some(r(TOLUENE, "toluene")), Sync { changed: false, outgoing: false }),
        (h(97 * MIN), Some(r(TOLUENE, "toluene")), Served { service: "shell jaunt".into() }),
        (h(61 * MIN), None, RosterChanged { what: "relay list updated by toluene".into() }),
        (h(25 * MIN), Some(r(PHENOL, "phenol")), Handshake { incoming: false, path: PathKind::Relay }),
        (h(24 * MIN), Some(r(PHENOL, "phenol")), Sync { changed: true, outgoing: true }),
        (h(11 * MIN), Some(r(PHENOL, "phenol")), Served { service: "cp".into() }),
        (h(40 * SEC), Some(r(XYLENE, "xylene")), Handshake { incoming: false, path: PathKind::Relay }),
        (h(39 * SEC), Some(r(XYLENE, "xylene")), Sync { changed: false, outgoing: true }),
        (1200, Some(r(TOLUENE, "toluene")), Served { service: "shell jaunt".into() }),
        (
            3500,
            Some(r(XYLENE, "xylene")),
            PathChanged { from: PathKind::Relay, to: PathKind::Direct, remote_addr: Some("203.0.113.40:7841".into()) },
        ),
        (6000, Some(r(PHENOL, "phenol")), Sync { changed: false, outgoing: true }),
        (
            9000,
            Some(r(TOLUENE, "toluene")),
            PathChanged { from: PathKind::Direct, to: PathKind::Direct, remote_addr: Some("172.20.10.4:41641".into()) },
        ),
        (13_000, Some(r(STYRENE, "styrene")), DialFailed { error: "no answer (asleep?)".into() }),
        (16_000, Some(r(PHENOL, "phenol")), Served { service: "exec".into() }),
        (21_000, Some(r(XYLENE, "xylene")), Sync { changed: true, outgoing: false }),
    ]
}

/// Runs the story forward in simulated time.
pub struct Sim {
    pub mesh: Mesh,
    events: Vec<(i64, Option<PeerRef>, EventKind)>,
    next: usize,
    /// Last whole second whose RTT samples were applied.
    sec: i64,
    seq: u64,
}

impl Sim {
    pub fn new() -> Sim {
        let mut mesh = Mesh::default();
        mesh.apply_status(&snapshot());
        let mut s = Sim { mesh, events: script(), next: 0, sec: -1, seq: 0 };
        s.advance(0);
        s
    }

    pub fn now(t: i64) -> u64 {
        (BASE as i64 + t) as u64
    }

    /// Apply everything up to demo time `t` (ms).
    pub fn advance(&mut self, t: i64) {
        while self.next < self.events.len() && self.events[self.next].0 <= t {
            let (at, peer, kind) = self.events[self.next].clone();
            self.seq += 1;
            let ev = Event { seq: self.seq, t_ms: Self::now(at), peer, kind };
            self.mesh.apply_event(&ev, Self::now(at), at >= 0);
            self.after_event(at);
            self.next += 1;
        }
        while self.sec < t.div_euclid(1000) {
            self.sec += 1;
            let at = self.sec * 1000;
            for p in [TOLUENE, XYLENE, PHENOL] {
                if let Some((v, path)) = rtt(p, at) {
                    let sample = RttSample { t_ms: Self::now(at), rtt_ms: v, path };
                    self.mesh.apply_rtt(&id(p), &sample, Self::now(at));
                }
            }
        }
        // Past the end of the story: a quiet sync now and then, so it keeps living.
        if t > 24_000 && (t / 1000) % 15 == 0 && self.next == self.events.len() {
            let k = (t / 15_000) as usize;
            let (seed, name) = [(XYLENE, "xylene"), (PHENOL, "phenol"), (TOLUENE, "toluene")][k % 3];
            let at = (t / 1000) * 1000;
            if !self.mesh.log.front().is_some_and(|l| l.t_ms == Self::now(at)) {
                let ev = Event { seq: 0, t_ms: Self::now(at), peer: Some(r(seed, name)), kind: EventKind::Sync { changed: false, outgoing: k.is_multiple_of(2) } };
                self.mesh.apply_event(&ev, Self::now(at), true);
            }
        }
    }

    fn after_event(&mut self, at: i64) {
        // Facts that come from status polls in real use.
        if at == 3500
            && let Some(p) = self.mesh.peers.iter_mut().find(|p| p.name == "xylene")
        {
            p.addr_kind = Some("public".into());
            p.addr = Some("203.0.113.40:7841".into());
        }
        if at == 9000
            && let Some(p) = self.mesh.peers.iter_mut().find(|p| p.name == "toluene")
        {
            p.addr_kind = Some("public".into());
        }
    }
}

/// Print lines; a closed pipe (`| head`) ends quietly.
fn print_lines(lines: impl IntoIterator<Item = String>) -> Result<i32> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for l in lines {
        if writeln!(out, "{l}").is_err() {
            return Ok(0);
        }
    }
    Ok(0)
}

fn parse_at(at: Option<f64>) -> Option<i64> {
    at.map(|s| (s * 1000.0) as i64)
}

pub async fn run(what: &str, at: Option<f64>, join: bool) -> Result<i32> {
    match what {
        "mesh" => mesh(parse_at(at)).await,
        "ls" => {
            let mut sim = Sim::new();
            let t = parse_at(at).unwrap_or(20_000);
            sim.advance(t);
            sim.mesh.pulses.clear();
            let c = super::ls::render(&sim.mesh, theme::width().saturating_sub(1), Sim::now(t));
            print_lines(c.lines(theme::theme().mode))
        }
        "link" => link(parse_at(at), join).await,
        "hop" => {
            let lines = [
                "$ jaunt toluene".to_string(),
                super::hop_banner_in("benzene", "toluene", PathKind::Direct, Some(0.8)),
                super::link_lost_line("toluene", 4),
                super::hop_banner_out("benzene", "toluene", true),
                "$ jaunt phenol".to_string(),
                super::hop_banner_in("benzene", "phenol", PathKind::Relay, Some(48.0)),
                super::hop_banner_out("benzene", "phenol", false),
            ];
            print_lines(lines)
        }
        other => bail!("no demo called '{other}': try mesh, link, ls or hop"),
    }
}

async fn mesh(at: Option<i64>) -> Result<i32> {
    if !crate::term::stdin_is_tty() || !crate::term::stdout_is_tty() {
        bail!("the demo needs a terminal ('jaunt demo ls' prints a static block)");
    }
    let mut sim = Sim::new();
    let mut tui = Tui::new();
    // xylene is selected: its path history shows the relay-to-direct upgrade.
    let mut st = ViewState { tag: "demo", selected: 1, ..ViewState::default() };
    let start = Instant::now();
    let clock = |start: &Instant| at.unwrap_or(start.elapsed().as_millis() as i64);
    tui.enter()?;
    let mut tick = tokio::time::interval(Duration::from_millis(66));
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = winch.recv() => tui.resized(),
            k = tui.key() => {
                let Some(k) = k else { return Ok(0) };
                match handle_key(&mut st, k, sim.mesh.peers.len()) {
                    Action::Quit => return Ok(0),
                    Action::Hop(i) => {
                        let name = sim.mesh.peers.get(i).map(|p| p.name.clone()).unwrap_or_default();
                        let t = Sim::now(clock(&start));
                        st.toast = Some((format!("demo: in real use, Enter opens a session on {name}"), Tone::Accent, t + 2500));
                    }
                    Action::Link => {
                        let t = Sim::now(clock(&start));
                        st.toast = Some(("demo: 'jaunt demo link' shows the linking ritual".into(), Tone::Accent, t + 2500));
                    }
                    Action::None => {}
                }
            }
        }
        let t = clock(&start);
        sim.advance(t);
        sim.mesh.prune(Sim::now(t));
        tui.draw(&sim.mesh, &mut st, Sim::now(t));
        if at.is_some() {
            // A still frame: draw once, then only wait for keys.
            tick.reset_after(Duration::from_secs(3600));
        }
    }
}

/// The ritual's updates with their demo times (ms).
fn link_script(role: PairRole) -> Vec<(i64, PairStep)> {
    let toluene = r(TOLUENE, "toluene");
    let benzene = r(0x07, "benzene");
    let sigil = vec!["#.#.#".into(), "##.##".into(), ".###.".into(), "#.#.#".into(), "##.##".into()];
    let words: Vec<String> = vec!["tremor".into(), "orbit".into(), "velvet".into()];
    let inviter = role == PairRole::Inviter;
    let other = if inviter { toluene.clone() } else { benzene.clone() };
    let mut v = vec![];
    if inviter {
        v.push((
            0,
            PairStep::Waiting {
                code: "4127-tremor-kudos".into(),
                expires_at_ms: Sim::now(300_000),
                relays: vec!["https://relay.jaunt.example".into()],
                first_machine: false,
            },
        ));
    } else {
        v.push((0, PairStep::Dialing { relays: vec!["https://relay.jaunt.example".into()] }));
    }
    v.extend([
        (4200, PairStep::Rendezvous),
        (4900, PairStep::KeyExchange),
        (5500, PairStep::Sealed { other: other.clone() }),
        (6000, PairStep::Confirm { other: other.clone(), sigil, hue: 1, words }),
        (9500, PairStep::Confirmed { here: !inviter }),
        (11_000, PairStep::Confirmed { here: inviter }),
        (11_300, PairStep::Linked { other, members: 5 }),
    ]);
    if inviter {
        v.extend([
            (12_000, PairStep::FanOut { machine: r(XYLENE, "xylene"), ok: true, detail: String::new() }),
            (12_400, PairStep::FanOut { machine: r(PHENOL, "phenol"), ok: true, detail: String::new() }),
            (13_200, PairStep::FanOut { machine: r(STYRENE, "styrene"), ok: false, detail: "asleep".into() }),
            (13_300, PairStep::FanOutDone { told: 2, not_yet: 1 }),
        ]);
    }
    v
}

async fn link(at: Option<i64>, join: bool) -> Result<i32> {
    let role = if join { PairRole::Joiner } else { PairRole::Inviter };
    let me = if join { "toluene" } else { "benzene" };
    let code = join.then(|| "4127-tremor-kudos".to_string());
    let script = link_script(role);
    let build = |t: i64| {
        let mut st = State::new(role, me, code.clone(), Sim::now(0));
        for (at, step) in script.iter().filter(|(a, _)| *a <= t) {
            // The prompt is shown, never answered, in the demo: "here" comes from the script.
            st.apply(&PairUpdate { role, step: step.clone() }, Sim::now(*at), false);
        }
        st
    };
    let width = theme::width().saturating_sub(1);
    let fancy = crate::term::stdout_is_tty();
    if let Some(t) = at {
        let c = ritual::draw(&build(t), width, Sim::now(t));
        return print_lines(c.lines(theme::theme().mode));
    }
    if !fancy {
        let lines = script.iter().flat_map(|(_, step)| ritual::plain_lines(&PairUpdate { role, step: step.clone() }));
        return print_lines(lines);
    }
    let _raw = crate::term::RawMode::enable()?;
    let mut keys = super::input::Keys::start();
    let mut out = super::canvas::Inline::new(theme::theme().mode);
    let start = Instant::now();
    let end = script.last().map(|(t, _)| *t).unwrap_or(0) + 1500;
    print!("\x1b[?25l");
    loop {
        let t = start.elapsed().as_millis() as i64;
        let c = ritual::draw(&build(t), width, Sim::now(t));
        out.draw(&c, &mut std::io::stdout())?;
        if t > end {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(66)) => {}
            k = keys.rx.recv() => if matches!(k, Some(Key::CtrlC) | Some(Key::Char('q')) | None) { break },
        }
    }
    keys.stop();
    out.finish(&mut std::io::stdout())?;
    print!("\x1b[?25h");
    Ok(0)
}
