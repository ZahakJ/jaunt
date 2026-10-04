//! The Shell service. Host side: a pty running `tmux new-session -A -s <session>` (or a
//! login shell when tmux is missing); when the stream dies, the pty client is killed and
//! tmux keeps the session. Client side: raw terminal, resize forwarding, escapes, and a
//! reconnect loop that re-attaches when the machine is back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::daemon::Daemon;
use crate::paths::Paths;
use crate::proto::{self, ExitStatus, FrameType, Reply, Service};
use crate::pty::{self, PtyMaster};
use crate::roster::MachineId;
use crate::term::{self, EscapeAction, Escapes, RawMode};
use crate::{ipc, ui};

pub struct ShellRequest {
    pub session: String,
    pub term: String,
    pub cols: u16,
    pub rows: u16,
    pub env: Vec<(String, String)>,
}

/// Environment the client may pass through (locale and terminal capabilities only).
fn allowed_env(key: &str) -> bool {
    matches!(key, "LANG" | "LANGUAGE" | "COLORTERM" | "TERM_PROGRAM" | "TERM_PROGRAM_VERSION")
        || (key.starts_with("LC_") && key.len() <= 32 && key[3..].bytes().all(|b| b.is_ascii_uppercase() || b == b'_'))
}

fn clean_env_value(v: &str) -> bool {
    v.len() <= 256 && v.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Variables that belong to the daemon's own supervision, never to a user shell.
const STRIP_ENV: &[&str] = &[
    "TMUX", "TMUX_PANE", "NOTIFY_SOCKET", "WATCHDOG_USEC", "WATCHDOG_PID", "LISTEN_FDS", "LISTEN_PID",
    "LISTEN_FDNAMES", "INVOCATION_ID", "JOURNAL_STREAM", "JAUNT_LOG",
];

pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn tmux_command(tmux: &std::path::Path, socket: Option<&str>) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(tmux);
    if let Some(s) = socket {
        c.arg("-L").arg(s);
    }
    for k in STRIP_ENV {
        c.env_remove(k);
    }
    c
}

/// Number of sessions on jaunt's tmux server: `None` without tmux, 0 with no server.
pub async fn count_sessions(socket: Option<String>) -> Option<u32> {
    let tmux = which("tmux")?;
    let mut c = tmux_command(&tmux, socket.as_deref());
    c.args(["list-sessions", "-F", "#{session_name}"]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    match tokio::time::timeout(Duration::from_secs(2), c.output()).await {
        Ok(Ok(out)) if out.status.success() => Some(out.stdout.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count() as u32),
        Ok(Ok(_)) => Some(0),
        _ => None,
    }
}

async fn refuse(send: &mut SendStream, reason: String) -> Result<()> {
    proto::write_json(send, FrameType::Reply, &Reply::Refused { reason }).await?;
    send.finish()?;
    Ok(())
}

/// Host side of a Shell stream.
pub async fn serve(d: &Arc<Daemon>, from: MachineId, req: ShellRequest, mut send: SendStream, recv: RecvStream) -> Result<()> {
    if !proto::valid_session(&req.session) {
        return refuse(&mut send, format!("'{}' is not a valid session name", req.session)).await;
    }
    let term_name = if proto::valid_term(&req.term) { req.term.clone() } else { "xterm-256color".to_string() };
    let cfg = d.config();
    let me = d.my_name();
    let tmux = which("tmux");
    let (master, slave) = pty::open(req.cols, req.rows).context("cannot open a pty")?;
    let mut cmd = match &tmux {
        Some(t) => {
            let mut c = tmux_command(t, cfg.tmux_socket().as_deref());
            c.args(["new-session", "-A", "-s", &req.session]);
            c
        }
        None => {
            let shell = std::env::var("SHELL").ok().filter(|s| s.starts_with('/')).unwrap_or_else(|| "/bin/sh".into());
            let base = shell.rsplit('/').next().unwrap_or("sh").to_string();
            let mut c = tokio::process::Command::new(&shell);
            c.arg0(format!("-{base}"));
            for k in STRIP_ENV {
                c.env_remove(k);
            }
            c
        }
    };
    for (k, v) in req.env.iter().take(32) {
        if allowed_env(k) && clean_env_value(v) {
            cmd.env(k, v);
        }
    }
    cmd.env("TERM", &term_name).env("JAUNT_FROM", d.name_of(&from)).env("JAUNT_MACHINE", &me);
    cmd.current_dir(home_dir()).kill_on_drop(false);
    pty::attach(&mut cmd, &slave)?;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return refuse(&mut send, format!("cannot start a shell on {me}: {e}")).await,
    };
    drop(slave);
    let pid = child.id().and_then(|p| rustix::process::Pid::from_raw(p as i32));
    proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
    if tmux.is_none() {
        let note = format!("tmux is not installed on {me}: this is a plain login shell and it will end if the link drops");
        proto::write_frame(&mut send, FrameType::Notice, note.as_bytes()).await?;
    }

    let master = Arc::new(PtyMaster::new(master)?);
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(16);
    let reader = master.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        while let Ok(n) = reader.read(&mut buf).await {
            if n == 0 || out_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    });
    let mut frames = proto::frame_reader(recv);
    let mut exited: Option<std::process::ExitStatus> = None;
    let mut stream_gone = false;
    loop {
        tokio::select! {
            chunk = out_rx.recv() => match chunk {
                Some(c) => if proto::write_frame(&mut send, FrameType::Data, &c).await.is_err() { stream_gone = true; break },
                None => break,
            },
            f = frames.recv(), if !stream_gone => match f {
                Some(Ok(f)) if f.kind == FrameType::Data => { let _ = master.write_all(&f.payload).await; }
                Some(Ok(f)) if f.kind == FrameType::Resize => {
                    if let Some((c, r)) = proto::decode_resize(&f.payload) { let _ = master.resize(c, r); }
                }
                Some(Ok(_)) => {}
                _ => { stream_gone = true; break }
            },
            st = child.wait(), if exited.is_none() => {
                exited = st.ok();
                // Collect what is still buffered in the pty, briefly, then report the exit.
                let deadline = tokio::time::sleep(Duration::from_millis(300));
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        c = out_rx.recv() => match c {
                            Some(c) => if proto::write_frame(&mut send, FrameType::Data, &c).await.is_err() { stream_gone = true; break },
                            None => break,
                        },
                        _ = &mut deadline => break,
                    }
                }
                break;
            }
        }
    }
    if stream_gone {
        // The client is gone: end the pty client; tmux keeps the session.
        kill_group(pid, &mut child).await;
        return Ok(());
    }
    let status = match exited {
        Some(s) => s,
        None => child.wait().await?,
    };
    use std::os::unix::process::ExitStatusExt;
    let st = ExitStatus { code: status.code(), signal: status.signal() };
    proto::write_json(&mut send, FrameType::Exit, &st).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    Ok(())
}

/// SIGHUP the child's process group, then SIGKILL if it is still there after 2 s.
pub async fn kill_group(pid: Option<rustix::process::Pid>, child: &mut tokio::process::Child) {
    if let Some(pid) = pid {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::HUP);
    }
    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        if let Some(pid) = pid {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = child.kill().await;
    }
}

// ------------------------------------------------------------------------- client side

enum Ended {
    /// The remote shell exited (or tmux detached).
    Exit(i32),
    /// The user typed `~.`.
    Disconnected,
    /// The stream went away.
    Lost,
}

fn shell_service(session: &str) -> Service {
    let (cols, rows) = term::size();
    let term = std::env::var("TERM").ok().filter(|t| proto::valid_term(t)).unwrap_or_else(|| "xterm-256color".into());
    let env: Vec<(String, String)> = std::env::vars().filter(|(k, v)| allowed_env(k) && clean_env_value(v)).collect();
    Service::Shell { session: session.to_string(), term, cols, rows, env }
}

/// `jaunt <machine>[:session]`. Returns the process exit code.
pub async fn hop(paths: &Paths, target: &str, session: &str) -> Result<i32> {
    if !term::stdin_is_tty() {
        anyhow::bail!("a hop needs a terminal; to run a command use: jaunt run {target} -- <command>");
    }
    // The first attempt runs before raw mode so errors print normally.
    let (stream, peer, path, rtt) = ipc::open(paths, target, shell_service(session)).await?;
    let me = ipc::call(paths, &proto::IpcRequest::Status)
        .await
        .ok()
        .and_then(|r| match r {
            proto::IpcResponse::Status(s) => Some(s.me.name),
            _ => None,
        })
        .unwrap_or_else(|| "here".into());
    eprintln!("{}", ui::hop_banner_in(&me, &peer.name, path, rtt));

    let raw = RawMode::enable().context("cannot put the terminal in raw mode")?;
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(32);
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 8192];
        while let Ok(n) = stdin.read(&mut buf).await {
            if n == 0 || stdin_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    });
    use tokio::signal::unix::{SignalKind, signal};
    let mut sig = Signals {
        winch: signal(SignalKind::window_change())?,
        term: signal(SignalKind::terminate())?,
        hup: signal(SignalKind::hangup())?,
    };
    let mut escapes = Escapes::default();
    let mut stream = Some(stream);
    let mut stdout = tokio::io::stdout();

    let code = 'outer: loop {
        let ended = match stream.take() {
            Some(s) => {
                let r = session_loop(s, &mut stdin_rx, &mut sig, &mut escapes, &mut stdout).await;
                match r {
                    Ok(e) => e,
                    Err(_) => Ended::Lost,
                }
            }
            None => Ended::Lost,
        };
        match ended {
            Ended::Exit(c) => break 'outer Some(c),
            Ended::Disconnected => break 'outer None,
            Ended::Lost => {}
        }
        // Reconnect loop with one status line.
        let _ = stdout.write_all(b"\r\n").await;
        let mut delay = 1u64;
        loop {
            let line = ui::link_lost_line(&peer.name, delay);
            let _ = stdout.write_all(format!("\r\x1b[2K{line}").as_bytes()).await;
            let _ = stdout.flush().await;
            let sleep = tokio::time::sleep(Duration::from_secs(delay));
            tokio::pin!(sleep);
            let mut tilde = false;
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    input = stdin_rx.recv() => {
                        let Some(input) = input else { break 'outer None };
                        for b in input {
                            if tilde && b == b'.' { break 'outer None; }
                            tilde = b == b'~';
                        }
                    }
                    _ = sig.term.recv() => break 'outer Some(143),
                    _ = sig.hup.recv() => break 'outer Some(129),
                }
            }
            if let Ok((s, _, _, _)) = ipc::open(paths, target, shell_service(session)).await {
                let _ = stdout.write_all(b"\r\x1b[2K").await;
                stream = Some(s);
                escapes = Escapes::default();
                continue 'outer;
            }
            delay = (delay * 2).min(10);
        }
    };
    drop(raw);
    match code {
        Some(c) => {
            eprintln!("\r\n{}", ui::hop_banner_out(&me, &peer.name, false));
            Ok(c)
        }
        None => {
            eprintln!("\r\n{}", ui::hop_banner_out(&me, &peer.name, true));
            Ok(0)
        }
    }
}

struct Signals {
    winch: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
}

async fn session_loop(
    stream: tokio::net::UnixStream,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
    sig: &mut Signals,
    escapes: &mut Escapes,
    stdout: &mut tokio::io::Stdout,
) -> Result<Ended> {
    let (r, mut w) = stream.into_split();
    let mut frames = proto::frame_reader(r);
    // Re-send the size: the window may have changed while the link was down.
    let (c, rws) = term::size();
    proto::write_frame(&mut w, FrameType::Resize, &proto::encode_resize(c, rws)).await?;
    loop {
        tokio::select! {
            input = stdin_rx.recv() => {
                let Some(input) = input else { return Ok(Ended::Disconnected) };
                let (bytes, action) = escapes.feed(&input);
                if !bytes.is_empty() && proto::write_frame(&mut w, FrameType::Data, &bytes).await.is_err() {
                    return Ok(Ended::Lost);
                }
                match action {
                    Some(EscapeAction::Disconnect) => return Ok(Ended::Disconnected),
                    Some(EscapeAction::Help) => {
                        stdout.write_all(term::ESCAPE_HELP.as_bytes()).await?;
                        stdout.flush().await?;
                    }
                    None => {}
                }
            }
            _ = sig.term.recv() => return Ok(Ended::Exit(143)),
            _ = sig.hup.recv() => return Ok(Ended::Exit(129)),
            _ = sig.winch.recv() => {
                let (c, r) = term::size();
                if proto::write_frame(&mut w, FrameType::Resize, &proto::encode_resize(c, r)).await.is_err() {
                    return Ok(Ended::Lost);
                }
            }
            f = frames.recv() => match f {
                Some(Ok(f)) => match f.kind {
                    FrameType::Data => {
                        stdout.write_all(&f.payload).await?;
                        stdout.flush().await?;
                    }
                    FrameType::Notice => {
                        let text = String::from_utf8_lossy(&f.payload).replace('\n', "\r\n");
                        stdout.write_all(format!("\r\njaunt: {text}\r\n").as_bytes()).await?;
                    }
                    FrameType::Exit => {
                        let st: ExitStatus = proto::parse_json(&f.payload, 1024)?;
                        return Ok(Ended::Exit(st.as_code()));
                    }
                    _ => {}
                },
                _ => return Ok(Ended::Lost),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_filter() {
        assert!(allowed_env("LANG"));
        assert!(allowed_env("LC_ALL"));
        assert!(!allowed_env("LD_PRELOAD"));
        assert!(!allowed_env("PATH"));
        assert!(!allowed_env("LC_x"));
        assert!(clean_env_value("en_US.UTF-8"));
        assert!(!clean_env_value("a\nb"));
    }
}
