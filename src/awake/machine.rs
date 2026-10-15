//! The host-mode state machine: a pure function of (inputs, previous state, time).
//!
//! Modes: `Off` (host mode off) · `Armed` (on, not holding: conditions not met or the lock
//! was refused) · `Holding` (lid lock held) · `Tripped` (released after overheating; stays
//! released until the lid is seen open) · `Suspending` (released with the lid closed; asks
//! for suspend after a short grace in case logind does not do it by itself).
//!
//! Everything unknown counts against holding: unknown power, no readable temperature.
//!
//! Durations ("hot for 120 s", "on battery for 10 s", "draining for 60 s") are leaky
//! accumulators, not "since the condition last became true": the time between two
//! samples counts towards a condition that held at the earlier sample, and time without it
//! pays the total back at half speed. So heat that dips below the limit for one sample
//! every two minutes still trips, and a flapping cable or a weak charger still releases.
//! After a power-caused release the lock is taken again only once mains has been steady
//! long enough to pay the accumulators back to zero.

use super::lid::Lid;
use super::power::Power;
use super::thermal::Heat;
use crate::proto::HostState;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// How long `Heat::Hot` must last before tripping.
    pub hot_for_ms: u64,
    /// A cable wiggle shorter than this does not release the lock.
    pub ac_debounce_ms: u64,
    /// Mains online but the battery draining for this long counts as battery.
    pub draining_ms: u64,
    /// After releasing with the lid closed, wait this long for logind before suspending.
    pub suspend_grace_ms: u64,
    /// If still awake after asking, ask again this much later.
    pub suspend_retry_ms: u64,
    /// After the lock is refused, try again this much later.
    pub acquire_retry_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            hot_for_ms: 120_000,
            ac_debounce_ms: 10_000,
            draining_ms: 60_000,
            suspend_grace_ms: 5_000,
            suspend_retry_ms: 60_000,
            acquire_retry_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Inputs {
    pub enabled: bool,
    pub power: Power,
    pub lid: Lid,
    pub docked: bool,
    pub heat: Heat,
    /// The lock is really held right now (the backend's truth, not our intent).
    pub held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Armed,
    Holding,
    Tripped,
    Suspending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Acquire,
    Release,
    Suspend,
}

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub mode: Mode,
    pub host: HostState,
    pub reason: String,
    /// Set by a thermal trip; cleared only when the lid is seen open (or host mode is turned off).
    pub latched: bool,
    trip_reason: Option<String>,
    last_at: Option<u64>,
    /// The previous sample's conditions: the time since then counts for (or against) them.
    prev_hot: bool,
    prev_battery: bool,
    prev_draining: bool,
    hot_ms: u64,
    battery_ms: u64,
    draining_ms: u64,
    /// Released because of the power; hold again only once it has been steady a while.
    power_hold_off: bool,
    suspend_at: Option<u64>,
    retry_at: Option<u64>,
    last_error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            mode: Mode::Off,
            host: HostState::Off,
            reason: "host mode is off".into(),
            latched: false,
            trip_reason: None,
            last_at: None,
            prev_hot: false,
            prev_battery: false,
            prev_draining: false,
            hot_ms: 0,
            battery_ms: 0,
            draining_ms: 0,
            power_hold_off: false,
            suspend_at: None,
            retry_at: None,
            last_error: None,
        }
    }
}

/// Count `dt` towards a condition that held over it, or pay back at half speed.
fn leak(acc: &mut u64, was_on: bool, dt: u64, cap: u64) {
    if was_on {
        *acc = acc.saturating_add(dt).min(cap);
    } else {
        *acc = acc.saturating_sub(dt / 2);
    }
}

impl State {
    /// One evaluation. Returns what to do, in order.
    pub fn step(&mut self, inp: &Inputs, now: u64, lim: &Limits) -> Vec<Action> {
        if !inp.enabled {
            *self = State::default();
            return if inp.held { vec![Action::Release] } else { vec![] };
        }

        let dt = self.last_at.map(|t| now.saturating_sub(t)).unwrap_or(0);
        self.last_at = Some(now);
        leak(&mut self.hot_ms, self.prev_hot, dt, lim.hot_for_ms);
        leak(&mut self.battery_ms, self.prev_battery, dt, lim.ac_debounce_ms.saturating_mul(2));
        leak(&mut self.draining_ms, self.prev_draining, dt, lim.draining_ms);
        self.prev_hot = matches!(inp.heat, Heat::Hot(_) | Heat::Critical(_));
        self.prev_battery = inp.power == Power::Battery;
        self.prev_draining = inp.power == Power::AcDraining;
        let (hot_for, battery_for, draining_for) = (self.hot_ms, self.battery_ms, self.draining_ms);
        if self.power_hold_off && battery_for == 0 && draining_for == 0 {
            self.power_hold_off = false;
        }

        if inp.lid == Lid::Open {
            self.latched = false;
            self.trip_reason = None;
        }
        match &inp.heat {
            Heat::Critical(why) => self.trip(format!("too hot: {why}")),
            Heat::Hot(why) if hot_for >= lim.hot_for_ms => {
                self.trip(format!("too hot: {why} for about {} s", hot_for / 1000));
            }
            _ => {}
        }

        // Why not to hold, if anything.
        let blocker: Option<(HostState, String)> = if self.latched {
            Some((HostState::TooHot, self.reason_if_latched()))
        } else {
            let unsteady = || (HostState::OnBattery, "mains power keeps dropping out; holding again once it has been steady for a while".to_string());
            match &inp.power {
                Power::Ac | Power::AcDraining if self.power_hold_off => Some(unsteady()),
                Power::Ac => None,
                Power::AcDraining if draining_for < lim.draining_ms => None,
                Power::AcDraining => {
                    Some((HostState::OnBattery, "the charger is too weak: the battery has been draining for a minute".into()))
                }
                // Debounce only while holding: a wiggle does not drop the lock, but a
                // machine on battery never takes it.
                Power::Battery if self.mode == Mode::Holding && inp.held && battery_for < lim.ac_debounce_ms => None,
                Power::Battery => Some((HostState::OnBattery, "on battery".into())),
                Power::Unknown(e) => Some((HostState::WillSleep, format!("cannot tell whether it is on mains power ({e})"))),
            }
            .or(match &inp.heat {
                Heat::Unknown(e) => Some((HostState::WillSleep, format!("cannot read the temperature ({e})"))),
                _ => None,
            })
        };

        match blocker {
            None => {
                self.suspend_at = None;
                if inp.held {
                    self.acquired();
                    return vec![];
                }
                if let Some(t) = self.retry_at
                    && now < t
                {
                    let why = format!(
                        "cannot take the lid lock ({}); trying again in {} s",
                        self.last_error.clone().unwrap_or_default(),
                        (t - now).div_ceil(1000)
                    );
                    self.set(Mode::Armed, HostState::WillSleep, &why);
                    return vec![];
                }
                self.set(Mode::Holding, HostState::Awake, "taking the lid lock");
                vec![Action::Acquire]
            }
            Some((host, why)) => {
                let mut acts = Vec::new();
                if inp.held {
                    acts.push(Action::Release);
                    if matches!(inp.power, Power::Battery | Power::AcDraining) && !self.latched {
                        self.power_hold_off = true;
                    }
                }
                if inp.lid == Lid::Closed && !inp.docked {
                    match self.suspend_at {
                        None => self.suspend_at = Some(now + lim.suspend_grace_ms),
                        Some(t) if now >= t => {
                            acts.push(Action::Suspend);
                            self.suspend_at = Some(now + lim.suspend_retry_ms);
                        }
                        Some(_) => {}
                    }
                    self.set(Mode::Suspending, host, &format!("{why}; lid closed, so this machine is going to sleep"));
                } else {
                    self.suspend_at = None;
                    let mode = if self.latched { Mode::Tripped } else { Mode::Armed };
                    self.set(mode, host, &why);
                }
                acts
            }
        }
    }

    /// The runner calls this when `Acquire` succeeded.
    pub fn acquired(&mut self) {
        self.retry_at = None;
        self.last_error = None;
        self.set(Mode::Holding, HostState::Awake, "holding the lid lock: on mains power and not too hot");
    }

    /// The runner calls this when `Acquire` failed.
    pub fn acquire_failed(&mut self, now: u64, err: &str, lim: &Limits) {
        self.retry_at = Some(now + lim.acquire_retry_ms);
        self.last_error = Some(err.to_string());
        self.set(Mode::Armed, HostState::WillSleep, &format!("cannot take the lid lock: {err}"));
    }

    fn trip(&mut self, why: String) {
        if !self.latched {
            self.latched = true;
            self.trip_reason = Some(why);
        }
    }

    fn reason_if_latched(&self) -> String {
        format!(
            "{}; stays released until the lid is opened",
            self.trip_reason.clone().unwrap_or_else(|| "too hot".into())
        )
    }

    fn set(&mut self, mode: Mode, host: HostState, why: &str) {
        self.mode = mode;
        self.host = host;
        self.reason = why.to_string();
    }
}
