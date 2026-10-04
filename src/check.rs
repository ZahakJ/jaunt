//! `jaunt check`: a basic diagnosis. Each line is `ok`, `warn` or `fail` plus words.

use std::os::unix::fs::MetadataExt;

use anyhow::Result;

use crate::config::Config;
use crate::paths::Paths;
use crate::proto::{IpcRequest, IpcResponse};
use crate::{identity, ipc, shell};

fn line(level: &str, msg: impl AsRef<str>) {
    println!("{level}\t{}", msg.as_ref());
}

/// Returns the number of failures.
pub async fn run(paths: &Paths) -> Result<usize> {
    let mut fails = 0;
    match identity::load(paths) {
        Ok(Some(k)) => line("ok", format!("key {} (id {})", paths.key_file().display(), k.public())),
        Ok(None) => line("warn", "no key yet; the daemon creates one on first start"),
        Err(e) => {
            fails += 1;
            line("fail", format!("{e:#}"));
        }
    }
    if let Ok(m) = std::fs::metadata(&paths.data_dir)
        && m.mode() & 0o077 != 0
    {
        line("warn", format!("{} is readable by other users", paths.data_dir.display()));
    }
    let cfg = match Config::load(paths) {
        Ok(c) => c,
        Err(e) => {
            fails += 1;
            line("fail", format!("{e:#}"));
            Config::default()
        }
    };
    match shell::which("tmux") {
        Some(t) => line("ok", format!("tmux at {} (sessions survive disconnects)", t.display())),
        None => line("warn", "tmux is not installed: hops here get a plain shell that ends when the link drops"),
    }
    let Some(mut s) = ipc::connect_existing(paths).await else {
        line("warn", "the daemon is not running (any jaunt command starts it; 'jaunt up' installs the service)");
        if cfg.bootstrap_relays().is_empty() {
            line("warn", "no relay configured in config.toml");
        }
        return Ok(fails);
    };
    match ipc::send_request(&mut s, &IpcRequest::Status).await? {
        IpcResponse::Status(st) => {
            line("ok", format!("daemon running (pid {}, jaunt {}), this machine is {}", st.daemon_pid, st.version, st.me.name));
            if st.relays.is_empty() {
                fails += 1;
                line("fail", "no relay configured: other machines cannot be found (jaunt relay set URL)");
            } else if st.relay_connected {
                line("ok", format!("relay connected ({})", st.relays.join(", ")));
            } else {
                fails += 1;
                line("fail", format!("relay not connected ({})", st.relays.join(", ")));
            }
            line("ok", format!("UDP bound on {}", st.bound.join(", ")));
            if st.peers.is_empty() {
                line("warn", "not linked to any other machine yet");
            }
            for p in &st.peers {
                let lvl = if p.state.as_str() == "offline" { "warn" } else { "ok" };
                let err = p.last_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default();
                line(lvl, format!("{}: {}{}", p.name, p.state.as_str(), err));
            }
        }
        IpcResponse::Error { message } => {
            fails += 1;
            line("fail", message);
        }
        _ => {}
    }
    Ok(fails)
}
