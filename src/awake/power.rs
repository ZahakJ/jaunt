//! Mains or battery, decided the way systemd's `on_ac_power()` does (v261,
//! `src/shared/battery-util.c`), with one change: where systemd ignores a read error, we
//! report the power as unknown, and the guard treats unknown as "do not hold".
//!
//! systemd's rules: batteries of peripherals (`scope=Device`, e.g. a wireless mouse) and
//! absent batteries are ignored; a USB supply counts only if its USB-C port is a sink (or
//! no port says it is a source); any other supply with `online` > 0 (1, or 2 for USB-PD)
//! means mains; otherwise any discharging battery means battery; otherwise mains.

use std::path::Path;

use super::sysfs::{Roots, list_dir, read_attr, read_int};

#[derive(Debug, Clone, PartialEq)]
pub enum Power {
    Ac,
    /// A supply is online but a system battery is still discharging (a charger too weak
    /// for the load). The guard treats this as battery after a minute.
    AcDraining,
    Battery,
    Unknown(String),
}

impl Power {
    pub fn words(&self) -> &'static str {
        match self {
            Power::Ac => "mains",
            Power::AcDraining => "mains, battery draining",
            Power::Battery => "battery",
            Power::Unknown(_) => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PowerReading {
    pub power: Power,
    /// Charge of the first system battery, 0-100.
    pub battery_pct: Option<u8>,
}

pub fn read(roots: &Roots) -> PowerReading {
    let dir = roots.sys.join("class/power_supply");
    let entries = match list_dir(&dir) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return PowerReading { power: Power::Unknown(format!("{} does not exist", dir.display())), battery_pct: None };
        }
        Err(e) => return PowerReading { power: Power::Unknown(e), battery_pct: None },
    };
    let mut ac_online = false;
    let mut discharging = false;
    let mut battery_pct = None;
    let mut error: Option<String> = None;
    let mut note = |e: String| {
        if error.is_none() {
            error = Some(e);
        }
    };
    for (_, path) in entries {
        let kind = match read_attr(&path.join("type")) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                note(e);
                continue;
            }
        };
        if kind == "Battery" {
            match battery_discharging(&path) {
                Ok(None) => {}
                Ok(Some((d, pct))) => {
                    discharging |= d;
                    if battery_pct.is_none() {
                        battery_pct = pct;
                    }
                }
                Err(e) => note(e),
            }
            continue;
        }
        if kind == "USB" {
            match usb_is_sink(roots, &path) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    note(e);
                    continue;
                }
            }
        }
        match read_int(&path.join("online")) {
            Ok(Some(v)) if v > 0 => ac_online = true,
            Ok(_) => {}
            Err(e) => note(e),
        }
    }
    let power = match (error, ac_online, discharging) {
        (Some(e), _, _) => Power::Unknown(e),
        (None, true, true) => Power::AcDraining,
        (None, true, false) => Power::Ac,
        (None, false, true) => Power::Battery,
        // No supply online and no battery discharging: systemd assumes mains (desktops).
        (None, false, false) => Power::Ac,
    };
    PowerReading { power, battery_pct }
}

/// `None` for a battery that does not count (a peripheral's, or not present).
fn battery_discharging(path: &Path) -> Result<Option<(bool, Option<u8>)>, String> {
    if let Ok(Some(scope)) = read_attr(&path.join("scope"))
        && scope == "Device"
    {
        return Ok(None);
    }
    if read_int(&path.join("present"))? == Some(0) {
        return Ok(None);
    }
    let status = read_attr(&path.join("status"))?.ok_or_else(|| format!("{} has no status", path.display()))?;
    let pct = read_int(&path.join("capacity")).ok().flatten().and_then(|v| u8::try_from(v.clamp(0, 100)).ok());
    Ok(Some((status == "Discharging", pct)))
}

/// systemd's `device_is_power_sink`: look at the USB-C ports (class `typec`) under the
/// supply's parent device; a sink if any is `[sink]`, or if none is `[source]`.
fn usb_is_sink(roots: &Roots, supply: &Path) -> Result<bool, String> {
    let real = std::fs::canonicalize(supply).map_err(|e| format!("cannot resolve {}: {e}", supply.display()))?;
    let mut parent = real.parent().map(Path::to_path_buf).unwrap_or_default();
    if parent.file_name().is_some_and(|n| n == "power_supply") {
        parent = parent.parent().map(Path::to_path_buf).unwrap_or_default();
    }
    let ports = list_dir(&roots.sys.join("class/typec"))?.unwrap_or_default();
    let (mut source, mut sink) = (false, false);
    for (_, port) in ports {
        let Ok(port_real) = std::fs::canonicalize(&port) else { continue };
        if !port_real.starts_with(&parent) || port_real == parent {
            continue;
        }
        let Ok(Some(role)) = read_attr(&port.join("power_role")) else { continue };
        if role.contains("[source]") {
            source = true;
        } else if role.contains("[sink]") {
            sink = true;
        }
    }
    Ok(sink || !source)
}
