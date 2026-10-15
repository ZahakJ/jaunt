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
| power or temperature cannot be read, a sensor read hangs for 2 s, or the CPU sensors that were there stop answering | the lock is not held: unknown means "sleep" |
| released because of the power (battery, weak charger) | taken again only once mains has been steady for a while (20 s to 2 min, depending on how long it was off) |
| released while the lid is closed (and no dock or second display) | logind normally suspends at once; if the machine is still awake 5 s later, jaunt runs `systemctl suspend` itself, and again a minute later if needed |
| the daemon crashes, is killed, or hangs | the lock is gone (see below), so logind suspends a closed laptop |

Durations are leaky buckets: time in the condition adds up, time out of it pays back at
half speed.

The lock is held by a `systemd-inhibit` child that
waits on a pipe from the daemon. If the daemon exits in any way, the pipe closes and the
child exits within milliseconds, so logind drops the lock.

If something hangs instead: sensor reads run on a helper thread with a 2 s deadline, and a
reading that does not come back counts as unknown, which releases the lock. If the guard
thread itself stops ticking for 25 s, the daemon kills the lock holder by its process id
(with or without systemd), `jaunt host status` says the guard is not answering instead of
showing the last state, and under systemd the daemon also stops sending the watchdog
keep-alive (`WatchdogSec=30`), so systemd restarts it.

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

## Power

The lock is never held on battery. jaunt does not manage displays: turn the internal
display off on lid close in your compositor, e.g. Hyprland's `bindl = , switch:on:Lid
Switch, exec, hyprctl keyword monitor "eDP-1, disable"`.

## What it needs

- systemd-logind, and **a local login since boot**. logind gives the lid lock to the
  user's background service only while that user has a local graphical session. After
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
| `logind-lid-on-ac.sh install` | writes a logind drop-in, `HandleLidSwitchExternalPower=ignore`, and reloads logind. No heat protection | `logind-lid-on-ac.sh undo` |
| `wake-on-lan.sh install <connection>` | sets NetworkManager wake-on-LAN/WLAN to `magic` for that connection | `wake-on-lan.sh undo <connection>` |

Waking a sleeping laptop is not part of jaunt. A magic packet works only from another
machine on the same LAN, and WoL must also be enabled in the firmware setup.

## macOS

A Mac cannot keep a closed lid awake without root
(`pmset disablesleep`) or Apple's clamshell conditions (power, an external display and
a keyboard or mouse). On macOS `jaunt host on` only holds `caffeinate -s` (prevents
system sleep on AC, tied to the daemon's pid) while the lid is open, and says so:
"closed-lid hosting needs root on macOS; holding caffeinate -s while the lid is open".
