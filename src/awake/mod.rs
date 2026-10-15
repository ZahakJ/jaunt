//! Host mode (design §9): keep a laptop awake and reachable with the lid closed, only while
//! it is on mains power and not too hot; fail towards sleeping in every other case.
//!
//! `sysfs`/`power`/`thermal`/`lid` read the machine, `machine` is the pure state machine,
//! `backend` takes the lock and suspends, `guard` runs the loop, `cli` is `jaunt host`.

pub mod backend;
pub mod cli;
pub mod guard;
pub mod lid;
pub mod machine;
pub mod power;
pub mod sysfs;
pub mod thermal;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::Duration;

use crate::config::HostConfig;
use crate::paths::Paths;

pub use guard::Handle;

pub fn limits(cfg: &HostConfig) -> (thermal::HeatLimits, machine::Limits) {
    let mut heat = thermal::HeatLimits::default();
    let mut lim = machine::Limits::default();
    if let Some(c) = cfg.hot_c.filter(|c| (50.0..=110.0).contains(c)) {
        heat.hot_c = c;
    }
    if let Some(m) = cfg.crit_margin_c.filter(|m| (0.0..=30.0).contains(m)) {
        heat.crit_margin_c = m;
    }
    if let Some(s) = cfg.hot_secs.filter(|s| (10..=600).contains(s)) {
        lim.hot_for_ms = Duration::from_secs(s).as_millis() as u64;
    }
    (heat, lim)
}

/// Sensor roots: the real `/sys` and `/proc`, or, in test mode only (`JAUNT_HOME` set),
/// `JAUNT_SYS_ROOT` / `JAUNT_PROC_ROOT`. With a fake tree logind is not asked.
pub fn roots(paths: &Paths) -> (sysfs::Roots, bool) {
    let mut r = sysfs::Roots::default();
    let mut real = true;
    if paths.jaunt_home.is_some() {
        if let Some(s) = std::env::var_os("JAUNT_SYS_ROOT") {
            r.sys = PathBuf::from(s);
            real = false;
        }
        if let Some(p) = std::env::var_os("JAUNT_PROC_ROOT") {
            r.proc = PathBuf::from(p);
            real = false;
        }
    }
    (r, real)
}

/// Start the guard thread for the daemon.
pub fn start(paths: &Paths, cfg: &HostConfig) -> Handle {
    let (heat, limits) = limits(cfg);
    let (roots, use_logind) = roots(paths);
    let test_mode = paths.jaunt_home.is_some();
    #[cfg(target_os = "macos")]
    let (backend, mac) = {
        let _ = test_mode;
        (backend::Caffeinate::new(), true)
    };
    #[cfg(not(target_os = "macos"))]
    let (backend, mac) = (backend::Systemd::new(test_mode), false);
    guard::spawn(guard::Guard {
        backend,
        roots,
        use_logind,
        heat,
        limits,
        enabled: cfg.enabled,
        state: machine::State::default(),
        mac,
        cpu_seen: false,
        stuck: Default::default(),
    })
}
