//! Wire types: frames, the `jaunt/1` service headers, and the local IPC API.
//!
//! Frame layout, on QUIC streams and on the IPC socket alike:
//! `[u32 big-endian payload length][u8 frame type][payload]`, payload at most 1 MiB.
//! The first frame of every QUIC stream is a `Header` (JSON [`Hello`]); the host answers
//! with a `Reply` frame, then service frames follow.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::roster::MachineId;

pub const ALPN: &[u8] = b"jaunt/1";
/// Spoken only on the temporary pairing endpoints (DESIGN §6).
pub const PAIR_ALPN: &[u8] = b"jaunt-pair/1";
pub const PROTO_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1 << 20;
pub const MAX_HEADER: usize = 64 * 1024;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    /// JSON `Hello`, first frame of a stream.
    Header = 1,
    /// JSON `Reply` to a header.
    Reply = 2,
    /// Bytes: stdin towards the host, stdout / pty output towards the client.
    Data = 3,
    /// Bytes: stderr of an exec.
    Stderr = 4,
    /// No more stdin.
    Eof = 5,
    /// u16 cols, u16 rows (big-endian).
    Resize = 6,
    /// JSON `ExitStatus`; last frame of a shell or exec.
    Exit = 7,
    /// UTF-8 text for the user (warnings).
    Notice = 8,
    /// JSON message (sync rosters, ping info, IPC).
    Json = 9,
}

impl FrameType {
    pub fn from_u8(b: u8) -> Option<FrameType> {
        use FrameType::*;
        Some(match b {
            1 => Header,
            2 => Reply,
            3 => Data,
            4 => Stderr,
            5 => Eof,
            6 => Resize,
            7 => Exit,
            8 => Notice,
            9 => Json,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameType,
    pub payload: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes is over the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    #[error("unknown frame type {0}")]
    UnknownType(u8),
    #[error("the stream ended in the middle of a frame")]
    Truncated,
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("expected a {expected:?} frame, got {got:?}")]
    Unexpected { expected: FrameType, got: FrameType },
    #[error("malformed message: {0}")]
    Malformed(String),
}

/// Read one frame. `Ok(None)` is a clean end of stream at a frame boundary.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>, FrameError> {
    let mut head = [0u8; 5];
    let mut got = 0;
    while got < head.len() {
        let n = r.read(&mut head[got..]).await?;
        if n == 0 {
            return if got == 0 { Ok(None) } else { Err(FrameError::Truncated) };
        }
        got += n;
    }
    let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let kind = FrameType::from_u8(head[4]).ok_or(FrameError::UnknownType(head[4]))?;
    // Grow as bytes arrive instead of reserving the announced length up front, so a header
    // that promises 1 MiB costs memory only once the bytes are really there (N1).
    let mut payload = Vec::with_capacity(len.min(64 * 1024));
    (&mut *r).take(len as u64).read_to_end(&mut payload).await?;
    if payload.len() < len {
        return Err(FrameError::Truncated);
    }
    Ok(Some(Frame { kind, payload }))
}

pub fn encode_frame(kind: FrameType, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(payload.len()));
    }
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.push(kind as u8);
    buf.extend_from_slice(payload);
    Ok(buf)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, kind: FrameType, payload: &[u8]) -> Result<(), FrameError> {
    let buf = encode_frame(kind, payload)?;
    w.write_all(&buf).await?;
    Ok(())
}

pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, kind: FrameType, v: &T) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(v).map_err(|e| FrameError::Malformed(e.to_string()))?;
    write_frame(w, kind, &bytes).await
}

/// Parse a JSON frame payload, refusing anything over `max` bytes.
pub fn parse_json<T: for<'de> Deserialize<'de>>(payload: &[u8], max: usize) -> Result<T, FrameError> {
    if payload.len() > max {
        return Err(FrameError::TooLarge(payload.len()));
    }
    serde_json::from_slice(payload).map_err(|e| FrameError::Malformed(e.to_string()))
}

/// Read a frame that must be of type `kind`; a clean end of stream is an error here.
pub async fn expect_frame<R: AsyncRead + Unpin>(r: &mut R, kind: FrameType) -> Result<Vec<u8>, FrameError> {
    match read_frame(r).await? {
        None => Err(FrameError::Truncated),
        Some(f) if f.kind == kind => Ok(f.payload),
        Some(f) => Err(FrameError::Unexpected { expected: kind, got: f.kind }),
    }
}

/// Read frames on a task of their own and hand them over a channel, so callers can
/// `select!` on frames without cancelling a half-read frame. The channel ends after a
/// clean end of stream or after the first error (which is delivered).
pub fn frame_reader<R>(mut r: R) -> tokio::sync::mpsc::Receiver<Result<Frame, FrameError>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            match read_frame(&mut r).await {
                Ok(Some(f)) => {
                    if tx.send(Ok(f)).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
    });
    rx
}

pub fn encode_resize(cols: u16, rows: u16) -> [u8; 4] {
    let c = cols.to_be_bytes();
    let r = rows.to_be_bytes();
    [c[0], c[1], r[0], r[1]]
}

pub fn decode_resize(p: &[u8]) -> Option<(u16, u16)> {
    if p.len() != 4 {
        return None;
    }
    Some((u16::from_be_bytes([p[0], p[1]]), u16::from_be_bytes([p[2], p[3]])))
}

// ---------------------------------------------------------------------------------------
// jaunt/1 service headers

/// First frame of every stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u32,
    pub service: Service,
}

/// Just the version, parsed first so a mismatch can be reported in words.
#[derive(Debug, Deserialize)]
pub struct HelloProbe {
    pub proto: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Service {
    /// Followed by one `Json` frame with the sender's roster; the reply carries the host's.
    Sync,
    /// Reply is followed by one `Json` frame with [`PingInfo`].
    Ping,
    Shell {
        session: String,
        term: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    Exec {
        argv: Vec<String>,
    },
    Tcp {
        port: u16,
    },
    /// `push`: the client sends a tree to be written at `path` on the host; `pull`: the
    /// host sends the tree at `path`. Body: see `services::copy`.
    Copy {
        op: CopyOp,
        path: String,
    },
    /// After `jaunt unlink --kill-sessions` (H9): end the tmux sessions that these machines
    /// created on the host, for those of them the host has revoked. Reply is followed by
    /// one `Json` frame with the number of sessions ended.
    KillSessions {
        of: Vec<crate::roster::MachineId>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CopyOp {
    Push,
    Pull,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reply {
    Ok,
    Refused { reason: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ExitStatus {
    /// Shell convention: the code, or 128 + signal.
    pub fn as_code(&self) -> i32 {
        match (self.code, self.signal) {
            (Some(c), _) => c,
            (None, Some(s)) => 128 + s,
            _ => 255,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PingInfo {
    pub name: String,
    pub version: String,
    /// tmux sessions on this machine's jaunt tmux server, if tmux is there.
    pub sessions: Option<u32>,
    /// Host-mode state (filled in by the awake guard; `None` = not reported).
    pub host: Option<String>,
    pub time_ms: u64,
}

/// Validate a shell header's free-form strings before they reach a command line or env.
pub fn valid_session(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        && !s.starts_with('-')
}

pub fn valid_term(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b))
}

// ---------------------------------------------------------------------------------------
// Local IPC (unix socket). Every message is one `Json` frame.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    /// -> `Status`
    Status,
    /// -> `Status`, then up to `history` past events as `Event`, then live `Event`s
    /// (and `Rtt` samples) until the client hangs up. After a lag, a fresh `Status`.
    Subscribe {
        #[serde(default)]
        history: u32,
    },
    /// Open a service stream on a peer. -> `Opened` (then the socket carries the raw
    /// stream frames in both directions) or `Error`.
    Open { target: String, service: Service },
    /// Trust a machine without pairing (tests). -> `Ok` or `Error`
    AddPeer { id: String, name: String },
    /// -> `Ok`
    Rename { name: String },
    /// Set the fleet relay list (synced to every machine). -> `Ok`
    SetRelays { relays: Vec<String> },
    /// Ask the daemon to exit. -> `Ok`
    Shutdown,
    /// Invite a machine (§6). -> a stream of `Pair` updates on this socket; at step
    /// `confirm` the daemon waits for a `PairAnswer` frame from the client (unless `yes`);
    /// ends with `Ok` or `Error`. Closing the socket cancels the pairing.
    Link {
        #[serde(default)]
        yes: bool,
    },
    /// Join a mesh with a code. Same conversation as `Link`.
    Join {
        code: String,
        #[serde(default)]
        relays: Vec<String>,
        #[serde(default)]
        yes: bool,
    },
    /// The local human's answer at the `confirm` step (sent on a `Link`/`Join` socket).
    PairAnswer { accept: bool },
    /// Revoke a machine (§7). -> `Told` per machine, then `Unlinked`; or `Error`.
    Unlink {
        target: String,
        #[serde(default)]
        cascade: bool,
        /// Also end the tmux sessions the revoked machine(s) created, here and on every
        /// member that confirmed.
        #[serde(default)]
        kill_sessions: bool,
    },
    /// Tombstone this machine and tell the others. -> `Told`s, then `Unlinked`. The client
    /// then stops the daemon and wipes the key and roster.
    Leave,
    /// What `jaunt check` needs beyond `Status`. -> `Check`
    Check,
    /// How long since anything (data, acks, keep-alives) arrived from a peer. -> `Quiet`
    Quiet { id: crate::roster::MachineId },
    /// Turn host mode (§9) on or off; persisted in config.toml. -> `Ok`
    SetHost { on: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcResponse {
    Status(StatusSnapshot),
    Event(Event),
    Rtt { peer: MachineId, sample: RttSample },
    Opened { peer: PeerRef, path: PathKind, rtt_ms: Option<f64> },
    Ok { message: String },
    Error { message: String },
    /// Progress of a pairing ritual.
    Pair(PairUpdate),
    /// One machine's answer to a pushed roster change (link fan-out, unlink, leave).
    Told { peer: PeerRef, ok: bool, detail: String },
    /// Result of `Unlink` / `Leave`.
    Unlinked(UnlinkReport),
    Check(CheckReport),
    /// `quiet_ms`: since the last datagram from the peer; `None` without a live connection.
    Quiet { quiet_ms: Option<u64> },
}

/// The daemon's view of the network, for `jaunt check`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CheckReport {
    /// Latest iroh net report; `None` until the first one finished.
    pub net: Option<NetSummary>,
    /// Every remote address this daemon has sent to or holds a socket with.
    pub contacts: Vec<Contact>,
    /// IP addresses the relay host names resolve to now.
    pub relay_ips: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NetSummary {
    /// QUIC address discovery answered over IPv4 / IPv6.
    pub udp_v4: bool,
    pub udp_v6: bool,
    pub global_v4: Option<String>,
    pub global_v6: Option<String>,
    /// The public address differs per destination (hard NAT: direct paths are unlikely).
    pub mapping_varies: Option<bool>,
    pub probes: Vec<RelayProbe>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelayProbe {
    pub url: String,
    /// "https", "qad_ipv4" or "qad_ipv6".
    pub probe: String,
    pub ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    /// `ip:port`.
    pub addr: String,
    /// "udp" (a QUIC path to a peer) or "tcp" / "udp-socket" (an open socket right now).
    pub via: String,
    /// The peer this path belongs to, if any.
    pub peer: Option<String>,
    pub last_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PairRole {
    Inviter,
    Joiner,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairUpdate {
    pub role: PairRole,
    #[serde(flatten)]
    pub step: PairStep,
}

/// The steps of the ritual, in order. `fan_out` repeats once per machine told.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum PairStep {
    /// Inviter: the code is live. `first_machine` = this machine's mesh is just itself.
    Waiting { code: String, expires_at_ms: u64, relays: Vec<String>, first_machine: bool },
    /// Joiner: dialling the pairing endpoint.
    Dialing { relays: Vec<String> },
    /// The two pairing endpoints are connected (nothing is authenticated yet).
    Rendezvous,
    /// SPAKE2 finished (a wrong code is only detected at the next step).
    KeyExchange,
    /// Real ids and names were exchanged under the PAKE key: the code was right.
    /// (Fields are not called `peer`: that name belongs to the enclosing event.)
    Sealed { other: PeerRef },
    /// Show these on both screens and ask the human. `sigil` is 5 rows of 5 cells,
    /// `#` = filled, `.` = empty, mirror-symmetric; `hue` 0..6 is a colour hint.
    Confirm { other: PeerRef, sigil: Vec<String>, hue: u8, words: Vec<String> },
    /// One side said yes. `here` = this machine's human.
    Confirmed { here: bool },
    /// Both said yes; the roster is written here.
    Linked { other: PeerRef, members: usize },
    /// Inviter: one other machine's acknowledgement of the new entry.
    FanOut { machine: PeerRef, ok: bool, detail: String },
    /// Inviter: every other machine was tried.
    FanOutDone { told: usize, not_yet: usize },
    Failed { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UnlinkReport {
    /// Machines tombstoned by this request.
    pub revoked: Vec<PeerRef>,
    /// Machines that answered with a roster holding the tombstone(s).
    pub confirmed: Vec<PeerRef>,
    /// Machines that could not be reached or did not confirm.
    pub not_heard: Vec<PeerRef>,
    /// Members that a revoked machine added and that stay linked (no `--cascade`).
    pub added_by_revoked: Vec<PeerRef>,
    /// Processes ended here because of the revocation.
    pub sessions_killed: u32,
    /// With `kill_sessions`: tmux sessions the revoked machine(s) created, ended here and on
    /// the members that confirmed; `None` when not asked for.
    #[serde(default)]
    pub tmux_sessions_killed: Option<u32>,
    /// Of `revoked`, the machine that was revoked before this request (`unlink <name>
    /// --cascade` or `--kill-sessions` after `<name>` was unlinked).
    #[serde(default)]
    pub already_revoked: Vec<PeerRef>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Direct,
    Relay,
    Unknown,
}

impl PathKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            PathKind::Direct => "direct",
            PathKind::Relay => "relay",
            PathKind::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerState {
    /// Live connection, direct path selected.
    Direct,
    /// Live connection through the relay.
    Relay,
    /// A dial is in progress and there is no live connection.
    Connecting,
    Offline,
}

impl PeerState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PeerState::Direct => "direct",
            PeerState::Relay => "relay",
            PeerState::Connecting => "connecting",
            PeerState::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerRef {
    pub id: MachineId,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RttSample {
    pub t_ms: u64,
    pub rtt_ms: f64,
    pub path: PathKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusSnapshot {
    pub me: PeerRef,
    pub version: String,
    pub daemon_pid: u32,
    pub started_at_ms: u64,
    pub relays: Vec<String>,
    /// Home relay connected right now.
    pub relay_connected: bool,
    /// Local sockets the endpoint is bound to.
    pub bound: Vec<String>,
    pub peers: Vec<PeerStatus>,
    pub revoked: Vec<RevokedStatus>,
    /// This machine's host-mode guard (§9).
    #[serde(default)]
    pub host: Option<HostReport>,
}

/// Host mode in one word, for `ls`, the mesh view and peers' `PingInfo::host`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    /// Host mode is off: the machine sleeps as configured.
    Off,
    /// The lid lock is held: closing the lid keeps the machine awake and reachable.
    Awake,
    /// On battery: not holding; closing the lid sleeps.
    OnBattery,
    /// Tripped by heat; released until the lid is opened again.
    TooHot,
    /// On, but not holding for another reason (see `reason`): closing the lid sleeps.
    WillSleep,
}

impl HostState {
    pub fn label(self) -> &'static str {
        match self {
            HostState::Off => "off",
            HostState::Awake => "awake",
            HostState::OnBattery => "on battery",
            HostState::TooHot => "too hot",
            HostState::WillSleep => "will sleep on lid close",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostReport {
    pub state: HostState,
    /// Plain words: why the guard is in this state.
    pub reason: String,
    /// Plain words: what closing the lid does right now.
    pub on_lid_close: String,
    pub lock_held: bool,
    /// "open", "closed" or "unknown".
    pub lid: String,
    /// "mains", "battery", "mains, battery draining" or "unknown".
    pub power: String,
    /// Hottest sensor, e.g. "k10temp Tctl 54 °C".
    pub hottest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerStatus {
    pub id: MachineId,
    pub name: String,
    pub state: PeerState,
    /// Selected path's remote address (`ip:port` or the relay URL).
    pub remote_addr: Option<String>,
    /// "lan", "public", "loopback", "relay", ... for the mesh view's label.
    pub addr_kind: Option<String>,
    /// Latest round-trip time on the selected path.
    pub rtt_ms: Option<f64>,
    /// About the last minute, one sample per second, oldest first.
    pub rtt_history: Vec<RttSample>,
    pub connected_since_ms: Option<u64>,
    pub last_seen_ms: Option<u64>,
    /// From the peer's last Ping answer.
    pub version: Option<String>,
    pub sessions: Option<u32>,
    pub host: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RevokedStatus {
    pub id: MachineId,
    pub name: Option<String>,
    pub revoked_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    /// Increases by one per event within one daemon run.
    pub seq: u64,
    pub t_ms: u64,
    pub peer: Option<PeerRef>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    DaemonStarted { version: String },
    /// A QUIC connection completed its handshake. `incoming` = the peer dialled us.
    Handshake { incoming: bool, path: PathKind },
    /// The selected path changed (relay -> direct, roam to another address, ...).
    PathChanged { from: PathKind, to: PathKind, remote_addr: Option<String> },
    Disconnected { reason: String },
    /// A dial failed (logged once per change of error text).
    DialFailed { error: String },
    /// A roster exchange finished. `changed` = our roster changed because of it.
    Sync { changed: bool, outgoing: bool },
    /// The roster changed (peer added, renamed, revoked, relays changed).
    RosterChanged { what: String },
    /// A service stream was opened on this machine by a peer.
    Served { service: String },
    /// A connection was refused (not a member, bad header, ...).
    Refused { reason: String },
    /// Pairing progress (same steps as the IPC `pair` messages).
    Pair {
        role: PairRole,
        #[serde(flatten)]
        step: PairStep,
    },
    /// `peer` was tombstoned (here or learned by sync). `by` = who revoked it;
    /// `sessions_killed` = processes it had running here that were ended.
    Revoked { by: PeerRef, sessions_killed: u32, added: Vec<PeerRef> },
    /// `peer` answered (or not) a pushed revocation.
    Told { ok: bool, detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, FrameType::Data, b"hello").await.unwrap();
        write_frame(&mut buf, FrameType::Eof, b"").await.unwrap();
        let mut r = &buf[..];
        let f = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(f, Frame { kind: FrameType::Data, payload: b"hello".to_vec() });
        let f = read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!(f.kind, FrameType::Eof);
        assert!(read_frame(&mut r).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn rejects_oversized_frames() {
        let mut buf = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        buf.push(3);
        let mut r = &buf[..];
        assert!(matches!(read_frame(&mut r).await, Err(FrameError::TooLarge(_))));
        assert!(encode_frame(FrameType::Data, &vec![0; MAX_FRAME + 1]).is_err());
        // exactly at the limit is fine
        let ok = encode_frame(FrameType::Data, &vec![7; MAX_FRAME]).unwrap();
        let mut r = &ok[..];
        assert_eq!(read_frame(&mut r).await.unwrap().unwrap().payload.len(), MAX_FRAME);
    }

    #[tokio::test]
    async fn rejects_unknown_type_and_truncation() {
        let mut r: &[u8] = &[0, 0, 0, 0, 200];
        assert!(matches!(read_frame(&mut r).await, Err(FrameError::UnknownType(200))));
        let mut r: &[u8] = &[0, 0, 0];
        assert!(matches!(read_frame(&mut r).await, Err(FrameError::Truncated)));
        let mut r: &[u8] = &[0, 0, 0, 9, 3, 1, 2];
        assert!(matches!(read_frame(&mut r).await, Err(FrameError::Truncated)));
        let mut r: &[u8] = &[0, 0, 0, 0, 3];
        assert!(matches!(expect_frame(&mut r, FrameType::Header).await, Err(FrameError::Unexpected { .. })));
    }

    #[test]
    fn header_parsing() {
        let h = Hello { proto: 1, service: Service::Exec { argv: vec!["hostname".into()] } };
        let s = serde_json::to_vec(&h).unwrap();
        let back: Hello = parse_json(&s, MAX_HEADER).unwrap();
        assert_eq!(back.service, h.service);
        assert!(parse_json::<Hello>(br#"{"proto":1,"service":{"kind":"teleport"}}"#, MAX_HEADER).is_err());
        assert!(parse_json::<Hello>(b"\xff\xfe", MAX_HEADER).is_err());
        assert!(parse_json::<Hello>(&vec![b' '; MAX_HEADER + 1], MAX_HEADER).is_err());
        let probe: HelloProbe = parse_json(br#"{"proto":7,"service":{"kind":"warp"}}"#, MAX_HEADER).unwrap();
        assert_eq!(probe.proto, 7);
    }

    #[test]
    fn resize_and_validation() {
        assert_eq!(decode_resize(&encode_resize(120, 36)), Some((120, 36)));
        assert_eq!(decode_resize(&[1, 2, 3]), None);
        assert!(valid_session("jaunt"));
        assert!(valid_session("work.2"));
        assert!(!valid_session("-t"));
        assert!(!valid_session("a b"));
        assert!(!valid_session("a;rm"));
        assert!(valid_term("xterm-256color"));
        assert!(!valid_term("xterm\n"));
        assert_eq!(ExitStatus { code: None, signal: Some(9) }.as_code(), 137);
    }

    #[test]
    fn ipc_json_shape() {
        let r = IpcRequest::Open { target: "fern".into(), service: Service::Ping };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"op":"open","target":"fern","service":{"kind":"ping"}}"#);
        let e = Event { seq: 1, t_ms: 2, peer: None, kind: EventKind::Sync { changed: true, outgoing: false } };
        let s = serde_json::to_string(&IpcResponse::Event(e.clone())).unwrap();
        let back: IpcResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back, IpcResponse::Event(e));
        // Pairing steps nest a tagged enum inside the tagged event and response.
        let step = PairStep::Confirm {
            other: PeerRef { id: MachineId([1; 32]), name: "b".into() },
            sigil: vec!["#.#.#".into(); 5],
            hue: 2,
            words: vec!["a".into(), "b".into(), "c".into()],
        };
        let e = Event { seq: 3, t_ms: 4, peer: None, kind: EventKind::Pair { role: PairRole::Joiner, step: step.clone() } };
        let s = serde_json::to_string(&IpcResponse::Event(e.clone())).unwrap();
        assert!(s.contains(r#""event":"pair""#) && s.contains(r#""step":"confirm""#) && s.contains(r#""role":"joiner""#), "{s}");
        assert_eq!(serde_json::from_str::<IpcResponse>(&s).unwrap(), IpcResponse::Event(e));
        let u = IpcResponse::Pair(PairUpdate { role: PairRole::Inviter, step: PairStep::FanOutDone { told: 1, not_yet: 0 } });
        let s = serde_json::to_string(&u).unwrap();
        assert_eq!(s, r#"{"kind":"pair","role":"inviter","step":"fan_out_done","told":1,"not_yet":0}"#);
        assert_eq!(serde_json::from_str::<IpcResponse>(&s).unwrap(), u);
    }
}
