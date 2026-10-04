//! Host mode over fake sysfs/procfs trees and a fake logind/inhibitor.

use std::fs;
use std::path::PathBuf;

use super::backend::fake::Fake;
use super::guard::Guard;
use super::lid::{self, Lid, Logind};
use super::machine::{Limits, Mode, State};
use super::power::{self, Power};
use super::sysfs::Roots;
use super::thermal::{self, Heat, HeatLimits};
use crate::proto::HostState;

struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(tag: &str) -> Tree {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tmp"));
        let root = base.join(format!("awake-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sys/class/power_supply")).unwrap();
        fs::create_dir_all(root.join("sys/class/hwmon")).unwrap();
        fs::create_dir_all(root.join("proc")).unwrap();
        Tree { root }
    }
    fn roots(&self) -> Roots {
        Roots { sys: self.root.join("sys"), proc: self.root.join("proc") }
    }
    fn file(&self, rel: &str, val: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, format!("{val}\n")).unwrap();
    }
    fn rm(&self, rel: &str) {
        let p = self.root.join(rel);
        let _ = fs::remove_file(&p);
        let _ = fs::remove_dir_all(&p);
    }
    /// A path that exists but cannot be read as a file (a directory): read fails with EISDIR.
    fn unreadable(&self, rel: &str) {
        self.rm(rel);
        fs::create_dir_all(self.root.join(rel)).unwrap();
    }
    fn ac(&self, online: u8) {
        self.file("sys/class/power_supply/AC/type", "Mains");
        self.file("sys/class/power_supply/AC/online", &online.to_string());
    }
    fn bat(&self, status: &str) {
        self.file("sys/class/power_supply/BAT0/type", "Battery");
        self.file("sys/class/power_supply/BAT0/present", "1");
        self.file("sys/class/power_supply/BAT0/status", status);
        self.file("sys/class/power_supply/BAT0/capacity", "80");
    }
    fn mouse(&self) {
        let d = "sys/class/power_supply/hidpp_battery_0";
        self.file(&format!("{d}/type"), "Battery");
        self.file(&format!("{d}/scope"), "Device");
        self.file(&format!("{d}/status"), "Discharging");
        self.file(&format!("{d}/online"), "1");
    }
    fn cpu(&self, c: f64) {
        self.file("sys/class/hwmon/hwmon0/name", "k10temp");
        self.file("sys/class/hwmon/hwmon0/temp1_label", "Tctl");
        self.file("sys/class/hwmon/hwmon0/temp1_input", &((c * 1000.0) as i64).to_string());
    }
    fn lid(&self, state: &str) {
        self.file("proc/acpi/button/lid/LID0/state", &format!("state:      {state}"));
    }
    /// A laptop on mains, cool, lid open.
    fn laptop(tag: &str) -> Tree {
        let t = Tree::new(tag);
        t.ac(1);
        t.bat("Charging");
        t.mouse();
        t.cpu(50.0);
        t.lid("open");
        t
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn guard(t: &Tree) -> Guard<Fake> {
    Guard {
        backend: Fake::default(),
        roots: t.roots(),
        use_logind: true,
        heat: HeatLimits::default(),
        limits: Limits::default(),
        enabled: true,
        state: State::default(),
        mac: false,
    }
}

const S: u64 = 1000;

// ------------------------------------------------------------------ power

#[test]
fn power_mains_and_battery() {
    let t = Tree::laptop("pw1");
    assert_eq!(power::read(&t.roots()).power, Power::Ac);
    t.ac(0);
    t.bat("Discharging");
    let r = power::read(&t.roots());
    assert_eq!(r.power, Power::Battery);
    assert_eq!(r.battery_pct, Some(80));
    t.bat("Full");
    assert_eq!(power::read(&t.roots()).power, Power::Ac, "no supply online and nothing discharging: mains, as systemd");
    t.ac(1);
    t.bat("Discharging");
    assert_eq!(power::read(&t.roots()).power, Power::AcDraining);
}

#[test]
fn power_ignores_peripheral_batteries_and_absent_ones() {
    let t = Tree::new("pw2");
    t.mouse();
    assert_eq!(power::read(&t.roots()).power, Power::Ac, "a discharging mouse must not mean battery");
    t.bat("Discharging");
    t.file("sys/class/power_supply/BAT0/present", "0");
    assert_eq!(power::read(&t.roots()).power, Power::Ac);
}

#[test]
fn power_no_supplies_is_mains_missing_class_is_unknown() {
    let t = Tree::new("pw3");
    assert_eq!(power::read(&t.roots()).power, Power::Ac);
    t.rm("sys/class/power_supply");
    assert!(matches!(power::read(&t.roots()).power, Power::Unknown(_)));
}

#[test]
fn power_unreadable_attribute_is_unknown() {
    let t = Tree::laptop("pw4");
    t.unreadable("sys/class/power_supply/AC/online");
    assert!(matches!(power::read(&t.roots()).power, Power::Unknown(_)));
    let t = Tree::laptop("pw5");
    t.unreadable("sys/class/power_supply/BAT0/status");
    assert!(matches!(power::read(&t.roots()).power, Power::Unknown(_)));
}

#[cfg(unix)]
#[test]
fn power_usb_c_online_2_sink_and_source() {
    let t = Tree::new("pw6");
    t.bat("Discharging");
    // Real sysfs layout: class entries are symlinks into /sys/devices; the typec port and the
    // power supply share the UCSI device as parent.
    let dev = "sys/devices/ucsi";
    t.file(&format!("{dev}/power_supply/ucsi-source-psy-1/type"), "USB");
    t.file(&format!("{dev}/power_supply/ucsi-source-psy-1/online"), "2");
    t.file(&format!("{dev}/typec/port0/power_role"), "source [sink]");
    std::os::unix::fs::symlink(
        t.root.join(format!("{dev}/power_supply/ucsi-source-psy-1")),
        t.root.join("sys/class/power_supply/ucsi-source-psy-1"),
    )
    .unwrap();
    fs::create_dir_all(t.root.join("sys/class/typec")).unwrap();
    std::os::unix::fs::symlink(t.root.join(format!("{dev}/typec/port0")), t.root.join("sys/class/typec/port0")).unwrap();
    assert_eq!(power::read(&t.roots()).power, Power::AcDraining, "online=2 on a sink port counts as mains");
    t.file(&format!("{dev}/typec/port0/power_role"), "[source] sink");
    assert_eq!(power::read(&t.roots()).power, Power::Battery, "a port giving power away is not mains");
}

// ------------------------------------------------------------------ thermal and lid

#[test]
fn thermal_zones_hwmon_and_none() {
    let t = Tree::new("th1");
    assert!(matches!(thermal::assess(&thermal::read(&t.roots()), HeatLimits::default()), Heat::Unknown(_)));
    t.file("sys/class/thermal/thermal_zone0/type", "x86_pkg_temp");
    t.file("sys/class/thermal/thermal_zone0/temp", "60000");
    t.file("sys/class/thermal/thermal_zone0/trip_point_0_type", "passive");
    t.file("sys/class/thermal/thermal_zone0/trip_point_0_temp", "95000");
    t.file("sys/class/thermal/thermal_zone0/trip_point_1_type", "critical");
    t.file("sys/class/thermal/thermal_zone0/trip_point_1_temp", "105000");
    let s = thermal::read(&t.roots());
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].crit, Some(105.0));
    assert!(s[0].cpu);
    assert_eq!(thermal::assess(&s, HeatLimits::default()), Heat::Ok);
    t.file("sys/class/thermal/thermal_zone0/temp", "91000");
    assert!(matches!(thermal::assess(&thermal::read(&t.roots()), HeatLimits::default()), Heat::Hot(_)));
    t.file("sys/class/thermal/thermal_zone0/temp", "100500");
    assert!(matches!(thermal::assess(&thermal::read(&t.roots()), HeatLimits::default()), Heat::Critical(_)));
}

#[test]
fn thermal_gpu_hot_is_not_cpu_hot_and_junk_is_ignored() {
    let t = Tree::new("th2");
    t.cpu(60.0);
    t.file("sys/class/hwmon/hwmon1/name", "amdgpu");
    t.file("sys/class/hwmon/hwmon1/temp1_input", "95000");
    t.file("sys/class/hwmon/hwmon2/name", "nvme");
    t.file("sys/class/hwmon/hwmon2/temp1_input", "42000");
    t.file("sys/class/hwmon/hwmon2/temp1_crit", "84850");
    t.file("sys/class/hwmon/hwmon2/temp1_max", "81850");
    t.file("sys/class/hwmon/hwmon2/temp2_input", "41000");
    t.file("sys/class/hwmon/hwmon2/temp2_max", "65261850");
    t.unreadable("sys/class/hwmon/hwmon2/temp3_input");
    let s = thermal::read(&t.roots());
    assert_eq!(s.len(), 4);
    assert_eq!(thermal::assess(&s, HeatLimits::default()), Heat::Ok);
    t.file("sys/class/hwmon/hwmon2/temp1_input", "82000");
    assert!(matches!(thermal::assess(&thermal::read(&t.roots()), HeatLimits::default()), Heat::Critical(_)));
    t.file("sys/class/hwmon/hwmon2/temp1_crit", "99000");
    assert!(matches!(thermal::assess(&thermal::read(&t.roots()), HeatLimits::default()), Heat::Hot(_)), "NVMe past its max");
}

#[test]
fn lid_from_logind_then_procfs() {
    let t = Tree::new("lid");
    assert_eq!(lid::decide(&Logind::default(), &t.roots()), Lid::Unknown);
    t.lid("closed");
    assert_eq!(lid::decide(&Logind::default(), &t.roots()), Lid::Closed);
    let l = lid::parse_busctl_bools("b false\nb true\nb true\n");
    assert_eq!(l, Logind { lid_closed: Some(false), docked: Some(true), on_external_power: Some(true) });
    assert_eq!(lid::decide(&l, &t.roots()), Lid::Open, "logind wins over procfs");
    assert_eq!(lid::parse_busctl_bools("garbage"), Logind::default());
}

// ------------------------------------------------------------------ transitions

#[test]
fn holds_on_mains_and_cool() {
    let t = Tree::laptop("st1");
    let mut g = guard(&t);
    let r = g.tick(0);
    assert_eq!(g.backend.acquires, 1);
    assert!(r.lock_held);
    assert_eq!(r.state, HostState::Awake);
    assert_eq!(r.on_lid_close, "stays awake and reachable");
    g.tick(5 * S);
    assert_eq!(g.backend.acquires, 1, "no re-acquire while held");
}

#[test]
fn ac_lost_with_lid_closed_releases_then_suspends() {
    let t = Tree::laptop("st2");
    let mut g = guard(&t);
    g.tick(0);
    t.lid("closed");
    g.tick(5 * S);
    assert!(g.backend.holding);
    t.ac(0);
    t.bat("Discharging");
    g.tick(10 * S);
    assert!(g.backend.holding, "cable wiggle: not released before 10 s");
    t.ac(1);
    t.bat("Charging");
    g.tick(15 * S);
    assert!(g.backend.holding);
    t.ac(0);
    t.bat("Discharging");
    g.tick(20 * S);
    g.tick(25 * S);
    assert!(g.backend.holding);
    let r = g.tick(30 * S);
    assert!(!g.backend.holding, "released after 10 s on battery");
    assert_eq!(r.state, HostState::OnBattery);
    assert_eq!(g.state.mode, Mode::Suspending);
    assert_eq!(g.backend.suspends, 0, "grace for logind first");
    g.tick(35 * S);
    assert_eq!(g.backend.suspends, 1, "then suspend ourselves");
    g.tick(40 * S);
    assert_eq!(g.backend.suspends, 1, "not again at once");
    g.tick(95 * S);
    assert_eq!(g.backend.suspends, 2, "retry if still awake a minute later");
}

#[test]
fn ac_lost_with_lid_open_releases_without_suspend() {
    let t = Tree::laptop("st3");
    let mut g = guard(&t);
    g.tick(0);
    t.ac(0);
    t.bat("Discharging");
    for i in 1..20 {
        g.tick(i * 5 * S);
    }
    assert!(!g.backend.holding);
    assert_eq!(g.backend.suspends, 0);
    assert_eq!(g.state.mode, Mode::Armed);
}

#[test]
fn never_takes_the_lock_on_battery() {
    let t = Tree::laptop("st4");
    t.ac(0);
    t.bat("Discharging");
    let mut g = guard(&t);
    g.tick(0);
    g.tick(5 * S);
    assert_eq!(g.backend.acquires, 0);
}

#[test]
fn hot_for_120s_releases_suspends_and_waits_for_lid_open() {
    let t = Tree::laptop("st5");
    let mut g = guard(&t);
    g.tick(0);
    t.lid("closed");
    t.cpu(93.0);
    let mut now = 5 * S;
    while now < 120 * S {
        g.tick(now);
        assert!(g.backend.holding, "not yet 120 s at {now}");
        now += 5 * S;
    }
    g.tick(125 * S);
    assert!(!g.backend.holding);
    assert!(g.state.latched);
    assert_eq!(g.tick(130 * S).state, HostState::TooHot);
    assert_eq!(g.backend.suspends, 1);
    // Cooled down and still on mains, lid still closed: stays released.
    t.cpu(45.0);
    for i in 0..30 {
        g.tick(135 * S + i * 5 * S);
    }
    assert!(!g.backend.holding);
    assert_eq!(g.backend.acquires, 1);
    // Lid opened: re-armed and holding again.
    t.lid("open");
    let r = g.tick(400 * S);
    assert!(r.lock_held);
    assert!(!g.state.latched);
}

#[test]
fn short_heat_spike_does_not_trip() {
    let t = Tree::laptop("st6");
    let mut g = guard(&t);
    g.tick(0);
    t.cpu(95.0);
    for i in 1..20 {
        g.tick(i * 5 * S);
    }
    t.cpu(60.0);
    g.tick(100 * S);
    t.cpu(95.0);
    g.tick(105 * S);
    g.tick(200 * S);
    assert!(g.backend.holding, "the 120 s window restarts after cooling");
    assert!(!g.state.latched);
}

#[test]
fn near_critical_trips_at_once() {
    let t = Tree::laptop("st7");
    let mut g = guard(&t);
    g.tick(0);
    t.file("sys/class/hwmon/hwmon0/temp1_crit", "95000");
    t.cpu(91.0);
    g.tick(5 * S);
    assert!(!g.backend.holding);
    assert!(g.state.latched);
}

#[test]
fn unreadable_power_fails_towards_sleep() {
    let t = Tree::laptop("st8");
    let mut g = guard(&t);
    g.tick(0);
    t.lid("closed");
    t.unreadable("sys/class/power_supply/AC/online");
    let r = g.tick(5 * S);
    assert!(!g.backend.holding, "unknown power must release at once, no debounce");
    assert!(r.reason.contains("cannot tell"));
    g.tick(10 * S);
    assert_eq!(g.backend.suspends, 1);
}

#[test]
fn missing_sensors_fail_towards_sleep() {
    let t = Tree::laptop("st9");
    let mut g = guard(&t);
    g.tick(0);
    t.rm("sys/class/hwmon/hwmon0");
    let r = g.tick(5 * S);
    assert!(!g.backend.holding);
    assert!(r.reason.contains("temperature"));
    t.rm("sys/class/power_supply");
    t.cpu(40.0);
    g.tick(10 * S);
    assert!(!g.backend.holding);
    assert_eq!(g.backend.acquires, 1);
}

#[test]
fn logind_disagreeing_on_power_fails_towards_sleep() {
    let t = Tree::laptop("st10");
    let mut g = guard(&t);
    g.backend.logind = Logind { lid_closed: Some(false), docked: Some(false), on_external_power: Some(false) };
    g.tick(0);
    assert_eq!(g.backend.acquires, 0);
}

#[test]
fn refused_lock_retries_after_a_minute_and_says_why() {
    let t = Tree::laptop("st11");
    let mut g = guard(&t);
    g.backend.refuse = Some("Access denied".into());
    let r = g.tick(0);
    assert!(!r.lock_held);
    assert_eq!(r.state, HostState::WillSleep);
    assert!(r.reason.contains("Access denied"));
    g.tick(30 * S);
    assert_eq!(g.backend.acquires, 1);
    g.backend.refuse = None;
    let r = g.tick(61 * S);
    assert_eq!(g.backend.acquires, 2);
    assert!(r.lock_held);
}

#[test]
fn holder_crash_is_noticed_and_retaken() {
    let t = Tree::laptop("st12");
    let mut g = guard(&t);
    g.tick(0);
    g.backend.crash = true;
    let r = g.tick(5 * S);
    assert!(!r.lock_held);
    g.backend.crash = false;
    g.tick(10 * S);
    assert!(g.backend.holding);
}

#[test]
fn turning_off_releases_without_suspending() {
    let t = Tree::laptop("st13");
    let mut g = guard(&t);
    g.tick(0);
    g.enabled = false;
    let r = g.tick(5 * S);
    assert!(!g.backend.holding);
    assert_eq!(r.state, HostState::Off);
    assert_eq!(g.backend.suspends, 0);
}

#[test]
fn docked_lid_closed_on_battery_releases_without_suspend() {
    let t = Tree::laptop("st14");
    let mut g = guard(&t);
    g.backend.logind = Logind { lid_closed: Some(true), docked: Some(true), on_external_power: None };
    g.tick(0);
    t.ac(0);
    t.bat("Discharging");
    for i in 1..10 {
        g.tick(i * 5 * S);
    }
    assert!(!g.backend.holding);
    assert_eq!(g.backend.suspends, 0);
}

#[test]
fn weak_charger_counts_as_battery_after_a_minute() {
    let t = Tree::laptop("st15");
    let mut g = guard(&t);
    g.tick(0);
    t.bat("Discharging");
    g.tick(5 * S);
    g.tick(60 * S);
    assert!(g.backend.holding);
    let r = g.tick(66 * S);
    assert!(!g.backend.holding);
    assert_eq!(r.state, HostState::OnBattery);
}

#[test]
fn lid_unknown_never_suspends() {
    let t = Tree::laptop("st16");
    t.rm("proc/acpi");
    let mut g = guard(&t);
    g.tick(0);
    t.ac(0);
    t.bat("Discharging");
    for i in 1..30 {
        g.tick(i * 5 * S);
    }
    assert!(!g.backend.holding);
    assert_eq!(g.backend.suspends, 0, "only suspend when the lid is known to be closed");
}

#[test]
fn state_machine_pure_off() {
    let mut s = State::default();
    let inp = super::machine::Inputs {
        enabled: false,
        power: Power::Ac,
        lid: Lid::Closed,
        docked: false,
        heat: Heat::Ok,
        held: true,
    };
    assert_eq!(s.step(&inp, 0, &Limits::default()), vec![super::machine::Action::Release]);
    assert_eq!(s.mode, Mode::Off);
}

