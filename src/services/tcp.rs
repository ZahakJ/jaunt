//! `Tcp { port }`: the host connects to its own loopback port and splices it with the
//! stream. After the `Reply`, both directions carry `Data` frames; `Eof` is a half-close.
//! A side finishes its stream only when both directions are done, because the client's
//! IPC relay ends the whole stream as soon as either half closes.
//! Client commands: `jaunt fwd` (local listener) and `jaunt pipe` (stdio, for ProxyCommand).
//!
//! `Dial { host, port }` is the same to any address the host reaches (`jaunt socks`), and
//! `Listen { port, to }` is a reverse forward (`jaunt rfwd`): the host listens on its
//! loopback and dials each connection back to the requester as a `Tcp` stream.

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

/// Host side of `Dial`: like `Tcp`, to any host and port this machine can reach. A member
/// may already run any command here, so this grants nothing new.
pub async fn serve_dial(d: &Arc<Daemon>, host: String, port: u16, mut send: SendStream, recv: RecvStream) -> Result<()> {
    if port == 0 || host.is_empty() || host.len() > 255 || host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return refuse(&mut send, "not a host and port".into()).await;
    }
    let target = if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    // `localhost` as the loopback forward does it: both families, whatever /etc/hosts says.
    let connect = async {
        if host.eq_ignore_ascii_case("localhost") { connect_loopback(port).await } else { TcpStream::connect(&target).await }
    };
    let tcp = match tokio::time::timeout(CONNECT_TIMEOUT * 2, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return refuse(&mut send, format!("{} cannot reach {target}: {e}", d.my_name())).await,
        Err(_) => return refuse(&mut send, format!("{} cannot reach {target}: timed out", d.my_name())).await,
    };
    let _ = tcp.set_nodelay(true);
    proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
    let (tr, tw) = tcp.into_split();
    match splice(tr, tw, recv, &mut send).await {
        Ok(()) => {
            send.finish()?;
            let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        }
        Err(_) => {
            let _ = send.reset(0u32.into());
        }
    }
    Ok(())
}

/// Host side of `Listen`: listen on loopback `port` while the stream stays open, and
/// carry each connection to `to` on the requester's loopback.
pub async fn serve_listen(d: &Arc<Daemon>, from: MachineId, port: u16, to: u16, mut send: SendStream, recv: RecvStream) -> Result<()> {
    if port == 0 || to == 0 {
        return refuse(&mut send, "port 0 is not a port".into()).await;
    }
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(l) => l,
        Err(e) => return refuse(&mut send, format!("{} cannot listen on 127.0.0.1:{port}: {e}", d.my_name())).await,
    };
    proto::write_json(&mut send, FrameType::Reply, &Reply::Ok).await?;
    let mut frames = proto::frame_reader(recv);
    loop {
        tokio::select! {
            f = frames.recv() => match f {
                Some(Ok(_)) => {}
                _ => return Ok(()),
            },
            a = listener.accept() => {
                let Ok((tcp, _)) = a else { continue };
                let d = d.clone();
                tokio::spawn(async move {
                    let _ = tcp.set_nodelay(true);
                    let back = async {
                        let conn = d.connect(from).await?;
                        crate::daemon::open_service(&conn, Service::Tcp { port: to }).await
                    };
                    match back.await {
                        Ok((mut w, r)) => {
                            let (tr, tw) = tcp.into_split();
                            if splice(tr, tw, r, &mut w).await.is_ok() {
                                let _ = w.finish();
                            }
                        }
                        Err(e) => tracing::debug!("reverse forward {port} -> {to}: {e:#}"),
                    }
                });
            }
        }
    }
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

/// `jaunt rfwd <machine> <port>[:<localport>]`: <port> on the machine's loopback reaches
/// <localport> here. Runs until interrupted.
pub async fn rfwd(paths: &Paths, machine: &str, spec: &str) -> Result<i32> {
    let (port, local) = parse_fwd_spec(spec)?;
    let (stream, peer, _, _) = ipc::open(paths, machine, Service::Listen { port, to: local }).await?;
    eprintln!("jaunt: 127.0.0.1:{port} on {} now reaches port {local} here; Ctrl-C stops", peer.name);
    let (mut r, _w) = stream.into_split();
    let mut b = [0u8; 256];
    // The stream carries nothing more; it ends when the machine goes away.
    while r.read(&mut b).await.map(|n| n > 0).unwrap_or(false) {}
    eprintln!("jaunt: the link to {} ended; the reverse forward is gone", peer.name);
    Ok(1)
}

/// `jaunt socks <machine> [port]`: a SOCKS5 proxy on 127.0.0.1 whose connections leave
/// from the machine (names are resolved there too).
pub async fn socks(paths: &Paths, machine: &str, local: u16) -> Result<i32> {
    let status = crate::status(paths).await?;
    if !status.peers.iter().any(|p| p.name == machine || p.name.starts_with(machine)) {
        bail!("no machine called '{machine}'");
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, local))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{local}"))?;
    eprintln!("jaunt: SOCKS5 proxy on 127.0.0.1:{local}, leaving from {machine}; Ctrl-C stops");
    eprintln!("       e.g. curl --proxy socks5h://127.0.0.1:{local} https://example.com");
    let paths = Arc::new(paths.clone());
    let machine: Arc<str> = machine.into();
    loop {
        let (tcp, _) = listener.accept().await.context("accept failed")?;
        let (paths, machine) = (paths.clone(), machine.clone());
        tokio::spawn(async move {
            if let Err(e) = socks_conn(&paths, &machine, tcp).await {
                tracing::debug!("socks: {e:#}");
            }
        });
    }
}

async fn socks_conn(paths: &Paths, machine: &str, mut tcp: TcpStream) -> Result<()> {
    let _ = tcp.set_nodelay(true);
    let (host, port) = match socks_handshake(&mut tcp).await? {
        Ok(t) => t,
        Err(code) => {
            tcp.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Ok(());
        }
    };
    match ipc::open(paths, machine, Service::Dial { host, port }).await {
        Ok((stream, _, _, _)) => {
            tcp.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            let (r, mut w) = stream.into_split();
            let (tr, tw) = tcp.into_split();
            splice(tr, tw, r, &mut w).await
        }
        Err(e) => {
            tracing::debug!("socks {machine}: {e:#}");
            // 5: connection refused; 4: host unreachable.
            let code = if format!("{e:#}").contains("refused") { 5 } else { 4 };
            tcp.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            Ok(())
        }
    }
}

/// SOCKS5, no authentication, CONNECT only. `Err(code)` is the reply code to send.
pub async fn socks_handshake<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<Result<(String, u16), u8>> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != 5 {
        bail!("not SOCKS5");
    }
    let mut methods = vec![0u8; head[1] as usize];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        s.write_all(&[5, 0xff]).await?;
        bail!("the client wants authentication");
    }
    s.write_all(&[5, 0]).await?;
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut n = [0u8; 1];
            s.read_exact(&mut n).await?;
            let mut name = vec![0u8; n[0] as usize];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).unwrap_or_default()
        }
        4 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        _ => return Ok(Err(8)),
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    if req[1] != 1 {
        return Ok(Err(7));
    }
    Ok(Ok((host, u16::from_be_bytes(port))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn socks5_requests() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(&[5, 1, 0, 5, 1, 0, 3, 11]).await.unwrap();
        client.write_all(b"example.com").await.unwrap();
        client.write_all(&443u16.to_be_bytes()).await.unwrap();
        let got = socks_handshake(&mut server).await.unwrap();
        assert_eq!(got, Ok(("example.com".into(), 443)));
        let mut ack = [0u8; 2];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack, [5, 0]);

        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(&[5, 1, 0, 5, 1, 0, 4]).await.unwrap();
        client.write_all(&Ipv6Addr::LOCALHOST.octets()).await.unwrap();
        client.write_all(&[0, 22]).await.unwrap();
        assert_eq!(socks_handshake(&mut server).await.unwrap(), Ok(("::1".into(), 22)));

        // BIND is not offered.
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(&[5, 1, 0, 5, 2, 0, 1, 1, 2, 3, 4, 0, 80]).await.unwrap();
        assert_eq!(socks_handshake(&mut server).await.unwrap(), Err(7));

        // Username/password only: refused.
        let (mut client, mut server) = tokio::io::duplex(1024);
        client.write_all(&[5, 1, 2]).await.unwrap();
        assert!(socks_handshake(&mut server).await.is_err());
        let mut ack = [0u8; 2];
        client.read_exact(&mut ack).await.unwrap();
        assert_eq!(ack, [5, 0xff]);
    }

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
