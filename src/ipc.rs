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
    private_dir(&paths.data_dir)?;
    private_dir(&paths.runtime_dir)?;
    let mut lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(paths.lock_file())
        .context("cannot open the daemon lock file")?;
    if rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        let elsewhere = recorded_socket(paths).map(|s| format!("; its socket is {}", s.display())).unwrap_or_default();
        bail!("another jaunt daemon is already running with this key (lock {} is held{elsewhere})", paths.lock_file().display());
    }
    // Say where this daemon listens, for clients whose XDG_RUNTIME_DIR differs.
    {
        use std::io::{Seek, Write};
        lock.set_len(0)?;
        lock.seek(std::io::SeekFrom::Start(0))?;
        lock.write_all(paths.socket().as_os_str().as_encoded_bytes())?;
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
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                // Out of file descriptors and the like: do not spin a core on it (L15).
                tracing::warn!("ipc accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
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
        IpcRequest::Unlink { target, cascade, kill_sessions } => {
            crate::trust::serve_ipc(d, s, crate::trust::Request::Unlink { target, cascade, kill_sessions }).await
        }
        IpcRequest::Leave => crate::trust::serve_ipc(d, s, crate::trust::Request::Leave).await,
        IpcRequest::SetHost { on } => reply(&mut s, &result(d.set_host(on))).await,
        IpcRequest::Check => reply(&mut s, &IpcResponse::Check(crate::check::report(&d).await)).await,
        IpcRequest::Quiet { id } => reply(&mut s, &IpcResponse::Quiet { quiet_ms: d.quiet_ms(&id) }).await,
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

/// The socket the running daemon wrote into the lock file, if any.
fn recorded_socket(paths: &Paths) -> Option<PathBuf> {
    let text = crate::paths::read_capped(&paths.lock_file(), 4096).ok().flatten()?;
    let s = String::from_utf8(text).ok()?;
    let p = PathBuf::from(s.trim());
    p.is_absolute().then_some(p)
}

/// Connect to a socket and make sure this user's own daemon is on the other end (L7):
/// the directory holding it is a real directory of this user, and the process that
/// accepted runs as this user.
async fn connect_checked(sock: &std::path::Path) -> Result<UnixStream> {
    if let Some(dir) = sock.parent() {
        crate::paths::check_owned_dir(dir)?;
    }
    let s = UnixStream::connect(sock).await?;
    let uid = rustix::process::getuid().as_raw();
    match s.peer_cred() {
        Ok(c) if c.uid() == uid => Ok(s),
        Ok(c) => bail!("the socket {} is served by another user (uid {}); refusing to talk to it", sock.display(), c.uid()),
        Err(e) => bail!("cannot check who serves {}: {e}", sock.display()),
    }
}

/// This home's daemon: at our socket, or where the lock file says it listens (L6).
async fn connect_any(paths: &Paths) -> Result<UnixStream> {
    let sock = paths.socket();
    let first = connect_checked(&sock).await;
    if first.is_ok() {
        return first;
    }
    if let Some(other) = recorded_socket(paths).filter(|o| *o != sock)
        && let Ok(s) = connect_checked(&other).await
    {
        return Ok(s);
    }
    first
}

/// Connect to the daemon, starting it if it is not running.
pub async fn connect(paths: &Paths) -> Result<UnixStream> {
    match connect_any(paths).await {
        Ok(s) => return Ok(s),
        // Someone else's socket or directory: say so, never start a daemon next to it.
        Err(e) if e.downcast_ref::<std::io::Error>().is_none() => return Err(e),
        Err(_) => {}
    }
    start_daemon(paths)?;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(s) = connect_any(paths).await {
            return Ok(s);
        }
    }
    bail!("the jaunt daemon did not come up; see {}", paths.daemon_log().display())
}

/// Connect only if the daemon is already running.
pub async fn connect_existing(paths: &Paths) -> Option<UnixStream> {
    connect_any(paths).await.ok()
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
    // L2/L15: the daemon outlives this command and serves every peer, so it gets neither
    // this command's directory nor its whole environment (tokens, agent sockets): a fixed
    // directory, an allowlisted environment and a PATH of absolute directories only.
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .current_dir(daemon_dir())
        .env_clear()
        .envs(daemon_env(std::env::vars_os()));
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

/// Where a daemon runs: the home directory, else `/`.
pub fn daemon_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir()).unwrap_or_else(|| PathBuf::from("/"))
}

/// PATH without empty or relative entries (each would make a program name resolve against
/// the current directory), and without repeats. Never empty.
pub fn clean_path(path: Option<&std::ffi::OsStr>) -> std::ffi::OsString {
    let mut seen = Vec::new();
    if let Some(p) = path {
        for d in std::env::split_paths(p) {
            if d.is_absolute() && !seen.contains(&d) {
                seen.push(d);
            }
        }
    }
    if seen.is_empty() {
        return "/usr/local/bin:/usr/bin:/bin".into();
    }
    std::env::join_paths(seen).unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
}

/// The environment a self-started daemon keeps: identity, locale, session plumbing that
/// host mode needs (D-Bus, XDG), jaunt's own settings, and a cleaned PATH.
pub fn daemon_env(vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    const KEEP: &[&str] = &["HOME", "USER", "LOGNAME", "SHELL", "LANG", "LANGUAGE", "TZ", "DBUS_SESSION_BUS_ADDRESS", "TMUX_TMPDIR"];
    let mut out = Vec::new();
    let mut path = None;
    for (k, v) in vars {
        let Some(name) = k.to_str() else { continue };
        if name == "PATH" {
            path = Some(v);
        } else if KEEP.contains(&name) || name.starts_with("LC_") || name.starts_with("XDG_") || name.starts_with("JAUNT_") {
            out.push((k, v));
        }
    }
    out.push(("PATH".into(), clean_path(path.as_deref())));
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// L2: an empty or relative PATH entry never reaches the daemon.
    #[test]
    fn daemon_path_has_only_absolute_entries() {
        assert_eq!(clean_path(Some("/nonexistent::/usr/bin:.:bin:/usr/bin".as_ref())), OsString::from("/nonexistent:/usr/bin"));
        assert_eq!(clean_path(Some("::.".as_ref())), OsString::from("/usr/local/bin:/usr/bin:/bin"));
        assert_eq!(clean_path(None), OsString::from("/usr/local/bin:/usr/bin:/bin"));
    }

    /// L15: tokens and agent sockets of the command that started the daemon stay behind.
    #[test]
    fn daemon_env_is_an_allowlist() {
        let vars = [
            ("HOME", "/home/x"), ("SSH_AUTH_SOCK", "/tmp/agent"), ("GITHUB_TOKEN", "t"), ("LC_ALL", "C.UTF-8"),
            ("XDG_RUNTIME_DIR", "/run/user/1"), ("JAUNT_HOME", "/j"), ("PATH", "::/usr/bin"), ("TMUX", "/tmp/tmux"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = daemon_env(vars.into_iter());
        let names: Vec<String> = env.iter().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["HOME", "LC_ALL", "XDG_RUNTIME_DIR", "JAUNT_HOME", "PATH"]);
        assert_eq!(env.last().unwrap().1, OsString::from("/usr/bin"));
    }
}

/// Is the single-instance lock free, i.e. has a stopping daemon fully gone?
pub fn lock_is_free(paths: &Paths) -> bool {
    let Ok(f) = std::fs::OpenOptions::new().read(true).open(paths.lock_file()) else { return true };
    let free = rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok();
    if free {
        let _ = rustix::fs::flock(&f, rustix::fs::FlockOperation::Unlock);
    }
    free
}
