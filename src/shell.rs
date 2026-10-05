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

/// `tmux new-session -e` exists from tmux 3.0 on. Asked once per daemon.
fn tmux_has_new_session_env(tmux: &std::path::Path) -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| {
        let out = std::process::Command::new(tmux).arg("-V").output().ok();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        tmux_version_at_least_3(&text)
    })
}

/// Parse `tmux 3.7c`, `tmux next-3.5`, `tmux 2.6`.
fn tmux_version_at_least_3(v: &str) -> bool {
    let num = v.split_whitespace().nth(1).unwrap_or("").trim_start_matches("next-");
    num.split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|m| m >= 3)
}

/// Find a program on PATH. Only absolute PATH entries count: an empty or relative entry
/// (`::`, `.`) would make the program depend on the current directory (L2).
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|d| d.is_absolute())
        .map(|d| d.join(name))
        .find(|p| p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false))
}

/// Does jaunt's tmux server have a session with exactly this name?
pub async fn tmux_has_session(tmux: &std::path::Path, socket: Option<&str>, session: &str) -> bool {
    let mut c = tmux_command(tmux, socket);
    c.args(["has-session", "-t", &format!("={session}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    matches!(tokio::time::timeout(Duration::from_secs(2), c.status()).await, Ok(Ok(st)) if st.success())
}

/// End the session with exactly this name; true when tmux did.
pub async fn tmux_kill_session(tmux: &std::path::Path, socket: Option<&str>, session: &str) -> bool {
    let mut c = tmux_command(tmux, socket);
    c.args(["kill-session", "-t", &format!("={session}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    matches!(tokio::time::timeout(Duration::from_secs(2), c.status()).await, Ok(Ok(st)) if st.success())
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
            // A running tmux server gives new sessions its own (stale) environment; `-e`
            // (tmux >= 3.0) sets these two for the new session. Names are roster-validated.
            if tmux_has_new_session_env(t) {
                c.arg("-e").arg(format!("JAUNT_FROM={}", d.name_of(&from)));
                c.arg("-e").arg(format!("JAUNT_MACHINE={me}"));
            }
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
    // Registered so that unlinking `from` ends this pty client at once.
    let _ticket = d.children.track(from, pid);
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
/// DESIGN §8: a hop from inside tmux without jaunt.tmux gets a one-time hint about how
/// keys reach the remote tmux. Shown once per machine (a marker in the state directory).
fn tmux_hint_once(paths: &Paths, peer: &str) {
    if std::env::var_os("TMUX").is_none() {
        return;
    }
    let marker = paths.state_dir.join("tmux-hint-shown");
    if marker.exists() {
        return;
    }
    // By absolute path (L2): an empty or relative PATH entry must not pick up a `tmux`
    // planted in whatever directory this command runs in.
    let snippet = which("tmux")
        .and_then(|tmux| {
            std::process::Command::new(tmux)
                .args(["show-options", "-gqv", "@jaunt_tmux"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
        })
        .map(|o| o.stdout.starts_with(b"1"))
        .unwrap_or(false);
    if !snippet {
        eprintln!(
            "inside tmux, the prefix goes to this tmux: press it twice to send it to {peer}'s, \
             or run 'jaunt tmux-setup' for an F12 switch (shown once)"
        );
    }
    let _ = crate::paths::private_dir(&paths.state_dir).and_then(|_| {
        std::fs::write(&marker, b"").map_err(anyhow::Error::from)
    });
}

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
    tmux_hint_once(paths, &peer.name);

    let raw = RawMode::enable().context("cannot put the terminal in raw mode")?;
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(32);
    // Stops when the hop ends, so no blocked read is left behind to steal the next key
    // (the mesh view reads stdin again after a hop).
    let _pump = term::StdinPump::start(stdin_tx);
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
                let r = session_loop(s, &mut stdin_rx, &mut sig, &mut escapes, &mut stdout, paths, &peer).await;
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

/// H3: typed input with no output back for this long, and nothing at all (data, acks,
/// keep-alives) from the machine for this long, shows the stall line.
const STALL_AFTER: Duration = Duration::from_secs(3);

/// Ask the daemon how long the peer has been silent (`None`: no live connection).
async fn peer_quiet(paths: &Paths, id: MachineId) -> Option<Option<u64>> {
    let req = proto::IpcRequest::Quiet { id };
    // Never starts a daemon: only asks the one that carries this session.
    let ask = async {
        let mut s = ipc::connect_existing(paths).await?;
        ipc::send_request(&mut s, &req).await.ok()
    };
    match tokio::time::timeout(Duration::from_secs(1), ask).await {
        Ok(Some(proto::IpcResponse::Quiet { quiet_ms })) => Some(quiet_ms),
        _ => None,
    }
}

async fn session_loop(
    stream: tokio::net::UnixStream,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
    sig: &mut Signals,
    escapes: &mut Escapes,
    stdout: &mut tokio::io::Stdout,
    paths: &Paths,
    peer: &proto::PeerRef,
) -> Result<Ended> {
    let (r, mut w) = stream.into_split();
    let mut frames = proto::frame_reader(r);
    // Re-send the size: the window may have changed while the link was down.
    let (c, rws) = term::size();
    proto::write_frame(&mut w, FrameType::Resize, &proto::encode_resize(c, rws)).await?;
    // The first input that has not been answered by any output yet.
    let mut pending_since: Option<tokio::time::Instant> = None;
    let mut stalled = false;
    let mut check = tokio::time::interval(Duration::from_secs(1));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = check.tick(), if pending_since.is_some_and(|t| t.elapsed() >= STALL_AFTER) => {
                // Silence after typing may be a quiet program (a password prompt): only the
                // daemon knows whether the machine itself still answers (acks, keep-alives).
                let quiet = match peer_quiet(paths, peer.id).await {
                    Some(Some(ms)) => (ms >= STALL_AFTER.as_millis() as u64).then_some(ms / 1000),
                    Some(None) => pending_since.map(|t| t.elapsed().as_secs()),
                    None => None,
                };
                match quiet {
                    Some(s) => {
                        let (_, rows) = term::size();
                        let line = ui::stalled_line(&peer.name, s.max(STALL_AFTER.as_secs()));
                        stdout.write_all(format!("\x1b7\x1b[{rows};1H\x1b[2K{line}\x1b8").as_bytes()).await?;
                        stdout.flush().await?;
                        stalled = true;
                    }
                    None if stalled => {
                        redraw_remote(&mut w).await?;
                        stalled = false;
                    }
                    None => {}
                }
            }
            input = stdin_rx.recv() => {
                let Some(input) = input else { return Ok(Ended::Disconnected) };
                let (bytes, action) = escapes.feed(&input);
                if !bytes.is_empty() && proto::write_frame(&mut w, FrameType::Data, &bytes).await.is_err() {
                    return Ok(Ended::Lost);
                }
                if !bytes.is_empty() && pending_since.is_none() {
                    pending_since = Some(tokio::time::Instant::now());
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
                        pending_since = None;
                        if stalled {
                            stalled = false;
                            redraw_remote(&mut w).await?;
                        }
                        stdout.write_all(&f.payload).await?;
                        stdout.flush().await?;
                    }
                    FrameType::Notice => {
                        // From the host, printed outside the PTY data: no escapes (R5).
                        let text = term::safe_text(&String::from_utf8_lossy(&f.payload), 500, true).replace('\n', "\r\n");
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

/// The stall line was drawn over the bottom row: make the remote side redraw the screen
/// (two resizes make tmux, or any full-screen program, repaint everything).
async fn redraw_remote(w: &mut tokio::net::unix::OwnedWriteHalf) -> Result<()> {
    let (c, r) = term::size();
    proto::write_frame(w, FrameType::Resize, &proto::encode_resize(c, r.saturating_sub(1).max(1))).await?;
    proto::write_frame(w, FrameType::Resize, &proto::encode_resize(c, r)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_versions() {
        assert!(tmux_version_at_least_3("tmux 3.7c\n"));
        assert!(tmux_version_at_least_3("tmux 3.0a"));
        assert!(tmux_version_at_least_3("tmux next-3.5"));
        assert!(!tmux_version_at_least_3("tmux 2.6"));
        assert!(!tmux_version_at_least_3("garbage"));
        assert!(!tmux_version_at_least_3(""));
    }

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
