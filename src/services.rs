//! Exec (run one command without a pty). Tcp and Copy are refused for now by the daemon.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::daemon::Daemon;
use crate::paths::Paths;
use crate::proto::{self, ExitStatus, FrameType, Reply, Service};
use crate::roster::MachineId;
use crate::{ipc, shell};

pub const MAX_ARGS: usize = 4096;
pub const MAX_ARGV_BYTES: usize = 256 * 1024;

pub fn check_argv(argv: &[String]) -> Result<(), String> {
    if argv.is_empty() || argv[0].is_empty() {
        return Err("no command given".into());
    }
    if argv.len() > MAX_ARGS {
        return Err(format!("too many arguments ({} > {MAX_ARGS})", argv.len()));
    }
    if argv.iter().map(|a| a.len() + 1).sum::<usize>() > MAX_ARGV_BYTES {
        return Err("the command line is too long".into());
    }
    if argv.iter().any(|a| a.contains('\0')) {
        return Err("an argument contains a NUL byte".into());
    }
    Ok(())
}

/// Host side of an Exec stream: argv is run directly, never through a shell.
pub async fn serve_exec(d: &Arc<Daemon>, from: MachineId, argv: Vec<String>, mut send: SendStream, recv: RecvStream) -> Result<()> {
    if let Err(why) = check_argv(&argv) {
        proto::write_json(&mut send, FrameType::Reply, &Reply::Refused { reason: why }).await?;
        send.finish()?;
        return Ok(());
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
        .env("JAUNT_FROM", d.name_of(&from))
        .env("JAUNT_MACHINE", d.my_name())
        .env_remove("NOTIFY_SOCKET")
        .env_remove("WATCHDOG_USEC")
        .env_remove("WATCHDOG_PID")
        .env_remove("TMUX")
        .process_group(0)
        .kill_on_drop(false);
    proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("jaunt: cannot run {} on {}: {e}\n", argv[0], d.my_name());
            proto::write_frame(&mut send, FrameType::Stderr, msg.as_bytes()).await?;
            let code = if e.kind() == std::io::ErrorKind::NotFound { 127 } else { 126 };
            proto::write_json(&mut send, FrameType::Exit, &ExitStatus { code: Some(code), signal: None }).await?;
            send.finish()?;
            return Ok(());
        }
    };
    let pid = child.id().and_then(|p| rustix::process::Pid::from_raw(p as i32));

    // stdout and stderr readers feed one writer (the stream), tagged by frame type.
    let (out_tx, mut out_rx) = mpsc::channel::<(FrameType, Vec<u8>)>(16);
    for (kind, pipe) in [
        (FrameType::Data, child.stdout.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)),
        (FrameType::Stderr, child.stderr.take().map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)),
    ] {
        let Some(mut pipe) = pipe else { continue };
        let tx = out_tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            while let Ok(n) = pipe.read(&mut buf).await {
                if n == 0 || tx.send((kind, buf[..n].to_vec())).await.is_err() {
                    break;
                }
            }
        });
    }
    drop(out_tx);
    let mut stdin = child.stdin.take();
    let mut frames = proto::frame_reader(recv);
    let mut stream_gone = false;
    let mut outputs_done = false;
    while !outputs_done {
        tokio::select! {
            o = out_rx.recv() => match o {
                Some((kind, bytes)) => if proto::write_frame(&mut send, kind, &bytes).await.is_err() { stream_gone = true; break },
                None => outputs_done = true,
            },
            f = frames.recv(), if !stream_gone => match f {
                Some(Ok(f)) if f.kind == FrameType::Data => {
                    if let Some(s) = stdin.as_mut()
                        && s.write_all(&f.payload).await.is_err()
                    {
                        stdin = None;
                    }
                }
                Some(Ok(f)) if f.kind == FrameType::Eof => stdin = None,
                Some(Ok(_)) => {}
                _ => { stream_gone = true; break }
            },
        }
    }
    if stream_gone {
        shell::kill_group(pid, &mut child).await;
        return Ok(());
    }
    drop(stdin);
    // Outputs are closed; the process may still run (it closed them itself). Keep
    // watching the stream so a client that leaves still ends it.
    let status = loop {
        tokio::select! {
            st = child.wait() => break st?,
            f = frames.recv(), if !stream_gone => match f {
                Some(Ok(_)) => {}
                _ => { shell::kill_group(pid, &mut child).await; return Ok(()) }
            },
        }
    };
    use std::os::unix::process::ExitStatusExt;
    let st = ExitStatus { code: status.code(), signal: status.signal() };
    proto::write_json(&mut send, FrameType::Exit, &st).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
    Ok(())
}

/// `jaunt run <machine> -- argv...`: stdio and the exit status pass through.
pub async fn run(paths: &Paths, target: &str, argv: Vec<String>) -> Result<i32> {
    if let Err(why) = check_argv(&argv) {
        anyhow::bail!("{why}");
    }
    let (stream, peer, _, _) = ipc::open(paths, target, Service::Exec { argv }).await?;
    let (r, mut w) = stream.into_split();
    let pump = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = proto::write_frame(&mut w, FrameType::Eof, b"").await;
                    break;
                }
                Ok(n) => {
                    if proto::write_frame(&mut w, FrameType::Data, &buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        // Keep the write half open until the process exits: closing it ends the stream.
        std::future::pending::<()>().await;
    });
    let mut frames = proto::frame_reader(r);
    let mut out = tokio::io::stdout();
    let mut err = tokio::io::stderr();
    let code = loop {
        match frames.recv().await {
            Some(Ok(f)) => match f.kind {
                FrameType::Data => {
                    out.write_all(&f.payload).await?;
                    out.flush().await?;
                }
                FrameType::Stderr | FrameType::Notice => {
                    err.write_all(&f.payload).await?;
                    err.flush().await?;
                }
                FrameType::Exit => {
                    let st: ExitStatus = proto::parse_json(&f.payload, 1024)?;
                    break st.as_code();
                }
                _ => {}
            },
            Some(Err(e)) => {
                eprintln!("jaunt: the link to {} broke: {e}", peer.name);
                break 255;
            }
            None => {
                eprintln!("jaunt: the link to {} was lost before the command finished", peer.name);
                break 255;
            }
        }
    };
    pump.abort();
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_limits() {
        assert!(check_argv(&[]).is_err());
        assert!(check_argv(&["".into()]).is_err());
        assert!(check_argv(&["ls".into(), "-l".into()]).is_ok());
        assert!(check_argv(&vec!["x".into(); MAX_ARGS + 1]).is_err());
        assert!(check_argv(&["a\0b".into()]).is_err());
        assert!(check_argv(&["x".repeat(MAX_ARGV_BYTES)]).is_err());
    }
}
