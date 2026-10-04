//! Bare `jaunt` on a terminal: the mesh view fed by the daemon's status and event
//! stream. Enter hops (the view steps aside and comes back after), `l` links.
//! `pick` mode is the tmux popup: Enter opens the hop in a new tmux window and exits.

use std::time::Duration;

use anyhow::{Result, bail};

use super::model::{Mesh, Tone};
use super::tui::{Action, Tui, handle_key, wait_key};
use super::view::ViewState;
use crate::paths::Paths;
use crate::proto::{self, FrameType, IpcRequest, IpcResponse, MAX_FRAME};
use crate::roster::now_ms;
use crate::{ipc, term};

const HISTORY: u32 = 60;
/// Shortest time between two frames (about 15 fps).
const FRAME_MS: u64 = 66;
/// How long the tag's dot stays bright after the daemon sent something.
const BEAT_MS: u64 = 400;

enum Next {
    Quit,
    Hop(String),
    Link,
}

pub async fn run(paths: &Paths, pick: bool) -> Result<i32> {
    if !term::stdin_is_tty() || !term::stdout_is_tty() {
        bail!("the mesh view needs a terminal; 'jaunt ls' prints the same as text");
    }
    let mut tui = Tui::new();
    let mut st = ViewState { tag: if pick { "pick" } else { "live" }, ..ViewState::default() };
    loop {
        let next = session(paths, &mut tui, &mut st).await;
        tui.leave();
        match next? {
            Next::Quit => return Ok(0),
            Next::Hop(name) => {
                if pick && std::env::var_os("TMUX").is_some() {
                    return open_window(&name);
                }
                if let Err(e) = crate::hop(paths, &name).await {
                    eprintln!("jaunt: {e:#}");
                    wait_key("press a key to go back to the mesh").await;
                }
            }
            Next::Link => {
                if let Err(e) = crate::cli_trust::link(paths, false).await {
                    eprintln!("jaunt: {e:#}");
                }
                wait_key("press a key to go back to the mesh").await;
            }
        }
    }
}

/// The popup's hop: a new tmux window named after the machine, running `jaunt hop`.
fn open_window(name: &str) -> Result<i32> {
    if !crate::roster::valid_name(name) {
        bail!("'{name}' is not a machine name jaunt can pass to tmux");
    }
    let exe = std::env::current_exe()?;
    let status = std::process::Command::new("tmux").arg("new-window").arg("-n").arg(name).arg(exe).arg("hop").arg(name).status()?;
    Ok(if status.success() { 0 } else { 1 })
}

async fn session(paths: &Paths, tui: &mut Tui, st: &mut ViewState) -> Result<Next> {
    let mut s = ipc::connect(paths).await?;
    proto::write_json(&mut s, FrameType::Json, &IpcRequest::Subscribe { history: HISTORY }).await?;
    let (rd, _wr) = s.into_split();
    let mut frames = proto::frame_reader(rd);
    let mut mesh = Mesh::default();
    tui.enter()?;
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut term_sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut poll = tokio::time::interval(Duration::from_secs(2));
    // Redraws: at most ~15 a second while something moves or data keeps arriving; when
    // idle, once a second (ages) plus once when the tag's dot settles. No busy loop: the
    // task sleeps until the next frame is due.
    let mut last_draw = 0u64;
    let mut dirty = true;
    loop {
        let now = now_ms();
        let mut due = last_draw + 1000;
        if dirty || mesh.animating(now) {
            due = due.min(last_draw + FRAME_MS);
        }
        if st.beat_ms + BEAT_MS > last_draw {
            due = due.min(st.beat_ms + BEAT_MS);
        }
        if let Some(t) = &st.toast {
            due = due.min(t.2);
        }
        let wait = Duration::from_millis(due.saturating_sub(now));
        tokio::select! {
            f = frames.recv() => {
                let f = match f {
                    Some(Ok(f)) => f,
                    Some(Err(e)) => bail!("lost the daemon: {e}"),
                    None => bail!("the daemon closed the connection"),
                };
                let now = now_ms();
                match proto::parse_json::<IpcResponse>(&f.payload, MAX_FRAME)? {
                    IpcResponse::Status(s) => mesh.apply_status(&s),
                    IpcResponse::Event(e) => {
                        mesh.apply_event(&e, now, e.t_ms + 1500 >= now);
                        st.beat_ms = now;
                    }
                    IpcResponse::Rtt { peer, sample } => mesh.apply_rtt(&peer, &sample, now),
                    _ => {}
                }
                dirty = true;
            }
            k = tui.key() => {
                let Some(k) = k else { return Ok(Next::Quit) };
                match handle_key(st, k, mesh.peers.len()) {
                    Action::Quit => return Ok(Next::Quit),
                    Action::Hop(i) => {
                        if let Some(p) = mesh.peers.get(i) {
                            return Ok(Next::Hop(p.name.clone()));
                        }
                    }
                    Action::Link => return Ok(Next::Link),
                    Action::None => {}
                }
                // Keys answer at once.
                let now = now_ms();
                tui.draw(&mesh, st, now);
                last_draw = now;
                dirty = false;
            }
            _ = tokio::time::sleep(wait) => {
                let now = now_ms();
                mesh.prune(now);
                if st.toast.as_ref().is_some_and(|t| now >= t.2) {
                    st.toast = None;
                }
                tui.draw(&mesh, st, now);
                last_draw = now;
                dirty = false;
            }
            _ = poll.tick() => {
                // Sessions and host state come from pings, which have no events.
                if let Ok(Ok(IpcResponse::Status(s))) = tokio::time::timeout(Duration::from_secs(1), ipc::call(paths, &IpcRequest::Status)).await {
                    mesh.apply_status(&s);
                    dirty = true;
                } else {
                    st.toast = Some(("the daemon is not answering".into(), Tone::Bad, now_ms() + 3000));
                }
            }
            _ = winch.recv() => {
                tui.resized();
                dirty = true;
            }
            _ = term_sig.recv() => return Ok(Next::Quit),
        }
    }
}
