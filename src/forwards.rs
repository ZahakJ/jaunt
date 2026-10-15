//! Forwards held by the daemon (`jaunt fwd --keep`, `jaunt open`): a listener on
//! 127.0.0.1 whose every connection becomes a `Tcp` stream to the machine. Each connection
//! dials afresh, so a forward outlives the link going down, the machine sleeping or either
//! side roaming; it only stops with `jaunt unfwd` (or, unless kept, with the daemon).
//! Kept forwards live in config.toml and start with the daemon.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::net::TcpListener;

use crate::config::ForwardConfig;
use crate::daemon::Daemon;
use crate::proto::{ForwardStatus, Service};

/// While the local port is taken (a kept forward at daemon start), try again this often.
const REBIND: Duration = Duration::from_secs(10);

struct Held {
    status: ForwardStatus,
    active: Arc<AtomicU32>,
    error: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
pub struct Forwards {
    held: Mutex<BTreeMap<String, Held>>,
}

impl Forwards {
    pub fn list(&self) -> Vec<ForwardStatus> {
        let g = self.held.lock().unwrap_or_else(|e| e.into_inner());
        g.values()
            .map(|h| {
                let mut s = h.status.clone();
                s.active = h.active.load(Ordering::Relaxed);
                s.last_error = h.error.lock().ok().and_then(|e| e.clone());
                s
            })
            .collect()
    }

    fn remove(&self, key: &str) -> Option<ForwardStatus> {
        let mut g = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let name = g.iter().find(|(n, h)| *n == key || h.status.local.to_string() == key).map(|(n, _)| n.clone())?;
        let h = g.remove(&name)?;
        h.task.abort();
        Some(h.status)
    }
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}

async fn bind(local: u16, port: u16) -> std::io::Result<TcpListener> {
    if local != 0 {
        return TcpListener::bind((Ipv4Addr::LOCALHOST, local)).await;
    }
    // The same port number as on the machine when it is free here; otherwise any.
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(l) => Ok(l),
        Err(_) => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await,
    }
}

/// Start (or replace) a forward. Returns its status with the port it listens on.
pub async fn add(d: &Arc<Daemon>, machine: String, port: u16, local: u16, name: Option<String>, keep: bool) -> Result<ForwardStatus> {
    if port == 0 {
        bail!("port 0 is not a port");
    }
    let peer = d.resolve(&machine)?;
    let machine = d.name_of(&peer);
    let name = name.unwrap_or_else(|| format!("{machine}:{port}"));
    if !valid_name(&name) {
        bail!("'{name}' is not a forward name (letters, digits, '-', '_', '.', ':')");
    }
    // Asking again for the same forward is not an error: it is already there.
    if let Some(s) = d.forwards.list().into_iter().find(|s| s.name == name && s.machine == machine && s.port == port)
        && (local == 0 || local == s.local)
        && (s.keep || !keep)
    {
        return Ok(s);
    }
    d.forwards.remove(&name);
    let listener = bind(local, port).await.map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1:{}: {e}", if local == 0 { port } else { local }))?;
    let local = listener.local_addr()?.port();
    let status = ForwardStatus { name: name.clone(), machine: machine.clone(), port, local, keep, active: 0, last_error: None };
    if keep {
        save(d, |fwds| {
            fwds.retain(|f| f.name != name);
            fwds.push(ForwardConfig { name: name.clone(), machine: machine.clone(), port, local });
        })?;
    }
    start(d, status.clone(), Some(listener));
    Ok(status)
}

pub fn remove(d: &Arc<Daemon>, key: &str) -> Result<String> {
    let Some(s) = d.forwards.remove(key) else { bail!("no forward called '{key}' (jaunt ls lists them)") };
    if s.keep {
        save(d, |fwds| fwds.retain(|f| f.name != s.name))?;
    }
    Ok(format!("stopped {} (127.0.0.1:{} to port {} on {})", s.name, s.local, s.port, s.machine))
}

fn save(d: &Arc<Daemon>, edit: impl FnOnce(&mut Vec<ForwardConfig>)) -> Result<()> {
    d.edit_config(|c| edit(&mut c.forwards))
}

/// The kept forwards, at daemon start.
pub fn start_kept(d: &Arc<Daemon>) {
    for f in d.config().forwards {
        let status = ForwardStatus { name: f.name, machine: f.machine, port: f.port, local: f.local, keep: true, active: 0, last_error: None };
        start(d, status, None);
    }
}

fn start(d: &Arc<Daemon>, status: ForwardStatus, listener: Option<TcpListener>) {
    let active = Arc::new(AtomicU32::new(0));
    let error = Arc::new(Mutex::new(None));
    let task = tokio::spawn(serve(d.weak(), status.clone(), listener, active.clone(), error.clone()));
    let mut g = d.forwards.held.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(old) = g.insert(status.name.clone(), Held { status, active, error, task }) {
        old.task.abort();
    }
}

fn set_error(error: &Mutex<Option<String>>, e: Option<String>) {
    if let Ok(mut g) = error.lock() {
        *g = e;
    }
}

async fn serve(
    d: std::sync::Weak<Daemon>,
    s: ForwardStatus,
    listener: Option<TcpListener>,
    active: Arc<AtomicU32>,
    error: Arc<Mutex<Option<String>>>,
) {
    let listener = match listener {
        Some(l) => l,
        None => loop {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, s.local)).await {
                Ok(l) => {
                    set_error(&error, None);
                    break l;
                }
                Err(e) => {
                    set_error(&error, Some(format!("127.0.0.1:{} is taken: {e}", s.local)));
                    tokio::time::sleep(REBIND).await;
                }
            }
        },
    };
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Some(d) = d.upgrade() else { return };
        let (machine, port) = (s.machine.clone(), s.port);
        let (active, error) = (active.clone(), error.clone());
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            match d.open_stream(&machine, Service::Tcp { port }).await {
                Ok((_, _, _, mut send, recv)) => {
                    set_error(&error, None);
                    active.fetch_add(1, Ordering::Relaxed);
                    let (tr, tw) = tcp.into_split();
                    if crate::services::tcp::splice(tr, tw, recv, &mut send).await.is_ok() {
                        let _ = send.finish();
                    }
                    active.fetch_sub(1, Ordering::Relaxed);
                }
                Err(e) => set_error(&error, Some(crate::desktop::clean_line(&format!("{e:#}"), 200))),
            }
        });
    }
}
