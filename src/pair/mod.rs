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
    // The event log never carries the code itself.
    let logged = match &step {
        PairStep::Waiting { code, expires_at_ms, relays, first_machine } => PairStep::Waiting {
            code: code.split('-').next().map(|n| format!("{n}-…")).unwrap_or_default(),
            expires_at_ms: *expires_at_ms,
            relays: relays.clone(),
            first_machine: *first_machine,
        },
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
    Me { id: d.me, name: d.roster_snapshot().entries.get(&d.me).map(|e| e.name.clone()).unwrap_or_else(|| d.my_name()) }
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

    // Both confirmed: write the roster, then tell every other machine.
    let peer = invited.peer.clone();
    let entry = invited.entry;
    d.edit_roster(format!("linked {}", peer.name), |r| {
        if r.tombstones.contains_key(&entry.id) {
            bail!("{} was unlinked meanwhile", peer.name);
        }
        let mut add = Roster::default();
        add.entries.insert(entry.id, entry.clone());
        Ok(r.merge(&add))
    })?;
    let members = d.roster_snapshot().members().count();
    let _ = steps.send(PairStep::Linked { other: d.peer_ref(&peer.id), members });
    fan_out(d, &peer, steps).await;
    Ok(format!("linked {} ({})", d.name_of(&peer.id), peer.id.short()))
}

async fn invite_on(d: &Arc<Daemon>, ep: &Endpoint, slot: &mut Slot, steps: &Steps, answer: Answer) -> Result<flow::Invited> {
    let deadline = tokio::time::Instant::from_std(slot.expires());
    loop {
        let incoming = match tokio::time::timeout_at(deadline, ep.accept()).await {
            Err(_) => bail!("the code expired before anyone used it (codes last 5 minutes); run 'jaunt link' again"),
            Ok(None) => bail!("the pairing endpoint closed"),
            Ok(Some(i)) => i,
        };
        let Ok(Ok(conn)) = timeout(CONNECT_TIMEOUT, incoming).await else { continue };
        if conn.alpn() != PAIR_ALPN {
            conn.close(0u32.into(), b"pairing only");
            continue;
        }
        let _ = steps.send(PairStep::Rendezvous);
        // Someone who connects but never speaks does not use up the code.
        let Ok(Ok((mut send, mut recv))) = timeout(FIRST_MESSAGE_TIMEOUT, conn.accept_bi()).await else {
            conn.close(0u32.into(), b"too slow");
            continue;
        };
        let first = match flow::read_first(&mut recv, FIRST_MESSAGE_TIMEOUT).await {
            Ok(f) => f,
            Err(e) => {
                tracing::info!("pairing: ignored a connection that did not start properly: {e:#}");
                conn.close(0u32.into(), b"bad start");
                continue;
            }
        };
        // From here this attempt is the code's only one, whatever happens.
        let code = slot.take(Instant::now())?;
        let roster = d.roster_snapshot();
        let res = flow::invite(&mut recv, &mut send, &code, &first, &me(d), &roster, now_ms(), steps, answer).await;
        let _ = send.finish();
        let _ = timeout(Duration::from_secs(2), send.stopped()).await;
        close(&conn, ep).await;
        return res;
    }
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
        bail!("no relay is known here; give the one the other machine uses: jaunt join CODE --relay URL");
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
    let addr = net::dial_addr(crate::roster::MachineId::from_endpoint(&target), relays)?;
    let conn = match timeout(CONNECT_TIMEOUT, ep.connect(addr, PAIR_ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => bail!("cannot reach the inviting machine ({e:#}); is 'jaunt link' still showing this code, and do both machines use the same relay?"),
        Err(_) => bail!("no inviting machine answered within {} s; is 'jaunt link' still showing this code, and do both machines use the same relay?", CONNECT_TIMEOUT.as_secs()),
    };
    let _ = steps.send(PairStep::Rendezvous);
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| anyhow!("cannot talk to the inviting machine: {e}"))?;
    let res = flow::join(&mut recv, &mut send, code, &me(d), steps, answer).await;
    let _ = send.finish();
    let _ = timeout(Duration::from_secs(2), send.stopped()).await;
    close(&conn, ep).await;
    res
}
