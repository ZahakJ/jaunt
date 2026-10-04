//! `Tcp { port }`: the host connects to its own loopback port and splices it with the
//! stream. After the `Reply`, both directions carry `Data` frames; `Eof` is a half-close.
//! A side finishes its stream only when both directions are done, because the client's
//! IPC relay ends the whole stream as soon as either half closes.
//! Client commands: `jaunt fwd` (local listener) and `jaunt pipe` (stdio, for ProxyCommand).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::endpoint::{RecvStream, SendStream};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::daemon::Daemon;
use crate::paths::Paths;
use crate::proto::{self, FrameType, Reply, Service};
use crate::roster::MachineId;
use crate::ipc;

const CHUNK: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Host side. Only loopback is ever dialled; the port comes from the peer, nothing else.
pub async fn serve(d: &Arc<Daemon>, _from: MachineId, port: u16, mut send: SendStream, recv: RecvStream) -> Result<()> {
    if port == 0 {
        return refuse(&mut send, "port 0 is not a port".into()).await;
    }
    let tcp = match connect_loopback(port).await {
        Ok(s) => s,
        Err(e) => {
            let why = format!("nothing accepts connections on port {port} of {} ({e})", d.my_name());
            return refuse(&mut send, why).await;
        }
    };
    let _ = tcp.set_nodelay(true);
    proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
    let (tr, tw) = tcp.into_split();
    match splice(tr, tw, recv, &mut send).await {
        Ok(()) => {
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        }
        Err(e) => {
            let _ = send.reset(0u32.into());
            tracing::debug!("tcp {port}: {e:#}");
        }
    }
    Ok(())
}

async fn refuse(send: &mut SendStream, reason: String) -> Result<()> {
    proto::write_json(send, FrameType::Reply, &Reply::Refused { reason }).await?;
    send.finish()?;
    Ok(())
}

/// 127.0.0.1 first, then ::1 (a service may listen on only one of them).
async fn connect_loopback(port: u16) -> std::io::Result<TcpStream> {
    let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
    let mut first = None;
    for a in [v4, v6] {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(a)).await {
            Ok(Ok(s)) => return Ok(s),
            // Report the IPv4 error: the IPv6 one is often just "no IPv6 here".
            Ok(Err(e)) => first = first.or(Some(e)),
            Err(_) => first = first.or(Some(std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out"))),
        }
    }
    Err(first.unwrap_or_else(|| std::io::Error::other("no address")))
}

/// Splice a byte stream (`lr`/`lw`) with a framed stream (`fr`/`fw`) until both directions
/// have ended. An error, or the framed side ending without `Eof`, aborts both.
pub async fn splice<LR, LW, FR, FW>(mut lr: LR, mut lw: LW, mut fr: FR, fw: &mut FW) -> Result<()>
where
    LR: AsyncRead + Unpin,
    LW: AsyncWrite + Unpin,
    FR: AsyncRead + Unpin,
    FW: AsyncWrite + Unpin,
{
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = lr.read(&mut buf).await.context("local read")?;
            if n == 0 {
                proto::write_frame(fw, FrameType::Eof, b"").await?;
                fw.flush().await?;
                return anyhow::Ok(());
            }
            proto::write_frame(fw, FrameType::Data, &buf[..n]).await?;
            fw.flush().await?;
        }
    };
    let down = async {
        loop {
            match proto::read_frame(&mut fr).await? {
                Some(f) if f.kind == FrameType::Data => {
                    lw.write_all(&f.payload).await.context("local write")?;
                    lw.flush().await?;
                }
                Some(f) if f.kind == FrameType::Eof => {
                    let _ = lw.shutdown().await;
                    return anyhow::Ok(());
                }
                Some(_) => {}
                None => bail!("the other side went away"),
            }
        }
    };
    tokio::try_join!(up, down)?;
    Ok(())
}

/// `jaunt pipe <machine> [port]`: stdin/stdout to the machine's loopback port.
/// Prints nothing on stdout but the data (it is used as an ssh ProxyCommand).
pub async fn pipe(paths: &Paths, machine: &str, port: u16) -> Result<i32> {
    let (stream, _peer, _, _) = ipc::open(paths, machine, Service::Tcp { port }).await?;
    let (r, mut w) = stream.into_split();
    splice(tokio::io::stdin(), tokio::io::stdout(), r, &mut w).await?;
    Ok(0)
}

/// Parse `<port>[:<localport>]`.
pub fn parse_fwd_spec(spec: &str) -> Result<(u16, u16)> {
    let port = |s: &str| -> Result<u16> {
        match s.parse::<u16>() {
            Ok(p) if p > 0 => Ok(p),
            _ => bail!("'{s}' is not a port number (1-65535)"),
        }
    };
    match spec.split_once(':') {
        Some((r, l)) => Ok((port(r)?, port(l)?)),
        None => {
            let p = port(spec)?;
            Ok((p, p))
        }
    }
}

/// `jaunt fwd <machine> <port>[:<localport>]`: listen on 127.0.0.1:<localport> and carry
/// each connection to <port> on the machine's loopback. Runs until interrupted.
pub async fn fwd(paths: &Paths, machine: &str, spec: &str) -> Result<i32> {
    let (port, local) = parse_fwd_spec(spec)?;
    // Fail early on an unknown machine name instead of on the first connection.
    let status = crate::status(paths).await?;
    if !status.peers.iter().any(|p| p.name == machine || p.name.starts_with(machine)) {
        bail!("no machine called '{machine}'");
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, local))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{local}"))?;
    eprintln!("jaunt: forwarding 127.0.0.1:{local} to port {port} on {machine}; Ctrl-C stops");
    let paths = Arc::new(paths.clone());
    let machine: Arc<str> = machine.into();
    loop {
        let (tcp, from) = listener.accept().await.context("accept failed")?;
        let paths = paths.clone();
        let machine = machine.clone();
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            match ipc::open(&paths, &machine, Service::Tcp { port }).await {
                Ok((stream, _, _, _)) => {
                    let (r, mut w) = stream.into_split();
                    let (tr, tw) = tcp.into_split();
                    if let Err(e) = splice(tr, tw, r, &mut w).await {
                        tracing::debug!("forward from {from}: {e:#}");
                    }
                }
                Err(e) => eprintln!("jaunt: connection from {from} not forwarded: {e:#}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fwd_specs() {
        assert_eq!(parse_fwd_spec("8080").unwrap(), (8080, 8080));
        assert_eq!(parse_fwd_spec("22:2222").unwrap(), (22, 2222));
        assert!(parse_fwd_spec("0").is_err());
        assert!(parse_fwd_spec("70000").is_err());
        assert!(parse_fwd_spec("a:1").is_err());
        assert!(parse_fwd_spec("1:").is_err());
    }

    #[tokio::test]
    async fn splice_half_close_both_ways() {
        // local side: a duplex pipe; framed side: frames written into a buffer.
        let (mut app, local) = tokio::io::duplex(1 << 16);
        let (lr, lw) = tokio::io::split(local);
        let mut incoming = Vec::new();
        proto::write_frame(&mut incoming, FrameType::Data, b"from-remote").await.unwrap();
        proto::write_frame(&mut incoming, FrameType::Eof, b"").await.unwrap();
        let mut out = Vec::new();
        app.write_all(b"from-local").await.unwrap();
        app.shutdown().await.unwrap();
        splice(lr, lw, &incoming[..], &mut out).await.unwrap();
        let mut got = Vec::new();
        app.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"from-remote");
        let mut r = &out[..];
        let f = proto::read_frame(&mut r).await.unwrap().unwrap();
        assert_eq!((f.kind, &f.payload[..]), (FrameType::Data, &b"from-local"[..]));
        assert_eq!(proto::read_frame(&mut r).await.unwrap().unwrap().kind, FrameType::Eof);
    }

    #[tokio::test]
    async fn splice_aborts_when_remote_vanishes() {
        let (_app, local) = tokio::io::duplex(1024);
        let (lr, lw) = tokio::io::split(local);
        let mut out = Vec::new();
        // No Eof: the framed side just ends.
        assert!(splice(lr, lw, &b""[..], &mut out).await.is_err());
    }
}
