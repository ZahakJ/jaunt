//! Client side of the desktop tools: tabs, send-tab and URL bridging, ports and open,
//! held forwards, clipboard, notifications.

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;

use crate::desktop;
use crate::ipc;
use crate::paths::Paths;
use crate::proto::{self, ClipOp, FrameType, IpcRequest, IpcResponse, Service};
use crate::term;
use crate::ui::picker::{self, Row};

/// Open `service` on `machine` and read the one `Json` frame it answers with.
async fn fetch<T: DeserializeOwned>(paths: &Paths, machine: &str, service: Service) -> Result<T> {
    let (mut s, _, _, _) = ipc::open(paths, machine, service).await?;
    let payload = proto::expect_frame(&mut s, FrameType::Json).await?;
    Ok(proto::parse_json(&payload, proto::MAX_FRAME)?)
}

/// The machine hopped in here (`active`: only while its session is open).
async fn visitor(paths: &Paths) -> Result<String> {
    ipc::call_ok(paths, &IpcRequest::Visitor).await
}

// ---------------------------------------------------------------- tabs

pub async fn tabs(paths: &Paths, machine: &str, all: bool, list: bool) -> Result<i32> {
    let tabs: Vec<proto::Tab> = fetch(paths, machine, Service::Tabs).await?;
    if tabs.is_empty() {
        eprintln!("jaunt: no open browser tabs found on {machine}");
        return Ok(1);
    }
    let chosen: Vec<usize> = if all {
        (0..tabs.len()).collect()
    } else if list || !term::stdout_is_tty() || !term::stdin_is_tty() {
        for t in &tabs {
            println!("{}\t{}\t{}", t.browser, term::safe_text(&t.title, 300, false), t.url);
        }
        return Ok(0);
    } else {
        let rows: Vec<Row> = tabs.iter().map(|t| Row { text: t.title.clone(), detail: format!("{}  {}", t.browser, t.url) }).collect();
        picker::pick(&format!("tabs on {machine}"), &rows, false).await?
    };
    let mut failed = 0;
    for i in &chosen {
        if let Err(e) = desktop::open_url(&tabs[*i].url).await {
            eprintln!("jaunt: {}: {e:#}", tabs[*i].url);
            failed += 1;
        }
    }
    if !chosen.is_empty() {
        eprintln!("jaunt: opened {} of {} tabs from {machine}", chosen.len() - failed, tabs.len());
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

/// `jaunt send-tab [machine] URL...`: with no machine, the one hopped in here.
pub async fn send_tab(paths: &Paths, mut args: Vec<String>) -> Result<i32> {
    let machine = match args.first() {
        Some(a) if !a.contains("://") => args.remove(0),
        _ => visitor(paths).await?,
    };
    if args.is_empty() {
        bail!("no URL given");
    }
    for url in &args {
        desktop::check_url(url).map_err(anyhow::Error::msg)?;
    }
    for url in args {
        fetch::<()>(paths, &machine, Service::OpenUrl { url }).await?;
    }
    Ok(0)
}

/// `jaunt xdg-open` (the session shim): web URLs to the visiting machine, the rest (and
/// everything when nobody is hopped in) to the real xdg-open.
pub async fn xdg_open(paths: &Paths, args: Vec<String>) -> Result<i32> {
    if let [url] = args.as_slice()
        && desktop::check_url(url).is_ok()
        && let Ok(machine) = active_visitor(paths).await
    {
        match fetch::<()>(paths, &machine, Service::OpenUrl { url: url.clone() }).await {
            Ok(()) => {
                eprintln!("jaunt: opened on {machine}: {url}");
                return Ok(0);
            }
            Err(e) => eprintln!("jaunt: could not open it on {machine} ({e:#}); opening here"),
        }
    }
    let Some(real) = desktop::real_xdg_open(paths) else { bail!("no xdg-open here") };
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(real);
    // BROWSER is the shim: the real xdg-open must not hand the URL back to it.
    if std::env::var_os("BROWSER").is_some_and(|b| std::path::Path::new(&b).starts_with(desktop::shim_dir(paths))) {
        cmd.env_remove("BROWSER");
    }
    let err = cmd.args(args).exec();
    bail!("cannot run xdg-open: {err}")
}

/// Only a machine whose hop is open right now (the shim must not send URLs to a laptop
/// that hopped in yesterday).
async fn active_visitor(paths: &Paths) -> Result<String> {
    if std::env::var_os("TMUX").is_none() && std::env::var_os("JAUNT_FROM").is_none() {
        bail!("not in a session jaunt started");
    }
    let s = crate::status(paths).await?;
    let v = visitor(paths).await?;
    if s.peers.iter().any(|p| p.name == v && p.state != proto::PeerState::Offline) { Ok(v) } else { bail!("{v} is offline") }
}

// ---------------------------------------------------------------- ports and open

pub async fn ports(paths: &Paths, machine: &str, json: bool) -> Result<i32> {
    let ports: Vec<proto::Port> = fetch(paths, machine, Service::Ports).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ports)?);
        return Ok(0);
    }
    if ports.is_empty() {
        eprintln!("jaunt: nothing listens on {machine}'s loopback");
        return Ok(1);
    }
    let label = |p: &proto::Port| {
        let mut s = format!("{:>5}  {}", p.port, p.process.as_deref().unwrap_or("?"));
        if let Some(w) = &p.what {
            s.push(' ');
            s.push_str(w);
        }
        s
    };
    if !term::stdout_is_tty() || !term::stdin_is_tty() {
        for p in &ports {
            println!("{}\t{}\t{}\t{}", p.port, p.bind, p.process.as_deref().unwrap_or("-"), p.what.as_deref().unwrap_or("-"));
        }
        return Ok(0);
    }
    let rows: Vec<Row> = ports.iter().map(|p| Row { text: label(p), detail: p.bind.clone() }).collect();
    let Some(&i) = picker::pick(&format!("ports on {machine}: a key forwards and opens"), &rows, true).await?.first() else {
        return Ok(0);
    };
    open(paths, &format!("{machine}:{}", ports[i].port), false, 0).await
}

/// `<machine>:<port>[/path]`
pub fn parse_open_target(t: &str) -> Result<(String, u16, String)> {
    let Some((machine, rest)) = t.split_once(':') else { bail!("expected <machine>:<port>, e.g. toluene:8080") };
    let (port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    match port.parse::<u16>() {
        Ok(p) if p > 0 && !machine.is_empty() => Ok((machine.to_string(), p, path.to_string())),
        _ => bail!("'{t}' is not <machine>:<port>"),
    }
}

/// `jaunt open <machine>:<port>[/path]`: a forward held by the daemon, then the browser.
pub async fn open(paths: &Paths, target: &str, keep: bool, local: u16) -> Result<i32> {
    let (machine, port, path) = parse_open_target(target)?;
    let f = forward(paths, machine, port, local, None, keep).await?;
    let url = format!("http://127.0.0.1:{}{path}", f.local);
    println!("{} -> {url}   (jaunt unfwd {} stops it)", f.name, f.name);
    if let Err(e) = desktop::open_url(&url).await {
        eprintln!("jaunt: cannot open a browser here ({e:#}); the forward is up");
    }
    Ok(0)
}

pub async fn forward(paths: &Paths, machine: String, port: u16, local: u16, name: Option<String>, keep: bool) -> Result<proto::ForwardStatus> {
    match ipc::call(paths, &IpcRequest::Forward { machine, port, local, name, keep }).await? {
        IpcResponse::Forward(f) => Ok(f),
        IpcResponse::Error { message } => bail!("{message}"),
        other => bail!("unexpected answer from the daemon: {other:?}"),
    }
}

pub async fn fwd_keep(paths: &Paths, machine: &str, spec: &str, name: Option<String>) -> Result<i32> {
    let (port, local) = crate::services::tcp::parse_fwd_spec(spec)?;
    let f = forward(paths, machine.to_string(), port, local, name, true).await?;
    println!("{}: 127.0.0.1:{} -> port {} on {}, kept across restarts (jaunt unfwd {} stops it)", f.name, f.local, f.port, f.machine, f.name);
    Ok(0)
}

// ---------------------------------------------------------------- clipboard

pub async fn clip_get(paths: &Paths, machine: &str) -> Result<i32> {
    let text: String = fetch(paths, machine, Service::Clip { op: ClipOp::Get }).await?;
    use std::io::Write;
    std::io::stdout().write_all(text.as_bytes())?;
    Ok(0)
}

pub async fn clip_set(paths: &Paths, machine: &str, text: Vec<String>) -> Result<i32> {
    let text = if text.is_empty() {
        let mut s = String::new();
        use std::io::Read;
        std::io::stdin().take(desktop::MAX_CLIP as u64 + 1).read_to_string(&mut s)?;
        s
    } else {
        text.join(" ")
    };
    if text.len() > desktop::MAX_CLIP {
        bail!("larger than {} KiB", desktop::MAX_CLIP / 1024);
    }
    let (s, peer, _, _) = ipc::open(paths, machine, Service::Clip { op: ClipOp::Set }).await?;
    let (mut r, mut w) = s.into_split();
    crate::daemon::send_clip(&mut w, &mut r, &text).await.map_err(|e| anyhow::anyhow!("{}: {e:#}", peer.name))?;
    Ok(0)
}

pub async fn clip_status(paths: &Paths) -> Result<i32> {
    let on = crate::config::Config::load(paths)?.clipboard;
    let tool = desktop::clip_tool_name().unwrap_or("none found");
    println!("clipboard sharing {} (tool: {tool})", if on { "on" } else { "off" });
    Ok(0)
}

// ---------------------------------------------------------------- notifications

/// `jaunt notify [--to machine] [text...]`: a notification on the machine hopped in here.
pub async fn notify(paths: &Paths, to: Option<String>, text: Vec<String>) -> Result<i32> {
    let machine = match to {
        Some(m) => m,
        None => visitor(paths).await?,
    };
    let text = text.join(" ");
    let (title, body) = match text.split_once('\n') {
        Some((t, b)) => (t.to_string(), b.to_string()),
        None if text.is_empty() => ("done".to_string(), String::new()),
        None => (text, String::new()),
    };
    fetch::<()>(paths, &machine, Service::Notify { title, body }).await?;
    Ok(0)
}

/// After `jaunt run --notify`: tell this machine's desktop how it went.
pub async fn notify_local(machine: &str, argv: &[String], code: i32, took: std::time::Duration) {
    let cmd = desktop::clean_line(&argv.join(" "), 80);
    let secs = took.as_secs();
    let took = if secs >= 60 { format!("{}m{:02}s", secs / 60, secs % 60) } else { format!("{secs}s") };
    let how = if code == 0 { "finished".to_string() } else { format!("failed ({code})") };
    let _ = desktop::notify(&format!("{machine} · {cmd}"), &format!("{how} after {took}")).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_targets() {
        assert_eq!(parse_open_target("benzene:7707").unwrap(), ("benzene".into(), 7707, "/".into()));
        assert_eq!(parse_open_target("b:80/a/b?c").unwrap(), ("b".into(), 80, "/a/b?c".into()));
        assert!(parse_open_target("benzene").is_err());
        assert!(parse_open_target(":80").is_err());
        assert!(parse_open_target("b:0").is_err());
        assert!(parse_open_target("b:x").is_err());
    }
}
