//! Client side of `jaunt link`, `join`, `unlink`, `leave`: send the request, show each
//! step (`ui::ritual`), ask the human at the sigil check. The conversation itself is the
//! IPC described in docs/internals.md.

use std::io::Write;

use anyhow::{Result, bail};
use tokio::net::unix::OwnedWriteHalf;

use crate::config::Config;
use crate::pair::code::Code;
use crate::paths::Paths;
use crate::proto::{self, FrameType, IpcRequest, IpcResponse, MAX_FRAME, PairRole, UnlinkReport};
use crate::ui::ritual::{Input as RitualInput, Ritual};
use crate::{ipc, net, term};

async fn send(wr: &mut OwnedWriteHalf, req: &IpcRequest) -> Result<()> {
    proto::write_json(wr, FrameType::Json, req).await?;
    Ok(())
}

/// The link/join conversation, shown by `ui::ritual`. Returns the exit code.
async fn pair(paths: &Paths, req: IpcRequest, yes: bool) -> Result<i32> {
    if !yes && !term::stdin_is_tty() {
        bail!("linking needs a terminal to compare the sigils (--yes skips that check; for tests only)");
    }
    let (role, code) = match &req {
        IpcRequest::Join { code, .. } => (PairRole::Joiner, Some(code.clone())),
        _ => (PairRole::Inviter, None),
    };
    let s = ipc::connect(paths).await?;
    let me = crate::status(paths).await.map(|s| s.me.name).unwrap_or_else(|_| crate::config::default_name());
    let (rd, mut wr) = s.into_split();
    send(&mut wr, &req).await?;
    let mut frames = proto::frame_reader(rd);
    let mut view = Ritual::start(role, &me, code, yes);
    loop {
        tokio::select! {
            f = frames.recv() => {
                let f = match f {
                    Some(Ok(f)) => f,
                    Some(Err(e)) => {
                        view.finish(false, &format!("lost the daemon: {e}")).await;
                        return Ok(1);
                    }
                    None => {
                        view.finish(false, "the daemon closed the connection").await;
                        return Ok(1);
                    }
                };
                match proto::parse_json::<IpcResponse>(&f.payload, MAX_FRAME)? {
                    IpcResponse::Pair(u) => view.step(&u),
                    IpcResponse::Ok { message } => {
                        view.finish(true, &message).await;
                        return Ok(0);
                    }
                    IpcResponse::Error { message } => {
                        view.finish(false, &message).await;
                        return Ok(1);
                    }
                    _ => {}
                }
            }
            i = view.input() => match i {
                RitualInput::Answer(accept) => send(&mut wr, &IpcRequest::PairAnswer { accept }).await?,
                RitualInput::Cancel => {
                    view.finish(false, "cancelled").await;
                    return Ok(130);
                }
            },
        }
    }
}

pub async fn link(paths: &Paths, yes: bool) -> Result<i32> {
    pair(paths, IpcRequest::Link { yes }, yes).await
}

fn prompt_relay() -> Result<String> {
    print!("This machine does not know a relay yet. Relay URL (on the other machine, 'jaunt relay' shows it): ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

pub async fn join(paths: &Paths, code_words: Vec<String>, relay: Option<String>, yes: bool) -> Result<i32> {
    // Check the code here first, so a typo never reaches (and burns) the other machine.
    let code = Code::parse(&code_words.join(" "))?.to_string();
    let mut cfg = Config::load(paths)?;
    let running = ipc::connect_existing(paths).await.is_some();
    let daemon_relays = if running { crate::status(paths).await.map(|s| s.relays).unwrap_or_default() } else { Vec::new() };
    let mut relays: Vec<String> = relay.into_iter().collect();
    let from_user = !relays.is_empty();
    if relays.is_empty() {
        relays = cfg.bootstrap_relays();
    }
    if relays.is_empty() && daemon_relays.is_empty() {
        if !term::stdin_is_tty() {
            bail!("this machine does not know a relay yet: jaunt join CODE --relay URL (or set JAUNT_RELAY)");
        }
        relays = vec![prompt_relay()?];
    }
    net::parse_relays(&relays)?;
    if cfg.relays.is_empty() && (from_user || daemon_relays.is_empty()) && !relays.is_empty() {
        cfg.relays = relays.clone();
        cfg.save(paths)?;
    }
    // A daemon that started without any relay has its endpoint built without one; start it
    // again so it uses the relay from now on.
    if running && daemon_relays.is_empty() && !relays.is_empty() {
        crate::stop_daemon(paths).await?;
    }
    pair(paths, IpcRequest::Join { code, relays, yes }, yes).await
}

/// Print `Told` lines as they come; return the final report.
async fn revoke_conversation(paths: &Paths, req: IpcRequest) -> Result<UnlinkReport> {
    let mut s = ipc::connect(paths).await?;
    proto::write_json(&mut s, FrameType::Json, &req).await?;
    loop {
        let Some(f) = proto::read_frame(&mut s).await? else { bail!("the daemon closed the connection") };
        match proto::parse_json::<IpcResponse>(&f.payload, MAX_FRAME)? {
            IpcResponse::Told { peer, ok, detail } => {
                println!("  {} {}: {detail}", if ok { "told" } else { "not yet" }, peer.name);
            }
            IpcResponse::Unlinked(r) => return Ok(r),
            IpcResponse::Error { message } => bail!("{message}"),
            _ => {}
        }
    }
}

fn names(v: &[proto::PeerRef]) -> String {
    v.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
}

pub async fn unlink(paths: &Paths, target: String, cascade: bool) -> Result<i32> {
    let r = revoke_conversation(paths, IpcRequest::Unlink { target, cascade }).await?;
    let ids: Vec<String> = r.revoked.iter().map(|p| format!("{} ({})", p.name, p.id.short())).collect();
    println!("unlinked {}: refused here from now on, connections closed, {} process(es) it ran here ended", ids.join(", "), r.sessions_killed);
    if !r.confirmed.is_empty() {
        println!("confirmed by: {}", names(&r.confirmed));
    }
    if !r.not_heard.is_empty() {
        println!(
            "not heard yet: {} -- until they sync with a machine that knows, they still accept {}. They learn within about 30 s of being online.",
            names(&r.not_heard),
            names(&r.revoked)
        );
    }
    if !r.added_by_revoked.is_empty() {
        let first = r.revoked.first().map(|p| p.name.clone()).unwrap_or_default();
        println!(
            "warning: {first} had added {}; they stay linked. To revoke them too: jaunt unlink {first} --cascade",
            names(&r.added_by_revoked)
        );
    }
    Ok(0)
}

/// Tombstone this machine and tell the others; prints who confirmed.
pub async fn announce_leave(paths: &Paths) -> Result<UnlinkReport> {
    let r = revoke_conversation(paths, IpcRequest::Leave).await?;
    if !r.confirmed.is_empty() {
        println!("confirmed by: {}", names(&r.confirmed));
    }
    if !r.not_heard.is_empty() {
        println!("not heard yet: {} (they learn when they next sync with any machine that knows)", names(&r.not_heard));
    }
    Ok(r)
}

pub async fn leave(paths: &Paths, yes: bool) -> Result<i32> {
    if !yes {
        if !term::stdin_is_tty() {
            bail!("leave needs --yes when not run on a terminal");
        }
        print!("This takes this machine out of the mesh and wipes its key and roster. Type 'leave' to go on: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if line.trim() != "leave" {
            println!("nothing changed");
            return Ok(1);
        }
    }
    let s = crate::status(paths).await?;
    if s.peers.is_empty() {
        println!("this machine is not linked to any other");
    } else {
        announce_leave(paths).await?;
    }
    crate::wipe_identity(paths).await?;
    println!("this machine left the mesh; its key and roster are gone. To link it again: jaunt join CODE");
    Ok(0)
}
