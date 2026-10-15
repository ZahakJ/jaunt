//! The daemon: owns the key and the one endpoint, keeps a warm connection to every roster
//! member, serves incoming streams, syncs the roster, and records paths, RTT and events.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream};
use iroh::{Endpoint, RelayUrl};
use tokio::sync::{Notify, watch};
use tokio::time::timeout;

use crate::config::Config;
use crate::events::EventLog;
use crate::net::{self, CLOSE_NORMAL, CLOSE_NOT_MEMBER, Members, PathInfo};
use crate::paths::Paths;
use crate::proto::{
    self, EventKind, FrameType, Hello, HelloProbe, MAX_FRAME, MAX_HEADER, PROTO_VERSION, PathKind, PeerRef,
    PeerState, PeerStatus, PingInfo, Reply, RevokedStatus, RttSample, Service, StatusSnapshot,
};
use crate::roster::{MachineId, Resolve, Roster, now_ms};
use crate::children::Children;
use crate::{identity, ipc, services, shell, systemd};

const RETRY_EVERY: Duration = Duration::from_secs(30);
const RECONNECT_AFTER_LOSS: Duration = Duration::from_secs(2);
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(10);
const RTT_HISTORY: usize = 60;
/// A peer that refuses us as "not a member" right after linking usually just has not
/// written its roster yet: try again this soon, this many times, before the normal cadence.
const REFUSED_RETRY: Duration = Duration::from_secs(1);
const REFUSED_FAST_TRIES: u32 = 15;
/// A settling round syncs with every other member, bounded by this.
const SETTLE_LIMIT: Duration = Duration::from_secs(5);
/// No authenticated contact with any member for this long counts as an absence.
const ABSENT_AFTER_MS: u64 = 60_000;
/// The daemon's 1 s monitor not running for this long means it was frozen or suspended.
const TICK_GAP_MS: u64 = 5_000;
/// While unsettled, the monitor starts a round on its own this often.
const ROUND_BACKGROUND_MS: u64 = 30_000;
/// How long a revoked peer's stream may still run so that the process it started gets the
/// follow-up SIGKILL (`children::kill_peer` sends it after 1 s).
const REVOKED_GRACE: Duration = Duration::from_secs(3);

#[derive(Default)]
struct Peer {
    conn: Option<Connection>,
    /// Every connection adopted for this peer that may still be open (the current one
    /// included), so that revoking it closes all of them.
    conns: Vec<Connection>,
    /// Consecutive "not a member" refusals from the peer on connections we dialled.
    refusals: u32,
    /// The connection whose handshake was reported (an outgoing one after its first sync).
    announced: Option<usize>,
    /// Datagrams received on the current connection, and when that count last grew.
    rx_seen: (u64, u64),
    conn_seq: u64,
    /// The current connection was dialled by the peer.
    incoming: bool,
    since: Option<u64>,
    path: Option<PathInfo>,
    history: VecDeque<RttSample>,
    last_seen: Option<u64>,
    info: Option<PingInfo>,
    last_error: Option<String>,
    dialing: bool,
    task_running: bool,
    kick: Arc<Notify>,
    dial_lock: Arc<tokio::sync::Mutex<()>>,
}

pub struct Daemon {
    pub paths: Paths,
    pub me: MachineId,
    pub ep: Endpoint,
    pub events: EventLog,
    members: Arc<Members>,
    cfg: Mutex<Config>,
    roster: Mutex<Roster>,
    peers: Mutex<HashMap<MachineId, Peer>>,
    relays: Mutex<Vec<RelayUrl>>,
    roster_tx: watch::Sender<u64>,
    shutdown_tx: watch::Sender<bool>,
    conn_seq: AtomicU64,
    started_at: u64,
    sessions_cache: Mutex<Option<(Instant, Option<u32>)>>,
    weak: std::sync::Weak<Daemon>,
    /// Processes running here on behalf of peers (ended when a peer is revoked).
    pub children: Arc<Children>,
    /// Remote addresses of QUIC paths, for the contact audit in `jaunt check`.
    pub contacts: crate::check::Contacts,
    /// Held for the whole of a link or join: one pairing at a time.
    pub pairing: tokio::sync::Mutex<()>,
    /// Codes this machine already tried as a joiner (each is tried once).
    pub tried_codes: crate::pair::TriedCodes,
    /// Host-mode guard (§9).
    pub host: crate::awake::Handle,
    /// The settling round after an absence.
    settle: Settle,
    /// Last working direct addresses of each peer, persisted as dial hints.
    addrs: Mutex<crate::addrs::AddrBook>,
    /// Which machine created which tmux session here (persisted).
    tmux_owners: Mutex<BTreeMap<String, MachineId>>,
    /// Forwards this daemon holds open.
    pub forwards: crate::forwards::Forwards,
    /// Clipboard sharing.
    pub clip: crate::clipsync::ClipState,
    /// Machines hopped in here right now, oldest first, and the last one that was.
    hops: Mutex<(Vec<MachineId>, Option<MachineId>)>,
}

/// A machine that comes back from an absence (daemon start, a freeze or suspend, or no
/// contact with any member for a minute) may hold a roster that misses a revocation. From
/// then on it is *unsettled*: it serves a peer's Shell, Exec, Tcp, Copy or KillSessions
/// only once it has completed a Sync with a member other than that peer, or when no member
/// other than that peer exists. Otherwise the first such request waits for a settling round
/// that started after the requester's connection was established (so the network was up
/// while it ran; rounds during an outage do not count), at most `SETTLE_LIMIT`. If that
/// round reached no other member, the machine is settled provisionally and serves without
/// waiting until the next absence; the keepers go on dialling, and a tombstone that arrives
/// later ends the revoked machine's connections and processes as usual. What remains: a
/// revoked machine is served only if no informed member answered within 5 s of its first
/// request, and then only until an informed member is reachable.
struct Settle {
    state: Mutex<SettleState>,
    /// The most recently started settling round that is still running: its start (wall
    /// ms) and a receiver that turns true when it ends.
    current: Mutex<Option<(u64, watch::Receiver<bool>)>>,
    /// Start of the most recently started round, and the latest start of a round that
    /// has completed.
    last_round_start: AtomicU64,
    last_done_start: AtomicU64,
    /// Wall-clock ms of the monitor's last tick, of the last contact, and of the moment
    /// this machine was last settled.
    last_tick: AtomicU64,
    last_contact: AtomicU64,
    last_settled: AtomicU64,
}

#[derive(Debug, Default)]
struct SettleState {
    /// Set while unsettled: since when.
    unsettled_since: Option<u64>,
    /// The members a Sync completed with since then.
    synced: HashSet<MachineId>,
}

impl SettleState {
    /// May a request from `peer` be served? `others` are the members other than this machine.
    fn serves(&self, peer: &MachineId, others: &HashSet<MachineId>) -> bool {
        self.unsettled_since.is_none() || self.synced.iter().any(|x| x != peer) || others.iter().all(|x| x == peer)
    }

    /// Would every member be served (then the machine is settled)?
    fn serves_all(&self, others: &HashSet<MachineId>) -> bool {
        others.len() <= 1 || self.synced.intersection(others).count() >= 2
    }
}

impl Settle {
    fn new(now: u64) -> Settle {
        Settle {
            state: Mutex::new(SettleState { unsettled_since: Some(now), synced: HashSet::new() }),
            current: Mutex::new(None),
            last_round_start: AtomicU64::new(0),
            last_done_start: AtomicU64::new(0),
            last_tick: AtomicU64::new(now),
            last_contact: AtomicU64::new(now),
            last_settled: AtomicU64::new(0),
        }
    }

    /// Called at least once a second (and before a gated request): notice a return from an
    /// absence at `now` and become unsettled. A freeze or suspend (no call for over 5 s)
    /// always starts over; no contact with any member for a minute makes a settled machine
    /// unsettled. Returns why, for a new absence.
    fn notice(&self, now: u64) -> Option<String> {
        let gap = now.saturating_sub(self.last_tick.swap(now, Ordering::SeqCst));
        if gap > TICK_GAP_MS {
            self.unsettle(now);
            return Some(format!("this machine was frozen or asleep for {} s", gap / 1000));
        }
        if self.silent(now) && self.is_settled() {
            self.unsettle(now);
            return Some("no contact with any member for a minute".into());
        }
        None
    }

    fn silent(&self, now: u64) -> bool {
        let quiet = now.saturating_sub(self.last_contact.load(Ordering::Relaxed));
        let settled = now.saturating_sub(self.last_settled.load(Ordering::Relaxed));
        quiet > ABSENT_AFTER_MS && settled > ABSENT_AFTER_MS
    }

    fn is_settled(&self) -> bool {
        self.state.lock().map(|s| s.unsettled_since.is_none()).unwrap_or(false)
    }

    /// Start over: nothing synced since `now` yet.
    fn unsettle(&self, now: u64) {
        if let Ok(mut s) = self.state.lock() {
            s.unsettled_since = Some(now);
            s.synced.clear();
        }
    }

    /// A Sync with `id` completed at `now`; settled once every member would be served.
    fn synced(&self, id: MachineId, others: &HashSet<MachineId>, now: u64) {
        self.last_contact.store(now, Ordering::Relaxed);
        if let Ok(mut s) = self.state.lock()
            && s.unsettled_since.is_some()
        {
            s.synced.insert(id);
            self.settle_if_done(&mut s, others, now);
        }
    }

    fn settle_if_done(&self, s: &mut SettleState, others: &HashSet<MachineId>, now: u64) {
        if s.unsettled_since.is_some() && s.serves_all(others) {
            s.unsettled_since = None;
            s.synced.clear();
            self.last_settled.store(now, Ordering::Relaxed);
        }
    }

    fn serves(&self, peer: &MachineId, others: &HashSet<MachineId>) -> bool {
        self.state.lock().map(|s| s.serves(peer, others)).unwrap_or(false)
    }

    fn unsettled_since(&self) -> Option<u64> {
        self.state.lock().ok().and_then(|s| s.unsettled_since)
    }

    /// A settling round that started at `start` has completed.
    fn round_done(&self, start: u64) {
        self.last_done_start.fetch_max(start, Ordering::SeqCst);
    }

    /// A requester whose connection was established at `established`: if a round that
    /// started at or after that moment (and after the absence began) has completed, the
    /// network was up while it ran, so what it could not reach was not reachable. Become
    /// settled provisionally (until the next absence). True when settled now.
    fn settle_provisionally(&self, established: u64, now: u64) -> bool {
        let Ok(mut s) = self.state.lock() else { return false };
        let Some(since) = s.unsettled_since else { return true };
        if self.last_done_start.load(Ordering::SeqCst) >= since.max(established) {
            s.unsettled_since = None;
            s.synced.clear();
            self.last_settled.store(now, Ordering::Relaxed);
            return true;
        }
        false
    }
}

fn effective_relays(cfg: &Config, roster: &Roster) -> Vec<String> {
    if roster.fleet.updated_at > 0 { roster.fleet.relays.clone() } else { cfg.bootstrap_relays() }
}

/// The machines this one talks to: every member but itself, or nobody once this machine
/// has been revoked (it left, or another member unlinked it).
fn member_set(roster: &Roster, me: &MachineId) -> HashSet<MachineId> {
    if roster.tombstones.contains_key(me) {
        return HashSet::new();
    }
    roster.members().map(|e| e.id).filter(|id| id != me).collect()
}

/// Words for why a connection ended.
pub fn describe_close(e: &ConnectionError) -> String {
    match e {
        ConnectionError::TimedOut => "no answer for 30 s (timed out)".into(),
        ConnectionError::ApplicationClosed(c) => {
            let why = crate::term::safe_text(&String::from_utf8_lossy(&c.reason), 200, false);
            if why.is_empty() { "closed by the other side".into() } else { format!("closed by the other side: {why}") }
        }
        ConnectionError::LocallyClosed => "closed here".into(),
        ConnectionError::Reset => "reset by the other side".into(),
        other => other.to_string(),
    }
}

pub async fn run(paths: Paths) -> Result<()> {
    let (listener, _guard) = ipc::bind_server(&paths)?;
    crate::paths::private_dir(&paths.state_dir)?;
    let cfg = Config::load(&paths)?;
    let secret = identity::load_or_create(&paths)?;
    let me = MachineId::from_endpoint(&secret.public());
    let mut roster = Roster::load(&paths)?;
    if !roster.entries.contains_key(&me) && !roster.tombstones.contains_key(&me) {
        let name = cfg.name.clone().unwrap_or_else(crate::config::default_name);
        roster.add(me, &name, me, now_ms())?;
        roster.save(&paths)?;
    }
    if roster.tombstones.contains_key(&me) {
        tracing::warn!("this machine was removed from the mesh; run 'jaunt reset' and link it again");
    }
    let relay_strs = effective_relays(&cfg, &roster);
    let relays = net::parse_relays(&relay_strs)?;
    if relays.is_empty() {
        tracing::warn!("no relay configured: peers cannot be reached (set one with 'jaunt relay set URL')");
    }
    let members = Arc::new(Members::default());
    members.set(member_set(&roster, &me));
    let addrs = crate::addrs::AddrBook::load(&paths);
    let opts = |port: Option<u16>, secret: iroh::SecretKey| net::EndpointOptions {
        secret,
        relays: &relays,
        relay_ca: cfg.relay_ca.as_deref(),
        // Off unless asked for: it changes the router's configuration.
        portmapper: cfg.portmap.unwrap_or(false),
        port,
        members: members.clone(),
    };
    // Without a fixed port, bind the one used last time when it is free, so the
    // public address peers remember (and the NAT mapping behind it) stays the same.
    let ep = match (cfg.port, addrs.own_port()) {
        (Some(p), _) => net::build_endpoint(opts(Some(p), secret)).await?,
        (None, Some(last)) => match net::build_endpoint(opts(Some(last), secret.clone())).await {
            Ok(ep) => ep,
            Err(e) => {
                tracing::info!("port {last} (used last time) is not free ({e:#}); taking any");
                net::build_endpoint(opts(None, secret)).await?
            }
        },
        (None, None) => net::build_endpoint(opts(None, secret)).await?,
    };
    let tmux_owners = crate::addrs::load_tmux_owners(&paths);
    let host = crate::awake::start(&paths, &cfg.host);
    let (roster_tx, _) = watch::channel(0u64);
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let d = Arc::new_cyclic(|weak| Daemon {
        weak: weak.clone(),
        events: EventLog::new(paths.events_file()),
        paths,
        me,
        ep,
        members,
        cfg: Mutex::new(cfg),
        roster: Mutex::new(roster),
        peers: Mutex::new(HashMap::new()),
        relays: Mutex::new(relays),
        roster_tx,
        shutdown_tx,
        conn_seq: AtomicU64::new(0),
        started_at: now_ms(),
        sessions_cache: Mutex::new(None),
        children: Arc::new(Children::default()),
        contacts: Default::default(),
        pairing: tokio::sync::Mutex::new(()),
        tried_codes: Default::default(),
        host,
        settle: Settle::new(now_ms()),
        addrs: Mutex::new(addrs),
        tmux_owners: Mutex::new(tmux_owners),
        forwards: Default::default(),
        clip: Default::default(),
        hops: Mutex::new((Vec::new(), None)),
    });
    d.note_own_port();
    d.events.emit(Some(d.me_ref()), EventKind::DaemonStarted { version: proto::VERSION.into() });
    tracing::info!("jaunt daemon {} id {} listening on {:?}", proto::VERSION, d.me, d.ep.bound_sockets());

    tokio::spawn(d.clone().accept_loop());
    tokio::spawn(d.clone().supervisor());
    // A daemon start is a return from absence: settle before serving shells and commands.
    d.begin_round("daemon start");
    tokio::spawn(d.clone().monitor());
    tokio::spawn(ipc::serve(d.clone(), listener));
    crate::forwards::start_kept(&d);
    tokio::spawn(crate::clipsync::watch(d.clone()));
    systemd::notify("READY=1");
    {
        // With or without systemd, a stuck host-mode guard never keeps the lid lock.
        let d2 = d.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                d2.host.failsafe();
            }
        });
    }
    if let Some(every) = systemd::watchdog_interval() {
        // Pet the watchdog only while the host-mode guard is ticking: if it hangs while
        // holding the lid lock, systemd kills the daemon and the lock goes with it.
        let d2 = d.clone();
        tokio::spawn(async move {
            loop {
                if d2.host.alive() {
                    systemd::notify("WATCHDOG=1");
                } else {
                    tracing::error!("the host-mode guard is stuck; letting the watchdog restart the daemon");
                }
                tokio::time::sleep(every).await;
            }
        });
    }

    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM: shutting down"),
        _ = int.recv() => tracing::info!("SIGINT: shutting down"),
        _ = hup.recv() => tracing::info!("SIGHUP: shutting down"),
        _ = shutdown_rx.wait_for(|v| *v) => tracing::info!("shutdown requested"),
    }
    systemd::notify("STOPPING=1");
    // Tell peers now instead of letting them time out.
    let conns: Vec<Connection> = d.peers.lock().map(|p| p.values().filter_map(|p| p.conn.clone()).collect()).unwrap_or_default();
    for c in conns {
        c.close(CLOSE_NORMAL.into(), b"daemon stopping");
    }
    let _ = timeout(Duration::from_secs(3), d.ep.close()).await;
    Ok(())
}

impl Daemon {
    pub fn me_ref(&self) -> PeerRef {
        PeerRef { id: self.me, name: self.my_name() }
    }

    pub fn my_name(&self) -> String {
        self.roster.lock().map(|r| r.display_name(&self.me)).unwrap_or_default()
    }

    pub fn name_of(&self, id: &MachineId) -> String {
        self.roster.lock().map(|r| r.display_name(id)).unwrap_or_else(|_| id.short())
    }

    pub fn peer_ref(&self, id: &MachineId) -> PeerRef {
        PeerRef { id: *id, name: self.name_of(id) }
    }

    pub fn config(&self) -> Config {
        self.cfg.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn weak(&self) -> std::sync::Weak<Daemon> {
        self.weak.clone()
    }

    /// Change config.toml and the daemon's copy together.
    pub fn edit_config(&self, edit: impl FnOnce(&mut Config)) -> Result<()> {
        let mut c = self.cfg.lock().map_err(|_| anyhow!("config lock poisoned"))?;
        edit(&mut c);
        c.save(&self.paths)
    }

    /// Members with a live connection right now.
    pub fn live_peers(&self) -> Vec<MachineId> {
        let g = self.peers.lock();
        g.map(|g| {
            g.iter()
                .filter(|(id, p)| self.is_member(id) && p.conn.as_ref().is_some_and(|c| c.close_reason().is_none()))
                .map(|(id, _)| *id)
                .collect()
        })
        .unwrap_or_default()
    }

    /// The machine hopped in here most recently whose session is still open, else the
    /// last one that hopped in at all (while it is still a member).
    pub fn visitor(&self) -> Result<String> {
        let g = self.hops.lock().map_err(|_| anyhow!("lock poisoned"))?;
        let id = g.0.iter().rev().find(|id| self.is_member(id)).or(g.1.as_ref().filter(|id| self.is_member(id)));
        match id {
            Some(id) => Ok(self.name_of(id)),
            None => bail!("no machine has hopped in here since the daemon started; name one"),
        }
    }

    fn hop_started(&self, id: MachineId) {
        if let Ok(mut g) = self.hops.lock() {
            g.0.push(id);
            g.1 = Some(id);
        }
    }

    fn hop_ended(&self, id: MachineId) {
        if let Ok(mut g) = self.hops.lock()
            && let Some(i) = g.0.iter().rposition(|x| *x == id)
        {
            g.0.remove(i);
        }
    }

    pub fn set_clipboard(&self, on: bool) -> Result<String> {
        if on && crate::desktop::clip_tool_name().is_none() && !cfg!(target_os = "macos") {
            bail!("no clipboard tool on {}: install wl-clipboard (Wayland) or xclip (X11)", self.my_name());
        }
        self.edit_config(|c| c.clipboard = on)?;
        Ok(if on {
            "clipboard sharing on: text copied here goes to linked machines that share theirs, and theirs comes here".into()
        } else {
            "clipboard sharing off".into()
        })
    }

    pub fn is_member(&self, id: &MachineId) -> bool {
        self.members.contains(id)
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub(crate) fn relays(&self) -> Vec<RelayUrl> {
        self.relays.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn with_peer<T>(&self, id: &MachineId, f: impl FnOnce(&mut Peer) -> T) -> Option<T> {
        let mut g = self.peers.lock().ok()?;
        Some(f(g.entry(*id).or_default()))
    }

    fn live_conn(&self, id: &MachineId) -> Option<Connection> {
        self.with_peer(id, |p| p.conn.clone()).flatten().filter(|c| c.close_reason().is_none())
    }

    // ---------------------------------------------------------------- connections

    async fn accept_loop(self: Arc<Self>) {
        while let Some(incoming) = self.ep.accept().await {
            let d = self.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        d.adopt(conn, true);
                    }
                    Err(e) => tracing::debug!("incoming connection failed: {e:#}"),
                }
            });
        }
    }

    /// Make `conn` the current connection to its peer and start its helpers. Returns the
    /// connection to use (the existing one when `conn` loses a simultaneous-dial race).
    fn adopt(self: &Arc<Self>, conn: Connection, incoming: bool) -> Connection {
        let id = MachineId::from_endpoint(&conn.remote_id());
        if !self.is_member(&id) {
            conn.close(CLOSE_NOT_MEMBER.into(), b"not a member of this mesh");
            return conn;
        }
        let seq = self.conn_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let path = net::selected_path(&conn);
        let kind = path.as_ref().map(|p| p.kind).unwrap_or(PathKind::Unknown);
        // When both sides dial at once there are two connections. Both machines keep the
        // one dialled by the smaller id and close the other after a short grace period.
        let new_dialer_is_smaller = if incoming { id < self.me } else { self.me < id };
        let (current, duplicate) = self
            .with_peer(&id, |p| {
                let existing = p.conn.clone().filter(|c| c.close_reason().is_none());
                let fresh = p.since.map(|t| now_ms().saturating_sub(t) < 10_000).unwrap_or(false);
                match existing {
                    Some(old) if fresh && p.incoming != incoming => {
                        let old_dialer_is_smaller = !new_dialer_is_smaller;
                        if old_dialer_is_smaller {
                            return (old, Some(conn.clone()));
                        }
                        Self::set_current(p, &conn, seq, incoming, path.clone());
                        (conn.clone(), Some(old))
                    }
                    // A peer dials only when it has no live connection to us, so a new
                    // connection outside a dial race (an old one of any age in the same
                    // direction, or an old one older than the race window) means the old
                    // one is dead on its side (it restarted, or its end timed out): close
                    // ours now rather than leave it open and unaccounted for.
                    Some(old) => {
                        old.close(CLOSE_NORMAL.into(), b"replaced by a new connection");
                        Self::set_current(p, &conn, seq, incoming, path.clone());
                        (conn.clone(), None)
                    }
                    None => {
                        Self::set_current(p, &conn, seq, incoming, path.clone());
                        (conn.clone(), None)
                    }
                }
            })
            .unwrap_or((conn.clone(), None));
        // Every adopted connection is remembered until it closes, so a revocation reaches all.
        self.with_peer(&id, |p| {
            p.conns.retain(|c| c.close_reason().is_none());
            p.conns.push(conn.clone());
        });
        if let Some(dup) = duplicate {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                dup.close(CLOSE_NORMAL.into(), b"duplicate connection");
            });
        }
        // An outgoing connection reports its handshake once its first sync went through
        // The other side may still refuse us while it learns of this machine.
        if incoming && current.stable_id() == conn.stable_id() {
            self.with_peer(&id, |p| p.announced = Some(conn.stable_id()));
            self.events.emit(Some(self.peer_ref(&id)), EventKind::Handshake { incoming, path: kind });
        }
        tokio::spawn(self.clone().serve_conn(conn.clone(), id));
        tokio::spawn(self.clone().watch_conn(conn.clone(), id, seq));
        tokio::spawn(self.clone().on_closed(conn.clone(), id, seq));
        if !incoming {
            let d = self.clone();
            tokio::spawn(async move { d.sync_with(conn, id).await });
        }
        current
    }

    fn set_current(p: &mut Peer, conn: &Connection, seq: u64, incoming: bool, path: Option<PathInfo>) {
        p.conn = Some(conn.clone());
        p.conn_seq = seq;
        p.incoming = incoming;
        p.since = Some(now_ms());
        p.last_seen = Some(now_ms());
        p.path = path;
        p.last_error = None;
        p.rx_seen = (0, now_ms());
    }

    async fn on_closed(self: Arc<Self>, conn: Connection, id: MachineId, seq: u64) {
        let err = conn.closed().await;
        let reason = describe_close(&err);
        // The peer closed a connection we dialled, as not a member, before any sync: it
        // does not know this machine yet (a link being formed) or it revoked us.
        let refused = matches!(&err, ConnectionError::ApplicationClosed(c) if u64::from(c.error_code) == u64::from(CLOSE_NOT_MEMBER));
        let (was_current, refusals) = self
            .with_peer(&id, |p| {
                p.conns.retain(|c| c.stable_id() != conn.stable_id() && c.close_reason().is_none());
                if p.conn_seq == seq {
                    p.conn = None;
                    p.path = None;
                    p.since = None;
                    if refused {
                        p.refusals += 1;
                    }
                    (true, p.refusals)
                } else {
                    (false, 0)
                }
            })
            .unwrap_or((false, 0));
        if !was_current {
            return;
        }
        let delay = if refused {
            // One quiet line per streak instead of a handshake and a "link down" each time.
            let text = format!("{} refuses this machine as not a member there (it may not know of it yet)", self.name_of(&id));
            let first = self
                .with_peer(&id, |p| {
                    let new = p.last_error.as_deref() != Some(text.as_str());
                    p.last_error = Some(text.clone());
                    new
                })
                .unwrap_or(false);
            if first {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::DialFailed { error: text });
            }
            (refusals <= REFUSED_FAST_TRIES).then_some(REFUSED_RETRY)
        } else {
            self.events.emit(Some(self.peer_ref(&id)), EventKind::Disconnected { reason });
            Some(RECONNECT_AFTER_LOSS)
        };
        if let Some(delay) = delay {
            let kick = self.with_peer(&id, |p| p.kick.clone());
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Some(k) = kick {
                    k.notify_one();
                }
            });
        }
    }

    /// Path changes, one RTT sample per second, and a Ping every few seconds.
    async fn watch_conn(self: Arc<Self>, conn: Connection, id: MachineId, seq: u64) {
        use n0_future::StreamExt;
        let mut path_events = conn.path_events();
        let mut events_open = true;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut ping = tokio::time::interval(PING_EVERY);
        loop {
            tokio::select! {
                _ = conn.closed() => break,
                ev = path_events.next(), if events_open => {
                    if ev.is_none() { events_open = false; }
                    self.refresh_path(&conn, &id, seq);
                }
                _ = tick.tick() => {
                    let rx = conn.stats().udp_rx.datagrams;
                    self.with_peer(&id, |p| {
                        if p.conn_seq == seq && p.rx_seen.0 != rx {
                            p.rx_seen = (rx, now_ms());
                        }
                    });
                    self.refresh_path(&conn, &id, seq);
                    self.sample_rtt(&conn, &id, seq);
                    self.contacts.note_conn(&conn, &self.name_of(&id));
                }
                _ = ping.tick() => {
                    let d = self.clone();
                    let c = conn.clone();
                    tokio::spawn(async move { d.ping(c, id).await });
                }
            }
        }
    }

    fn refresh_path(&self, conn: &Connection, id: &MachineId, seq: u64) {
        let now = net::selected_path(conn);
        // A working direct path's address is worth remembering for the next dial.
        if let Some(p) = &now
            && p.kind == PathKind::Direct
            && let Ok(sa) = p.remote.parse::<std::net::SocketAddr>()
        {
            let changed = self.addrs.lock().map(|mut b| b.note(*id, sa)).unwrap_or(false);
            if changed {
                self.save_addrs();
            }
        }
        let changed = self
            .with_peer(id, |p| {
                if p.conn_seq != seq {
                    return None;
                }
                let before = p.path.clone();
                let differs = match (&before, &now) {
                    (Some(a), Some(b)) => a.kind != b.kind || a.remote != b.remote,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if now.is_some() {
                    p.path = now.clone();
                }
                differs.then_some((before, now.clone()))
            })
            .flatten();
        if let Some((before, Some(after))) = changed {
            let from = before.map(|b| b.kind).unwrap_or(PathKind::Unknown);
            self.events.emit(
                Some(self.peer_ref(id)),
                EventKind::PathChanged { from, to: after.kind, remote_addr: Some(after.remote) },
            );
        }
    }

    fn sample_rtt(&self, conn: &Connection, id: &MachineId, seq: u64) {
        let Some(path) = net::selected_path(conn) else { return };
        let sample = RttSample { t_ms: now_ms(), rtt_ms: path.rtt.as_secs_f64() * 1000.0, path: path.kind };
        let current = self
            .with_peer(id, |p| {
                if p.conn_seq != seq {
                    return false;
                }
                if p.history.len() >= RTT_HISTORY {
                    p.history.pop_front();
                }
                p.history.push_back(sample);
                p.last_seen = Some(sample.t_ms);
                true
            })
            .unwrap_or(false);
        if current {
            self.events.rtt(*id, sample);
        }
    }

    async fn ping(&self, conn: Connection, id: MachineId) {
        let res = timeout(Duration::from_secs(5), async {
            let (mut send, mut recv) = open_service(&conn, Service::Ping).await?;
            let payload = proto::expect_frame(&mut recv, FrameType::Json).await?;
            let _ = send.finish();
            Ok::<PingInfo, anyhow::Error>(proto::parse_json(&payload, MAX_HEADER)?)
        })
        .await;
        match res {
            Ok(Ok(info)) => {
                self.note_contact();
                // Shown on this machine's terminal: no control characters, short.
                let info = PingInfo {
                    name: crate::term::safe_text(&info.name, 64, false),
                    version: crate::term::safe_text(&info.version, 32, false),
                    host: info.host.map(|h| crate::term::safe_text(&h, 64, false)),
                    ..info
                };
                self.with_peer(&id, |p| p.info = Some(info));
            }
            Ok(Err(e)) => tracing::debug!("ping {} failed: {e:#}", id.short()),
            Err(_) => tracing::debug!("ping {} timed out", id.short()),
        }
    }

    /// The current live connection to `id`, dialling if there is none.
    pub async fn connect(self: &Arc<Self>, id: MachineId) -> Result<Connection> {
        if let Some(c) = self.live_conn(&id) {
            return Ok(c);
        }
        let lock = self.with_peer(&id, |p| p.dial_lock.clone()).ok_or_else(|| anyhow!("internal lock poisoned"))?;
        let _g = lock.lock().await;
        if let Some(c) = self.live_conn(&id) {
            return Ok(c);
        }
        let relays = self.relays();
        let has_hints = self.addrs.lock().map(|b| !b.hints(&id).is_empty()).unwrap_or(false);
        if relays.is_empty() && !has_hints {
            bail!("no relay is configured, so {} cannot be found (set one with: jaunt relay set URL)", self.name_of(&id));
        }
        let hints = self.addrs.lock().map(|b| b.hints(&id)).unwrap_or_default();
        let addr = net::dial_addr(id, &relays, &hints)?;
        let res = {
            let _dialing = DialFlag::set(self, id);
            timeout(DIAL_TIMEOUT, self.ep.connect(addr, proto::ALPN)).await
        };
        let err = match res {
            Ok(Ok(conn)) => return Ok(self.adopt(conn, false)),
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => format!("no answer within {} s", DIAL_TIMEOUT.as_secs()),
        };
        let first_time = self
            .with_peer(&id, |p| {
                let new = p.last_error.as_deref() != Some(err.as_str());
                p.last_error = Some(err.clone());
                new
            })
            .unwrap_or(false);
        if first_time {
            self.events.emit(Some(self.peer_ref(&id)), EventKind::DialFailed { error: err.clone() });
        }
        bail!("cannot reach {}: {err}", self.name_of(&id))
    }

    /// Spawn one keeper task per member; members that leave stop their task themselves.
    async fn supervisor(self: Arc<Self>) {
        let mut rx = self.roster_tx.subscribe();
        loop {
            let ids: Vec<MachineId> =
                self.roster.lock().map(|r| member_set(&r, &self.me).into_iter().collect()).unwrap_or_default();
            for id in ids {
                let start = self.with_peer(&id, |p| !std::mem::replace(&mut p.task_running, true)).unwrap_or(false);
                if start {
                    tokio::spawn(self.clone().keep_warm(id));
                }
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    async fn keep_warm(self: Arc<Self>, id: MachineId) {
        let mut rx = self.roster_tx.subscribe();
        let Some(kick) = self.with_peer(&id, |p| p.kick.clone()) else { return };
        loop {
            if !self.is_member(&id) {
                // `roster_changed` already closed the connection, except for a peer whose
                // own sync revoked it (`jaunt leave`), which still needs to read its answer.
                if let Ok(mut g) = self.peers.lock()
                    && let Some(p) = g.remove(&id)
                {
                    let conns: Vec<Connection> = p.conn.into_iter().chain(p.conns).collect();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(3)).await;
                        for c in conns {
                            c.close(CLOSE_NOT_MEMBER.into(), b"removed from the mesh");
                        }
                    });
                }
                return;
            }
            if self.live_conn(&id).is_none()
                && let Err(e) = self.connect(id).await
            {
                tracing::debug!("{e:#}");
            }
            tokio::select! {
                _ = tokio::time::sleep(RETRY_EVERY) => {}
                _ = kick.notified() => {}
                r = rx.changed() => if r.is_err() { return },
            }
        }
    }

    // ---------------------------------------------------------------- streams (host side)

    async fn serve_conn(self: Arc<Self>, conn: Connection, id: MachineId) {
        // When this connection was established (it is adopted right after the
        // handshake), so a settling round that started later ran with the network up.
        let established = now_ms();
        while let Ok((send, recv)) = conn.accept_bi().await {
            let d = self.clone();
            let c = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = d.serve_stream(c, id, established, send, recv).await {
                    tracing::debug!("stream from {} ended: {e:#}", id.short());
                }
            });
        }
    }

    async fn serve_stream(
        self: Arc<Self>,
        conn: Connection,
        id: MachineId,
        established: u64,
        mut send: SendStream,
        mut recv: RecvStream,
    ) -> Result<()> {
        let payload = timeout(HEADER_TIMEOUT, proto::expect_frame(&mut recv, FrameType::Header))
            .await
            .context("no request header within 10 s")??;
        let probe: HelloProbe = match proto::parse_json(&payload, MAX_HEADER) {
            Ok(p) => p,
            Err(e) => return self.refuse_logged(&id, &mut send, &format!("malformed request header ({e})")).await,
        };
        if probe.proto != PROTO_VERSION {
            let why = format!(
                "{} speaks jaunt protocol {PROTO_VERSION} (jaunt {}) and the request used protocol {}; update jaunt on the older machine",
                self.my_name(),
                proto::VERSION,
                probe.proto
            );
            return self.refuse_logged(&id, &mut send, &why).await;
        }
        let hello: Hello = match proto::parse_json(&payload, MAX_HEADER) {
            Ok(h) => h,
            Err(_) => {
                let why = format!("{} (jaunt {}) does not know this request", self.my_name(), proto::VERSION);
                return self.refuse_logged(&id, &mut send, &why).await;
            }
        };
        if !self.is_member(&id) {
            return self.refuse_logged(&id, &mut send, "not a member of this mesh").await;
        }
        if !matches!(hello.service, Service::Sync | Service::Ping) {
            // After an absence, nothing but Sync and Ping until this machine has
            // synced with a member other than `id`, or a round that started after `id`'s
            // connection was established reached none; then ask again whether `id` is one.
            if !self.wait_settled(id, established).await {
                tracing::info!("serving {} although settling did not finish in time", id.short());
            }
            if !self.is_member(&id) {
                return self.refuse_logged(&id, &mut send, "not a member of this mesh").await;
            }
        }
        let served = self.clone().serve_service(conn, id, hello.service, send, recv);
        tokio::pin!(served);
        // Whatever a peer runs here ends when it is revoked, streams included.
        tokio::select! {
            r = &mut served => r,
            _ = self.until_revoked(id) => {
                // Not dropped at once: the serving future holds the process's registration
                // (`children::Ticket`), and `kill_peer` sends its SIGKILL a second after the
                // SIGHUP only to processes still registered. Dropping it here would let a
                // command that ignores SIGHUP outlive the revocation.
                let _ = timeout(REVOKED_GRACE, &mut served).await;
                tracing::info!("ended a stream of {}: it was unlinked", id.short());
                Ok(())
            }
        }
    }

    /// Resolves once `id` is no longer a member.
    async fn until_revoked(&self, id: MachineId) {
        let mut rx = self.roster_tx.subscribe();
        while self.is_member(&id) {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn serve_service(self: Arc<Self>, conn: Connection, id: MachineId, service: Service, mut send: SendStream, recv: RecvStream) -> Result<()> {
        match service {
            Service::Sync => self.serve_sync(conn, id, send, recv).await,
            Service::KillSessions { of } => {
                proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
                let n = self.kill_tmux_sessions_of(&of).await;
                proto::write_json(&mut send, FrameType::Json, &n).await?;
                send.finish()?;
                Ok(())
            }
            Service::Ping => {
                self.note_contact();
                proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
                let info = PingInfo {
                    name: self.my_name(),
                    version: proto::VERSION.into(),
                    sessions: self.session_count().await,
                    host: Some(self.host.report().state.label().to_string()),
                    time_ms: now_ms(),
                };
                proto::write_json(&mut send, FrameType::Json, &info).await?;
                send.finish()?;
                Ok(())
            }
            Service::Shell { session, term, cols, rows, env } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "shell".into() });
                self.note_tmux_creator(&session, id).await;
                let req = shell::ShellRequest { session, term, cols, rows, env };
                self.hop_started(id);
                let r = shell::serve(&self, id, req, send, recv).await;
                self.hop_ended(id);
                r
            }
            Service::Tabs => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "tabs".into() });
                let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
                let tabs = tokio::task::spawn_blocking(move || services::tabs::read_all(&home)).await.unwrap_or_default();
                reply_json(&mut send, &tabs).await
            }
            Service::Ports => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "ports".into() });
                let ports = services::ports::list().await;
                reply_json(&mut send, &ports).await
            }
            Service::OpenUrl { url } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "open url".into() });
                match crate::desktop::open_url(&url).await {
                    Ok(()) => reply_json(&mut send, &()).await,
                    Err(e) => self.refuse_logged(&id, &mut send, &format!("{e:#}")).await,
                }
            }
            Service::Notify { title, body } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "notify".into() });
                let title = format!("{} · {}", self.name_of(&id), crate::desktop::clean_line(&title, 100));
                match crate::desktop::notify(&title, &body).await {
                    Ok(()) => reply_json(&mut send, &()).await,
                    Err(e) => refuse(&mut send, &format!("{e:#}")).await,
                }
            }
            Service::Clip { op } => {
                let what = if op == proto::ClipOp::Get { "clipboard read" } else { "clipboard write" };
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: what.into() });
                if !self.config().clipboard {
                    return refuse(&mut send, &format!("{} does not share its clipboard (jaunt clip on, there)", self.my_name())).await;
                }
                match op {
                    proto::ClipOp::Get => match crate::desktop::clip_get().await {
                        Ok(t) => reply_json(&mut send, &t.unwrap_or_default()).await,
                        Err(e) => refuse(&mut send, &format!("{e:#}")).await,
                    },
                    proto::ClipOp::Set => {
                        proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
                        let mut recv = recv;
                        let payload = timeout(HEADER_TIMEOUT, proto::expect_frame(&mut recv, FrameType::Json)).await??;
                        let text: String = proto::parse_json(&payload, MAX_FRAME)?;
                        self.clip.note(&text);
                        let done = match crate::desktop::clip_set(&text).await {
                            Ok(()) => Reply::Ok,
                            Err(e) => Reply::Refused { reason: format!("{e:#}") },
                        };
                        reply_json_only(&mut send, &done).await
                    }
                }
            }
            Service::Dial { host, port } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: format!("socks {host}:{port}") });
                services::tcp::serve_dial(&self, host, port, send, recv).await
            }
            Service::Listen { port, to } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: format!("reverse forward {port}") });
                services::tcp::serve_listen(&self, id, port, to, send, recv).await
            }
            Service::Exec { argv } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "exec".into() });
                services::serve_exec(&self, id, argv, send, recv).await
            }
            Service::Tcp { port } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: format!("tcp {port}") });
                services::tcp::serve(&self, id, port, send, recv).await
            }
            Service::Copy { op, path } => {
                let what = match op { proto::CopyOp::Push => "copy in", proto::CopyOp::Pull => "copy out" };
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: what.into() });
                services::copy::serve(&self, id, op, path, send, recv).await
            }
        }
    }

    async fn refuse_logged(&self, id: &MachineId, send: &mut SendStream, reason: &str) -> Result<()> {
        self.events.emit(Some(self.peer_ref(id)), EventKind::Refused { reason: reason.to_string() });
        refuse(send, reason).await
    }

    pub async fn session_count(&self) -> Option<u32> {
        if let Ok(g) = self.sessions_cache.lock()
            && let Some((at, n)) = *g
            && at.elapsed() < Duration::from_secs(5)
        {
            return n;
        }
        let n = shell::count_sessions(self.config().tmux_socket()).await;
        if let Ok(mut g) = self.sessions_cache.lock() {
            *g = Some((Instant::now(), n));
        }
        n
    }

    // ---------------------------------------------------------------- roster sync

    async fn serve_sync(&self, conn: Connection, id: MachineId, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
        let payload = timeout(HEADER_TIMEOUT, proto::expect_frame(&mut recv, FrameType::Json)).await??;
        let theirs: Roster = proto::parse_json(&payload, MAX_FRAME)?;
        let changed = match self.apply_remote_roster(&theirs, &id) {
            Ok(c) => c,
            Err(e) => {
                // Said in words, so `unlink` reports a rejected merge as such.
                let why = format!("{} could not take your roster: {e:#}", self.my_name());
                return self.refuse_logged(&id, &mut send, &why).await;
            }
        };
        self.note_synced(id);
        proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
        let ours = self.roster_snapshot();
        proto::write_json(&mut send, FrameType::Json, &ours).await?;
        send.finish()?;
        self.events.emit(Some(self.peer_ref(&id)), EventKind::Sync { changed, outgoing: false });
        if !self.is_member(&id) {
            // The sender's roster revoked the sender itself (`jaunt leave`): it was spared
            // the immediate close so it could read this answer as its acknowledgement.
            let _ = timeout(Duration::from_secs(2), send.stopped()).await;
            conn.close(CLOSE_NOT_MEMBER.into(), b"removed from the mesh");
        }
        Ok(())
    }

    /// One roster exchange on `conn`: send `ours`, return the peer's merged roster.
    async fn exchange_roster(conn: &Connection, ours: &Roster) -> Result<Roster> {
        let (mut send, mut recv) = conn.open_bi().await?;
        let hello = Hello { proto: PROTO_VERSION, service: Service::Sync };
        proto::write_json(&mut send, FrameType::Header, &hello).await?;
        proto::write_json(&mut send, FrameType::Json, ours).await?;
        let reply: Reply = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Reply).await?, MAX_HEADER)?;
        if let Reply::Refused { reason } = reply {
            bail!("sync refused: {}", crate::term::safe_text(&reason, 300, false));
        }
        let theirs: Roster = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Json).await?, MAX_FRAME)?;
        let _ = send.finish();
        Ok(theirs)
    }

    /// Sync with `id` now (dialling if needed) and return its roster after the merge: the
    /// acknowledgement used by link fan-out, unlink and leave. With `push`, that roster is
    /// sent instead of ours and the answer is not merged here (leave).
    pub async fn sync_once(self: &Arc<Self>, id: MachineId, push: Option<&Roster>, limit: Duration) -> Result<Roster> {
        let res = timeout(limit, async {
            let conn = self.connect(id).await?;
            let ours = match push {
                Some(r) => r.clone(),
                None => self.roster_snapshot(),
            };
            let theirs = Self::exchange_roster(&conn, &ours).await?;
            self.note_contact();
            if push.is_none() {
                let changed = self.apply_remote_roster(&theirs, &id)?;
                self.note_synced(id);
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Sync { changed, outgoing: true });
            }
            Ok(theirs)
        })
        .await;
        match res {
            Ok(r) => r,
            Err(_) => bail!("no answer within {} s", limit.as_secs()),
        }
    }

    async fn sync_with(self: Arc<Self>, conn: Connection, id: MachineId) {
        let res = timeout(Duration::from_secs(15), async {
            let theirs = Self::exchange_roster(&conn, &self.roster_snapshot()).await?;
            self.note_contact();
            let changed = self.apply_remote_roster(&theirs, &id)?;
            self.note_synced(id);
            Ok::<bool, anyhow::Error>(changed)
        })
        .await;
        match res {
            Ok(Ok(changed)) => {
                // The first sync on a connection we dialled is when the link is really up.
                let first = self
                    .with_peer(&id, |p| {
                        p.refusals = 0;
                        p.last_error = None;
                        let current = p.conn.as_ref().is_some_and(|c| c.stable_id() == conn.stable_id());
                        let first = current && p.announced != Some(conn.stable_id());
                        if first {
                            p.announced = Some(conn.stable_id());
                        }
                        first
                    })
                    .unwrap_or(false);
                if first {
                    let kind = net::selected_path(&conn).map(|p| p.kind).unwrap_or(PathKind::Unknown);
                    self.events.emit(Some(self.peer_ref(&id)), EventKind::Handshake { incoming: false, path: kind });
                }
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Sync { changed, outgoing: true })
            }
            Ok(Err(e)) => tracing::info!("sync with {} failed: {e:#}", id.short()),
            Err(_) => tracing::info!("sync with {} timed out", id.short()),
        }
    }

    pub fn roster_snapshot(&self) -> Roster {
        self.roster.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn apply_remote_roster(&self, theirs: &Roster, from: &MachineId) -> Result<bool> {
        let mut theirs = theirs.clone();
        // Item by item: a bad entry or a full roster never costs the tombstones.
        for what in theirs.sanitize_remote().into_iter().chain(theirs.drop_malformed()) {
            tracing::warn!("ignored {what} in the roster from {}", from.short());
        }
        let (changed, before) = {
            let mut r = self.roster.lock().map_err(|_| anyhow!("roster lock poisoned"))?;
            let before = r.clone();
            let mut merged = r.clone();
            let (changed, skipped) = merged.merge_bounded(&theirs);
            if skipped > 0 {
                tracing::warn!("the roster from {} would pass the limits; {skipped} new item(s) not taken", from.short());
            }
            if !changed {
                (false, before)
            } else {
                merged.validate().context("merging would exceed the roster limits")?;
                merged.save(&self.paths)?;
                *r = merged;
                // Membership changes under the roster lock, so no stream slips in between.
                self.members.set(member_set(&r, &self.me));
                (true, before)
            }
        };
        if changed {
            self.roster_changed(format!("synced from {}", self.name_of(from)), &before, Some(*from));
        }
        Ok(changed)
    }

    /// Apply a local roster edit, persist it and spread it.
    pub(crate) fn edit_roster(&self, what: String, f: impl FnOnce(&mut Roster) -> Result<bool>) -> Result<bool> {
        let (changed, before) = {
            let mut r = self.roster.lock().map_err(|_| anyhow!("roster lock poisoned"))?;
            let before = r.clone();
            let mut next = r.clone();
            if !f(&mut next)? {
                (false, before)
            } else {
                next.validate()?;
                next.save(&self.paths)?;
                *r = next;
                self.members.set(member_set(&r, &self.me));
                (true, before)
            }
        };
        if changed {
            self.roster_changed(what, &before, None);
        }
        Ok(changed)
    }

    /// Everything that follows a roster change: the handshake allowlist, relays, ending
    /// what revoked machines have here, events, and pushing the change on. `spare` is a
    /// peer whose sync is being answered right now; its connection is closed after the
    /// answer instead (see `serve_sync`).
    fn roster_changed(&self, what: String, before: &Roster, spare: Option<MachineId>) {
        let after = self.roster_snapshot();
        let members = member_set(&after, &self.me);
        // Membership first: from here on the handshake hook and every stream open refuse
        // the revoked ids.
        self.members.set(members.clone());
        if after.fleet.updated_at > 0 {
            self.apply_relays(after.fleet.relays.clone());
        }
        // End what non-members are running here, and close their connections.
        let mut killed: HashMap<MachineId, u32> = HashMap::new();
        for peer in self.children.peers() {
            if !members.contains(&peer) {
                killed.insert(peer, self.children.kill_peer(&peer) as u32);
            }
        }
        if let Ok(g) = self.peers.lock() {
            for (id, p) in g.iter() {
                if !members.contains(id) && Some(*id) != spare {
                    // Every connection, not only the current one.
                    for c in p.conn.iter().chain(p.conns.iter()) {
                        c.close(CLOSE_NOT_MEMBER.into(), b"removed from the mesh");
                    }
                }
            }
        }
        for id in after.new_tombstones(before) {
            let Some(t) = after.tombstones.get(&id) else { continue };
            let name = after.entries.get(&id).map(|e| e.name.clone()).unwrap_or_else(|| id.short());
            let by = PeerRef { id: t.revoked_by, name: after.entries.get(&t.revoked_by).map(|e| e.name.clone()).unwrap_or_else(|| t.revoked_by.short()) };
            let added: Vec<PeerRef> = after
                .added_by_closure(&id)
                .into_iter()
                .filter(|x| *x != self.me)
                .map(|x| PeerRef { id: x, name: after.display_name(&x) })
                .collect();
            if id == self.me {
                tracing::warn!("this machine was unlinked by {}; run 'jaunt reset' and link it again to rejoin", by.name);
            } else if !added.is_empty() {
                let names: Vec<&str> = added.iter().map(|p| p.name.as_str()).collect();
                tracing::warn!("{name} was unlinked; it had added {} (still linked; 'jaunt unlink {name} --cascade' revokes them too)", names.join(", "));
            }
            let sessions_killed = killed.get(&id).copied().unwrap_or(0);
            self.events.emit(Some(PeerRef { id, name }), EventKind::Revoked { by, sessions_killed, added });
        }
        self.events.emit(None, EventKind::RosterChanged { what });
        self.roster_tx.send_modify(|v| *v += 1);
        self.push_sync_all();
    }

    fn push_sync_all(&self) {
        let conns: Vec<(MachineId, Connection)> = self
            .peers
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|(id, _)| self.is_member(id))
                    .filter_map(|(id, p)| p.conn.clone().map(|c| (*id, c)))
                    .collect()
            })
            .unwrap_or_default();
        let Some(d) = self.weak.upgrade() else { return };
        for (id, conn) in conns {
            tokio::spawn(d.clone().sync_with(conn, id));
        }
    }

    fn apply_relays(&self, list: Vec<String>) {
        let Ok(urls) = net::parse_relays(&list) else { return };
        let old = self.relays();
        if old == urls {
            return;
        }
        if let Ok(mut g) = self.relays.lock() {
            *g = urls.clone();
        }
        let ep = self.ep.clone();
        tokio::spawn(async move {
            for u in &urls {
                if !old.contains(u) {
                    ep.insert_relay(u.clone(), Arc::new(u.clone().into())).await;
                }
            }
            for u in &old {
                if !urls.contains(u) {
                    ep.remove_relay(u).await;
                }
            }
        });
    }

    /// Milliseconds since anything arrived from `id` on its live connection.
    pub fn quiet_ms(&self, id: &MachineId) -> Option<u64> {
        let conn = self.live_conn(id)?;
        let rx = conn.stats().udp_rx.datagrams;
        let now = now_ms();
        self.with_peer(id, |p| {
            if p.rx_seen.0 != rx {
                p.rx_seen = (rx, now);
            }
            now.saturating_sub(p.rx_seen.1)
        })
    }

    // ---------------------------------------------------------------- settling

    fn note_contact(&self) {
        self.settle.last_contact.store(now_ms(), Ordering::Relaxed);
    }

    fn others(&self) -> HashSet<MachineId> {
        self.roster.lock().map(|r| member_set(&r, &self.me)).unwrap_or_default()
    }

    /// A Sync with `id` completed (either direction) and its roster was merged here.
    fn note_synced(&self, id: MachineId) {
        let now = now_ms();
        let was = self.settle.is_settled();
        self.settle.synced(id, &self.others(), now);
        if !was && self.settle.is_settled() {
            tracing::info!("settled: synced with a member since the absence");
        }
    }

    /// One settling round: sync with every other member at once, at most `SETTLE_LIMIT`,
    /// dialling through the relay and the remembered direct addresses. Each completed Sync
    /// counts (`note_synced`); a round that reached nobody settles nothing. Returns a
    /// receiver that turns true when the round is over. Rounds may overlap.
    fn begin_round(self: &Arc<Self>, why: &str) -> watch::Receiver<bool> {
        let (done_tx, done_rx) = watch::channel(false);
        let start = now_ms();
        if let Ok(mut g) = self.settle.current.lock() {
            *g = Some((start, done_rx.clone()));
        }
        self.settle.last_round_start.store(start, Ordering::Relaxed);
        let others: Vec<MachineId> = self.others().into_iter().collect();
        tracing::info!("settling ({why}): syncing with {} other member(s)", others.len());
        let d = self.clone();
        tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + SETTLE_LIMIT;
            let mut set = tokio::task::JoinSet::new();
            for id in others {
                let d2 = d.clone();
                set.spawn(async move {
                    // A connection from before the absence can be dead without knowing it
                    // yet: give it 2.5 s, then close it so that the retry dials afresh.
                    for _ in 0..3 {
                        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        let old = d2.live_conn(&id);
                        let limit = if old.is_some() { left.min(Duration::from_millis(2500)) } else { left };
                        match d2.sync_once(id, None, limit).await {
                            Ok(_) => return true,
                            Err(e) => {
                                tracing::debug!("settling: sync with {} failed: {e:#}", id.short());
                                if let Some(c) = old {
                                    c.close(CLOSE_NORMAL.into(), b"no answer");
                                }
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                        }
                    }
                    false
                });
            }
            let (mut ok, mut missed) = (0, 0);
            while let Ok(Some(r)) = tokio::time::timeout_at(deadline + Duration::from_millis(200), set.join_next()).await {
                if matches!(r, Ok(true)) { ok += 1 } else { missed += 1 }
            }
            set.abort_all();
            let now = now_ms();
            let others = d.others();
            if let Ok(mut s) = d.settle.state.lock() {
                d.settle.settle_if_done(&mut s, &others, now);
            }
            let settled = d.settle.is_settled();
            tracing::info!(
                "settling round done: synced with {ok}, not reached {missed}; {}",
                if settled { "settled" } else { "still unsettled (shells and commands wait for a round each)" }
            );
            if let Ok(mut g) = d.settle.current.lock()
                && g.as_ref().is_some_and(|(s, _)| *s == start)
            {
                *g = None;
            }
            d.settle.round_done(start);
            let _ = done_tx.send(true);
        });
        done_rx
    }

    /// For a Shell, Exec, Tcp, Copy or KillSessions request from `peer` on a connection
    /// established at `established`: while unsettled, wait (at most once per absence) for a
    /// settling round that started at or after that moment and after the absence began,
    /// joining a running one or starting one. A round that synced with a member other than
    /// `peer` settles this machine; one that completed without reaching any settles it
    /// provisionally (until the next absence). False only if no round finished in time.
    async fn wait_settled(self: &Arc<Self>, peer: MachineId, established: u64) -> bool {
        if let Some(why) = self.settle.notice(now_ms()) {
            self.begin_round(&why);
        }
        if self.settle.serves(&peer, &self.others()) || self.provisional(peer, established) {
            return true;
        }
        let Some(since) = self.settle.unsettled_since() else { return true };
        let after = since.max(established);
        let running = self
            .settle
            .current
            .lock()
            .ok()
            .and_then(|g| g.as_ref().filter(|(start, _)| *start >= after).map(|(_, rx)| rx.clone()));
        let mut rx = match running {
            Some(rx) => rx,
            None => self.begin_round(&format!("a request from {} while unsettled", self.name_of(&peer))),
        };
        let _ = timeout(SETTLE_LIMIT + Duration::from_secs(1), rx.wait_for(|done| *done)).await;
        self.settle.serves(&peer, &self.others()) || self.provisional(peer, established)
    }

    fn provisional(&self, peer: MachineId, established: u64) -> bool {
        let ok = self.settle.settle_provisionally(established, now_ms());
        if ok {
            tracing::warn!(
                "settled provisionally: a settling round since {}'s connection reached no other member; serving until the next absence",
                self.name_of(&peer)
            );
        }
        ok
    }

    /// Once a second: notice a freeze or suspend (the tick came late) or a long silence,
    /// and while unsettled run a round now and then. Once a minute: keep the detached
    /// daemon's log bounded.
    async fn monitor(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            n += 1;
            if n.is_multiple_of(60) {
                rotate_own_log(&self.paths.daemon_log());
            }
            let now = now_ms();
            if let Some(why) = self.settle.notice(now) {
                self.begin_round(&why);
            } else if !self.settle.is_settled()
                && self.settle.current.lock().map(|g| g.is_none()).unwrap_or(false)
                && now.saturating_sub(self.settle.last_round_start.load(Ordering::Relaxed)) > ROUND_BACKGROUND_MS
            {
                self.begin_round("still unsettled");
            }
        }
    }

    // ---------------------------------------------------------------- address hints

    fn save_addrs(&self) {
        let members = self.members.snapshot();
        if let Ok(mut b) = self.addrs.lock() {
            b.retain_peers(|id| members.contains(id));
            if let Err(e) = b.save(&self.paths) {
                tracing::debug!("cannot save address hints: {e:#}");
            }
        }
    }

    fn note_own_port(&self) {
        let port = self.ep.bound_sockets().iter().find(|s| s.is_ipv4()).map(|s| s.port());
        if let Some(p) = port
            && self.addrs.lock().map(|mut b| b.set_own_port(p)).unwrap_or(false)
        {
            self.save_addrs();
        }
    }

    // ---------------------------------------------------------------- tmux sessions

    /// Remember `from` as the creator of `session` if it does not exist yet.
    async fn note_tmux_creator(&self, session: &str, from: MachineId) {
        let Some(tmux) = shell::which("tmux") else { return };
        if shell::tmux_has_session(&tmux, self.config().tmux_socket().as_deref(), session).await {
            return;
        }
        let snapshot = match self.tmux_owners.lock() {
            Ok(mut g) => {
                g.insert(session.to_string(), from);
                g.clone()
            }
            Err(_) => return,
        };
        if let Err(e) = crate::addrs::save_tmux_owners(&self.paths, &snapshot) {
            tracing::debug!("cannot save tmux session owners: {e:#}");
        }
    }

    /// Kill the tmux sessions that revoked machines among `of` created here. Machines that
    /// are still members are never touched. Returns how many sessions were ended.
    pub async fn kill_tmux_sessions_of(&self, of: &[MachineId]) -> u32 {
        let roster = self.roster_snapshot();
        let doomed: Vec<String> = match self.tmux_owners.lock() {
            Ok(g) => g
                .iter()
                .filter(|(_, owner)| of.contains(owner) && roster.tombstones.contains_key(owner))
                .map(|(name, _)| name.clone())
                .collect(),
            Err(_) => return 0,
        };
        let Some(tmux) = shell::which("tmux") else { return 0 };
        let socket = self.config().tmux_socket();
        let mut n = 0;
        for name in &doomed {
            if shell::tmux_kill_session(&tmux, socket.as_deref(), name).await {
                n += 1;
            }
        }
        let snapshot = match self.tmux_owners.lock() {
            Ok(mut g) => {
                for name in &doomed {
                    g.remove(name);
                }
                g.clone()
            }
            Err(_) => return n,
        };
        let _ = crate::addrs::save_tmux_owners(&self.paths, &snapshot);
        n
    }

    // ---------------------------------------------------------------- IPC operations

    pub fn snapshot(&self) -> StatusSnapshot {
        let roster = self.roster_snapshot();
        let names = roster.display_names();
        let peers_g = self.peers.lock();
        let mut peers = Vec::new();
        for e in roster.members().filter(|e| e.id != self.me) {
            let name = names.get(&e.id).cloned().unwrap_or_else(|| e.name.clone());
            let p = peers_g.as_ref().ok().and_then(|g| g.get(&e.id));
            let live = p.and_then(|p| p.conn.as_ref()).map(|c| c.close_reason().is_none()).unwrap_or(false);
            let path = p.and_then(|p| p.path.clone());
            let state = match (live, path.as_ref().map(|p| p.kind), p.map(|p| p.dialing).unwrap_or(false)) {
                (true, Some(PathKind::Direct), _) => PeerState::Direct,
                (true, _, _) => PeerState::Relay,
                (false, _, true) => PeerState::Connecting,
                _ => PeerState::Offline,
            };
            peers.push(PeerStatus {
                id: e.id,
                name,
                state,
                remote_addr: if live { path.as_ref().map(|p| p.remote.clone()) } else { None },
                addr_kind: if live { path.as_ref().map(|p| p.addr_kind.to_string()) } else { None },
                rtt_ms: if live { path.as_ref().map(|p| p.rtt.as_secs_f64() * 1000.0) } else { None },
                rtt_history: p.map(|p| p.history.iter().copied().collect()).unwrap_or_default(),
                connected_since_ms: if live { p.and_then(|p| p.since) } else { None },
                last_seen_ms: p.and_then(|p| p.last_seen),
                version: p.and_then(|p| p.info.as_ref().map(|i| i.version.clone())),
                sessions: p.and_then(|p| p.info.as_ref().and_then(|i| i.sessions)),
                host: p.and_then(|p| p.info.as_ref().and_then(|i| i.host.clone())),
                last_error: p.and_then(|p| p.last_error.clone()),
            });
        }
        drop(peers_g);
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        let revoked = roster
            .tombstones
            .values()
            .map(|t| RevokedStatus {
                id: t.id,
                name: roster.entries.get(&t.id).map(|e| e.name.clone()),
                revoked_at_ms: t.revoked_at,
            })
            .collect();
        StatusSnapshot {
            me: PeerRef { id: self.me, name: names.get(&self.me).cloned().unwrap_or_else(|| self.my_name()) },
            version: proto::VERSION.into(),
            daemon_pid: std::process::id(),
            started_at_ms: self.started_at,
            relays: self.relays().iter().map(|u| u.to_string()).collect(),
            relay_connected: net::relay_connected(&self.ep),
            bound: self.ep.bound_sockets().iter().map(|s| s.to_string()).collect(),
            peers,
            revoked,
            host: Some(self.host.report()),
            forwards: self.forwards.list(),
        }
    }

    pub fn resolve(&self, target: &str) -> Result<MachineId> {
        let r = self.roster_snapshot();
        match r.resolve(target, Some(&self.me)) {
            Resolve::Found(id) => Ok(id),
            Resolve::NotFound => {
                if r.resolve(target, None) == Resolve::Found(self.me) {
                    bail!("{target} is this machine (to take it out of the mesh: jaunt leave)");
                }
                let names: Vec<String> =
                    r.display_names().into_iter().filter(|(id, _)| *id != self.me).map(|(_, n)| n).collect();
                if names.is_empty() {
                    bail!("no machine called '{target}': this machine is not linked to any other yet");
                }
                bail!("no machine called '{target}' (known: {})", names.join(", "))
            }
            Resolve::Ambiguous(names) => bail!("'{target}' could mean {}; type more of the name", names.join(" or ")),
        }
    }

    /// The machine `unlink` acts on. Members first; with `revoked_too` (`--cascade` or
    /// `--kill-sessions`), a machine that is already revoked is found too (by name, prefix
    /// or id prefix), so what it added can still be revoked and its sessions ended.
    pub fn resolve_unlink(&self, target: &str, revoked_too: bool) -> Result<MachineId> {
        let r = self.roster_snapshot();
        if r.resolve(target, Some(&self.me)) != Resolve::NotFound || r.resolve(target, None) == Resolve::Found(self.me) {
            return self.resolve(target);
        }
        match r.resolve_revoked(target, Some(&self.me)) {
            Resolve::Found(id) if revoked_too => Ok(id),
            Resolve::Found(id) => bail!(
                "{} is already unlinked; to revoke the machines it added: jaunt unlink {target} --cascade (--kill-sessions ends the tmux sessions it created)",
                r.revoked_label(&id)
            ),
            Resolve::Ambiguous(labels) => {
                bail!("'{target}' could mean the unlinked machines {}; give the start of the id instead", labels.join(" or "))
            }
            Resolve::NotFound => self.resolve(target),
        }
    }

    /// Open a service stream on a peer and wait for its answer.
    pub async fn open_stream(
        self: &Arc<Self>,
        target: &str,
        service: Service,
    ) -> Result<(PeerRef, PathKind, Option<f64>, SendStream, RecvStream)> {
        let id = self.resolve(target)?;
        let peer = self.peer_ref(&id);
        let conn = self.connect(id).await?;
        let (mut send, mut recv) = open_service_nowait(&conn, service).await?;
        let reply = match timeout(REPLY_TIMEOUT, proto::expect_frame(&mut recv, FrameType::Reply)).await {
            Ok(r) => r.with_context(|| format!("{} did not answer properly", peer.name))?,
            Err(_) => {
                // A connection that cannot answer within 10 s is presumed dead; drop it so
                // the next attempt dials afresh instead of waiting for the idle timeout.
                conn.close(CLOSE_NORMAL.into(), b"no answer");
                let _ = send.reset(0u32.into());
                bail!("{} did not answer within {} s", peer.name, REPLY_TIMEOUT.as_secs());
            }
        };
        match proto::parse_json::<Reply>(&reply, MAX_HEADER)? {
            Reply::Ok => {}
            Reply::Refused { reason } => bail!("{} refused: {}", peer.name, crate::term::safe_text(&reason, 300, false)),
        }
        let path = net::selected_path(&conn);
        Ok((
            peer,
            path.as_ref().map(|p| p.kind).unwrap_or(PathKind::Unknown),
            path.map(|p| p.rtt.as_secs_f64() * 1000.0),
            send,
            recv,
        ))
    }

    /// Ask member `id` to end the tmux sessions the (revoked) machines `of` created there.
    pub async fn remote_kill_sessions(self: &Arc<Self>, id: MachineId, of: Vec<MachineId>) -> Result<u32> {
        timeout(Duration::from_secs(10), async {
            let conn = self.connect(id).await?;
            let (mut send, mut recv) = open_service(&conn, Service::KillSessions { of }).await?;
            let n: u32 = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Json).await?, MAX_HEADER)?;
            let _ = send.finish();
            Ok(n)
        })
        .await
        .map_err(|_| anyhow!("no answer within 10 s"))?
    }

    pub fn add_peer(&self, id: MachineId, name: &str) -> Result<String> {
        // Outside test mode the pairing ritual is the only way into the mesh.
        if self.paths.jaunt_home.is_none() {
            bail!("adding a machine without the pairing ritual works only in test mode (JAUNT_HOME set); use jaunt link / jaunt join");
        }
        if id == self.me {
            bail!("that is this machine's own id");
        }
        id.to_endpoint()?;
        let me = self.me;
        let changed = self.edit_roster(format!("added {name}"), |r| r.add(id, name, me, now_ms()))?;
        Ok(if changed { format!("added {name} ({})", id.short()) } else { format!("{name} was already there") })
    }

    pub fn rename(&self, name: &str) -> Result<String> {
        let me = self.me;
        self.edit_roster(format!("this machine is now called {name}"), |r| r.rename(me, name, now_ms()))?;
        if let Ok(mut c) = self.cfg.lock() {
            c.name = Some(name.to_string());
            c.save(&self.paths)?;
        }
        Ok(format!("this machine is now called {name}"))
    }

    pub fn set_host(&self, on: bool) -> Result<String> {
        if let Ok(mut c) = self.cfg.lock() {
            c.host.enabled = on;
            c.save(&self.paths)?;
        }
        self.host.set_enabled(on);
        Ok(if on { "host mode on".into() } else { "host mode off: the lid lock is released".into() })
    }

    pub fn set_relays(&self, relays: Vec<String>) -> Result<String> {
        net::parse_relays(&relays)?;
        let me = self.me;
        let list = relays.join(", ");
        self.edit_roster(format!("relays set to {list}"), |r| Ok(r.set_relays(relays, me, now_ms())))?;
        Ok(format!("relays: {list}"))
    }
}

/// A detached daemon writes its log to `daemon.log` through stdout/stderr. Past 1 MiB
/// it becomes `daemon.log.1` (replacing the older one) and a new file takes its place, as
/// `events.jsonl` does. Only when stderr really is that file (not under systemd).
fn rotate_own_log(log: &std::path::Path) {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    const MAX_LOG: u64 = 1 << 20;
    let Ok(m) = std::fs::metadata(log) else { return };
    if m.len() < MAX_LOG {
        return;
    }
    let Ok(err) = rustix::fs::fstat(rustix::stdio::stderr()) else { return };
    if err.st_ino != m.ino() || err.st_dev != m.dev() {
        return;
    }
    let old = log.with_extension("log.1");
    if std::fs::rename(log, &old).is_err() {
        return;
    }
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(log) else { return };
    // dup2 onto the standard descriptors replaces them in one step for every writer.
    let _ = rustix::stdio::dup2_stdout(&f);
    let _ = rustix::stdio::dup2_stderr(&f);
}

/// Marks a peer as "connecting" for as long as it lives (also when the dial is cancelled).
struct DialFlag<'a>(&'a Daemon, MachineId);

impl<'a> DialFlag<'a> {
    fn set(d: &'a Daemon, id: MachineId) -> DialFlag<'a> {
        d.with_peer(&id, |p| p.dialing = true);
        DialFlag(d, id)
    }
}

impl Drop for DialFlag<'_> {
    fn drop(&mut self) {
        self.0.with_peer(&self.1, |p| p.dialing = false);
    }
}

async fn refuse(send: &mut SendStream, reason: &str) -> Result<()> {
    proto::write_json(send, FrameType::Reply, &Reply::Refused { reason: reason.to_string() }).await?;
    send.finish()?;
    Ok(())
}

/// `Reply::Ok`, one `Json` frame, and the end of the stream.
async fn reply_json<T: serde::Serialize>(send: &mut SendStream, v: &T) -> Result<()> {
    proto::write_json(send, FrameType::Reply, &Reply::Ok).await?;
    proto::write_json(send, FrameType::Json, v).await?;
    send.finish()?;
    let _ = timeout(Duration::from_secs(2), send.stopped()).await;
    Ok(())
}

/// One `Json` frame and the end of the stream (the `Reply` went out before).
async fn reply_json_only<T: serde::Serialize>(send: &mut SendStream, v: &T) -> Result<()> {
    proto::write_json(send, FrameType::Json, v).await?;
    send.finish()?;
    let _ = timeout(Duration::from_secs(2), send.stopped()).await;
    Ok(())
}

/// Send clipboard text on a `Clip { op: Set }` stream whose reply was Ok, and read the result.
pub async fn send_clip<W, R>(send: &mut W, recv: &mut R, text: &str) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    proto::write_json(send, FrameType::Json, &text).await?;
    let done: Reply = proto::parse_json(&proto::expect_frame(recv, FrameType::Json).await?, MAX_HEADER)?;
    match done {
        Reply::Ok => Ok(()),
        Reply::Refused { reason } => bail!("{}", crate::term::safe_text(&reason, 300, false)),
    }
}

/// Open a stream, send the header, and wait for an Ok reply.
pub async fn open_service(conn: &Connection, service: Service) -> Result<(SendStream, RecvStream)> {
    let (send, mut recv) = open_service_nowait(conn, service).await?;
    let reply: Reply = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Reply).await?, MAX_HEADER)?;
    match reply {
        Reply::Ok => Ok((send, recv)),
        // Peer text; it can end up on this machine's terminal (unlink's report).
        Reply::Refused { reason } => bail!("refused: {}", crate::term::safe_text(&reason, 300, false)),
    }
}

async fn open_service_nowait(conn: &Connection, service: Service) -> Result<(SendStream, RecvStream)> {
    let (mut send, recv) = conn.open_bi().await.context("cannot open a stream")?;
    proto::write_json(&mut send, FrameType::Header, &Hello { proto: PROTO_VERSION, service }).await?;
    Ok((send, recv))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_ends(s: &Settle, others: &HashSet<MachineId>, now: u64) {
        let mut st = s.state.lock().unwrap();
        s.settle_if_done(&mut st, others, now);
    }

    /// What counts as an absence, and what ends one (this machine is b; a and c are
    /// the other members; c is the one asking).
    #[test]
    fn unsettled_until_synced_with_a_member_other_than_the_requester() {
        let (a, c, d) = (MachineId([1; 32]), MachineId([3; 32]), MachineId([4; 32]));
        let others: HashSet<MachineId> = [a, c].into();
        let t0 = 1_000_000;
        let s = Settle::new(t0);
        assert!(!s.is_settled(), "a daemon start is an absence");
        assert!(!s.serves(&c, &others));
        round_ends(&s, &others, t0 + 5_000);
        assert!(!s.is_settled(), "a round that reached nobody settles nothing by itself");
        s.synced(c, &others, t0 + 5_100);
        assert!(!s.serves(&c, &others), "a sync with the requester itself does not count for it");
        assert!(s.serves(&a, &others), "but it does for any other requester");
        assert!(!s.is_settled());
        s.synced(a, &others, t0 + 5_200);
        assert!(s.is_settled() && s.serves(&c, &others), "synced with a: settled");

        // A freeze or suspend: the monitor did not run for over 5 s.
        assert!(s.notice(t0 + 1_000).is_none(), "a normal second");
        assert!(s.notice(t0 + 1_000 + TICK_GAP_MS + 1).is_some(), "frozen or asleep");
        assert!(!s.serves(&c, &others));
        s.synced(a, &others, t0 + 20_000);
        assert!(s.serves(&c, &others) && !s.serves(&a, &others), "c is served after a sync with a; a is not yet");
        s.synced(c, &others, t0 + 20_100);
        assert!(s.is_settled());

        // A network-only outage: the monitor keeps ticking, nobody answers. After a minute
        // of silence the machine is unsettled, and no amount of time settles it again.
        let t1 = t0 + 20_100;
        s.last_tick.store(t1, Ordering::Relaxed);
        let mut became = 0;
        for k in 1..=90 {
            let now = t1 + k * 1_000;
            if s.notice(now).is_some() {
                became += 1;
                assert!(now - t1 > ABSENT_AFTER_MS);
            }
            if k % 5 == 0 {
                // Background rounds during the outage: they start and end reaching nobody.
                round_ends(&s, &others, now);
                s.round_done(now - 5_000);
            }
        }
        assert_eq!(became, 1, "one absence, noticed once");
        assert!(!s.is_settled() && !s.serves(&c, &others), "rounds that reached nobody and time alone settle nothing");
        // The network returns at t1 + 91 s; c connects at once. The rounds that ran during
        // the outage started before c's connection: they do not count for it.
        let c_conn = t1 + 91_000;
        assert!(!s.settle_provisionally(c_conn, c_conn + 10), "rounds from before the requester's connection do not count");
        assert!(!s.is_settled());
        // c's first request starts a round; a answers within it: settled, c refused later.
        s.synced(c, &others, c_conn + 50);
        assert!(!s.serves(&c, &others), "contact with c alone does not let c in");
        s.synced(a, &others, c_conn + 900);
        assert!(s.serves(&c, &others) && s.is_settled());

        // Provisional: after a freeze, the first request's round (started after its
        // connection) reaches no other member: settled until the next absence, and the
        // second request is served without waiting.
        let t2 = c_conn + 10_000;
        s.last_tick.store(t2, Ordering::Relaxed);
        assert!(s.notice(t2 + TICK_GAP_MS + 1_000).is_some());
        let a_conn = t2 - 20_000; // a's connection survived the freeze
        assert!(!s.serves(&a, &others) && !s.settle_provisionally(a_conn, t2 + 6_100), "no round since the absence yet");
        s.round_done(t2 + 6_500); // started after the absence (t2 + 6 s), reached nobody
        round_ends(&s, &others, t2 + 12_000);
        assert!(!s.is_settled(), "not fully settled by the round alone");
        assert!(s.settle_provisionally(a_conn, t2 + 12_001), "the first request is then served");
        assert!(s.is_settled() && s.serves(&a, &others) && s.serves(&c, &others), "the second request needs no wait");
        let t3 = t2 + 40_000;
        s.last_tick.store(t3, Ordering::Relaxed);
        assert!(s.notice(t3 + TICK_GAP_MS + 1).is_some(), "the next absence ends the provisional state");
        assert!(!s.serves(&a, &others) && !s.settle_provisionally(a_conn, t3 + TICK_GAP_MS + 2));

        // Nobody else to ask: the only other member is the requester, or there is none.
        let only_c: HashSet<MachineId> = [c].into();
        let s = Settle::new(t0);
        assert!(s.serves(&c, &only_c), "no member but c could have heard of a revocation of c");
        round_ends(&s, &only_c, t0 + 10);
        assert!(s.is_settled());
        let s = Settle::new(t0);
        round_ends(&s, &HashSet::new(), t0 + 10);
        assert!(s.is_settled(), "no other members");
        // A third member that is not reached keeps it unsettled for the two that are.
        let three: HashSet<MachineId> = [a, c, d].into();
        let s = Settle::new(t0);
        s.synced(a, &three, t0 + 10);
        assert!(!s.is_settled() && s.serves(&c, &three) && s.serves(&d, &three) && !s.serves(&a, &three));
    }
}
