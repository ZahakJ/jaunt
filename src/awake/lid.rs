//! Lid state: logind's `LidClosed` first, `/proc/acpi/button/lid/*/state` as fallback.

use super::sysfs::{Roots, list_dir, read_attr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lid {
    Open,
    Closed,
    Unknown,
}

impl Lid {
    pub fn words(self) -> &'static str {
        match self {
            Lid::Open => "open",
            Lid::Closed => "closed",
            Lid::Unknown => "unknown",
        }
    }
}

/// What logind says (`None` = could not ask).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Logind {
    pub lid_closed: Option<bool>,
    /// True with a dock or more than one display: logind then uses `HandleLidSwitchDocked`.
    pub docked: Option<bool>,
    pub on_external_power: Option<bool>,
}

/// Parse `busctl get-property ... LidClosed Docked OnExternalPower`: one `b true|false` per line.
pub fn parse_busctl_bools(out: &str) -> Logind {
    let mut vals = out.lines().map(|l| match l.trim() {
        "b true" => Some(true),
        "b false" => Some(false),
        _ => None,
    });
    Logind { lid_closed: vals.next().flatten(), docked: vals.next().flatten(), on_external_power: vals.next().flatten() }
}

/// ACPI button driver: `state:      open|closed`. Closed if any lid says closed.
pub fn read_procfs(roots: &Roots) -> Lid {
    let Ok(Some(lids)) = list_dir(&roots.proc.join("acpi/button/lid")) else { return Lid::Unknown };
    let mut seen = Lid::Unknown;
    for (_, path) in lids {
        let Ok(Some(s)) = read_attr(&path.join("state")) else { continue };
        let v = s.strip_prefix("state:").map(str::trim).unwrap_or("");
        match v {
            "closed" => return Lid::Closed,
            "open" => seen = Lid::Open,
            _ => {}
        }
    }
    seen
}

pub fn decide(logind: &Logind, roots: &Roots) -> Lid {
    match logind.lid_closed {
        Some(true) => Lid::Closed,
        Some(false) => Lid::Open,
        None => read_procfs(roots),
    }
}
