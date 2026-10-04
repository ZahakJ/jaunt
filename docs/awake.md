# Host mode: a laptop that stays reachable with the lid closed

`jaunt host on` asks this machine's jaunt daemon to keep it awake and reachable when you
close the lid, **only while it is on mains power and not too hot**. `jaunt host off`
undoes it. The setting is saved in `config.toml` (`[host] enabled = true`).

```
jaunt host on        # start holding the lid lock (when it is safe)
jaunt host status    # what is true now, what closing the lid would do, what is missing
jaunt host off       # release; the lid works as before
jaunt ls             # every machine's host state: awake / will sleep on lid close / on battery / too hot
```

## What it does

The daemon takes a systemd-logind inhibitor lock, `handle-lid-switch:sleep` in `block`
mode. With it held, logind does nothing when the lid closes, and `systemctl suspend`
(from an idle daemon, a key binding, or someone in a remote shell) is refused. It never
takes an `idle` lock, so the screen still locks and blanks as usual.

The guard checks every 5 seconds:

| Condition | What happens |
| --- | --- |
| on mains, every temperature below the limit | the lock is held |
| on battery for 10 s (a shorter cable wiggle is ignored) | the lock is released |
| on mains but the battery has drained for 60 s (a weak USB-C charger) | treated as battery |
| a CPU sensor at ≥ 90 °C for 120 s, or any sensor within 5 °C of its critical point | the lock is released, and it **stays released until the lid is opened** |
| power or temperature cannot be read | the lock is not held: unknown means "sleep" |
| released while the lid is closed (and no dock or second display) | logind normally suspends at once; if the machine is still awake 5 s later, jaunt runs `systemctl suspend` itself, and again a minute later if needed |
| the daemon crashes, is killed, or hangs | the lock is gone (see below), so logind suspends a closed laptop |

How the lock dies with the daemon: the lock is held by a `systemd-inhibit` child that
waits on a pipe from the daemon. If the daemon exits in any way, the pipe closes and the
child exits within milliseconds, so logind drops the lock. If the guard hangs, the daemon
stops sending systemd's watchdog keep-alive (`WatchdogSec=30` in the unit). systemd then
kills it, and the lock goes with it.

Mains power is detected the way systemd does it. Batteries in mice and keyboards are
ignored, and USB-C ports that are giving power away do not count as mains. Temperatures
come from `/sys/class/thermal` and `/sys/class/hwmon`. The limits can be changed in
`config.toml`:

```toml
[host]
enabled = true
hot_c = 90          # sustained limit, °C (50-110)
hot_secs = 120      # how long it must last (10-600)
crit_margin_c = 5   # trip at once this close to a sensor's critical point (0-30)
```

## What it costs

- **Battery**: none. The lock is never held on battery.
- **Power on mains**: a laptop awake with the lid closed draws roughly 3-8 W while idle
  (an estimate, not measured). Turn the internal display off on lid close in your
  compositor, for example Hyprland's `bindl = , switch:on:Lid Switch, exec, hyprctl
  keyword monitor "eDP-1, disable"`. jaunt does not manage displays.
- **Heat**: a closed lid can make a laptop a few degrees hotter under load (reports,
  not measured here). Do not leave a hosting laptop in a bag or on a bed. The heat limit
  is a backstop, not a cooling plan.

## What it needs

- systemd-logind, and **a local login since boot**. logind gives the lid lock to the
  owner's background service only while that user has a local graphical session. After
  a reboot with nobody logged in, the lock is refused. `jaunt host status` and
  `jaunt check` say so, and the guard tries again every minute. You can close this gap
  once, as root, with `scripts/root-extras/host-polkit-rule.sh`.
- The daemon must run as the user service (`jaunt up`). A daemon started from an SSH
  login belongs to that remote session, and logind refuses it.

### Optional root extras (`scripts/root-extras/`)

Each script prints exactly what it will change and how to undo it, and asks you to type
`yes`. Each has an `undo` mode. jaunt never runs them.

| Script | Changes | Undo |
| --- | --- | --- |
| `host-polkit-rule.sh install <user>` | writes `/etc/polkit-1/rules.d/49-jaunt-host.rules`: lets that user take the lid and sleep locks without a local login | `host-polkit-rule.sh undo` |
| `logind-lid-on-ac.sh install` | writes a logind drop-in, `HandleLidSwitchExternalPower=ignore`, and reloads logind. **No heat protection**; the polkit rule is better | `logind-lid-on-ac.sh undo` |
| `wake-on-lan.sh install <connection>` | sets NetworkManager wake-on-LAN/WLAN to `magic` for that connection | `wake-on-lan.sh undo <connection>` |

Waking a sleeping laptop is not part of jaunt. A magic packet works only from another
machine on the same LAN, and WoL must also be enabled in the firmware setup.

## What has been verified

On the always-on desktop where this was built (no lid, no battery, no thermal zones;
logind counts it as docked because of two displays):

- **Unit tests** run over fake sysfs/procfs trees and a fake logind for every transition:
  - holding on mains
  - a cable wiggle
  - mains lost with the lid closed: release, then suspend after the grace, then a retry
  - mains lost with the lid open: release only
  - never taking the lock on battery
  - 120 s hot: release, suspend, and stay released after cooling until the lid opens
  - a short spike that does not trip
  - near-critical: trips at once
  - unreadable power and missing sensors: no hold, and suspend if the lid is closed
  - logind disagreeing about power
  - the lock refused: retry after a minute, with the reason shown
  - the holder dying: taken again
  - docked: no suspend
  - weak charger
  - unknown lid: never suspend
  - power-supply cases: the Logitech mouse battery, a USB-C supply with `online=2` as sink and as source, an absent battery, no supplies at all
  - thermal cases: junk NVMe limits, a hot GPU that does not count as CPU heat
- **The real lock on this desktop**, through the daemon in a `JAUNT_HOME` sandbox:
  - `jaunt host on` took it; `systemd-inhibit --list` showed `jaunt … sleep:handle-lid-switch … block`.
  - `kill -9` of the daemon removed it within 1 s, and no holder process was left.
  - `jaunt host off` and SIGTERM both released it.

**Not verified** (no laptop was available):

- that logind really suspends a closed laptop when the lock is released
- that `systemctl suspend` from the guard works
- real heat behaviour with the lid closed
- the systemd watchdog actually killing a hung daemon
- the no-local-login refusal
- every root-extra script (they pass shellcheck and have never been run)

## First test on a real laptop

Do this with the laptop on a desk, in sight.

1. `jaunt up`, then log in locally. Run `jaunt host on` and `jaunt host status`: it should say
   `awake`, and `systemd-inhibit --list` should show `jaunt`.
2. Close the lid on mains. From another machine, run `jaunt ls` (expect `awake`), then `jaunt <laptop>`.
3. Pull the charger with the lid closed. It should be asleep within about 15 s.
4. Plug in and open the lid. Then close it and run `stress-ng --cpu 0 --timeout 300s`
   from another machine. It should release and sleep within 2 to 3 minutes of reaching
   90 °C. Open the lid, and `host status` should show `awake` again.
5. With the lid closed on mains, `kill -9` the daemon. It should sleep at once.
6. Reboot and do not log in. `jaunt host status` from another machine should say the lock
   is refused. Then decide whether you want the polkit rule.
7. Optionally, log `jaunt host status` every minute for an hour with the lid closed, to
   see the real temperature curve.

## macOS

Unverified, never built. A Mac cannot keep a closed lid awake without root
(`pmset disablesleep`) or Apple's clamshell conditions (power, an external display and
a keyboard or mouse). On macOS `jaunt host on` only holds `caffeinate -s` (prevents
system sleep on AC, tied to the daemon's pid) while the lid is open, and says so:
"closed-lid hosting needs root on macOS; holding caffeinate -s while the lid is open".
