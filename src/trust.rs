//! Revocation (design §7), daemon side: `unlink` tombstones machines here (which at once
//! closes their connections and ends their processes, see `Daemon::roster_changed`), then
//! pushes the tombstones to every other member and reports who confirmed. `leave` pushes
//! this machine's own tombstone first and applies it here last, so the pushes still go out.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::net::UnixStream;

use crate::daemon::Daemon;
use crate::proto::{self, EventKind, FrameType, IpcResponse, PeerRef, UnlinkReport};
use crate::roster::{MachineId, Roster, now_ms};

const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

pub enum Request {
    Unlink { target: String, cascade: bool, kill_sessions: bool },
    Leave,
}

pub async fn serve_ipc(d: Arc<Daemon>, mut s: UnixStream, req: Request) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<IpcResponse>();
    let work = async {
        match req {
            Request::Unlink { target, cascade, kill_sessions } => unlink(&d, &target, cascade, kill_sessions, &tx).await,
            Request::Leave => leave(&d, &tx).await,
        }
    };
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            r = &mut work => break r,
            Some(m) = rx.recv() => { let _ = proto::write_json(&mut s, FrameType::Json, &m).await; }
        }
    };
    while let Ok(m) = rx.try_recv() {
        let _ = proto::write_json(&mut s, FrameType::Json, &m).await;
    }
    let last = match result {
        Ok(report) => IpcResponse::Unlinked(report),
        Err(e) => IpcResponse::Error { message: format!("{e:#}") },
    };
    proto::write_json(&mut s, FrameType::Json, &last).await?;
    Ok(())
}

fn refs(d: &Daemon, roster: &Roster, ids: impl IntoIterator<Item = MachineId>) -> Vec<PeerRef> {
    ids.into_iter()
        .map(|id| PeerRef { id, name: roster.entries.get(&id).map(|e| e.name.clone()).unwrap_or_else(|| d.name_of(&id)) })
        .collect()
}

/// Push to every listed machine at once; `heard` decides from its answer whether it took
/// the change. Reports each answer on `tx` and as an event.
async fn push_and_collect(
    d: &Arc<Daemon>,
    to: Vec<MachineId>,
    push: Option<Roster>,
    heard: impl Fn(&Roster) -> bool + Send + Sync + Clone + 'static,
    tx: &tokio::sync::mpsc::UnboundedSender<IpcResponse>,
) -> (Vec<PeerRef>, Vec<PeerRef>) {
    let mut tasks = Vec::new();
    for id in to {
        let d2 = d.clone();
        let push = push.clone();
        let heard = heard.clone();
        tasks.push(tokio::spawn(async move {
            let peer = d2.peer_ref(&id);
            let (ok, detail) = match d2.sync_once(id, push.as_ref(), PUSH_TIMEOUT).await {
                Ok(theirs) if heard(&theirs) => (true, "confirmed".to_string()),
                Ok(_) => (false, "answered without taking the change".to_string()),
                Err(e) => (false, format!("not reached: {e:#}")),
            };
            (peer, ok, detail)
        }));
    }
    let (mut yes, mut no) = (Vec::new(), Vec::new());
    for t in tasks {
        let Ok((peer, ok, detail)) = t.await else { continue };
        d.events.emit(Some(peer.clone()), EventKind::Told { ok, detail: detail.clone() });
        let _ = tx.send(IpcResponse::Told { peer: peer.clone(), ok, detail });
        if ok { yes.push(peer) } else { no.push(peer) }
    }
    (yes, no)
}

async fn unlink(
    d: &Arc<Daemon>,
    target: &str,
    cascade: bool,
    kill_sessions: bool,
    tx: &tokio::sync::mpsc::UnboundedSender<IpcResponse>,
) -> Result<UnlinkReport> {
    let id = d.resolve_unlink(target, cascade || kill_sessions)?;
    let roster = d.roster_snapshot();
    let already: Vec<MachineId> = roster.tombstones.contains_key(&id).then_some(id).into_iter().collect();
    let added: BTreeSet<MachineId> = roster.added_by_closure(&id).into_iter().filter(|x| *x != d.me).collect();
    let mut victims: BTreeSet<MachineId> = [id].into();
    if cascade {
        victims.extend(added.iter().copied());
    }
    let sessions_killed: usize = victims.iter().map(|v| d.children.count_for(v)).sum();
    let me = d.me;
    let names: Vec<String> = victims.iter().map(|v| d.name_of(v)).collect();
    let list = victims.clone();
    d.edit_roster(format!("unlinked {}", names.join(", ")), move |r| {
        let now = now_ms();
        let mut changed = false;
        for v in &list {
            changed |= r.revoke(*v, me, now);
        }
        Ok(changed)
    })?;
    let after = d.roster_snapshot();
    let others: Vec<MachineId> = after.members().map(|e| e.id).filter(|x| *x != d.me).collect();
    let wanted = victims.clone();
    let (confirmed, not_heard) =
        push_and_collect(d, others, None, move |r: &Roster| wanted.iter().all(|v| r.tombstones.contains_key(v)), tx).await;
    let tmux_sessions_killed = if kill_sessions {
        let of: Vec<MachineId> = victims.iter().copied().collect();
        let mut n = d.kill_tmux_sessions_of(&of).await;
        // The members that confirmed hold the tombstone, so they agree to end them too.
        let mut tasks = Vec::new();
        for p in &confirmed {
            let (d2, id, of) = (d.clone(), p.id, of.clone());
            tasks.push(tokio::spawn(async move { (id, d2.remote_kill_sessions(id, of).await) }));
        }
        for t in tasks {
            let Ok((id, res)) = t.await else { continue };
            let peer = d.peer_ref(&id);
            let (ok, detail) = match res {
                Ok(k) => {
                    n += k;
                    (true, format!("ended {k} tmux session(s) the revoked machine created there"))
                }
                Err(e) => (false, format!("could not end its tmux sessions there: {e:#}")),
            };
            let _ = tx.send(IpcResponse::Told { peer, ok, detail });
        }
        Some(n)
    } else {
        None
    };
    Ok(UnlinkReport {
        revoked: refs(d, &after, victims),
        confirmed,
        not_heard,
        added_by_revoked: if cascade { Vec::new() } else { refs(d, &after, added) },
        sessions_killed: sessions_killed as u32,
        tmux_sessions_killed,
        already_revoked: refs(d, &after, already),
    })
}

async fn leave(d: &Arc<Daemon>, tx: &tokio::sync::mpsc::UnboundedSender<IpcResponse>) -> Result<UnlinkReport> {
    let roster = d.roster_snapshot();
    if roster.tombstones.contains_key(&d.me) {
        bail!("this machine is already out of the mesh");
    }
    let mut gone = roster.clone();
    gone.revoke(d.me, d.me, now_ms());
    let others: Vec<MachineId> = roster.members().map(|e| e.id).filter(|x| *x != d.me).collect();
    let me = d.me;
    let (confirmed, not_heard) =
        push_and_collect(d, others, Some(gone.clone()), move |r: &Roster| r.tombstones.contains_key(&me), tx).await;
    let sessions_killed = d.children.peers().iter().map(|p| d.children.count_for(p)).sum::<usize>() as u32;
    d.edit_roster("left the mesh".into(), move |r| Ok(r.merge(&gone)))?;
    Ok(UnlinkReport {
        revoked: vec![d.me_ref()],
        confirmed,
        not_heard,
        added_by_revoked: Vec::new(),
        sessions_killed,
        tmux_sessions_killed: None,
        already_revoked: Vec::new(),
    })
}
