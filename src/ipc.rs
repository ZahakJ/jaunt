//! The local control socket: `$XDG_RUNTIME_DIR/jaunt/sock` (directory 0700, socket 0600,
//! peer uid checked). Server side lives in the daemon; client side starts the daemon when
//! it is not running. Every message is one `Json` frame (see `proto::IpcRequest`).

use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

use crate::daemon::Daemon;
use crate::paths::{Paths, private_dir};
use crate::proto::{self, FrameType, IpcRequest, IpcResponse, MAX_FRAME, MAX_HEADER, PathKind, PeerRef, Service};
use crate::roster::MachineId;
use crate::systemd;

/// Holds the single-instance lock and removes the socket on drop.
pub struct ServerGuard {
    _lock: fs::File,
    sock: PathBuf,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.sock);
    }
}

pub fn bind_server(paths: &Paths) -> Result<(UnixListener, ServerGuard)> {
    private_dir(&paths.runtime_dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(paths.lock_file())
        .context("cannot open the daemon lock file")?;
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        bail!("another jaunt daemon is already running (lock {} is held)", paths.lock_file().display());
    }
    let sock = paths.socket();
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).with_context(|| format!("cannot listen on {}", sock.display()))?;
    fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?;
    Ok((listener, ServerGuard { _lock: lock, sock }))
}

pub async fn serve(d: Arc<Daemon>, listener: UnixListener) {
    let uid = rustix::process::getuid().as_raw();
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        match stream.peer_cred() {
            Ok(c) if c.uid() == uid => {}
            _ => continue,
        }
        let d = d.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(d, stream).await {
                tracing::debug!("ipc client: {e:#}");
            }
        });
    }
}

async fn reply(w: &mut UnixStream, r: &IpcResponse) -> Result<()> {
    proto::write_json(w, FrameType::Json, r).await?;
    Ok(())
}

fn result(r: Result<String>) -> IpcResponse {
    match r {
        Ok(message) => IpcResponse::Ok { message },
        Err(e) => IpcResponse::Error { message: format!("{e:#}") },
    }
}

async fn handle(d: Arc<Daemon>, mut s: UnixStream) -> Result<()> {
    let payload = timeout(Duration::from_secs(10), proto::expect_frame(&mut s, FrameType::Json)).await??;
    let req: IpcRequest = match proto::parse_json(&payload, MAX_HEADER) {
        Ok(r) => r,
        Err(e) => {
            return reply(&mut s, &IpcResponse::Error { message: format!("bad request ({e}); is the client the same version as the daemon ({})?", proto::VERSION) }).await;
        }
    };
    match req {
        IpcRequest::Status => reply(&mut s, &IpcResponse::Status(d.snapshot())).await,
        IpcRequest::Subscribe { history } => subscribe(&d, s, history as usize).await,
        IpcRequest::AddPeer { id, name } => {
            let r = id.parse::<MachineId>().and_then(|id| d.add_peer(id, &name));
            reply(&mut s, &result(r)).await
        }
        IpcRequest::Rename { name } => reply(&mut s, &result(d.rename(&name))).await,
        IpcRequest::SetRelays { relays } => reply(&mut s, &result(d.set_relays(relays))).await,
        IpcRequest::Link { yes } => crate::pair::serve_ipc(d, s, crate::pair::Request::Link, yes).await,
        IpcRequest::Join { code, relays, yes } => crate::pair::serve_ipc(d, s, crate::pair::Request::Join { code, relays }, yes).await,
        IpcRequest::PairAnswer { .. } => {
            reply(&mut s, &IpcResponse::Error { message: "no link or join is waiting for an answer on this connection".into() }).await
        }
        IpcRequest::Unlink { target, cascade } => crate::trust::serve_ipc(d, s, crate::trust::Request::Unlink { target, cascade }).await,
        IpcRequest::Leave => crate::trust::serve_ipc(d, s, crate::trust::Request::Leave).await,
        IpcRequest::Shutdown => {
            reply(&mut s, &IpcResponse::Ok { message: "daemon stopping".into() }).await?;
            d.shutdown();
            Ok(())
        }
        IpcRequest::Open { target, service } => match d.open_stream(&target, service).await {
            Err(e) => reply(&mut s, &IpcResponse::Error { message: format!("{e:#}") }).await,
            Ok((peer, path, rtt_ms, mut send, mut recv)) => {
                reply(&mut s, &IpcResponse::Opened { peer, path, rtt_ms }).await?;
                // From here the socket carries the stream's frames untouched.
                let (mut ur, mut uw) = s.into_split();
                tokio::select! {
                    _ = tokio::io::copy(&mut ur, &mut send) => {
                        // The client went away: end the stream so the host stops the process.
                        let _ = send.reset(0u32.into());
                        let _ = recv.stop(0u32.into());
                    }
                    _ = tokio::io::copy(&mut recv, &mut uw) => {
                        let _ = uw.shutdown().await;
                    }
                }
                Ok(())
            }
        },
    }
}

async fn subscribe(d: &Arc<Daemon>, s: UnixStream, history: usize) -> Result<()> {
    let mut rx = d.events.subscribe();
    let (mut r, mut w) = s.into_split();
    proto::write_json(&mut w, FrameType::Json, &IpcResponse::Status(d.snapshot())).await?;
    for ev in d.events.history(history.min(500)) {
        proto::write_json(&mut w, FrameType::Json, &IpcResponse::Event(ev)).await?;
    }
    // Detect the client hanging up even when no events flow.
    let mut gone = Box::pin(async move {
        let mut b = [0u8; 64];
        loop {
            match tokio::io::AsyncReadExt::read(&mut r, &mut b).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    });
    loop {
        tokio::select! {
            _ = &mut gone => return Ok(()),
            m = rx.recv() => match m {
                Ok(msg) => proto::write_json(&mut w, FrameType::Json, &msg).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    proto::write_json(&mut w, FrameType::Json, &IpcResponse::Status(d.snapshot())).await?
                }
                Err(_) => return Ok(()),
            },
        }
    }
}

// ------------------------------------------------------------------------- client side

/// Connect to the daemon, starting it if it is not running.
pub async fn connect(paths: &Paths) -> Result<UnixStream> {
    let sock = paths.socket();
    if let Ok(s) = UnixStream::connect(&sock).await {
        return Ok(s);
    }
    start_daemon(paths)?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(s) = UnixStream::connect(&sock).await {
            return Ok(s);
        }
    }
    bail!("the jaunt daemon did not come up; see {}", paths.daemon_log().display())
}

/// Connect only if the daemon is already running.
pub async fn connect_existing(paths: &Paths) -> Option<UnixStream> {
    UnixStream::connect(paths.socket()).await.ok()
}

fn start_daemon(paths: &Paths) -> Result<()> {
    if systemd::unit_installed(paths) {
        systemd::start_unit()?;
        eprintln!("jaunt: started the daemon (systemctl --user start jaunt)");
        return Ok(());
    }
    private_dir(&paths.state_dir)?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths.daemon_log())
        .context("cannot open the daemon log")?;
    let exe = std::env::current_exe().context("cannot find the jaunt binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    // SAFETY: setsid is async-signal-safe; detaches from this terminal's session.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            rustix::process::setsid()?;
            Ok(())
        });
    }
    let child = cmd.spawn().context("cannot start the jaunt daemon")?;
    eprintln!("jaunt: started the daemon (pid {}, log {})", child.id(), paths.daemon_log().display());
    Ok(())
}

pub async fn send_request(s: &mut UnixStream, req: &IpcRequest) -> Result<IpcResponse> {
    proto::write_json(s, FrameType::Json, req).await?;
    let payload = proto::expect_frame(s, FrameType::Json).await.context("the daemon closed the connection")?;
    Ok(proto::parse_json(&payload, MAX_FRAME)?)
}

/// One request, one response (starting the daemon if needed).
pub async fn call(paths: &Paths, req: &IpcRequest) -> Result<IpcResponse> {
    let mut s = connect(paths).await?;
    send_request(&mut s, req).await
}

/// Like `call`, but turns `Error` into an error and `Ok` into its message.
pub async fn call_ok(paths: &Paths, req: &IpcRequest) -> Result<String> {
    match call(paths, req).await? {
        IpcResponse::Ok { message } => Ok(message),
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected answer from the daemon: {other:?}"),
    }
}

/// Open a service stream on `target` through the daemon.
pub async fn open(paths: &Paths, target: &str, service: Service) -> Result<(UnixStream, PeerRef, PathKind, Option<f64>)> {
    let mut s = connect(paths).await?;
    match send_request(&mut s, &IpcRequest::Open { target: target.to_string(), service }).await? {
        IpcResponse::Opened { peer, path, rtt_ms } => Ok((s, peer, path, rtt_ms)),
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected answer from the daemon: {other:?}"),
    }
}
