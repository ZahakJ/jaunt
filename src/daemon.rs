//! The daemon: owns the key and the one endpoint, keeps a warm connection to every roster
//! member, serves incoming streams, syncs the roster, and records paths, RTT and events.

use std::collections::{HashMap, HashSet, VecDeque};
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
use crate::{identity, ipc, services, shell, systemd};

const RETRY_EVERY: Duration = Duration::from_secs(30);
const RECONNECT_AFTER_LOSS: Duration = Duration::from_secs(2);
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(10);
const RTT_HISTORY: usize = 60;

#[derive(Default)]
struct Peer {
    conn: Option<Connection>,
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
}

fn effective_relays(cfg: &Config, roster: &Roster) -> Vec<String> {
    if roster.fleet.updated_at > 0 { roster.fleet.relays.clone() } else { cfg.bootstrap_relays() }
}

fn member_set(roster: &Roster, me: &MachineId) -> HashSet<MachineId> {
    roster.members().map(|e| e.id).filter(|id| id != me).collect()
}

/// Words for why a connection ended.
pub fn describe_close(e: &ConnectionError) -> String {
    match e {
        ConnectionError::TimedOut => "no answer for 30 s (timed out)".into(),
        ConnectionError::ApplicationClosed(c) => {
            let why = String::from_utf8_lossy(&c.reason).to_string();
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
    let ep = net::build_endpoint(net::EndpointOptions {
        secret,
        relays: &relays,
        relay_ca: cfg.relay_ca.as_deref(),
        // Off by default under JAUNT_HOME (tests) so a test never asks a real gateway for a mapping.
        portmapper: cfg.portmapper.unwrap_or(paths.jaunt_home.is_none()),
        port: cfg.port,
        members: members.clone(),
    })
    .await?;
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
    });
    d.events.emit(Some(d.me_ref()), EventKind::DaemonStarted { version: proto::VERSION.into() });
    tracing::info!("jaunt daemon {} id {} listening on {:?}", proto::VERSION, d.me, d.ep.bound_sockets());

    tokio::spawn(d.clone().accept_loop());
    tokio::spawn(d.clone().supervisor());
    tokio::spawn(ipc::serve(d.clone(), listener));
    systemd::notify("READY=1");
    if let Some(every) = systemd::watchdog_interval() {
        tokio::spawn(async move {
            loop {
                systemd::notify("WATCHDOG=1");
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

    pub fn is_member(&self, id: &MachineId) -> bool {
        self.members.contains(id)
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    fn relays(&self) -> Vec<RelayUrl> {
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
                    // connection outside a dial race means the old one is dead on its side
                    // (it restarted, or its end timed out): drop ours now rather than
                    // waiting for the idle timeout.
                    Some(old) if !fresh => {
                        old.close(CLOSE_NORMAL.into(), b"replaced by a new connection");
                        Self::set_current(p, &conn, seq, incoming, path.clone());
                        (conn.clone(), None)
                    }
                    _ => {
                        Self::set_current(p, &conn, seq, incoming, path.clone());
                        (conn.clone(), None)
                    }
                }
            })
            .unwrap_or((conn.clone(), None));
        if let Some(dup) = duplicate {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(3)).await;
                dup.close(CLOSE_NORMAL.into(), b"duplicate connection");
            });
        }
        if current.stable_id() == conn.stable_id() {
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
    }

    async fn on_closed(self: Arc<Self>, conn: Connection, id: MachineId, seq: u64) {
        let reason = describe_close(&conn.closed().await);
        let was_current = self
            .with_peer(&id, |p| {
                if p.conn_seq == seq {
                    p.conn = None;
                    p.path = None;
                    p.since = None;
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if was_current {
            self.events.emit(Some(self.peer_ref(&id)), EventKind::Disconnected { reason });
            let kick = self.with_peer(&id, |p| p.kick.clone());
            tokio::spawn(async move {
                tokio::time::sleep(RECONNECT_AFTER_LOSS).await;
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
                    self.refresh_path(&conn, &id, seq);
                    self.sample_rtt(&conn, &id, seq);
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
        if relays.is_empty() {
            bail!("no relay is configured, so {} cannot be found (set one with: jaunt relay set URL)", self.name_of(&id));
        }
        let addr = net::dial_addr(id, &relays)?;
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
                if let Ok(mut g) = self.peers.lock()
                    && let Some(p) = g.remove(&id)
                    && let Some(c) = p.conn
                {
                    c.close(CLOSE_NOT_MEMBER.into(), b"removed from the mesh");
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
        while let Ok((send, recv)) = conn.accept_bi().await {
            let d = self.clone();
            tokio::spawn(async move {
                if let Err(e) = d.serve_stream(id, send, recv).await {
                    tracing::debug!("stream from {} ended: {e:#}", id.short());
                }
            });
        }
    }

    async fn serve_stream(self: Arc<Self>, id: MachineId, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
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
        match hello.service {
            Service::Sync => self.serve_sync(id, send, recv).await,
            Service::Ping => {
                proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
                let info = PingInfo {
                    name: self.my_name(),
                    version: proto::VERSION.into(),
                    sessions: self.session_count().await,
                    host: None,
                    time_ms: now_ms(),
                };
                proto::write_json(&mut send, FrameType::Json, &info).await?;
                send.finish()?;
                Ok(())
            }
            Service::Shell { session, term, cols, rows, env } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "shell".into() });
                let req = shell::ShellRequest { session, term, cols, rows, env };
                shell::serve(&self, id, req, send, recv).await
            }
            Service::Exec { argv } => {
                self.events.emit(Some(self.peer_ref(&id)), EventKind::Served { service: "exec".into() });
                services::serve_exec(&self, id, argv, send, recv).await
            }
            other => {
                let why = format!("{} is not implemented yet on {} (jaunt {})", other.name(), self.my_name(), proto::VERSION);
                refuse(&mut send, &why).await
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

    async fn serve_sync(&self, id: MachineId, mut send: SendStream, mut recv: RecvStream) -> Result<()> {
        let payload = timeout(HEADER_TIMEOUT, proto::expect_frame(&mut recv, FrameType::Json)).await??;
        let theirs: Roster = proto::parse_json(&payload, MAX_FRAME)?;
        let changed = self.apply_remote_roster(&theirs, &id);
        proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
        let ours = self.roster_snapshot();
        proto::write_json(&mut send, FrameType::Json, &ours).await?;
        send.finish()?;
        let changed = changed?;
        self.events.emit(Some(self.peer_ref(&id)), EventKind::Sync { changed, outgoing: false });
        Ok(())
    }

    async fn sync_with(self: Arc<Self>, conn: Connection, id: MachineId) {
        let res = timeout(Duration::from_secs(15), async {
            let (mut send, mut recv) = conn.open_bi().await?;
            let hello = Hello { proto: PROTO_VERSION, service: Service::Sync };
            proto::write_json(&mut send, FrameType::Header, &hello).await?;
            proto::write_json(&mut send, FrameType::Json, &self.roster_snapshot()).await?;
            let reply: Reply = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Reply).await?, MAX_HEADER)?;
            if let Reply::Refused { reason } = reply {
                bail!("sync refused: {reason}");
            }
            let theirs: Roster = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Json).await?, MAX_FRAME)?;
            let _ = send.finish();
            self.apply_remote_roster(&theirs, &id)
        })
        .await;
        match res {
            Ok(Ok(changed)) => self.events.emit(Some(self.peer_ref(&id)), EventKind::Sync { changed, outgoing: true }),
            Ok(Err(e)) => tracing::info!("sync with {} failed: {e:#}", id.short()),
            Err(_) => tracing::info!("sync with {} timed out", id.short()),
        }
    }

    pub fn roster_snapshot(&self) -> Roster {
        self.roster.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn apply_remote_roster(&self, theirs: &Roster, from: &MachineId) -> Result<bool> {
        theirs.validate().context("the peer sent an invalid roster")?;
        let changed = {
            let mut r = self.roster.lock().map_err(|_| anyhow!("roster lock poisoned"))?;
            let mut merged = r.clone();
            if !merged.merge(theirs) {
                false
            } else {
                merged.validate().context("merging would exceed the roster limits")?;
                merged.save(&self.paths)?;
                *r = merged;
                true
            }
        };
        if changed {
            self.roster_changed(format!("synced from {}", self.name_of(from)));
        }
        Ok(changed)
    }

    /// Apply a local roster edit, persist it and spread it.
    fn edit_roster(&self, what: String, f: impl FnOnce(&mut Roster) -> Result<bool>) -> Result<bool> {
        let changed = {
            let mut r = self.roster.lock().map_err(|_| anyhow!("roster lock poisoned"))?;
            let mut next = r.clone();
            if !f(&mut next)? {
                false
            } else {
                next.validate()?;
                next.save(&self.paths)?;
                *r = next;
                true
            }
        };
        if changed {
            self.roster_changed(what);
        }
        Ok(changed)
    }

    fn roster_changed(&self, what: String) {
        let (members, relay_strs) = match self.roster.lock() {
            Ok(r) => (member_set(&r, &self.me), r.fleet.relays.clone()),
            Err(_) => return,
        };
        self.members.set(members.clone());
        let fleet_set = self.roster.lock().map(|r| r.fleet.updated_at > 0).unwrap_or(false);
        if fleet_set {
            self.apply_relays(relay_strs);
        }
        // Close connections to machines that are no longer members.
        if let Ok(g) = self.peers.lock() {
            for (id, p) in g.iter() {
                if !members.contains(id)
                    && let Some(c) = &p.conn
                {
                    c.close(CLOSE_NOT_MEMBER.into(), b"removed from the mesh");
                }
            }
        }
        self.events.emit(None, EventKind::RosterChanged { what });
        self.roster_tx.send_modify(|v| *v += 1);
        self.push_sync_all();
    }

    fn push_sync_all(&self) {
        let conns: Vec<(MachineId, Connection)> = self
            .peers
            .lock()
            .map(|g| g.iter().filter_map(|(id, p)| p.conn.clone().map(|c| (*id, c))).collect())
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
        }
    }

    pub fn resolve(&self, target: &str) -> Result<MachineId> {
        let r = self.roster_snapshot();
        match r.resolve(target, Some(&self.me)) {
            Resolve::Found(id) => Ok(id),
            Resolve::NotFound => {
                if r.resolve(target, None) == Resolve::Found(self.me) {
                    bail!("{target} is this machine");
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
            Reply::Refused { reason } => bail!("{} refused: {reason}", peer.name),
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

    pub fn add_peer(&self, id: MachineId, name: &str) -> Result<String> {
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

    pub fn set_relays(&self, relays: Vec<String>) -> Result<String> {
        net::parse_relays(&relays)?;
        let me = self.me;
        let list = relays.join(", ");
        self.edit_roster(format!("relays set to {list}"), |r| Ok(r.set_relays(relays, me, now_ms())))?;
        Ok(format!("relays: {list}"))
    }
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

/// Open a stream, send the header, and wait for an Ok reply.
pub async fn open_service(conn: &Connection, service: Service) -> Result<(SendStream, RecvStream)> {
    let (send, mut recv) = open_service_nowait(conn, service).await?;
    let reply: Reply = proto::parse_json(&proto::expect_frame(&mut recv, FrameType::Reply).await?, MAX_HEADER)?;
    match reply {
        Reply::Ok => Ok((send, recv)),
        Reply::Refused { reason } => bail!("refused: {reason}"),
    }
}

async fn open_service_nowait(conn: &Connection, service: Service) -> Result<(SendStream, RecvStream)> {
    let (mut send, recv) = conn.open_bi().await.context("cannot open a stream")?;
    proto::write_json(&mut send, FrameType::Header, &Hello { proto: PROTO_VERSION, service }).await?;
    Ok((send, recv))
}
