//! systemd user unit (`jaunt up` / `jaunt down`) and the sd_notify protocol
//! (READY=1 and the watchdog keep-alive), written by hand: one datagram per message.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;

pub fn unit_text(exe: &Path, paths: &Paths) -> String {
    let env = match &paths.jaunt_home {
        Some(h) => format!("Environment=JAUNT_HOME={}\n", h.display()),
        None => String::new(),
    };
    format!(
        "[Unit]\n\
         Description=jaunt daemon (links this machine to the owner's other machines)\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=notify\n\
         ExecStart={exe} daemon\n\
         {env}\
         Restart=on-failure\n\
         RestartSec=2\n\
         # Restarting jaunt must never kill the tmux sessions it started.\n\
         KillMode=process\n\
         WatchdogSec=30\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display()
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let st = Command::new("systemctl").arg("--user").args(args).status().context("cannot run systemctl")?;
    if !st.success() {
        bail!("systemctl --user {} failed ({st})", args.join(" "));
    }
    Ok(())
}

/// Write the unit; enable and start it unless `JAUNT_HOME` is set (test mode never touches
/// the real user manager).
pub fn up(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe().context("cannot find the jaunt binary path")?;
    let unit = paths.unit_file();
    std::fs::create_dir_all(&paths.systemd_user_dir)?;
    std::fs::write(&unit, unit_text(&exe, paths)).with_context(|| format!("cannot write {}", unit.display()))?;
    println!("wrote {}", unit.display());
    if paths.jaunt_home.is_some() {
        println!("JAUNT_HOME is set: not asking systemd to start it (test mode)");
        return Ok(());
    }
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "jaunt.service"])?;
    println!("jaunt daemon enabled and started (systemctl --user status jaunt)");
    Ok(())
}

pub fn unit_installed(paths: &Paths) -> bool {
    paths.jaunt_home.is_none() && paths.unit_file().exists()
}

pub fn start_unit() -> Result<()> {
    systemctl(&["start", "jaunt.service"])
}

pub fn down_unit() -> Result<()> {
    systemctl(&["disable", "--now", "jaunt.service"])
}

/// Send one sd_notify message if `NOTIFY_SOCKET` is set.
pub fn notify(msg: &str) {
    let Some(sock) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let s = sock.to_string_lossy().to_string();
    let Ok(dgram) = std::os::unix::net::UnixDatagram::unbound() else { return };
    #[cfg(target_os = "linux")]
    let res = if let Some(name) = s.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| dgram.send_to_addr(msg.as_bytes(), &addr))
    } else {
        dgram.send_to(msg.as_bytes(), &s)
    };
    #[cfg(not(target_os = "linux"))]
    let res = dgram.send_to(msg.as_bytes(), &s);
    if let Err(e) = res {
        tracing::debug!("sd_notify failed: {e}");
    }
}

/// Half the watchdog interval systemd asked for, if any.
pub fn watchdog_interval() -> Option<Duration> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    if let Some(pid) = std::env::var("WATCHDOG_PID").ok().and_then(|p| p.parse::<u32>().ok())
        && pid != std::process::id()
    {
        return None;
    }
    Some(Duration::from_micros(usec / 2).max(Duration::from_millis(500)))
}
