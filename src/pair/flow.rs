//! The pairing conversation on one bidirectional stream, for both roles. Generic over the
//! stream so the unit tests can run it over in-memory pipes.
//!
//! ```text
//! joiner                                   inviter
//!   Json  {t:spake, v, msg: B}  ──────────▸   (code burnt on arrival)
//!         ◂──────────  Json {t:spake, v, msg: A}
//!   Data  seal(JoinerHello{id,name,proof})  ▸   open fails → Json {t:abort,"wrong code"}
//!         ◂──────────  Data seal(InviterHello{id,name,proof})
//!   (each side checks the other's proof: a signature by the claimed id over this session)
//!   (both show sigil + words, ask their human)
//!   Data  seal(Confirm{ok})  ◂──────────▸  Data seal(Confirm{ok})
//!         ◂──────────  Data seal(Welcome{roster,your_entry})   (only after both said yes)
//!   Data  seal(Done{ok})  ─────────────▸   (joiner checked the roster)
//!                                           inviter writes its roster
//!         ◂──────────  Data seal(Done{ok})
//!   joiner writes its roster
//! ```
//!
//! If the last `Done` is lost, the inviter has linked and the joiner has not; the joiner
//! can simply join again with a new code (an existing member that proves its key is
//! accepted again), so nothing is ever left half-linked for good.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::code::Code;
use super::crypto::{HumanCheck, NotOpened, Opener, Pake, Sealer, SessionKey};
use crate::proto::{self, FrameType, MAX_FRAME, PairRole, PairStep, PeerRef};
use crate::roster::{Entry, MachineId, Roster, valid_name};

/// 2: proofs of key possession, the check over the session key alone, the roster only
/// after both confirmations, and a final acknowledgement.
pub const PAIR_VERSION: u32 = 2;
/// How long a code stays valid.
pub const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Per-message wait while the machines talk to each other.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a human has to compare the sigils.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MAX_PLAIN: usize = 4 * 1024;
const MAX_HELLO: usize = 4 * 1024;
/// An unauthenticated abort reason is shown at most this long, control characters removed.
const MAX_REASON: usize = 200;

pub type Steps = mpsc::UnboundedSender<PairStep>;

/// Unauthenticated messages.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Plain {
    Spake { v: u32, msg: String },
    Abort { reason: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    id: MachineId,
    name: String,
    version: String,
    /// Signature by `id`'s key over this session (`SessionKey::prove`).
    proof: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Welcome {
    roster: Roster,
    your_entry: Entry,
}

#[derive(Debug, Serialize, Deserialize)]
struct Confirm {
    ok: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Done {
    ok: bool,
    #[serde(default)]
    why: String,
}

/// A live code on the inviter: one attempt, then burnt; dead after its lifetime.
pub struct Slot {
    code: Code,
    expires: Instant,
    burnt: bool,
}

impl Slot {
    pub fn new(code: Code, now: Instant) -> Slot {
        Slot { code, expires: now + CODE_LIFETIME, burnt: false }
    }

    pub fn expires(&self) -> Instant {
        self.expires
    }

    /// Take the code for one attempt. Every later call fails, whatever the attempt's outcome.
    pub fn take(&mut self, now: Instant) -> Result<Code> {
        if self.burnt {
            bail!("this code was already used once; run 'jaunt link' again for a new one");
        }
        if now >= self.expires {
            self.burnt = true;
            bail!("this code expired (codes last 5 minutes); run 'jaunt link' again");
        }
        self.burnt = true;
        Ok(self.code.clone())
    }
}

pub struct Me {
    pub id: MachineId,
    pub name: String,
    /// The machine's own key, to prove `id` (it never leaves this process).
    pub key: iroh::SecretKey,
}

#[derive(Debug)]
pub struct Invited {
    pub peer: PeerRef,
}

#[derive(Debug)]
pub struct Joined {
    pub inviter: PeerRef,
    /// The mesh as the inviter sent it, with this machine's entry in it.
    pub roster: Roster,
}

enum Incoming {
    Plain(Plain),
    Sealed(Vec<u8>),
}

/// An abort reason as it may be shown: it is unauthenticated text from whoever is on the
/// other end, so it is cleaned of control characters and cut short.
pub fn their_reason(reason: &str) -> String {
    crate::term::safe_text(reason.trim(), MAX_REASON, false)
}

fn stopped(reason: &str) -> anyhow::Error {
    anyhow!("the other machine stopped (its own words, not verified): \"{}\"", their_reason(reason))
}

async fn read_msg<R: AsyncRead + Unpin>(r: &mut R, wait: Duration) -> Result<Incoming> {
    let frame = timeout(wait, proto::read_frame(r))
        .await
        .map_err(|_| anyhow!("the other machine stopped answering"))??
        .ok_or_else(|| anyhow!("the other machine closed the pairing connection"))?;
    match frame.kind {
        FrameType::Json => Ok(Incoming::Plain(proto::parse_json(&frame.payload, MAX_PLAIN)?)),
        FrameType::Data => Ok(Incoming::Sealed(frame.payload)),
        other => bail!("unexpected {other:?} frame during pairing"),
    }
}

/// Read a sealed message; an `abort` from the other side becomes an error with its reason.
async fn read_sealed<R: AsyncRead + Unpin>(r: &mut R, opener: &mut Opener, wait: Duration) -> Result<Vec<u8>> {
    match read_msg(r, wait).await? {
        Incoming::Sealed(ct) => Ok(opener.open(&ct)?),
        Incoming::Plain(Plain::Abort { reason }) => Err(stopped(&reason)),
        Incoming::Plain(_) => bail!("unexpected message during pairing"),
    }
}

async fn send_plain<W: AsyncWrite + Unpin>(w: &mut W, m: &Plain) -> Result<()> {
    proto::write_json(w, FrameType::Json, m).await?;
    w.flush().await?;
    Ok(())
}

async fn send_sealed<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, sealer: &mut Sealer, v: &T) -> Result<()> {
    let pt = zeroize::Zeroizing::new(serde_json::to_vec(v)?);
    let ct = sealer.seal(&pt)?;
    if ct.len() > MAX_FRAME {
        bail!("the roster is too large to send in one pairing message");
    }
    proto::write_frame(w, FrameType::Data, &ct).await?;
    w.flush().await?;
    Ok(())
}

/// Best effort: tell the other side why we stop (it is unauthenticated, so it is only a courtesy).
async fn abort<W: AsyncWrite + Unpin>(w: &mut W, reason: &str) {
    let _ = send_plain(w, &Plain::Abort { reason: reason.to_string() }).await;
}

fn decode_spake(msg: &str) -> Result<Vec<u8>> {
    if msg.len() > 128 {
        bail!("oversized key-exchange message");
    }
    data_encoding::HEXLOWER_PERMISSIVE.decode(msg.as_bytes()).context("malformed key-exchange message")
}

fn version_error(v: u32) -> anyhow::Error {
    anyhow!("the other machine speaks pairing version {v}, this one {PAIR_VERSION}; update jaunt on the older one")
}

/// Read the joiner's first message (before the code is taken from its slot).
pub async fn read_first<R: AsyncRead + Unpin>(r: &mut R, wait: Duration) -> Result<Vec<u8>> {
    match read_msg(r, wait).await? {
        Incoming::Plain(Plain::Spake { v, msg }) if v == PAIR_VERSION => decode_spake(&msg),
        Incoming::Plain(Plain::Spake { v, .. }) => Err(version_error(v)),
        _ => bail!("the other machine did not start with a key exchange"),
    }
}

/// Both humans compare; each side's answer is sent sealed; both must be yes.
async fn confirm_both<R, W, A>(
    r: &mut R,
    w: &mut W,
    sealer: &mut Sealer,
    opener: &mut Opener,
    steps: &Steps,
    answer: A,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: Future<Output = bool>,
{
    let local = timeout(CONFIRM_TIMEOUT, answer);
    tokio::pin!(local);
    let remote = async {
        let pt = read_sealed(r, opener, CONFIRM_TIMEOUT + STEP_TIMEOUT).await?;
        let c: Confirm = proto::parse_json(&pt, MAX_PLAIN)?;
        Ok::<bool, anyhow::Error>(c.ok)
    };
    tokio::pin!(remote);
    let (mut here, mut there) = (false, false);
    while !(here && there) {
        tokio::select! {
            ok = &mut local, if !here => {
                let ok = ok.unwrap_or(false);
                let _ = send_sealed(w, sealer, &Confirm { ok }).await;
                if !ok {
                    bail!("not confirmed here; nothing was changed");
                }
                here = true;
                let _ = steps.send(PairStep::Confirmed { here: true });
            }
            ok = &mut remote, if !there => {
                if !ok? {
                    bail!("the other machine did not confirm (its human said the sigil differs, or gave up); nothing was changed");
                }
                there = true;
                let _ = steps.send(PairStep::Confirmed { here: false });
            }
        }
    }
    Ok(())
}

fn confirm_step(peer: &PeerRef, check: HumanCheck) -> PairStep {
    PairStep::Confirm { other: peer.clone(), sigil: check.sigil, hue: check.hue, words: check.words }
}

/// Open and check the other side's hello: a valid name and a proof that its sender holds
/// the key of the id it claims.
fn open_hello(pt: &[u8], key: &SessionKey, their_role: PairRole) -> Result<Hello> {
    let hello: Hello = proto::parse_json(pt, MAX_HELLO)?;
    key.check_proof(their_role, &hello.id, &hello.proof)?;
    Ok(hello)
}

/// Inviter side, after the joiner's first message arrived and the code was taken.
/// `roster` gives this machine's roster now; `now_ms` stamps the new entry; `commit`
/// writes the joiner's entry once both humans said yes and the joiner took the roster.
#[allow(clippy::too_many_arguments)]
pub async fn invite<R, W, A, S, C>(
    r: &mut R,
    w: &mut W,
    code: &Code,
    first: &[u8],
    me: &Me,
    roster: S,
    now_ms: u64,
    steps: &Steps,
    answer: A,
    commit: C,
) -> Result<Invited>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: Future<Output = bool>,
    S: Fn() -> Roster,
    C: FnOnce(&Entry) -> Result<()>,
{
    let (pake, msg) = Pake::start(PairRole::Inviter, code);
    let key = match pake.finish(first) {
        Ok(k) => k,
        Err(e) => {
            abort(w, "malformed key exchange; the code is burnt").await;
            return Err(e);
        }
    };
    send_plain(w, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&msg) }).await?;
    let _ = steps.send(PairStep::KeyExchange);
    let (mut sealer, mut opener) = key.channel(PairRole::Inviter);

    let pt = match read_sealed(r, &mut opener, STEP_TIMEOUT).await {
        Ok(pt) => pt,
        Err(e) if e.is::<NotOpened>() => {
            abort(w, "wrong code; that code is burnt now, ask for a new one (jaunt link)").await;
            bail!("someone tried a wrong code; the code is burnt. Run 'jaunt link' again for a new one");
        }
        Err(e) => return Err(e),
    };
    let hello = match open_hello(&pt, &key, PairRole::Joiner) {
        Ok(h) => h,
        Err(e) => {
            abort(w, "the joining machine's hello did not check out").await;
            return Err(e);
        }
    };
    let now_roster = roster();
    let refusal = if hello.id == me.id {
        Some("a machine cannot link to itself".to_string())
    } else if now_roster.tombstones.contains_key(&hello.id) {
        Some(format!(
            "machine {} was unlinked from this mesh and its key can never come back; run 'jaunt reset' on it, then join again",
            hello.id.short()
        ))
    } else if !valid_name(&hello.name) {
        // Cleaned: the name is the joiner's own text and ends up on this terminal.
        Some(format!("'{}' is not a valid machine name; rename it with 'jaunt name' first", their_reason(&hello.name)))
    } else {
        // An id that is already a member (it proved its key) may link again: that is how a
        // pairing whose last message was lost is finished.
        None
    };
    if let Some(why) = refusal {
        abort(w, &why).await;
        bail!(why);
    }
    let peer = PeerRef { id: hello.id, name: hello.name.clone() };
    let _ = steps.send(PairStep::Sealed { other: peer.clone() });
    let entry = Entry { id: hello.id, name: hello.name, added_at: now_ms, added_by: me.id, named_at: now_ms };
    let mine = Hello { id: me.id, name: me.name.clone(), version: proto::VERSION.into(), proof: key.prove(PairRole::Inviter, &me.key) };
    send_sealed(w, &mut sealer, &mine).await?;
    let _ = steps.send(confirm_step(&peer, key.human_check()));
    confirm_both(r, w, &mut sealer, &mut opener, steps, answer).await?;

    // Only now does the joiner learn who else is in the mesh.
    send_sealed(w, &mut sealer, &Welcome { roster: roster(), your_entry: entry.clone() }).await?;
    let pt = read_sealed(r, &mut opener, STEP_TIMEOUT).await?;
    let ack: Done = proto::parse_json(&pt, MAX_PLAIN)?;
    if !ack.ok {
        bail!("the joining machine did not take the roster: {}", their_reason(&ack.why));
    }
    let saved = commit(&entry);
    let done = match &saved {
        Ok(()) => Done { ok: true, why: String::new() },
        Err(e) => Done { ok: false, why: format!("{e:#}") },
    };
    send_sealed(w, &mut sealer, &done).await?;
    saved?;
    Ok(Invited { peer })
}

/// Joiner side. `me` is this machine's real id, name and key.
pub async fn join<R, W, A>(r: &mut R, w: &mut W, code: &Code, me: &Me, steps: &Steps, answer: A) -> Result<Joined>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: Future<Output = bool>,
{
    let (pake, msg) = Pake::start(PairRole::Joiner, code);
    send_plain(w, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&msg) }).await?;
    let theirs = match read_msg(r, STEP_TIMEOUT).await? {
        Incoming::Plain(Plain::Spake { v, msg }) if v == PAIR_VERSION => decode_spake(&msg)?,
        Incoming::Plain(Plain::Spake { v, .. }) => return Err(version_error(v)),
        Incoming::Plain(Plain::Abort { reason }) => return Err(stopped(&reason)),
        Incoming::Sealed(_) => bail!("unexpected message during pairing"),
    };
    let key = pake.finish(&theirs)?;
    let _ = steps.send(PairStep::KeyExchange);
    let (mut sealer, mut opener) = key.channel(PairRole::Joiner);
    let mine = Hello { id: me.id, name: me.name.clone(), version: proto::VERSION.into(), proof: key.prove(PairRole::Joiner, &me.key) };
    send_sealed(w, &mut sealer, &mine).await?;

    let pt = match read_sealed(r, &mut opener, STEP_TIMEOUT).await {
        Ok(pt) => pt,
        Err(e) if e.is::<NotOpened>() => {
            abort(w, "the answer did not open").await;
            bail!("the other side's answer did not open with this code: someone may be in the middle. Nothing was changed")
        }
        Err(e) => return Err(e),
    };
    let hello = match open_hello(&pt, &key, PairRole::Inviter) {
        Ok(h) if h.id != me.id && valid_name(&h.name) => h,
        Ok(_) => {
            abort(w, "the inviting machine's hello did not check out").await;
            bail!("the inviting machine sent an id or name that cannot be right; nothing was changed");
        }
        Err(e) => {
            abort(w, "the inviting machine's hello did not check out").await;
            return Err(e.context("nothing was changed"));
        }
    };
    let inviter = PeerRef { id: hello.id, name: hello.name };
    let _ = steps.send(PairStep::Sealed { other: inviter.clone() });
    let _ = steps.send(confirm_step(&inviter, key.human_check()));
    confirm_both(r, w, &mut sealer, &mut opener, steps, answer).await?;

    let pt = read_sealed(r, &mut opener, STEP_TIMEOUT).await?;
    let welcome: Welcome = proto::parse_json(&pt, MAX_FRAME)?;
    let checked = check_welcome(welcome, me, &inviter.id);
    let roster = match checked {
        Ok(r) => r,
        Err(e) => {
            let _ = send_sealed(w, &mut sealer, &Done { ok: false, why: format!("{e:#}") }).await;
            return Err(e);
        }
    };
    send_sealed(w, &mut sealer, &Done { ok: true, why: String::new() }).await?;
    let pt = read_sealed(r, &mut opener, STEP_TIMEOUT)
        .await
        .context("no final word from the inviting machine; it may have linked this machine: run 'jaunt join' again with a new code to finish")?;
    let done: Done = proto::parse_json(&pt, MAX_PLAIN)?;
    if !done.ok {
        bail!("the inviting machine could not save the link ({}); nothing was changed here", their_reason(&done.why));
    }
    Ok(Joined { inviter, roster })
}

/// The joiner's checks on the roster it is given.
fn check_welcome(w: Welcome, me: &Me, inviter: &MachineId) -> Result<Roster> {
    let mut roster = w.roster;
    roster.sanitize_remote();
    roster.drop_malformed();
    roster.validate()?;
    let e = &w.your_entry;
    if e.id != me.id || e.added_by != *inviter || !valid_name(&e.name) {
        bail!("the inviter sent an entry that is not for this machine");
    }
    if !roster.is_member(inviter) {
        bail!("the inviter is not a member of its own roster");
    }
    if roster.tombstones.contains_key(&me.id) {
        bail!("this machine's key was unlinked from that mesh; run 'jaunt reset' and join again");
    }
    let mut incoming = Roster::default();
    incoming.entries.insert(me.id, w.your_entry);
    roster.merge(&incoming);
    roster.validate().context("the inviter's roster is full")?;
    Ok(roster)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::Entry;
    use std::sync::{Arc, Mutex};
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf, split};

    type Pipe = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);
    type Checks = Vec<(Vec<String>, Vec<String>)>;

    fn key(seed: u8) -> iroh::SecretKey {
        iroh::SecretKey::from_bytes(&[seed; 32])
    }
    fn id(seed: u8) -> MachineId {
        MachineId::from_endpoint(&key(seed).public())
    }
    fn me(seed: u8, name: &str) -> Me {
        Me { id: id(seed), name: name.into(), key: key(seed) }
    }
    const A: u8 = 0xa;
    const B: u8 = 0xb;

    fn pipe() -> (Pipe, Pipe) {
        let (a, b) = tokio::io::duplex(4 << 20);
        (split(a), split(b))
    }

    fn code(w: &str) -> Code {
        Code { nameplate: "4127".into(), words: [w.into(), "kudos".into()] }
    }

    fn mesh() -> Roster {
        let mut r = Roster::default();
        r.entries.insert(id(A), Entry { id: id(A), name: "a".into(), added_at: 1, added_by: id(A), named_at: 1 });
        r
    }

    fn checks(rx: &mut mpsc::UnboundedReceiver<PairStep>) -> Checks {
        let mut out = Vec::new();
        while let Ok(s) = rx.try_recv() {
            if let PairStep::Confirm { sigil, words, .. } = s {
                out.push((sigil, words));
            }
        }
        out
    }

    /// The inviter as the daemon runs it: read the first message, take the code from the
    /// slot (burning it), then talk. Owns its pipe so the far side sees EOF when it ends.
    /// `saved` records what `commit` wrote.
    async fn inviter(p: Pipe, slot: &mut Slot, roster: Roster, tx: Steps, yes: bool, saved: Arc<Mutex<Vec<Entry>>>) -> Result<Invited> {
        let (mut r, mut w) = p;
        let first = read_first(&mut r, STEP_TIMEOUT).await?;
        let c = slot.take(Instant::now())?;
        let commit = move |e: &Entry| {
            saved.lock().unwrap().push(e.clone());
            Ok(())
        };
        invite(&mut r, &mut w, &c, &first, &me(A, "a"), || roster.clone(), 100, &tx, async move { yes }, commit).await
    }

    async fn joiner(p: Pipe, c: Code, tx: Steps, yes: bool) -> Result<Joined> {
        let (mut r, mut w) = p;
        join(&mut r, &mut w, &c, &me(B, "b"), &tx, async move { yes }).await
    }

    struct Run {
        i: Result<Invited>,
        j: Result<Joined>,
        checks: Checks,
        slot: Slot,
        saved: Vec<Entry>,
    }

    async fn run_pair(inv: Code, joi: Code, roster: Roster, yes: (bool, bool)) -> Run {
        let (pi, pj) = pipe();
        let (itx, mut irx) = mpsc::unbounded_channel();
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut slot = Slot::new(inv, Instant::now());
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (i, j) = tokio::join!(inviter(pi, &mut slot, roster, itx, yes.0, saved.clone()), joiner(pj, joi, jtx, yes.1));
        let mut c = checks(&mut irx);
        c.extend(checks(&mut jrx));
        let saved = saved.lock().unwrap().clone();
        Run { i, j, checks: c, slot, saved }
    }

    #[tokio::test]
    async fn honest_pairing_links_and_checks_match() {
        let r = run_pair(code("tremor"), code("tremor"), mesh(), (true, true)).await;
        let i = r.i.unwrap();
        let j = r.j.unwrap();
        assert_eq!(i.peer.name, "b");
        assert_eq!(i.peer.id, id(B));
        assert_eq!(j.inviter.name, "a");
        assert_eq!(j.inviter.id, id(A));
        assert!(j.roster.is_member(&id(A)) && j.roster.is_member(&id(B)));
        assert_eq!(r.saved.len(), 1, "the inviter wrote exactly one entry");
        assert_eq!(j.roster.entries[&id(B)], r.saved[0], "both sides wrote the same entry");
        assert_eq!(r.saved[0].added_by, id(A));
        assert_eq!(r.checks.len(), 2);
        assert_eq!(r.checks[0], r.checks[1], "both screens show the same sigil and words");
    }

    #[tokio::test]
    async fn wrong_code_fails_both_sides_and_burns() {
        let mut r = run_pair(code("tremor"), code("zoom"), mesh(), (true, true)).await;
        let ie = r.i.expect_err("inviter must fail").to_string();
        let je = r.j.expect_err("joiner must fail").to_string();
        assert!(ie.contains("wrong code") && ie.contains("burnt"), "{ie}");
        assert!(je.contains("wrong code"), "{je}");
        assert!(r.checks.is_empty(), "no sigil is shown for a wrong code");
        assert!(r.saved.is_empty());
        assert!(r.slot.take(Instant::now()).is_err(), "the right code no longer works after a wrong try");
    }

    #[tokio::test]
    async fn a_no_on_either_side_links_nothing() {
        for yes in [(true, false), (false, true)] {
            let r = run_pair(code("tremor"), code("tremor"), mesh(), yes).await;
            assert!(r.i.is_err() && r.j.is_err(), "{yes:?}");
            assert!(r.saved.is_empty(), "{yes:?}");
        }
    }

    #[tokio::test]
    async fn revoked_joiner_is_refused() {
        let mut roster = mesh();
        roster.revoke(id(B), id(A), 5);
        let r = run_pair(code("tremor"), code("tremor"), roster, (true, true)).await;
        assert!(r.i.err().unwrap().to_string().contains("unlinked"));
        assert!(r.j.err().unwrap().to_string().contains("unlinked"));
    }

    /// PAIR-8: the inviter linked but the joiner never heard the last word. The joiner joins
    /// again with a new code and the inviter accepts the existing member (it proves its key).
    #[tokio::test]
    async fn an_existing_member_may_link_again() {
        let mut roster = mesh();
        roster.entries.insert(id(B), Entry { id: id(B), name: "b".into(), added_at: 50, added_by: id(A), named_at: 50 });
        let r = run_pair(code("tremor"), code("tremor"), roster, (true, true)).await;
        assert!(r.i.is_ok(), "{:?}", r.i.err());
        let j = r.j.unwrap();
        assert!(j.roster.is_member(&id(A)) && j.roster.is_member(&id(B)));
    }

    #[test]
    fn slot_is_single_use_and_expires() {
        let t0 = Instant::now();
        let mut s = Slot::new(code("tremor"), t0);
        assert!(s.take(t0).is_ok());
        let e = s.take(t0).unwrap_err().to_string();
        assert!(e.contains("already used"), "{e}");
        let mut s = Slot::new(code("tremor"), t0);
        let e = s.take(t0 + CODE_LIFETIME).unwrap_err().to_string();
        assert!(e.contains("expired"), "{e}");
        assert!(s.take(t0).is_err(), "an expired code stays dead");
    }

    /// Mallory knows the nameplate (it is not secret) and sits between the two machines,
    /// guessing the words. Her one guess against the inviter fails and burns the code, and
    /// she cannot open the joiner's sealed hello, so she learns neither real id.
    #[tokio::test]
    async fn man_in_the_middle_with_nameplate_learns_nothing() {
        let (pi, to_inviter) = pipe();
        let (pj, to_joiner) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let real = code("tremor");
        let mut slot = Slot::new(real.clone(), Instant::now());
        let mallory = async move {
            let guess = code("zoom");
            let ((mut jr, mut jw), (mut ir, mut iw)) = (to_joiner, to_inviter);
            // Towards the joiner, pretend to be the inviter.
            let jfirst = read_first(&mut jr, STEP_TIMEOUT).await.unwrap();
            let (as_inv, m) = Pake::start(PairRole::Inviter, &guess);
            send_plain(&mut jw, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
            let (_, mut from_j) = as_inv.finish(&jfirst).unwrap().channel(PairRole::Inviter);
            let Incoming::Sealed(joiner_hello) = read_msg(&mut jr, STEP_TIMEOUT).await.unwrap() else { panic!("no hello") };
            let learned_joiner = from_j.open(&joiner_hello).is_ok();
            assert!(!joiner_hello.windows(32).any(|w| w == id(B).0), "the joiner's id must not travel in clear");
            // Towards the inviter, pretend to be the joiner with the same guess.
            let (as_join, m) = Pake::start(PairRole::Joiner, &guess);
            send_plain(&mut iw, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
            let Incoming::Plain(Plain::Spake { msg, .. }) = read_msg(&mut ir, STEP_TIMEOUT).await.unwrap() else { panic!() };
            let k = as_join.finish(&decode_spake(&msg).unwrap()).unwrap();
            let (mut to_i, mut from_i) = k.channel(PairRole::Joiner);
            let evil = key(0xee);
            let fake = Hello {
                id: MachineId::from_endpoint(&evil.public()),
                name: "evil".into(),
                version: "x".into(),
                proof: k.prove(PairRole::Joiner, &evil),
            };
            send_sealed(&mut iw, &mut to_i, &fake).await.unwrap();
            let learned_inviter = match read_msg(&mut ir, STEP_TIMEOUT).await {
                Ok(Incoming::Sealed(ct)) => from_i.open(&ct).is_ok(),
                _ => false,
            };
            (learned_joiner, learned_inviter)
        };
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (i, j, (lj, li)) =
            tokio::join!(inviter(pi, &mut slot, mesh(), tx.clone(), true, saved.clone()), joiner(pj, real, tx, true), mallory);
        assert!(!lj, "mallory opened the joiner's hello");
        assert!(!li, "mallory got a sealed answer from the inviter");
        assert!(i.err().unwrap().to_string().contains("burnt"));
        assert!(j.is_err());
        assert!(saved.lock().unwrap().is_empty());
        assert!(slot.take(Instant::now()).is_err(), "the code is burnt after mallory's one guess");
    }

    /// The mallory of the reviews (PAIR-3, PAIR-4, PAIR-7): she has the right code (read from
    /// a process list), plays the joiner, and tries (a) to register an id that is not a key,
    /// (b) to register someone else's key, (c) to read the roster before any human said yes.
    #[tokio::test]
    async fn a_code_holder_cannot_claim_a_key_it_does_not_hold_or_read_the_roster_early() {
        let someone_else = key(0x55);
        let not_a_key = MachineId([2; 32]);
        assert!(not_a_key.to_endpoint().is_err(), "the test needs bytes that are not a curve point");
        for (claimed, signer) in [(not_a_key, key(0x66)), (MachineId::from_endpoint(&someone_else.public()), key(0x66))] {
            let (pi, (mut mr, mut mw)) = pipe();
            let (tx, _rx) = mpsc::unbounded_channel();
            let c = code("tremor");
            let mut slot = Slot::new(c.clone(), Instant::now());
            let saved = Arc::new(Mutex::new(Vec::new()));
            let mallory = async move {
                let (p, m) = Pake::start(PairRole::Joiner, &c);
                send_plain(&mut mw, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
                let Incoming::Plain(Plain::Spake { msg, .. }) = read_msg(&mut mr, STEP_TIMEOUT).await.unwrap() else { panic!() };
                let k = p.finish(&decode_spake(&msg).unwrap()).unwrap();
                let (mut s, _) = k.channel(PairRole::Joiner);
                let hello = Hello { id: claimed, name: "ghost".into(), version: "x".into(), proof: k.prove(PairRole::Joiner, &signer) };
                send_sealed(&mut mw, &mut s, &hello).await.unwrap();
                read_msg(&mut mr, STEP_TIMEOUT).await
            };
            let (i, answer) = tokio::join!(inviter(pi, &mut slot, mesh(), tx, true, saved.clone()), mallory);
            assert!(i.unwrap_err().to_string().contains("key"), "the inviter refuses an unproven id");
            assert!(matches!(answer, Ok(Incoming::Plain(Plain::Abort { .. }))), "mallory gets an abort, not a sealed answer");
            assert!(saved.lock().unwrap().is_empty());
        }

        // (c) With her own key she gets as far as the sigil, but the hello she receives
        // carries no roster: that comes only after both humans said yes.
        let (pi, (mut mr, mut mw)) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let c = code("tremor");
        let mut slot = Slot::new(c.clone(), Instant::now());
        let mallory = async move {
            let (p, m) = Pake::start(PairRole::Joiner, &c);
            send_plain(&mut mw, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
            let Incoming::Plain(Plain::Spake { msg, .. }) = read_msg(&mut mr, STEP_TIMEOUT).await.unwrap() else { panic!() };
            let k = p.finish(&decode_spake(&msg).unwrap()).unwrap();
            let (mut s, mut o) = k.channel(PairRole::Joiner);
            let mk = key(0x77);
            let hello = Hello { id: MachineId::from_endpoint(&mk.public()), name: "m".into(), version: "x".into(), proof: k.prove(PairRole::Joiner, &mk) };
            send_sealed(&mut mw, &mut s, &hello).await.unwrap();
            let Incoming::Sealed(ct) = read_msg(&mut mr, STEP_TIMEOUT).await.unwrap() else { panic!() };
            let v: serde_json::Value = serde_json::from_slice(&o.open(&ct).unwrap()).unwrap();
            // The inviter's human says no; mallory hears it.
            let _ = read_msg(&mut mr, STEP_TIMEOUT).await;
            v
        };
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (i, seen) = tokio::join!(inviter(pi, &mut slot, mesh(), tx, false, saved.clone()), mallory);
        assert!(i.is_err());
        assert!(seen.get("roster").is_none() && seen.get("your_entry").is_none(), "{seen}");
        assert!(!seen.to_string().contains("\"entries\""), "{seen}");
        assert!(saved.lock().unwrap().is_empty());
    }

    /// PAIR-3: the check words depend on the session key alone, so an inviter id chosen
    /// after the key is known cannot change them, and a forged inviter id fails its proof.
    #[tokio::test]
    async fn a_forged_inviter_id_is_refused_by_the_joiner() {
        let (pj, (mut mr, mut mw)) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let c = code("tremor");
        let c2 = c.clone();
        let mallory = async move {
            let first = read_first(&mut mr, STEP_TIMEOUT).await.unwrap();
            let (p, m) = Pake::start(PairRole::Inviter, &c2);
            send_plain(&mut mw, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
            let k = p.finish(&first).unwrap();
            let (mut s, _) = k.channel(PairRole::Inviter);
            let _ = read_msg(&mut mr, STEP_TIMEOUT).await;
            // Claims a ground id it does not hold, signed with her own key.
            let hello = Hello { id: MachineId([0x77; 32]), name: "a".into(), version: "x".into(), proof: k.prove(PairRole::Inviter, &key(0x99)) };
            send_sealed(&mut mw, &mut s, &hello).await.unwrap();
            let _ = read_msg(&mut mr, STEP_TIMEOUT).await;
        };
        let (j, _) = tokio::join!(joiner(pj, c, tx, true), mallory);
        let e = format!("{:#}", j.unwrap_err());
        assert!(e.contains("nothing was changed"), "{e}");
    }

    /// PAIR-1: an abort reason is unauthenticated text; it reaches the error (and so the
    /// terminal and the event log) without any control character and cut short.
    #[tokio::test]
    async fn abort_reasons_are_cleaned_and_cut() {
        let (pj, (mut mr, mut mw)) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let evil = format!("\x1b]52;c;Y3VybCBldmlsfHNo\x07\x1b]0;pwned\x07{}", "x".repeat(500));
        let e2 = evil.clone();
        let mallory = async move {
            let _ = read_first(&mut mr, STEP_TIMEOUT).await;
            send_plain(&mut mw, &Plain::Abort { reason: e2 }).await.unwrap();
        };
        let (j, _) = tokio::join!(joiner(pj, code("tremor"), tx, true), mallory);
        let e = format!("{:#}", j.unwrap_err());
        assert!(!e.chars().any(|c| c.is_control()), "{e:?}");
        assert!(e.contains("not verified"), "{e}");
        assert!(e.len() < 400, "{}", e.len());
    }

    /// An honest joiner's recorded messages, replayed to a new inviter session with the same
    /// code, do not get through: the inviter's SPAKE2 share is fresh every time.
    #[tokio::test]
    async fn replayed_first_messages_do_not_open() {
        let c = code("tremor");
        let (jp, jmsg) = Pake::start(PairRole::Joiner, &c);
        let (ip, imsg) = Pake::start(PairRole::Inviter, &c);
        let jk = jp.finish(&imsg).unwrap();
        let (mut js, _) = jk.channel(PairRole::Joiner);
        let _ = ip.finish(&jmsg).unwrap();
        let hello = Hello { id: id(B), name: "b".into(), version: "1".into(), proof: jk.prove(PairRole::Joiner, &key(B)) };
        let sealed_hello = js.seal(&serde_json::to_vec(&hello).unwrap()).unwrap();

        let (pi, (mut xr, mut xw)) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut slot = Slot::new(c, Instant::now());
        let replayer = async move {
            send_plain(&mut xw, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&jmsg) }).await.unwrap();
            let _ = read_msg(&mut xr, STEP_TIMEOUT).await;
            proto::write_frame(&mut xw, FrameType::Data, &sealed_hello).await.unwrap();
            read_msg(&mut xr, STEP_TIMEOUT).await
        };
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (i, answer) = tokio::join!(inviter(pi, &mut slot, mesh(), tx, true, saved), replayer);
        assert!(i.err().unwrap().to_string().contains("wrong code"));
        assert!(matches!(answer, Ok(Incoming::Plain(Plain::Abort { .. }))));
    }

    /// A bit flipped in transit in a sealed message fails the pairing on both sides.
    #[tokio::test]
    async fn tampered_sealed_payload_fails() {
        let (pi, (mut mir, mut miw)) = pipe();
        let (pj, (mut mjr, mut mjw)) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let c = code("tremor");
        let mut slot = Slot::new(c.clone(), Instant::now());
        let j_to_i = async move {
            let mut flipped = false;
            while let Ok(Some(mut f)) = proto::read_frame(&mut mjr).await {
                if f.kind == FrameType::Data && !flipped {
                    f.payload[3] ^= 0x10;
                    flipped = true;
                }
                if proto::write_frame(&mut miw, f.kind, &f.payload).await.is_err() {
                    break;
                }
            }
        };
        let i_to_j = async move {
            while let Ok(Some(f)) = proto::read_frame(&mut mir).await {
                if proto::write_frame(&mut mjw, f.kind, &f.payload).await.is_err() {
                    break;
                }
            }
        };
        let fwd = tokio::spawn(async move { tokio::join!(j_to_i, i_to_j) });
        let saved = Arc::new(Mutex::new(Vec::new()));
        let (i, j) = tokio::join!(inviter(pi, &mut slot, mesh(), tx.clone(), true, saved), joiner(pj, c, tx, true));
        fwd.abort();
        assert!(i.err().unwrap().to_string().contains("wrong code"));
        assert!(j.is_err());
    }
}
