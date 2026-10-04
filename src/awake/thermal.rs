//! Temperatures from `/sys/class/thermal` zones and `/sys/class/hwmon`, with each sensor's
//! critical point where the kernel exposes one. Values are millidegrees Celsius.

use std::path::Path;

use super::sysfs::{Roots, list_dir, read_attr, read_int};

/// Readings outside this range are treated as junk (unconnected inputs, bogus limits such
/// as the 65261 °C "max" some NVMe drives report).
const SANE_C: std::ops::RangeInclusive<f64> = -40.0..=150.0;
const MAX_TEMPS_PER_CHIP: u32 = 32;

#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    /// e.g. "k10temp Tctl", "zone x86_pkg_temp".
    pub name: String,
    pub c: f64,
    pub crit: Option<f64>,
    /// Warning limit, only kept for NVMe drives (whose `max` means "too hot").
    pub max: Option<f64>,
    pub cpu: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Heat {
    Ok,
    /// Above the sustained limit now (the guard times how long).
    Hot(String),
    /// Within the margin of a critical point: trip at once.
    Critical(String),
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatLimits {
    pub hot_c: f64,
    pub crit_margin_c: f64,
}

impl Default for HeatLimits {
    fn default() -> Self {
        HeatLimits { hot_c: 90.0, crit_margin_c: 5.0 }
    }
}

fn is_cpu(chip: &str) -> bool {
    const CPU: &[&str] = &["k10temp", "zenpower", "coretemp", "acpitz", "x86_pkg_temp", "cpu_thermal", "cpu-thermal", "soc_thermal", "tcpu"];
    let c = chip.to_ascii_lowercase();
    CPU.contains(&c.as_str()) || c.starts_with("cpu")
}

fn millis(path: &Path) -> Option<f64> {
    let v = read_int(path).ok().flatten()? as f64 / 1000.0;
    SANE_C.contains(&v).then_some(v)
}

pub fn read(roots: &Roots) -> Vec<Sensor> {
    let mut out = Vec::new();
    zones(roots, &mut out);
    hwmon(roots, &mut out);
    out
}

fn zones(roots: &Roots, out: &mut Vec<Sensor>) {
    let Ok(Some(entries)) = list_dir(&roots.sys.join("class/thermal")) else { return };
    for (name, path) in entries {
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let Some(c) = millis(&path.join("temp")) else { continue };
        let kind = read_attr(&path.join("type")).ok().flatten().unwrap_or_else(|| name.clone());
        let mut crit = None;
        for i in 0..16 {
            match read_attr(&path.join(format!("trip_point_{i}_type"))) {
                Ok(Some(t)) if t == "critical" => {
                    crit = millis(&path.join(format!("trip_point_{i}_temp")));
                    break;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        out.push(Sensor { cpu: is_cpu(&kind), name: format!("zone {kind}"), c, crit, max: None });
    }
}

fn hwmon(roots: &Roots, out: &mut Vec<Sensor>) {
    let Ok(Some(entries)) = list_dir(&roots.sys.join("class/hwmon")) else { return };
    for (_, path) in entries {
        let Ok(Some(chip)) = read_attr(&path.join("name")) else { continue };
        for i in 1..=MAX_TEMPS_PER_CHIP {
            // A sensor whose input cannot be read right now (a powered-down GPU, EIO) is
            // skipped; if no sensor at all can be read the guard does not hold.
            let Some(c) = millis(&path.join(format!("temp{i}_input"))) else { continue };
            let label = read_attr(&path.join(format!("temp{i}_label"))).ok().flatten().unwrap_or_else(|| format!("temp{i}"));
            let crit = millis(&path.join(format!("temp{i}_crit"))).filter(|c| *c >= 40.0);
            let max = if chip == "nvme" { millis(&path.join(format!("temp{i}_max"))).filter(|m| *m >= 40.0) } else { None };
            out.push(Sensor { cpu: is_cpu(&chip), name: format!("{chip} {label}"), c, crit, max });
        }
    }
}

pub fn hottest(sensors: &[Sensor]) -> Option<&Sensor> {
    sensors.iter().max_by(|a, b| a.c.total_cmp(&b.c))
}

/// Classify the readings. The sustained limit applies to CPU sensors when there are any
/// (other chips, GPUs especially, run hotter by design), else to every sensor, and to an
/// NVMe drive past its own warning limit. The critical margin applies to every sensor.
pub fn assess(sensors: &[Sensor], lim: HeatLimits) -> Heat {
    if sensors.is_empty() {
        return Heat::Unknown("no temperature sensor could be read".into());
    }
    for s in sensors {
        if let Some(crit) = s.crit
            && s.c >= crit - lim.crit_margin_c
        {
            return Heat::Critical(format!("{} at {:.0} °C, critical point {:.0} °C", s.name, s.c, crit));
        }
    }
    let any_cpu = sensors.iter().any(|s| s.cpu);
    for s in sensors {
        if (s.cpu || !any_cpu) && s.c >= lim.hot_c {
            return Heat::Hot(format!("{} at {:.0} °C", s.name, s.c));
        }
        if let Some(max) = s.max
            && s.c >= max
        {
            return Heat::Hot(format!("{} at {:.0} °C, above its limit {:.0} °C", s.name, s.c, max));
        }
    }
    Heat::Ok
}
