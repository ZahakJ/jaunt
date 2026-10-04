//! `jaunt host on|off|status`, and the host-mode lines of `jaunt check`.

use std::time::Duration;

use anyhow::Result;

use crate::config::Config;
use crate::paths::Paths;
use crate::proto::{HostReport, IpcRequest, IpcResponse};
use crate::ipc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Info,
}

impl Level {
    pub fn word(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Info => "info",
        }
    }
}

#[cfg(not(target_os = "macos"))]
const RULE_FILE: &str = "/etc/polkit-1/rules.d/49-jaunt-host.rules";

pub async fn set(paths: &Paths, on: bool) -> Result<i32> {
    println!("{}", ipc::call_ok(paths, &IpcRequest::SetHost { on }).await?);
    // The guard acts at once on the change; give it a moment to report.
    let mut report = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        report = daemon_report(paths).await;
        if let Some(r) = &report
            && (r.state != crate::proto::HostState::Off) == on
        {
            break;
        }
    }
    if let Some(r) = report {
        print_report(&r);
    }
    if on {
        for (lvl, msg) in prereqs(paths) {
            if lvl == Level::Warn {
                println!("{}\t{msg}", lvl.word());
            }
        }
    }
    Ok(0)
}

pub async fn status(paths: &Paths) -> Result<i32> {
    let enabled = Config::load(paths).map(|c| c.host.enabled).unwrap_or(false);
    println!("host mode\t{}", if enabled { "on" } else { "off" });
    match daemon_report(paths).await {
        Some(r) => print_report(&r),
        None => println!(
            "daemon\t\tnot running; {}",
            if enabled { "host mode takes effect when it starts" } else { "nothing is held" }
        ),
    }
    for (lvl, msg) in prereqs(paths) {
        println!("{}\t\t{msg}", lvl.word());
    }
    Ok(0)
}

/// Ask a running daemon only: `status` should not start one.
async fn daemon_report(paths: &Paths) -> Option<HostReport> {
    let mut s = ipc::connect_existing(paths).await?;
    match ipc::send_request(&mut s, &IpcRequest::Status).await {
        Ok(IpcResponse::Status(st)) => st.host,
        _ => None,
    }
}

fn print_report(r: &HostReport) {
    println!("right now\t{}: {}", r.state.label(), r.reason);
    println!("on lid close\t{}", r.on_lid_close);
    if r.state != crate::proto::HostState::Off {
        let hot = r.hottest.as_deref().map(|h| format!(" · hottest {h}")).unwrap_or_default();
        println!("sensors\t\tlid {} · power {}{hot}", r.lid, r.power);
    }
    println!("lid lock\t{}", if r.lock_held { "held (systemd-inhibit --list shows 'jaunt')" } else { "not held" });
}

/// What host mode needs on this machine, in plain words.
#[cfg(target_os = "macos")]
pub fn prereqs(_paths: &Paths) -> Vec<(Level, String)> {
    vec![(
        Level::Warn,
        "closed-lid hosting needs root on macOS; host mode only holds caffeinate -s while the lid is open (unverified)".into(),
    )]
}

#[cfg(not(target_os = "macos"))]
pub fn prereqs(paths: &Paths) -> Vec<(Level, String)> {
    use super::backend::run_capture;
    let t = Duration::from_secs(3);
    let mut out = Vec::new();
    if crate::shell::which("systemd-inhibit").is_none() {
        out.push((Level::Warn, "systemd-inhibit is not installed: host mode needs systemd-logind".into()));
        return out;
    }
    let uid = rustix::process::getuid().as_raw().to_string();
    let display = run_capture(&["loginctl", "show-user", &uid, "--property=Display", "--value"], t)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let rule = match std::fs::metadata(RULE_FILE) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    if !display.is_empty() {
        out.push((Level::Ok, format!("local session {display} is logged in: logind lets the daemon take the lid lock")));
    } else if rule == Some(true) {
        out.push((Level::Ok, "no local session, but the polkit rule allows the lid lock".into()));
    } else {
        out.push((
            Level::Warn,
            "no local graphical session: logind refuses the lid lock until you log in at this machine once after \
             each boot, or until scripts/root-extras/host-polkit-rule.sh is installed"
                .into(),
        ));
    }
    match rule {
        Some(true) => out.push((Level::Info, format!("polkit rule {RULE_FILE} is installed"))),
        Some(false) => out.push((Level::Info, "polkit rule not installed (only needed to host before the first local login)".into())),
        None => out.push((Level::Info, "cannot see /etc/polkit-1/rules.d (root only), so cannot tell if the polkit rule is installed".into())),
    }
    if std::env::var_os("SSH_CONNECTION").is_some() && !crate::systemd::unit_installed(paths) {
        out.push((
            Level::Warn,
            "this is an SSH login and the jaunt service is not installed: a daemon started from here belongs to the SSH \
             session and logind refuses it the lid lock; run 'jaunt up' so it runs as a user service"
                .into(),
        ));
    }
    if let Ok(s) = run_capture(
        &[
            "busctl",
            "--system",
            "get-property",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "HandleLidSwitch",
            "HandleLidSwitchExternalPower",
            "HandleLidSwitchDocked",
            "Docked",
        ],
        t,
    ) {
        let v: Vec<String> = s.lines().map(|l| l.trim().trim_start_matches("s ").trim_start_matches("b ").trim_matches('"').to_string()).collect();
        if v.len() == 4 {
            let on_ac = if v[1].is_empty() { v[0].clone() } else { v[1].clone() };
            out.push((
                Level::Info,
                format!("without the lock, logind does this on lid close: {} (on mains: {on_ac}; docked: {})", v[0], v[2]),
            ));
            if v[3] == "true" {
                out.push((
                    Level::Info,
                    "logind counts this machine as docked (a dock or more than one display): it uses the docked setting".into(),
                ));
            }
        }
    }
    out
}
