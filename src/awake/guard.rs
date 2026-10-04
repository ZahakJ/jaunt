//! The guard loop: every 5 s (or at once on `host on|off`) read the sensors, ask logind,
//! step the state machine and carry out its actions. It runs on its own OS thread so a
//! stuck sysfs read or child process cannot stall the async runtime.
//!
//! A hung sensor (a sysfs read in a driver that never returns) must not leave the lock
//! held (L3). So: the sensors are read on a helper thread with a deadline, and a reading
//! that does not come back in time counts as "unknown", which releases the lock. And if the
//! guard thread itself stops ticking, the daemon's failsafe kills the lock holder by its
//! process id (`Handle::failsafe`), with or without systemd; under systemd the watchdog
//! also stops being petted.

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::backend::Backend;
use super::lid::{self, Lid, Logind};
use super::machine::{Action, Inputs, Limits, State};
use super::power::{self, Power};
use super::sysfs::Roots;
use super::thermal::{self, HeatLimits};
use crate::proto::{HostReport, HostState};

pub const POLL: Duration = Duration::from_secs(5);
/// A tick may legitimately take ~16 s (busctl 3 s, lock 3 s, suspend 10 s).
const STALE: Duration = Duration::from_secs(25);
/// How long the sensors may take to read before they count as unknown.
const SENSOR_DEADLINE: Duration = Duration::from_secs(2);
/// Reader threads stuck in a hung read that are tolerated before not trying any more.
const MAX_STUCK_READERS: usize = 4;

/// One reading of everything the guard looks at in sysfs and procfs.
struct Readings {
    pw: power::PowerReading,
    sensors: Vec<thermal::Sensor>,
    lid: Lid,
}

/// Read the sensors on a helper thread; `None` if they did not answer in time (the thread
/// is left behind, counted in `stuck`, and gives up its slot if the read ever returns).
fn read_with_deadline(roots: &Roots, logind: Logind, stuck: &Arc<AtomicUsize>) -> Option<Readings> {
    if stuck.load(Ordering::Relaxed) >= MAX_STUCK_READERS {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    let roots = roots.clone();
    let s = stuck.clone();
    s.fetch_add(1, Ordering::Relaxed);
    let spawned = std::thread::Builder::new().name("jaunt-sensors".into()).spawn(move || {
        let r = Readings { pw: power::read(&roots), sensors: thermal::read(&roots), lid: lid::decide(&logind, &roots) };
        s.fetch_sub(1, Ordering::Relaxed);
        let _ = tx.send(r);
    });
    if spawned.is_err() {
        stuck.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    rx.recv_timeout(SENSOR_DEADLINE).ok()
}

pub struct Guard<B: Backend> {
    pub backend: B,
    pub roots: Roots,
    /// Ask logind (false when the sensor roots point at a fake tree).
    pub use_logind: bool,
    pub heat: HeatLimits,
    pub limits: Limits,
    pub enabled: bool,
    pub state: State,
    /// macOS: no sensors or lid lock; hold caffeinate while on.
    pub mac: bool,
    /// A CPU temperature sensor was readable since host mode went on (L12).
    pub cpu_seen: bool,
    /// Sensor reader threads currently stuck in a read.
    pub stuck: Arc<AtomicUsize>,
}

impl<B: Backend> Guard<B> {
    pub fn tick(&mut self, now_ms: u64) -> HostReport {
        if self.mac {
            return self.tick_mac();
        }
        let before = (self.state.mode, self.state.host);
        if !self.enabled {
            // Off: touch no sensors (an NVMe temperature read can wake the drive).
            if self.backend.held() {
                self.backend.release();
            }
            self.state = State::default();
            self.cpu_seen = false;
            return off_report("host mode is off");
        }
        let logind = if self.use_logind { self.backend.logind() } else { Default::default() };
        let (pw, sensors, lid) = match read_with_deadline(&self.roots, logind, &self.stuck) {
            Some(r) => (r.pw, r.sensors, r.lid),
            None => {
                let why = format!("the sensors did not answer within {} s", SENSOR_DEADLINE.as_secs());
                (power::PowerReading { power: Power::Unknown(why), battery_pct: None }, Vec::new(), Lid::Unknown)
            }
        };
        let mut power = pw.power;
        if power == Power::Ac && logind.on_external_power == Some(false) {
            power = Power::Unknown("logind says battery while the power supplies say mains".into());
        }
        let any_cpu = sensors.iter().any(|s| s.cpu);
        let heat = if self.cpu_seen && !any_cpu && !sensors.is_empty() {
            // The CPU sensors went away; judging by a cool drive or Wi-Fi chip would hide
            // a hot CPU (L12).
            thermal::Heat::Unknown("the CPU temperature sensors stopped answering".into())
        } else {
            thermal::assess(&sensors, self.heat)
        };
        self.cpu_seen |= any_cpu;
        let docked = logind.docked.unwrap_or(false);
        let inputs = Inputs { enabled: self.enabled, power: power.clone(), lid, docked, heat, held: self.backend.held() };
        for act in self.state.step(&inputs, now_ms, &self.limits) {
            match act {
                Action::Acquire => match self.backend.acquire() {
                    Ok(()) => {
                        tracing::info!("host mode: lid lock taken");
                        self.state.acquired();
                    }
                    Err(e) => {
                        tracing::warn!("host mode: cannot take the lid lock: {e}");
                        self.state.acquire_failed(now_ms, &e, &self.limits);
                    }
                },
                Action::Release => {
                    self.backend.release();
                    tracing::info!("host mode: lid lock released ({})", self.state.reason);
                }
                Action::Suspend => {
                    tracing::warn!("host mode: asking to suspend ({})", self.state.reason);
                    if let Err(e) = self.backend.suspend() {
                        tracing::warn!("host mode: suspend failed: {e}");
                    }
                }
            }
        }
        if before != (self.state.mode, self.state.host) {
            tracing::info!("host mode: {} ({})", self.state.host.label(), self.state.reason);
        }
        let held = self.backend.held();
        let on_lid_close = if held {
            "stays awake and reachable"
        } else if docked {
            "does what logind does when docked (an external display or dock is attached)"
        } else {
            "sleeps as usual"
        };
        let power_words = match (&power, pw.battery_pct) {
            (Power::Ac, _) | (Power::Unknown(_), _) | (_, None) => power.words().to_string(),
            (_, Some(p)) => format!("{} ({p} %)", power.words()),
        };
        HostReport {
            state: self.state.host,
            reason: self.state.reason.clone(),
            on_lid_close: on_lid_close.into(),
            lock_held: held,
            lid: lid.words().into(),
            power: power_words,
            hottest: thermal::hottest(&sensors).map(|s| format!("{} {:.0} °C", s.name, s.c)),
        }
    }

    fn tick_mac(&mut self) -> HostReport {
        let held = self.backend.held();
        if self.enabled && !held {
            if let Err(e) = self.backend.acquire() {
                tracing::warn!("host mode: {e}");
            }
        } else if !self.enabled && held {
            self.backend.release();
        }
        let held = self.backend.held();
        HostReport {
            state: if self.enabled { HostState::WillSleep } else { HostState::Off },
            reason: if self.enabled {
                "closed-lid hosting needs root on macOS; holding caffeinate -s while the lid is open (unverified)".into()
            } else {
                "host mode is off".into()
            },
            on_lid_close: "sleeps (macOS)".into(),
            lock_held: held,
            lid: Lid::Unknown.words().into(),
            power: "unknown".into(),
            hottest: None,
        }
    }
}

/// The daemon's side: latest report, a way to switch host mode, and liveness for the watchdog.
pub struct Handle {
    report: Arc<Mutex<HostReport>>,
    tx: Mutex<mpsc::Sender<bool>>,
    last_tick: Arc<AtomicU64>,
    epoch: Instant,
    /// Process id of the lock holder (0 = none), for the failsafe.
    holder: Arc<AtomicI32>,
}

impl Handle {
    pub fn report(&self) -> HostReport {
        let mut r = self.report.lock().map(|r| r.clone()).unwrap_or_else(|_| off_report("the host-mode guard failed"));
        if !self.alive() && r.state != HostState::Off {
            // Never show a stale "awake" (L3).
            let age = self.epoch.elapsed().saturating_sub(Duration::from_millis(self.last_tick.load(Ordering::Relaxed)));
            r.state = HostState::WillSleep;
            r.reason = format!("the host-mode guard has not answered for {} s; the lid lock was let go", age.as_secs());
            r.lock_held = self.holder.load(Ordering::Relaxed) != 0;
            r.on_lid_close = "sleeps as usual".into();
        }
        r
    }

    /// Called by the daemon every few seconds, whatever runs it: if the guard thread is
    /// stuck, kill the lock holder by its pid so logind drops the lock (L3). The holder is
    /// the guard's own unreaped child, so its pid cannot have been reused.
    pub fn failsafe(&self) {
        if self.alive() {
            return;
        }
        let pid = self.holder.swap(0, Ordering::Relaxed);
        if pid > 0
            && let Some(p) = rustix::process::Pid::from_raw(pid)
        {
            tracing::error!("the host-mode guard is stuck; killing the lid-lock holder (pid {pid}) so the lock is released");
            let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
        }
    }

    pub fn set_enabled(&self, on: bool) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send(on);
        }
    }

    /// The guard finished a tick recently.
    pub fn alive(&self) -> bool {
        let last = Duration::from_millis(self.last_tick.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(last) < STALE
    }
}

fn off_report(reason: &str) -> HostReport {
    HostReport {
        state: HostState::Off,
        reason: reason.into(),
        on_lid_close: "sleeps as usual (host mode is off)".into(),
        lock_held: false,
        lid: "unknown".into(),
        power: "unknown".into(),
        hottest: None,
    }
}

pub fn spawn<B: Backend + 'static>(mut guard: Guard<B>) -> Handle {
    let epoch = Instant::now();
    let report = Arc::new(Mutex::new(off_report("host mode is off")));
    let last_tick = Arc::new(AtomicU64::new(0));
    let holder = Arc::new(AtomicI32::new(0));
    let (tx, rx) = mpsc::channel::<bool>();
    let (r, lt, h) = (report.clone(), last_tick.clone(), holder.clone());
    let spawned = std::thread::Builder::new().name("jaunt-host".into()).spawn(move || {
        loop {
            let now = epoch.elapsed().as_millis() as u64;
            let rep = guard.tick(now);
            h.store(guard.backend.holder_pid().unwrap_or(0), Ordering::Relaxed);
            if let Ok(mut g) = r.lock() {
                *g = rep;
            }
            lt.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
            match rx.recv_timeout(POLL) {
                Ok(on) => guard.enabled = on,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    guard.backend.release();
                    return;
                }
            }
        }
    });
    if let Err(e) = spawned {
        tracing::error!("cannot start the host-mode guard: {e}");
    }
    Handle { report, tx: Mutex::new(tx), last_tick, epoch, holder }
}
