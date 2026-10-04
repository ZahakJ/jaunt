//! `jaunt check`: a diagnosis in plain words. Each line is `ok`, `warn` or `fail`, a tab,
//! and a sentence; the exit status is 1 when anything failed.
//!
//! The daemon half (`Contacts`, `report`) records every remote address the daemon has sent
//! QUIC packets to and lists its open sockets, so the owner can see that it talks to the
//! relay and the peers and to nothing else.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

use anyhow::Result;
use iroh::TransportAddr;
use iroh::endpoint::Connection;

use crate::config::Config;
use crate::daemon::Daemon;
use crate::paths::{Paths, read_capped};
use crate::proto::{CheckReport, Contact, IpcRequest, IpcResponse, NetSummary, RelayProbe, StatusSnapshot};
use crate::roster::now_ms;
use crate::{identity, ipc, shell};

const MAX_CONTACTS: usize = 1024;
const MAX_PROC_NET: u64 = 8 << 20;

// ------------------------------------------------------------------------- daemon side

/// Remote addresses of QUIC paths, as seen on live connections (kept for the daemon's life).
#[derive(Default)]
pub struct Contacts(Mutex<HashMap<SocketAddr, Contact>>);

impl Contacts {
    pub fn note_conn(&self, conn: &Connection, peer: &str) {
        let Ok(mut g) = self.0.lock() else { return };
        let now = now_ms();
        for p in conn.paths().iter() {
            if let TransportAddr::Ip(sa) = p.remote_addr() {
                if g.len() >= MAX_CONTACTS && !g.contains_key(sa) {
                    // Forget the stalest one; the list is a diagnosis aid, not a log.
                    if let Some(old) = g.iter().min_by_key(|(_, c)| c.last_ms).map(|(k, _)| *k) {
                        g.remove(&old);
                    }
                }
                g.insert(*sa, Contact { addr: sa.to_string(), via: "udp".into(), peer: Some(peer.to_string()), last_ms: now });
            }
        }
    }

    fn list(&self) -> Vec<Contact> {
        self.0.lock().map(|g| g.values().cloned().collect()).unwrap_or_default()
    }
}

pub async fn report(d: &Daemon) -> CheckReport {
    use n0_watcher::Watcher;
    let net = d.ep.net_report().get().map(|r| NetSummary {
        udp_v4: r.udp_v4,
        udp_v6: r.udp_v6,
        global_v4: r.global_v4.map(|a| a.to_string()),
        global_v6: r.global_v6.map(|a| a.to_string()),
        mapping_varies: r.mapping_varies_by_dest(),
        probes: r
            .relay_latency
            .iter()
            .map(|(probe, url, lat)| RelayProbe {
                url: url.to_string(),
                probe: match format!("{probe:?}").as_str() {
                    "Https" => "https".into(),
                    "QadIpv4" => "qad_ipv4".into(),
                    "QadIpv6" => "qad_ipv6".into(),
                    other => other.to_lowercase(),
                },
                ms: lat.as_secs_f64() * 1000.0,
            })
            .collect(),
    });
    let mut contacts = d.contacts.list();
    contacts.extend(open_sockets());
    contacts.sort_by(|a, b| (&a.via, &a.addr).cmp(&(&b.via, &b.addr)));
    let mut relay_ips = Vec::new();
    for url in d.relays() {
        if let Some(host) = url.host_str() {
            let port = url.port_or_known_default().unwrap_or(443);
            if let Ok(Ok(addrs)) = tokio::time::timeout(std::time::Duration::from_secs(3), tokio::net::lookup_host((host, port))).await {
                relay_ips.extend(addrs.map(|a| a.ip().to_string()));
            }
        }
    }
    relay_ips.sort();
    relay_ips.dedup();
    CheckReport { net, contacts, relay_ips }
}

/// This process's sockets that have a remote end, from /proc (Linux only).
fn open_sockets() -> Vec<Contact> {
    let mut inodes = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
        for e in rd.flatten() {
            if let Ok(t) = std::fs::read_link(e.path())
                && let Some(n) = t.to_str().and_then(|s| s.strip_prefix("socket:[")).and_then(|s| s.strip_suffix(']'))
                && let Ok(n) = n.parse::<u64>()
            {
                inodes.insert(n);
            }
        }
    }
    let mut out = Vec::new();
    for (file, via) in [("tcp", "tcp"), ("tcp6", "tcp"), ("udp", "udp-socket"), ("udp6", "udp-socket")] {
        let Ok(Some(bytes)) = read_capped(std::path::Path::new(&format!("/proc/self/net/{file}")), MAX_PROC_NET) else { continue };
        for (remote, inode) in parse_proc_net(&String::from_utf8_lossy(&bytes)) {
            if inodes.contains(&inode) && !remote.ip().is_unspecified() {
                out.push(Contact { addr: remote.to_string(), via: via.into(), peer: None, last_ms: now_ms() });
            }
        }
    }
    out
}

/// `(remote address, inode)` for each line of /proc/net/{tcp,udp}[6].
fn parse_proc_net(text: &str) -> Vec<(SocketAddr, u64)> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let remote = parse_hex_addr(f.get(2)?)?;
            let inode = f.get(9)?.parse().ok()?;
            Some((remote, inode))
        })
        .collect()
}

/// `0100007F:0016` (IPv4) or 32 hex digits + port (IPv6); each 32-bit word is in host order.
fn parse_hex_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| -> Option<[u8; 4]> { Some(u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok()?.to_ne_bytes()) };
    let ip = match ip.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(0)?)),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            let v6 = Ipv6Addr::from(b);
            v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

// ------------------------------------------------------------------------- client side

struct Out {
    fails: usize,
    warns: usize,
}

impl Out {
    fn ok(&mut self, msg: impl AsRef<str>) {
        println!("ok\t{}", msg.as_ref());
    }
    fn warn(&mut self, msg: impl AsRef<str>) {
        self.warns += 1;
        println!("warn\t{}", msg.as_ref());
    }
    fn fail(&mut self, msg: impl AsRef<str>) {
        self.fails += 1;
        println!("fail\t{}", msg.as_ref());
    }
}

/// Returns the number of failures.
pub async fn run(paths: &Paths) -> Result<usize> {
    let mut o = Out { fails: 0, warns: 0 };
    o.ok(format!("jaunt {} (this command)", crate::proto::VERSION));
    check_files(paths, &mut o);
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            o.fail(format!("{e:#}"));
            Config::default()
        }
    };
    match shell::which("tmux") {
        Some(t) => o.ok(format!("tmux at {} (sessions here survive disconnects)", t.display())),
        None => o.warn("tmux is not installed: hops to this machine get a plain shell that ends when the link drops"),
    }
    if cfg.host.enabled {
        for (lvl, msg) in crate::awake::cli::prereqs(paths) {
            match lvl {
                crate::awake::cli::Level::Warn => o.warn(format!("host mode: {msg}")),
                _ => o.ok(format!("host mode: {msg}")),
            }
        }
    } else {
        o.ok("host mode is off (jaunt host on keeps a laptop reachable with the lid closed)");
    }
    check_unit(paths, &mut o).await;
    // Never start the daemon from here: report on what is running.
    let Some(mut s) = ipc::connect_existing(paths).await else {
        o.warn("the daemon is not running (any jaunt command starts it; 'jaunt up' installs the service)");
        if cfg.bootstrap_relays().is_empty() {
            o.warn("no relay configured in config.toml");
        }
        return Ok(o.fails);
    };
    let st = match ipc::send_request(&mut s, &IpcRequest::Status).await? {
        IpcResponse::Status(st) => st,
        IpcResponse::Error { message } => {
            o.fail(message);
            return Ok(o.fails);
        }
        other => {
            o.fail(format!("unexpected answer from the daemon: {other:?}"));
            return Ok(o.fails);
        }
    };
    let ver = if st.version == crate::proto::VERSION { String::new() } else { ", a different version from this command: restart it with 'jaunt down'".to_string() };
    o.ok(format!("daemon running (pid {}, jaunt {}{ver}); this machine is {}", st.daemon_pid, st.version, st.me.name));
    if !ver.is_empty() {
        o.warn("the daemon is not the same version as this command");
    }
    let rep = match ipc::call(paths, &IpcRequest::Check).await {
        Ok(IpcResponse::Check(r)) => Some(r),
        _ => {
            o.warn("the daemon does not answer the check request (older version?)");
            None
        }
    };
    check_relay(&st, rep.as_ref(), &mut o);
    check_peers(&st, &mut o);
    if let Some(r) = &rep {
        check_contacts(&st, r, &mut o);
    }
    Ok(o.fails)
}

fn check_files(paths: &Paths, o: &mut Out) {
    let uid = rustix::process::getuid().as_raw();
    match identity::load(paths) {
        Ok(Some(k)) => {
            let f = paths.key_file();
            match std::fs::metadata(&f) {
                Ok(m) if m.mode() & 0o077 != 0 => o.fail(format!("the key {} can be read by other users (mode {:o}); run chmod 600 on it", f.display(), m.mode() & 0o777)),
                Ok(m) if m.uid() != uid => o.fail(format!("the key {} belongs to another user", f.display())),
                Ok(_) => o.ok(format!("key {} is private (0600); id {}", f.display(), k.public())),
                Err(e) => o.fail(format!("cannot inspect {}: {e}", f.display())),
            }
        }
        Ok(None) => o.warn("no key yet; the daemon creates one on first start"),
        Err(e) => o.fail(format!("{e:#}")),
    }
    for dir in [&paths.data_dir, &paths.runtime_dir] {
        if let Ok(m) = std::fs::metadata(dir)
            && m.mode() & 0o077 != 0
        {
            o.fail(format!("{} is open to other users (mode {:o}); run chmod 700 on it", dir.display(), m.mode() & 0o777));
        }
    }
}

async fn check_unit(paths: &Paths, o: &mut Out) {
    let unit = paths.unit_file();
    if paths.jaunt_home.is_some() {
        o.ok("JAUNT_HOME is set (test mode): the systemd user service is not used");
        return;
    }
    if !unit.exists() {
        o.warn("no systemd user service: the daemon does not start at login ('jaunt up' installs it)");
        return;
    }
    let q = |what: &'static str| async move {
        tokio::process::Command::new("systemctl")
            .args(["--user", what, "jaunt.service"])
            .output()
            .await
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let (active, enabled) = (q("is-active").await, q("is-enabled").await);
    match (active.as_deref(), enabled.as_deref()) {
        (Some("active"), Some("enabled")) => o.ok("systemd user service jaunt.service is enabled and running"),
        (None, _) => o.warn("cannot ask systemd about jaunt.service (systemctl not available)"),
        (a, e) => o.warn(format!(
            "systemd user service jaunt.service is {} and {} ('jaunt up' enables and starts it)",
            a.unwrap_or("unknown"),
            e.unwrap_or("unknown")
        )),
    }
}

fn check_relay(st: &StatusSnapshot, rep: Option<&CheckReport>, o: &mut Out) {
    if st.relays.is_empty() {
        o.fail("no relay configured: other machines cannot find this one (jaunt relay set URL)");
        return;
    }
    let probes = rep.and_then(|r| r.net.as_ref()).map(|n| n.probes.clone()).unwrap_or_default();
    for url in &st.relays {
        let norm = url.trim_end_matches('/');
        let https = probes.iter().find(|p| p.probe == "https" && p.url.trim_end_matches('/') == norm);
        match https {
            Some(p) => o.ok(format!("relay {url} answers its web probe ({:.1} ms)", p.ms)),
            None if st.relay_connected => o.ok(format!("relay {url} is connected (no probe result yet)")),
            None => o.fail(format!("relay {url} does not answer: check the URL, DNS and that its HTTPS port is reachable")),
        }
    }
    if st.relay_connected {
        o.ok("relay connected: other machines can reach this one")
    } else {
        o.fail("not connected to any relay: other machines cannot reach this one")
    }
    let Some(net) = rep.and_then(|r| r.net.as_ref()) else {
        o.warn("no network report yet (the first takes a few seconds after the daemon starts)");
        return;
    };
    if net.udp_v4 || net.udp_v6 {
        let addr = [&net.global_v4, &net.global_v6].into_iter().flatten().cloned().collect::<Vec<_>>().join(", ");
        o.ok(format!("UDP address discovery through the relay works; public address {addr}"));
        if net.mapping_varies == Some(true) {
            o.warn("this network's NAT uses a different public port per destination (hard NAT): direct paths are unlikely and traffic will go through the relay");
        }
    } else {
        o.warn("UDP address discovery got no answer (relay UDP port 7842 blocked, or UDP blocked here): direct paths through NAT are unlikely");
    }
}

fn check_peers(st: &StatusSnapshot, o: &mut Out) {
    if st.peers.is_empty() {
        o.warn("not linked to any other machine yet (jaunt link)");
        return;
    }
    let mut direct = Vec::new();
    for p in &st.peers {
        let err = p.last_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default();
        let rtt = p.rtt_ms.map(|r| format!(", {r:.1} ms")).unwrap_or_default();
        let line = format!("{}: {}{rtt}{err}", p.name, p.state.as_str());
        match p.state.as_str() {
            "direct" => {
                direct.push(p.name.clone());
                o.ok(line)
            }
            "relay" => o.ok(line),
            _ => o.warn(line),
        }
    }
    if direct.is_empty() {
        o.warn("no direct path to any machine: everything goes through the relay");
    } else {
        o.ok(format!("direct path to {}", direct.join(", ")));
    }
}

fn check_contacts(st: &StatusSnapshot, rep: &CheckReport, o: &mut Out) {
    let mut unexpected = 0;
    for c in &rep.contacts {
        let ip = c.addr.parse::<SocketAddr>().map(|a| a.ip()).ok();
        let port = c.addr.parse::<SocketAddr>().map(|a| a.port()).ok();
        let what = if let Some(p) = &c.peer {
            format!("machine {p}")
        } else if ip.is_some_and(|ip| rep.relay_ips.contains(&ip.to_string())) {
            "the relay".to_string()
        } else if port == Some(53) {
            "the system DNS resolver".to_string()
        } else if ip.is_some_and(|ip| ip.is_loopback()) {
            "this machine (loopback)".to_string()
        } else {
            unexpected += 1;
            String::new()
        };
        if what.is_empty() {
            o.warn(format!("contact {} ({}): not the relay and not a known machine", c.addr, c.via));
        } else {
            println!("info\tcontact {} ({}): {what}", c.addr, c.via);
        }
    }
    let n = rep.contacts.len();
    if unexpected == 0 {
        o.ok(format!("{n} remote addresses seen, all the relay or linked machines ({} relays, {} machines)", st.relays.len(), st.peers.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_net_lines() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0277 0A64E7C6:01BB 01 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000000000000000 100 0 0 10 0\n";
        let v = parse_proc_net(tcp);
        assert_eq!(v, vec![("198.231.100.10:443".parse().unwrap(), 4242)]);
        // IPv4-mapped IPv6: ::ffff:198.51.100.10 port 7842
        let a = parse_hex_addr("0000000000000000FFFF00000A6433C6:1EA2").unwrap();
        assert_eq!(a, "198.51.100.10:7842".parse().unwrap());
        assert_eq!(parse_hex_addr("00000000:0000").unwrap().ip(), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert!(parse_hex_addr("zz:1").is_none());
        assert!(parse_proc_net("header\ngarbage line\n").is_empty());
    }
}
