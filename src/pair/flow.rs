//! The pairing conversation on one bidirectional stream, for both roles. Generic over the
//! stream so the unit tests can run it over in-memory pipes.
//!
//! ```text
//! joiner                                   inviter
//!   Json  {t:spake, v, msg: B}  ──────────▸   (code burnt on arrival)
//!         ◂──────────  Json {t:spake, v, msg: A}
//!   Data  seal(JoinerHello{id,name})  ────▸   open fails → Json {t:abort,"wrong code"}
//!         ◂──────────  Data seal(InviterHello{id,name,roster,your_entry})
//!   (both show sigil + words, ask their human)
//!   Data  seal(Confirm{ok})  ◂──────────▸  Data seal(Confirm{ok})
//!   (roster written on each side only after both said yes)
//! ```

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::code::Code;
use super::crypto::{HumanCheck, NotOpened, Opener, Pake, Sealer};
use crate::proto::{self, FrameType, MAX_FRAME, PairRole, PairStep, PeerRef};
use crate::roster::{Entry, MachineId, Roster, valid_name};

pub const PAIR_VERSION: u32 = 1;
/// How long a code stays valid.
pub const CODE_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Per-message wait while the machines talk to each other.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a human has to compare the sigils.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3 * 60);
const MAX_PLAIN: usize = 4 * 1024;
const MAX_JOINER_HELLO: usize = 4 * 1024;

pub type Steps = mpsc::UnboundedSender<PairStep>;

/// Unauthenticated messages.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Plain {
    Spake { v: u32, msg: String },
    Abort { reason: String },
}

#[derive(Debug, Serialize, Deserialize)]
struct JoinerHello {
    id: MachineId,
    name: String,
    version: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct InviterHello {
    id: MachineId,
    name: String,
    version: String,
    roster: Roster,
    your_entry: Entry,
}

#[derive(Debug, Serialize, Deserialize)]
struct Confirm {
    ok: bool,
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
}

pub struct Invited {
    pub peer: PeerRef,
    /// The joiner's roster entry, to be merged once both confirmed.
    pub entry: Entry,
}

pub struct Joined {
    pub inviter: PeerRef,
    /// The mesh as the inviter sent it, with this machine's entry in it.
    pub roster: Roster,
}

enum Incoming {
    Plain(Plain),
    Sealed(Vec<u8>),
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
        Incoming::Plain(Plain::Abort { reason }) => bail!("the other machine stopped: {reason}"),
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

/// Read the joiner's first message (before the code is taken from its slot).
pub async fn read_first<R: AsyncRead + Unpin>(r: &mut R, wait: Duration) -> Result<Vec<u8>> {
    match read_msg(r, wait).await? {
        Incoming::Plain(Plain::Spake { v, msg }) if v == PAIR_VERSION => decode_spake(&msg),
        Incoming::Plain(Plain::Spake { v, .. }) => {
            bail!("the other machine speaks pairing version {v}, this one {PAIR_VERSION}; update jaunt on the older one")
        }
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

/// Inviter side, after the joiner's first message arrived and the code was taken.
/// `roster` is this machine's roster now; `now_ms` stamps the new entry.
#[allow(clippy::too_many_arguments)]
pub async fn invite<R, W, A>(
    r: &mut R,
    w: &mut W,
    code: &Code,
    first: &[u8],
    me: &Me,
    roster: &Roster,
    now_ms: u64,
    steps: &Steps,
    answer: A,
) -> Result<Invited>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: Future<Output = bool>,
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

    let hello: JoinerHello = match read_sealed(r, &mut opener, STEP_TIMEOUT).await {
        Ok(pt) => proto::parse_json(&pt, MAX_JOINER_HELLO)?,
        Err(e) if e.is::<NotOpened>() => {
            abort(w, "wrong code; that code is burnt now, ask for a new one (jaunt link)").await;
            bail!("someone tried a wrong code; the code is burnt. Run 'jaunt link' again for a new one");
        }
        Err(e) => return Err(e),
    };
    let refusal = if hello.id == me.id {
        Some("a machine cannot link to itself".to_string())
    } else if roster.tombstones.contains_key(&hello.id) {
        Some(format!(
            "machine {} was unlinked from this mesh and its key can never come back; run 'jaunt reset' on it, then join again",
            hello.id.short()
        ))
    } else if roster.is_member(&hello.id) {
        Some(format!("machine {} is already linked", hello.id.short()))
    } else if !valid_name(&hello.name) {
        Some(format!("'{}' is not a valid machine name; rename it with 'jaunt name' first", hello.name))
    } else {
        None
    };
    if let Some(why) = refusal {
        abort(w, &why).await;
        bail!(why);
    }
    let peer = PeerRef { id: hello.id, name: hello.name.clone() };
    let _ = steps.send(PairStep::Sealed { other: peer.clone() });
    let entry = Entry { id: hello.id, name: hello.name, added_at: now_ms, added_by: me.id, named_at: now_ms };
    let reply = InviterHello {
        id: me.id,
        name: me.name.clone(),
        version: proto::VERSION.into(),
        roster: roster.clone(),
        your_entry: entry.clone(),
    };
    send_sealed(w, &mut sealer, &reply).await?;
    let _ = steps.send(confirm_step(&peer, key.human_check(&me.id, &peer.id)));
    confirm_both(r, w, &mut sealer, &mut opener, steps, answer).await?;
    Ok(Invited { peer, entry })
}

/// Joiner side. `me` is this machine's real id and name.
pub async fn join<R, W, A>(r: &mut R, w: &mut W, code: &Code, me: &Me, steps: &Steps, answer: A) -> Result<Joined>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    A: Future<Output = bool>,
{
    let (pake, msg) = Pake::start(PairRole::Joiner, code);
    send_plain(w, &Plain::Spake { v: PAIR_VERSION, msg: data_encoding::HEXLOWER.encode(&msg) }).await?;
    let theirs = match read_msg(r, STEP_TIMEOUT).await? {
        Incoming::Plain(Plain::Spake { msg, .. }) => decode_spake(&msg)?,
        Incoming::Plain(Plain::Abort { reason }) => bail!("the other machine stopped: {reason}"),
        Incoming::Sealed(_) => bail!("unexpected message during pairing"),
    };
    let key = pake.finish(&theirs)?;
    let _ = steps.send(PairStep::KeyExchange);
    let (mut sealer, mut opener) = key.channel(PairRole::Joiner);
    send_sealed(w, &mut sealer, &JoinerHello { id: me.id, name: me.name.clone(), version: proto::VERSION.into() }).await?;

    let pt = match read_sealed(r, &mut opener, STEP_TIMEOUT).await {
        Ok(pt) => pt,
        Err(e) if e.is::<NotOpened>() => {
            abort(w, "the answer did not open").await;
            bail!("the other side's answer did not open with this code: someone may be in the middle. Nothing was changed")
        }
        Err(e) => return Err(e),
    };
    let hello: InviterHello = proto::parse_json(&pt, MAX_FRAME)?;
    let mut roster = hello.roster;
    roster.sanitize_remote();
    let checked = roster.validate().and_then(|_| {
        let e = &hello.your_entry;
        if e.id != me.id || e.added_by != hello.id || !valid_name(&e.name) {
            bail!("the inviter sent an entry that is not for this machine");
        }
        if !roster.is_member(&hello.id) {
            bail!("the inviter is not a member of its own roster");
        }
        if roster.tombstones.contains_key(&me.id) {
            bail!("this machine's key was unlinked from that mesh; run 'jaunt reset' and join again");
        }
        Ok(())
    });
    if let Err(e) = checked {
        abort(w, &format!("{e:#}")).await;
        return Err(e);
    }
    let mut incoming = Roster::default();
    incoming.entries.insert(me.id, hello.your_entry.clone());
    roster.merge(&incoming);
    roster.validate().context("the inviter's roster is full")?;
    let inviter = PeerRef { id: hello.id, name: hello.name };
    let _ = steps.send(PairStep::Sealed { other: inviter.clone() });
    let _ = steps.send(confirm_step(&inviter, key.human_check(&hello.id, &me.id)));
    confirm_both(r, w, &mut sealer, &mut opener, steps, answer).await?;
    Ok(Joined { inviter, roster })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::Entry;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf, split};

    type Pipe = (ReadHalf<DuplexStream>, WriteHalf<DuplexStream>);
    type Checks = Vec<(Vec<String>, Vec<String>)>;

    const A: MachineId = MachineId([0xa; 32]);
    const B: MachineId = MachineId([0xb; 32]);

    fn pipe() -> (Pipe, Pipe) {
        let (a, b) = tokio::io::duplex(4 << 20);
        (split(a), split(b))
    }

    fn code(w: &str) -> Code {
        Code { nameplate: "4127".into(), words: [w.into(), "kudos".into()] }
    }

    fn mesh() -> Roster {
        let mut r = Roster::default();
        r.entries.insert(A, Entry { id: A, name: "a".into(), added_at: 1, added_by: A, named_at: 1 });
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
    async fn inviter(p: Pipe, slot: &mut Slot, roster: Roster, tx: Steps, yes: bool) -> Result<Invited> {
        let (mut r, mut w) = p;
        let first = read_first(&mut r, STEP_TIMEOUT).await?;
        let c = slot.take(Instant::now())?;
        invite(&mut r, &mut w, &c, &first, &Me { id: A, name: "a".into() }, &roster, 100, &tx, async move { yes }).await
    }

    async fn joiner(p: Pipe, c: Code, tx: Steps, yes: bool) -> Result<Joined> {
        let (mut r, mut w) = p;
        join(&mut r, &mut w, &c, &Me { id: B, name: "b".into() }, &tx, async move { yes }).await
    }

    async fn run_pair(inv: Code, joi: Code, roster: Roster, yes: (bool, bool)) -> (Result<Invited>, Result<Joined>, Checks, Slot) {
        let (pi, pj) = pipe();
        let (itx, mut irx) = mpsc::unbounded_channel();
        let (jtx, mut jrx) = mpsc::unbounded_channel();
        let mut slot = Slot::new(inv, Instant::now());
        let (i, j) = tokio::join!(inviter(pi, &mut slot, roster, itx, yes.0), joiner(pj, joi, jtx, yes.1));
        let mut c = checks(&mut irx);
        c.extend(checks(&mut jrx));
        (i, j, c, slot)
    }

    #[tokio::test]
    async fn honest_pairing_links_and_checks_match() {
        let (i, j, c, _) = run_pair(code("tremor"), code("tremor"), mesh(), (true, true)).await;
        let i = i.unwrap();
        let j = j.unwrap();
        assert_eq!(i.peer.name, "b");
        assert_eq!(j.inviter.name, "a");
        assert!(j.roster.is_member(&A) && j.roster.is_member(&B));
        assert_eq!(j.roster.entries[&B], i.entry);
        assert_eq!(i.entry.added_by, A);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], c[1], "both screens show the same sigil and words");
    }

    #[tokio::test]
    async fn wrong_code_fails_both_sides_and_burns() {
        let (i, j, c, mut slot) = run_pair(code("tremor"), code("zoom"), mesh(), (true, true)).await;
        let ie = i.err().expect("inviter must fail").to_string();
        let je = j.err().expect("joiner must fail").to_string();
        assert!(ie.contains("wrong code") && ie.contains("burnt"), "{ie}");
        assert!(je.contains("wrong code"), "{je}");
        assert!(c.is_empty(), "no sigil is shown for a wrong code");
        assert!(slot.take(Instant::now()).is_err(), "the right code no longer works after a wrong try");
    }

    #[tokio::test]
    async fn a_no_on_either_side_links_nothing() {
        for yes in [(true, false), (false, true)] {
            let (i, j, _, _) = run_pair(code("tremor"), code("tremor"), mesh(), yes).await;
            assert!(i.is_err() && j.is_err(), "{yes:?}");
        }
    }

    #[tokio::test]
    async fn revoked_joiner_is_refused() {
        let mut roster = mesh();
        roster.revoke(B, A, 5);
        let (i, j, _, _) = run_pair(code("tremor"), code("tremor"), roster, (true, true)).await;
        assert!(i.err().unwrap().to_string().contains("unlinked"));
        assert!(j.err().unwrap().to_string().contains("unlinked"));
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
            send_plain(&mut jw, &Plain::Spake { v: 1, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
            let (_, mut from_j) = as_inv.finish(&jfirst).unwrap().channel(PairRole::Inviter);
            let Incoming::Sealed(joiner_hello) = read_msg(&mut jr, STEP_TIMEOUT).await.unwrap() else { panic!("no hello") };
            let learned_joiner = from_j.open(&joiner_hello).is_ok();
            assert!(!joiner_hello.windows(32).any(|w| w == B.0), "the joiner's id must not travel in clear");
            // Towards the inviter, pretend to be the joiner with the same guess.
            let (as_join, m) = Pake::start(PairRole::Joiner, &guess);
            send_plain(&mut iw, &Plain::Spake { v: 1, msg: data_encoding::HEXLOWER.encode(&m) }).await.unwrap();
            let Incoming::Plain(Plain::Spake { msg, .. }) = read_msg(&mut ir, STEP_TIMEOUT).await.unwrap() else { panic!() };
            let (mut to_i, mut from_i) = as_join.finish(&decode_spake(&msg).unwrap()).unwrap().channel(PairRole::Joiner);
            let fake = JoinerHello { id: MachineId([0xee; 32]), name: "evil".into(), version: "x".into() };
            send_sealed(&mut iw, &mut to_i, &fake).await.unwrap();
            let learned_inviter = match read_msg(&mut ir, STEP_TIMEOUT).await {
                Ok(Incoming::Sealed(ct)) => from_i.open(&ct).is_ok(),
                _ => false,
            };
            (learned_joiner, learned_inviter)
        };
        let (i, j, (lj, li)) =
            tokio::join!(inviter(pi, &mut slot, mesh(), tx.clone(), true), joiner(pj, real, tx, true), mallory);
        assert!(!lj, "mallory opened the joiner's hello");
        assert!(!li, "mallory got a sealed answer from the inviter");
        assert!(i.err().unwrap().to_string().contains("burnt"));
        assert!(j.is_err());
        assert!(slot.take(Instant::now()).is_err(), "the code is burnt after mallory's one guess");
    }

    /// An honest joiner's recorded messages, replayed to a new inviter session with the same
    /// code, do not get through: the inviter's SPAKE2 share is fresh every time.
    #[tokio::test]
    async fn replayed_first_messages_do_not_open() {
        let c = code("tremor");
        let (jp, jmsg) = Pake::start(PairRole::Joiner, &c);
        let (ip, imsg) = Pake::start(PairRole::Inviter, &c);
        let (mut js, _) = jp.finish(&imsg).unwrap().channel(PairRole::Joiner);
        let _ = ip.finish(&jmsg).unwrap();
        let hello = JoinerHello { id: B, name: "b".into(), version: "1".into() };
        let sealed_hello = js.seal(&serde_json::to_vec(&hello).unwrap()).unwrap();

        let (pi, (mut xr, mut xw)) = pipe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut slot = Slot::new(c, Instant::now());
        let replayer = async move {
            send_plain(&mut xw, &Plain::Spake { v: 1, msg: data_encoding::HEXLOWER.encode(&jmsg) }).await.unwrap();
            let _ = read_msg(&mut xr, STEP_TIMEOUT).await;
            proto::write_frame(&mut xw, FrameType::Data, &sealed_hello).await.unwrap();
            read_msg(&mut xr, STEP_TIMEOUT).await
        };
        let (i, answer) = tokio::join!(inviter(pi, &mut slot, mesh(), tx, true), replayer);
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
        let (i, j) = tokio::join!(inviter(pi, &mut slot, mesh(), tx.clone(), true), joiner(pj, c, tx, true));
        fwd.abort();
        assert!(i.err().unwrap().to_string().contains("wrong code"));
        assert!(j.is_err());
    }
}
