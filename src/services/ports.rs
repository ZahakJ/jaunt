//! `Ports`: the host's listening TCP ports that its loopback reaches (bound to loopback or
//! to every address), with the owning process when it belongs to this user.
//! Linux reads `/proc/net/tcp{,6}` and maps socket inodes to processes through
//! `/proc/<pid>/fd`; macOS asks `lsof`. Client: `jaunt ports <machine>`.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

use crate::proto::Port;

const LISTEN: &str = "0A";
/// Start of Linux's default ephemeral port range.
const EPHEMERAL: u16 = 32768;

pub async fn list() -> Vec<Port> {
    if cfg!(target_os = "macos") {
        let out = tokio::process::Command::new("lsof").args(["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"]).output().await;
        return out.map(|o| parse_lsof(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default();
    }
    tokio::task::spawn_blocking(|| linux(Path::new("/proc"))).await.unwrap_or_default()
}

/// One `/proc/net/tcp{,6}` line: (bind, port, inode) of a listening socket.
fn parse_line(line: &str) -> Option<(String, u16, u64)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 10 || f[3] != LISTEN {
        return None;
    }
    let (addr, port) = f[1].split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let bind = match addr.len() {
        8 => Ipv4Addr::from(u32::from_str_radix(addr, 16).ok()?.swap_bytes()).to_string(),
        32 => {
            // Four 32-bit words, each in host (little-endian) order.
            let mut b = [0u8; 16];
            for w in 0..4 {
                let v = u32::from_str_radix(&addr[w * 8..w * 8 + 8], 16).ok()?.swap_bytes();
                b[w * 4..w * 4 + 4].copy_from_slice(&v.to_be_bytes());
            }
            let a = Ipv6Addr::from(b);
            a.to_ipv4_mapped().map(|v4| v4.to_string()).unwrap_or_else(|| a.to_string())
        }
        _ => return None,
    };
    Some((bind, port, f[9].parse().ok()?))
}

/// Reached by a connection to 127.0.0.1 or ::1.
fn loopback_reaches(bind: &str) -> bool {
    match bind.parse::<std::net::IpAddr>() {
        Ok(a) => a.is_loopback() || a.is_unspecified(),
        Err(_) => false,
    }
}

fn linux(proc: &Path) -> Vec<Port> {
    let mut socks = Vec::new();
    for f in ["net/tcp", "net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(proc.join(f)) {
            socks.extend(text.lines().skip(1).filter_map(parse_line).filter(|(b, _, _)| loopback_reaches(b)));
        }
    }
    let owners = socket_owners(proc);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let mut ports: BTreeMap<u16, Port> = BTreeMap::new();
    for (bind, port, inode) in socks {
        let owner = owners.get(&inode);
        let process = owner.and_then(|pid| process_name(proc, *pid));
        let what = owner.and_then(|pid| std::fs::read_link(proc.join(format!("{pid}/cwd"))).ok()).and_then(|cwd| {
            if Some(&cwd) == home.as_ref() || cwd == Path::new("/") {
                return None;
            }
            cwd.file_name().map(|n| n.to_string_lossy().trim_end_matches(" (deleted)").to_string())
        });
        // Unowned sockets in the ephemeral range are other users' or the kernel's noise.
        if process.is_none() && port >= EPHEMERAL {
            continue;
        }
        let p = Port { port, bind, process, what };
        // One row per port; keep the one that says the most.
        let better = ports.get(&port).is_none_or(|old| old.process.is_none() && p.process.is_some());
        if better {
            ports.insert(port, p);
        }
    }
    ports.into_values().map(clean).collect()
}

/// The program's file name (argv[0]): `comm` is what the main thread calls itself,
/// e.g. node's "node-MainThread".
fn process_name(proc: &Path, pid: u32) -> Option<String> {
    let cmdline = std::fs::read(proc.join(format!("{pid}/cmdline"))).ok()?;
    let argv0 = cmdline.split(|b| *b == 0).next().map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default();
    let base = argv0.rsplit('/').next().unwrap_or("").split_whitespace().next().unwrap_or("").to_string();
    if !base.is_empty() {
        return Some(base);
    }
    std::fs::read_to_string(proc.join(format!("{pid}/comm"))).ok().map(|s| s.trim().to_string())
}

/// Socket inode -> pid, over the processes this user may look into.
fn socket_owners(proc: &Path) -> HashMap<u64, u32> {
    let mut map = HashMap::new();
    let Ok(dir) = std::fs::read_dir(proc) else { return map };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(fds) = std::fs::read_dir(e.path().join("fd")) else { continue };
        for fd in fds.flatten() {
            if let Ok(t) = std::fs::read_link(fd.path())
                && let Some(n) = t.to_str().and_then(|s| s.strip_prefix("socket:[")).and_then(|s| s.strip_suffix(']'))
                && let Ok(inode) = n.parse()
            {
                map.entry(inode).or_insert(pid);
            }
        }
    }
    map
}

fn clean(mut p: Port) -> Port {
    p.process = p.process.map(|s| crate::desktop::clean_line(&s, 40));
    p.what = p.what.map(|s| crate::desktop::clean_line(&s, 60));
    p
}

/// `lsof -F pcn` output: `p<pid>`, `c<command>`, then `n<addr>:<port>` per socket.
fn parse_lsof(text: &str) -> Vec<Port> {
    let mut ports: BTreeMap<u16, Port> = BTreeMap::new();
    let mut cmd = None;
    for line in text.lines() {
        match line.split_at_checked(1) {
            Some(("p", _)) => cmd = None,
            Some(("c", c)) => cmd = Some(c.to_string()),
            Some(("n", n)) => {
                let Some((addr, port)) = n.rsplit_once(':') else { continue };
                let bind = match addr.trim_matches(|c| c == '[' || c == ']') {
                    "*" => "0.0.0.0".to_string(),
                    a => a.to_string(),
                };
                let Ok(port) = port.parse() else { continue };
                if loopback_reaches(&bind) {
                    ports.entry(port).or_insert_with(|| Port { port, bind, process: cmd.clone(), what: None });
                }
            }
            _ => {}
        }
    }
    ports.into_values().map(clean).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_net_lines() {
        let v4 = "   1: 0100007F:1E1B 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0000 100 0 0 10 0";
        assert_eq!(parse_line(v4), Some(("127.0.0.1".into(), 7707, 4242)));
        let any6 = "   0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 77 1 0000 100 0 0 10 0";
        assert_eq!(parse_line(any6), Some(("::".into(), 22, 77)));
        let lo6 = "   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 9 1 0000 100 0 0 10 0";
        assert_eq!(parse_line(lo6).unwrap().0, "::1");
        let mapped = "   0: 0000000000000000FFFF00000100007F:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 9 1 0000 100 0 0 10 0";
        assert_eq!(parse_line(mapped).unwrap().0, "127.0.0.1");
        let established = v4.replace(" 0A ", " 01 ");
        assert_eq!(parse_line(&established), None);
    }

    #[test]
    fn only_what_loopback_reaches() {
        for yes in ["127.0.0.1", "127.0.0.53", "::1", "0.0.0.0", "::"] {
            assert!(loopback_reaches(yes), "{yes}");
        }
        for no in ["192.168.1.10", "fe80::1", "x"] {
            assert!(!loopback_reaches(no), "{no}");
        }
    }

    #[test]
    fn lsof_output() {
        let text = "p100\ncnode\nn127.0.0.1:7707\nn[::1]:7707\np200\ncpostgres\nn*:5432\np300\ncsshd\nn192.168.1.4:22\n";
        let p = parse_lsof(text);
        let got: Vec<(u16, &str, Option<&str>)> = p.iter().map(|p| (p.port, p.bind.as_str(), p.process.as_deref())).collect();
        assert_eq!(got, [(5432, "0.0.0.0", Some("postgres")), (7707, "127.0.0.1", Some("node"))]);
    }

    /// The real /proc of the test process: a listener it opens is found with its name.
    #[cfg(target_os = "linux")]
    #[test]
    fn finds_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let ports = linux(Path::new("/proc"));
        let me = ports.iter().find(|p| p.port == port).expect("own listener listed");
        assert_eq!(me.bind, "127.0.0.1");
        assert!(me.process.is_some());
    }
}
