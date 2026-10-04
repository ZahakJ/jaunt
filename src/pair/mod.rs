//! The pairing ritual (DESIGN §6), daemon side: the inviter's temporary rendezvous
//! endpoint, the joiner's dial, and the IPC conversation that reports each step and asks
//! the local human to compare the sigils.

pub mod code;
pub mod crypto;
pub mod flow;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use iroh::Endpoint;
use iroh::endpoint::Connection;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::daemon::Daemon;
use crate::events::EventLog;
use crate::net;
use crate::proto::{self, EventKind, FrameType, IpcRequest, IpcResponse, PAIR_ALPN, PairRole, PairStep, PairUpdate, PeerRef};
use crate::roster::{Roster, now_ms};
use code::Code;
use flow::{Me, Slot, Steps};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(20);
const FAN_OUT_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections the inviter handles at once while waiting for a first message (PAIR-6).
const MAX_PENDING: usize = 16;

/// PAIR-5: codes this machine already tried as a joiner. A code is tried once: whatever
/// went wrong (a wrong guess by someone in the middle, an abort with a plausible excuse),
/// trying the same code again would hand that someone another guess. Kept as hashes, for
/// the life of a code.
#[derive(Default)]
pub struct TriedCodes(std::sync::Mutex<Vec<([u8; 32], Instant)>>);

impl TriedCodes {
    fn key(code: &Code) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"jaunt/pair/tried-code");
        h.update(code.password());
        h.finalize().into()
    }

    /// Record `code` as tried now; an error if it was tried within its lifetime.
    pub fn try_once(&self, code: &Code, now: Instant) -> Result<()> {
        let k = Self::key(code);
        let mut g = self.0.lock().map_err(|_| anyhow!("internal lock poisoned"))?;
        g.retain(|(_, at)| now.duration_since(*at) < flow::CODE_LIFETIME);
        if g.iter().any(|(h, _)| *h == k) {
            bail!("this code was already tried from this machine and is not tried twice; ask for a new one (jaunt link on the other machine)");
        }
        g.push((k, now));
        Ok(())
    }
}

pub enum Request {
    Link,
    Join { code: String, relays: Vec<String> },
}

type Answer = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Run one link or join for an IPC client: stream `pair` updates to it, take its answer at
/// the confirm step (or auto-accept with `yes`), finish with `ok` or `error`. The client
/// closing the socket cancels the pairing.
pub async fn serve_ipc(d: Arc<Daemon>, s: UnixStream, req: Request, yes: bool) -> Result<()> {
    let (mut rd, mut wr) = s.into_split();
    let Ok(_one_at_a_time) = d.pairing.try_lock() else {
        let msg = "another link or join is already running on this machine".to_string();
        return Ok(proto::write_json(&mut wr, FrameType::Json, &IpcResponse::Error { message: msg }).await?);
    };
    let role = match req {
        Request::Link => PairRole::Inviter,
        Request::Join { .. } => PairRole::Joiner,
    };
    let (steps_tx, mut steps_rx) = mpsc::unbounded_channel::<PairStep>();
    let (answer_tx, answer_rx) = oneshot::channel::<bool>();
    let (gone_tx, mut gone_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut answer_tx = Some(answer_tx);
        while let Ok(Some(f)) = proto::read_frame(&mut rd).await {
            if let Ok(IpcRequest::PairAnswer { accept }) = proto::parse_json(&f.payload, proto::MAX_HEADER)
                && let Some(tx) = answer_tx.take()
            {
                let _ = tx.send(accept);
            }
        }
        let _ = gone_tx.send(());
    });
    let answer: Answer = if yes { Box::pin(async { true }) } else { Box::pin(async move { answer_rx.await.unwrap_or(false) }) };

    let work = async {
        match req {
            Request::Link => invite(&d, &steps_tx, answer).await,
            Request::Join { code, relays } => join(&d, &code, relays, &steps_tx, answer).await,
        }
    };
    tokio::pin!(work);
    let mut client_here = true;
    let result = loop {
        tokio::select! {
            r = &mut work => break r,
            Some(step) = steps_rx.recv() => send_step(&d.events, &mut wr, role, step, &mut client_here).await,
            _ = &mut gone_rx, if client_here => break Err(anyhow!("cancelled (the command was stopped)")),
        }
    };
    while let Ok(step) = steps_rx.try_recv() {
        send_step(&d.events, &mut wr, role, step, &mut client_here).await;
    }
    let last = match result {
        Ok(message) => IpcResponse::Ok { message },
        Err(e) => {
            let reason = format!("{e:#}");
            send_step(&d.events, &mut wr, role, PairStep::Failed { reason: reason.clone() }, &mut client_here).await;
            IpcResponse::Error { message: reason }
        }
    };
    let _ = proto::write_json(&mut wr, FrameType::Json, &last).await;
    Ok(())
}

async fn send_step(events: &EventLog, wr: &mut tokio::net::unix::OwnedWriteHalf, role: PairRole, step: PairStep, client_here: &mut bool) {
    // The event log never carries the code itself, nor the sigil and words (PAIR-9).
    let logged = match &step {
        PairStep::Waiting { code, expires_at_ms, relays, first_machine } => PairStep::Waiting {
            code: code.split('-').next().map(|n| format!("{n}-…")).unwrap_or_default(),
            expires_at_ms: *expires_at_ms,
            relays: relays.clone(),
            first_machine: *first_machine,
        },
        PairStep::Confirm { other, .. } => PairStep::Confirm { other: other.clone(), sigil: Vec::new(), hue: 0, words: Vec::new() },
        other => other.clone(),
    };
    events.emit(None, EventKind::Pair { role, step: logged });
    if *client_here {
        let msg = IpcResponse::Pair(PairUpdate { role, step });
        if proto::write_json(wr, FrameType::Json, &msg).await.is_err() {
            *client_here = false;
        }
    }
}

fn me(d: &Daemon) -> Me {
    Me {
        id: d.me,
        name: d.roster_snapshot().entries.get(&d.me).map(|e| e.name.clone()).unwrap_or_else(|| d.my_name()),
        key: d.ep.secret_key().clone(),
    }
}

async fn close(conn: &Connection, ep: &Endpoint) {
    conn.close(0u32.into(), b"pairing done");
    let _ = timeout(Duration::from_secs(2), ep.close()).await;
}

// ------------------------------------------------------------------------------ inviter

async fn invite(d: &Arc<Daemon>, steps: &Steps, answer: Answer) -> Result<String> {
    let roster = d.roster_snapshot();
    if roster.tombstones.contains_key(&d.me) {
        bail!("this machine was removed from its mesh; run 'jaunt reset' first");
    }
    let relays = d.relays();
    if relays.is_empty() {
        bail!(
            "no relay is configured, and the new machine finds this one through the relay (set one with: jaunt relay set URL). \
             Linking over the LAN without a relay needs local discovery (mDNS), which this build does not have"
        );
    }
    let relay_strs: Vec<String> = relays.iter().map(|u| u.to_string()).collect();
    // The relay list becomes a fleet setting, so the new machine receives it.
    if roster.fleet.updated_at == 0 {
        d.set_relays(relay_strs.clone())?;
    }
    let first_machine = roster.members().all(|e| e.id == d.me);

    let code = Code::generate()?;
    let mut slot = Slot::new(code.clone(), Instant::now());
    let cfg = d.config();
    let ep = net::build_pair_endpoint(crypto::nameplate_secret(&code.nameplate), &relays, cfg.relay_ca.as_deref(), true).await?;
    let expires_at_ms = now_ms() + flow::CODE_LIFETIME.as_millis() as u64;
    let _ = steps.send(PairStep::Waiting { code: code.to_string(), expires_at_ms, relays: relay_strs, first_machine });
    let res = invite_on(d, &ep, &mut slot, steps, answer).await;
    let _ = timeout(Duration::from_secs(2), ep.close()).await;
    let invited = res?;

    // Both confirmed and the roster was written (by `commit` in the flow): tell the others.
    let peer = invited.peer.clone();
    let members = d.roster_snapshot().members().count();
    let _ = steps.send(PairStep::Linked { other: d.peer_ref(&peer.id), members });
    fan_out(d, &peer, steps).await;
    Ok(format!("linked {} ({})", d.name_of(&peer.id), peer.id.short()))
}

type Started = (Connection, iroh::endpoint::SendStream, iroh::endpoint::RecvStream, Vec<u8>);

/// One incoming connection up to its first message. Someone who connects but never speaks
/// does not use up the code, and does not hold up anyone else either: these run side by
/// side (PAIR-6), and the first well-formed first message takes the code's one attempt.
async fn first_message(incoming: iroh::endpoint::Incoming) -> Option<Started> {
    let conn = timeout(CONNECT_TIMEOUT, incoming).await.ok()?.ok()?;
    if conn.alpn() != PAIR_ALPN {
        conn.close(0u32.into(), b"pairing only");
        return None;
    }
    let Ok(Ok((send, mut recv))) = timeout(FIRST_MESSAGE_TIMEOUT, conn.accept_bi()).await else {
        conn.close(0u32.into(), b"too slow");
        return None;
    };
    match flow::read_first(&mut recv, FIRST_MESSAGE_TIMEOUT).await {
        Ok(f) => Some((conn, send, recv, f)),
        Err(e) => {
            tracing::info!("pairing: ignored a connection that did not start properly: {e:#}");
            conn.close(0u32.into(), b"bad start");
            None
        }
    }
}

async fn invite_on(d: &Arc<Daemon>, ep: &Endpoint, slot: &mut Slot, steps: &Steps, answer: Answer) -> Result<flow::Invited> {
    let deadline = tokio::time::Instant::from_std(slot.expires());
    let mut pending = tokio::task::JoinSet::new();
    let (conn, mut send, mut recv, first) = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                bail!("the code expired before anyone used it (codes last 5 minutes); run 'jaunt link' again")
            }
            incoming = ep.accept() => match incoming {
                None => bail!("the pairing endpoint closed"),
                Some(i) if pending.len() < MAX_PENDING => { pending.spawn(first_message(i)); }
                Some(i) => i.refuse(),
            },
            Some(done) = pending.join_next(), if !pending.is_empty() => {
                if let Ok(Some(started)) = done {
                    break started;
                }
            }
        }
    };
    // The others are dropped (and so closed): the code has one attempt only.
    pending.abort_all();
    let _ = steps.send(PairStep::Rendezvous);
    // From here this attempt is the code's only one, whatever happens.
    let code = slot.take(Instant::now())?;
    let d2 = d.clone();
    let commit = move |entry: &crate::roster::Entry| -> Result<()> {
        let entry = entry.clone();
        let name = entry.name.clone();
        d2.edit_roster(format!("linked {name}"), move |r| {
            if r.tombstones.contains_key(&entry.id) {
                bail!("{name} was unlinked meanwhile");
            }
            let mut add = Roster::default();
            add.entries.insert(entry.id, entry.clone());
            Ok(r.merge(&add))
        })?;
        Ok(())
    };
    let res = flow::invite(&mut recv, &mut send, &code, &first, &me(d), || d.roster_snapshot(), now_ms(), steps, answer, commit).await;
    let _ = send.finish();
    let _ = timeout(Duration::from_secs(2), send.stopped()).await;
    close(&conn, ep).await;
    res
}

/// Push the new roster to every other member and report each acknowledgement.
async fn fan_out(d: &Arc<Daemon>, new: &PeerRef, steps: &Steps) {
    let others: Vec<_> = d.roster_snapshot().members().map(|e| e.id).filter(|id| *id != d.me && *id != new.id).collect();
    let mut tasks = Vec::new();
    for id in others {
        let d2 = d.clone();
        let new_id = new.id;
        tasks.push(tokio::spawn(async move {
            let res = d2.sync_once(id, None, FAN_OUT_TIMEOUT).await;
            let (ok, detail) = match res {
                Ok(theirs) if theirs.is_member(&new_id) => (true, "knows the new machine".to_string()),
                Ok(_) => (false, "answered but did not take the new entry".to_string()),
                Err(e) => (false, format!("not reached ({e:#}); it learns on its next sync")),
            };
            (d2.peer_ref(&id), ok, detail)
        }));
    }
    let (mut told, mut not_yet) = (0, 0);
    for t in tasks {
        if let Ok((peer, ok, detail)) = t.await {
            if ok { told += 1 } else { not_yet += 1 }
            let _ = steps.send(PairStep::FanOut { machine: peer, ok, detail });
        }
    }
    let _ = steps.send(PairStep::FanOutDone { told, not_yet });
}

// ------------------------------------------------------------------------------- joiner

async fn join(d: &Arc<Daemon>, code: &str, relays: Vec<String>, steps: &Steps, answer: Answer) -> Result<String> {
    let code = Code::parse(code)?;
    let roster = d.roster_snapshot();
    if roster.tombstones.contains_key(&d.me) {
        bail!("this machine's key was unlinked; run 'jaunt reset' first, then join again");
    }
    let others: Vec<String> = roster.members().filter(|e| e.id != d.me).map(|e| e.name.clone()).collect();
    if !others.is_empty() {
        bail!(
            "this machine is already linked with {}; to add another machine run 'jaunt link' here instead, \
             or 'jaunt leave' first to move this one to a different mesh",
            others.join(", ")
        );
    }
    let relays = if relays.is_empty() { d.relays() } else { net::parse_relays(&relays)? };
    if relays.is_empty() {
        bail!("no relay is known here; give the one the other machine uses: jaunt join --relay URL");
    }
    let relay_strs: Vec<String> = relays.iter().map(|u| u.to_string()).collect();
    let cfg = d.config();
    let ep = net::build_pair_endpoint(iroh::SecretKey::generate(), &relays, cfg.relay_ca.as_deref(), false).await?;
    let _ = steps.send(PairStep::Dialing { relays: relay_strs });
    let res = join_on(d, &ep, &code, &relays, steps, answer).await;
    let _ = timeout(Duration::from_secs(2), ep.close()).await;
    let joined = res?;

    let inviter = joined.inviter.clone();
    let new = joined.roster;
    d.edit_roster(format!("joined the mesh through {}", inviter.name), move |r| {
        *r = new;
        Ok(true)
    })?;
    let members = d.roster_snapshot().members().count();
    let _ = steps.send(PairStep::Linked { other: d.peer_ref(&inviter.id), members });
    Ok(format!("linked to {} ({}); the mesh has {members} machines", d.name_of(&inviter.id), inviter.id.short()))
}

async fn join_on(d: &Arc<Daemon>, ep: &Endpoint, code: &Code, relays: &[iroh::RelayUrl], steps: &Steps, answer: Answer) -> Result<flow::Joined> {
    let target = crypto::nameplate_secret(&code.nameplate).public();
    let addr = net::dial_addr(crate::roster::MachineId::from_endpoint(&target), relays, &[])?;
    let conn = match timeout(CONNECT_TIMEOUT, ep.connect(addr, PAIR_ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => bail!("cannot reach the inviting machine ({e:#}); is 'jaunt link' still showing this code, and do both machines use the same relay?"),
        Err(_) => bail!("no inviting machine answered within {} s; is 'jaunt link' still showing this code, and do both machines use the same relay?", CONNECT_TIMEOUT.as_secs()),
    };
    let _ = steps.send(PairStep::Rendezvous);
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| anyhow!("cannot talk to the inviting machine: {e}"))?;
    // The first message spends the code on this machine too (PAIR-5).
    d.tried_codes.try_once(code, Instant::now())?;
    let res = flow::join(&mut recv, &mut send, code, &me(d), steps, answer).await;
    let _ = send.finish();
    let _ = timeout(Duration::from_secs(2), send.stopped()).await;
    close(&conn, ep).await;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PAIR-5: a code is tried once from the joining machine, whatever happened the first
    /// time; after the code's lifetime the record is forgotten.
    #[test]
    fn a_joiner_tries_each_code_once() {
        let t = TriedCodes::default();
        let c = Code::parse("4127-tremor-kudos").unwrap();
        let t0 = Instant::now();
        assert!(t.try_once(&c, t0).is_ok());
        let e = t.try_once(&Code::parse("4127 Tremor kudos").unwrap(), t0).unwrap_err().to_string();
        assert!(e.contains("already tried"), "{e}");
        assert!(t.try_once(&Code::parse("4128-tremor-kudos").unwrap(), t0).is_ok(), "another code is fine");
        assert!(t.try_once(&c, t0 + flow::CODE_LIFETIME).is_ok(), "forgotten once the code is dead anyway");
    }
}
