//! The host-mode state machine: a pure function of (inputs, previous state, time).
//!
//! Modes: `Off` (host mode off) · `Armed` (on, not holding: conditions not met or the lock
//! was refused) · `Holding` (lid lock held) · `Tripped` (released after overheating; stays
//! released until the lid is seen open) · `Suspending` (released with the lid closed; asks
//! for suspend after a short grace in case logind does not do it by itself).
//!
//! Everything unknown counts against holding: unknown power, no readable temperature.

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
    hot_since: Option<u64>,
    battery_since: Option<u64>,
    draining_since: Option<u64>,
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
            hot_since: None,
            battery_since: None,
            draining_since: None,
            suspend_at: None,
            retry_at: None,
            last_error: None,
        }
    }
}

fn since(slot: &mut Option<u64>, on: bool, now: u64) -> u64 {
    if on {
        now.saturating_sub(*slot.get_or_insert(now))
    } else {
        *slot = None;
        0
    }
}

impl State {
    /// One evaluation. Returns what to do, in order.
    pub fn step(&mut self, inp: &Inputs, now: u64, lim: &Limits) -> Vec<Action> {
        if !inp.enabled {
            *self = State::default();
            return if inp.held { vec![Action::Release] } else { vec![] };
        }

        let hot_for = since(&mut self.hot_since, matches!(inp.heat, Heat::Hot(_)), now);
        let battery_for = since(&mut self.battery_since, inp.power == Power::Battery, now);
        let draining_for = since(&mut self.draining_since, inp.power == Power::AcDraining, now);

        if inp.lid == Lid::Open {
            self.latched = false;
            self.trip_reason = None;
        }
        match &inp.heat {
            Heat::Critical(why) => self.trip(format!("too hot: {why}")),
            Heat::Hot(why) if hot_for >= lim.hot_for_ms => {
                self.trip(format!("too hot: {why} for {} s", hot_for / 1000));
            }
            _ => {}
        }

        // Why not to hold, if anything.
        let blocker: Option<(HostState, String)> = if self.latched {
            Some((HostState::TooHot, self.reason_if_latched()))
        } else {
            match &inp.power {
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
