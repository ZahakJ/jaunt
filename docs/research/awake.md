# Laptops that stay reachable ("host mode")

Research for jaunt, 2026-10-04. Tested on the always-on Arch desktop (systemd 261.2, polkit 127,
Hyprland 0.56.2, hypridle 0.1.8, upower 1.91.3). The desktop has **no lid, no system battery and no
`/sys/class/thermal` zones**, so anything about lid events, AC loss or closed-lid heat is
source-reading or UNVERIFIED. Tags: **VERIFIED** means I ran it or read the primary source today.
**UNVERIFIED** means it comes from memory or secondary sources.

## TL;DR

1. On systemd Linux, an ordinary user can keep a laptop awake with the lid closed **without root**.
   The guard takes a logind `handle-lid-switch` block inhibitor. A `sleep` block inhibitor on its
   own is **not** enough, because `LidSwitchIgnoreInhibited=yes` is the default. Polkit grants
   this without a password to a local active session (`allow_active=yes`), and also to a
   **systemd --user service** as long as the user has an active local graphical session
   ("display session"). Both VERIFIED on this desktop.
2. **The refutation and the caveat:** `inhibit-handle-lid-switch` has `allow_any=no`, and
   `inhibit-block-sleep` has `allow_any=auth_admin_keep`. Three cases follow:
   - When nobody is logged in locally (after a reboot, or with lingering and no session), the
     guard **cannot acquire** the lock.
   - From an SSH or other remote session it cannot acquire the lock either.
   - A lock that is already held **survives logout**, because logind treats a holder with no
     session as "active everywhere".

   So "host mode" works only after the owner has logged in once since boot. Two small root
   extras close this gap; see §5. The preferred one is a polkit rule.
3. The lock dies with its file descriptor. This was VERIFIED with `kill -9`: the holder died and
   `BlockInhibited` went from `sleep:handle-lid-switch` to `""` within 1 s. logind re-checks the
   lid **level** (not only the edge) after every event-loop iteration while the lid is closed. So
   if the guard crashes, or releases the lock with the lid closed, logind **suspends at once**
   unless the machine is docked or has more than one display. This is the safe direction
   (VERIFIED by reading the source; not run on a lid). The one unsafe case is a guard that
   **hangs** while holding the fd. Use a systemd `WatchdogSec=`.
4. Do **not** take an `idle` inhibitor. hypridle honours systemd idle inhibitors by default
   (`ignore_systemd_inhibit=false`), so an idle inhibitor would also stop screen **locking** and
   DPMS. That is a security regression. Take `handle-lid-switch:sleep` in mode `block` instead.
   The `sleep` part makes `systemctl suspend` from hypridle, KDE PowerDevil or a Hyprland
   `bindl` fail while hosting.
5. Wake-on-demand is weak. A user timer with `WakeSystem=` cannot work, because it needs
   privileges. `/sys/class/rtc/rtc0/wakealarm` is root-only. WoL and WoWLAN only work on the same
   L2 network, so the always-on desktop would have to be the "waker" for laptops at home. Wi-Fi
   WoWLAN under s2idle is unreliable. Surprise: on Arch, a wheel user at a local console *can*
   set NetworkManager `wake-on-lan` / `wake-on-wlan` without sudo, through polkit.
6. macOS: an unprivileged tool **cannot** keep a closed-lid MacBook awake without an external
   display. `caffeinate -s` only stops idle sleep, and only on AC. Closed-lid operation needs
   Apple's clamshell conditions (power + external display + input device) or
   `sudo pmset -a disablesleep 1` (root). This is UNVERIFIED on hardware.

## 1. Linux / systemd

### 1.1 Inhibitor kinds and modes (VERIFIED: `man systemd-inhibit`, `man org.freedesktop.login1`, systemd 261)

- `what` is one of `shutdown`, `sleep`, `idle`, `handle-power-key`, `handle-reboot-key`,
  `handle-suspend-key`, `handle-hibernate-key` or `handle-lid-switch`.
- `mode` is `block`, `delay` or **`block-weak`**. `block-weak` is newer: it "is not enforced
  against operations initiated by privileged clients or those owned by the user that also owns
  the inhibitor lock". `delay` is only valid for `sleep` and `shutdown`. The delay is capped by
  `InhibitDelayMaxUSec`, which is 5 s on this box.
- D-Bus: `org.freedesktop.login1.Manager.Inhibit(ssss) -> h`. The lock is released when the fd and
  all of its duplicates are closed. For Rust: call it with zbus and keep the `OwnedFd`. Do not
  leak the fd into children: logind's fd is CLOEXEC, but do not `dup` it into the tmux spawn.
- Busctl introspection (VERIFIED) shows these Manager properties: `LidClosed` (emits-change),
  `Docked`, `OnExternalPower` (no change signal, so it has to be polled), `BlockInhibited`,
  `BlockWeakInhibited`, `DelayInhibited` (emits-change), `HandleLidSwitch*` (const),
  `SleepOperation`, `PreparingForSleep`, and the signal `PrepareForSleep(b)`.
  `LidSwitchIgnoreInhibited` is **not** exposed over D-Bus.
- This desktop: `HandleLidSwitch="suspend"`, `HandleLidSwitchExternalPower=""` (unset),
  `HandleLidSwitchDocked="ignore"`, `Docked=true` (because of two monitors; there is no dock),
  `OnExternalPower=true`, `HoldoffTimeoutUSec=30s`.

### 1.2 Polkit actions and their defaults (VERIFIED: `/usr/share/polkit-1/actions/org.freedesktop.login1.policy`, polkit 127)

| action | any | inactive | active |
|---|---|---|---|
| inhibit-handle-lid-switch | **no** | yes | yes |
| inhibit-block-sleep | auth_admin_keep | yes | yes |
| inhibit-delay-sleep | yes | yes | yes |
| inhibit-block-idle | yes | yes | yes |
| suspend | auth_admin_keep | auth_admin_keep | **yes** |
| suspend-ignore-inhibit | auth_admin_keep | auth_admin_keep | auth_admin_keep |
| set-self-linger | **yes** | yes | yes |
| set-user-linger (others) | auth_admin_keep | auth_admin_keep | auth_admin_keep |

The source maps `block` **and** `block-weak` to the same `inhibit-block-*` action (`logind-dbus.c`
in method_inhibit, v261). Rules on this box: `50-default.rules` (admin = wheel), `empower.rules`,
and an upower rule that gives root `*-ignore-inhibit`. None of them changes the actions above.
`/etc/polkit-1/rules.d` is unreadable as this user, so local rules there are unknown.

**How polkit picks the "session" of a subject** (VERIFIED from
`polkit-org/polkit/src/polkitbackend/polkitbackendsessionmonitor-systemd.c`, main):
1. It uses the pidfd's or pid's session.
2. If the process is in no session, it falls back to `sd_uid_get_display(uid)`, the user's display
   session.
3. If there is none, `allow_any` applies.

Consequences:
- A process in `user@1000.service` belongs to no session. VERIFIED: `GetSessionByPID` says
  "does not belong to any known session". Polkit evaluates it against the graphical session (3,
  active, local), so it gets `allow_active`.
- **Test (VERIFIED):** `systemd-run --user --unit=jaunt-inhibit-test --wait ...` running
  `systemd-inhibit --no-ask-password` succeeded with exit 0 for `handle-lid-switch` (block),
  `sleep` (block), `sleep` (delay), `idle` and `sleep` (block-weak). A plain shell (itself in
  `user@1000.service/app.slice`) also took `handle-lid-switch`, and `--list` showed it.
- With no display session (not logged in, or after logout with linger): `handle-lid-switch` gives
  "no" and block-sleep needs admin. **Acquisition fails.** NOT TESTED, because logging out the
  live session is not allowed here. This follows from the policy file and the polkit code.
- An SSH session counts as a remote session, so `allow_any` applies and the lock is **denied**. So
  the guard must not be started from an SSH shell; it should be a user unit.
- `systemd-logind` (systemd 256+) shows a `Class=manager` session 1 for the user manager. It is
  *not* returned for service processes, so it does not change the above (VERIFIED with
  `loginctl list-sessions` and `GetSessionByPID`).

### 1.3 Does a handle-lid-switch block inhibitor really stop lid suspend? (VERIFIED by source, v261)

`logind-button.c` picks the action as follows:
- If docked or more than one display: `HandleLidSwitchDocked`.
- Otherwise, if `HandleLidSwitchExternalPower` is set and on AC: that value.
- Otherwise: `HandleLidSwitch`.

It then calls `manager_handle_action(m, INHIBIT_HANDLE_LID_SWITCH, ...)`, which does this:
- `ignore` means nothing happens.
- During the post-boot or post-resume holdoff (`HoldoffTimeoutSec`, 30 s) it does nothing.
- **If `handle-lid-switch` is inhibited (`MANAGER_IS_INHIBITED_IGNORE_INACTIVE`), it does nothing.**
  "Inactive" here means the holder's session is inactive. A holder with **no session counts as
  active** (`pidref_is_active_session` returns 1 when no session is assigned: "globally active
  on all ttys"). A holder inside a session that becomes inactive (a VT switch to another user) is
  ignored.
- Otherwise it suspends. The `sleep`/`idle` inhibitors are skipped, because
  `LidSwitchIgnoreInhibited=yes` is the default. That default is documented in
  `man logind.conf`: "the lid switch does not respect suspend blockers by default".

So yes: an ordinary user who holds `handle-lid-switch` (block) keeps logind from acting on the lid.
**A `sleep` block inhibitor alone does not**, which is a common misconception.

### 1.4 Releasing the lock while the lid is closed (VERIFIED by source, not on hardware)

On a lid close, logind installs an `sd_event_add_post` source (`button_install_check_event_source`,
with the comment "keep rechecking as long as the lid is closed"). It runs `button_recheck`, which
calls the action again with `is_edge=false` **after every event-loop iteration** until the lid
opens. Closing the inhibitor fd is itself an event. So releasing the lock with the lid closed
leads to a **suspend immediately** (within milliseconds), as long as:
- the machine is not docked and has at most one display,
- the holdoff is not running, and
- no other `handle-lid-switch` inhibitor exists.

The same holds for a crash or `SIGKILL` of the guard. The fd-release part was VERIFIED here with
`kill -9`.

Edge cases:
- If the owner set `HandleLidSwitchExternalPower=ignore`, a release while on AC does nothing. If
  AC is then lost, the next logind loop iteration selects `HandleLidSwitch=suspend`. logind sees
  D-Bus traffic all the time, including our own polls, so this is quick in practice (UNVERIFIED).
- After a resume with the lid still closed (WoL, RTC wake), the guard has **30 s of holdoff** to
  take the lock again before logind re-suspends. Poll faster than that, or react to
  `PrepareForSleep(false)`.

### 1.5 Unprivileged `systemctl suspend`

`CanSuspend` returns `"yes"` for this user (VERIFIED). That means polkit `suspend` is
`allow_active=yes`. It is refused when a **block** sleep inhibitor is held, even one held by the
same user: `uid_to_ignore` only exempts `block-weak`, and overriding needs
`suspend-ignore-inhibit`, which is auth_admin (VERIFIED by source, `logind-dbus.c` around line
2826). I did not call Suspend on this production desktop.

Side effect for jaunt: a remote user inside a jaunt-hosted tmux can run `systemctl suspend`. tmux
runs under the user manager, so polkit falls back to the display session and allows it. The
laptop then cannot be reached until someone wakes it. While hosting, the guard's `sleep` block
prevents this. That is a good reason to include `sleep` in the lock.

### 1.6 Compositors and desktops

- **Hyprland:** it does not take logind inhibitors. The inhibitor list on this desktop has none
  from Hyprland (VERIFIED, but this is a desktop). Lid handling is done by the user with
  `bindl = , switch:on:Lid Switch, exec, ...`. Common uses are `systemctl suspend`, disabling
  `eDP-1`, or DPMS ([Hyprland discussion #4842](https://github.com/hyprwm/Hyprland/discussions/4842),
  [hyprland-clamshell guide](https://bobbys.zone/guides/hyprland-clamshell); UNVERIFIED wiki URL,
  which returned 404 today). Hyprland's config is moving to Lua
  ([discussion #14858](https://github.com/hyprwm/Hyprland/discussions/14858)). A `bindl ...
  systemctl suspend` is defeated by our `sleep` block, so it fails with "inhibited". Disabling
  the internal monitor still works, which is what we want.
- **hypridle** ([wiki](https://wiki.hypr.land/Hypr-Ecosystem/hypridle/), VERIFIED today):
  `ignore_systemd_inhibit` (default false) means it honours `--what=idle` inhibitors, so **do not
  take `idle`**. `inhibit_sleep` (default 2) is why it holds a `sleep` *delay* inhibitor (seen in
  the list). A listener with `on-timeout = systemctl suspend` is blocked by our `sleep` block.
  Locking and DPMS continue. The owner's local `hypridle.conf` has no suspend listener (VERIFIED).
- **KDE PowerDevil** (UNVERIFIED): it permanently holds `handle-lid-switch` and performs its own
  configured lid action by calling logind Suspend. Our lid inhibitor is redundant there; our
  `sleep` block is what stops it. PowerDevil may show an error.
- **GNOME gsd-power** (UNVERIFIED): it takes `handle-lid-switch` only while external monitors are
  connected and otherwise leaves the lid to logind. Its idle auto-suspend is a logind Suspend call
  and is blocked by `sleep`.
- **Screen with the lid closed:** logind does nothing to displays. Many ECs turn the backlight off
  on lid close, but the compositor may keep `eDP` active, which costs power and heat
  (UNVERIFIED). Recommend that users add a `bindl` that disables `eDP-1`. The guard should not
  manage displays.

### 1.7 Linger

`loginctl enable-linger` on one's own user maps to `set-self-linger`, which is `yes` for any, so
**no root is needed** (VERIFIED in the policy). This user already has `Linger=yes`. Linger keeps
the user manager, and with it the jaunt daemon, running without a login. It does **not** give
the right to take the lid lock (see 1.2).

## 2. Detection

### Power (VERIFIED: kernel ABI `sysfs-class-power` and systemd `src/shared/battery-util.c` v261)

- `type` is one of `Battery`, `UPS`, `Mains`, `USB` or `Wireless`. `online` can be 0, 1 (fixed) or
  **2 (programmable, USB-PD)**, so test `>= 1`, not `== 1`.
- **Surprise (VERIFIED here):** `/sys/class/power_supply/hidpp_battery_0` is a Logitech mouse. It
  has `type=Battery`, `scope=Device`, `status=Discharging` and `online=1`. A naive check would
  decide "on battery" from it. You must skip `scope=Device`.
- systemd's `on_ac_power()` algorithm, which is worth copying exactly:
  1. Skip USB supplies that are in USB-C *source* role (checked through the typec parent).
  2. Batteries count only if not `scope=Device`, if `present`, and if `status == Discharging`.
  3. Any non-battery supply with `online > 0` means AC.
  4. Otherwise, any discharging battery means battery.
  5. Otherwise, **AC is assumed**, including on read failure.
- logind's `OnExternalPower` is the same function (`manager_is_on_external_power`) and is what
  decides `HandleLidSwitchExternalPower`. Poll it as a cross-check. It has no change signal.
- Extra guard rule: if AC is "online" but the battery is `Discharging` for more than 60 s (a weak
  USB-C charger), treat it as **battery**. Use `capacity` (0–100) for the level.

### Lid

- Prefer logind's `LidClosed` (emits PropertiesChanged; reliable on ACPI and non-ACPI).
- `/proc/acpi/button/lid/*/state` (`state:      open|closed`) exists only on ACPI with the button
  driver. It is absent on this desktop (VERIFIED: no `/proc/acpi/button`), and it is a
  fallback/diagnostic only.
- `Docked` is true with more than one display. That is true here with no dock at all (VERIFIED).

### Temperature

- `/sys/class/thermal/thermal_zone*/{type,temp,trip_point_N_{type,temp}}` is in millidegrees C.
  Laptops usually have `x86_pkg_temp` and ACPI `acpitz` zones with `critical`, `hot` and `passive`
  trips. **This AMD desktop has no thermal zones at all** (VERIFIED), so the guard must also read
  hwmon.
- hwmon: `/sys/class/hwmon/hwmon*/name` plus `temp*_input`, with optional `temp*_crit` and
  `temp*_max`. Seen here: `k10temp` 78.9 °C with no crit (a busy desktop), `nvme` crit 84.85 °C,
  `amdgpu`, `spd5118`, and Wi-Fi `mt7921_phy0`. Use the CPU sensors `k10temp`/`zenpower` (Tctl),
  `coretemp` (Package id 0) and `acpitz`, and also `nvme`.
- **How hot does a closed-lid laptop on AC get?** No measurement exists here. Secondary and
  anecdotal sources
  ([Tom's Guide forum](https://forums.tomsguide.com/threads/does-the-lid-affect-cooling-performance.418677/),
  [laptopbee](https://www.laptopbee.com/how-to-use-a-laptop-with-the-lid-closed-without-overheating/),
  [Lenovo](https://forums.lenovo.com/t5/Gaming-Laptops/Lenovo-Slim-5-Gen-8-overheating-with-lid-closed/m-p/5291142),
  [ASUS ROG](https://rog-forum.asus.com/t5/rog-zephyrus-series/rog-zephyrus-laptop-very-hot-when-lid-is-closed/td-p/840804))
  suggest +5–10 °C under load with the lid closed. At idle the change is small, and can even drop
  3–5 °C once the panel is off. There are outlier reports of 70–100 °C caused by firmware or
  drivers that do not idle. All of this is UNVERIFIED. Typical idle laptop CPU is 40–55 °C. Intel
  TjMax is about 100 °C and AMD Tctl limits are about 95 °C (UNVERIFIED, from memory).
- **Proposed thresholds** (configurable; tune on real laptops):
  - **Trip** when any CPU sensor is ≥ 90 °C for 120 s continuously, or any sensor is within 5 °C of
    its own `crit` or `critical` trip at any moment (immediate), or `nvme` is ≥ its `max`.
  - **Re-arm** only after the lid is opened (latch). Otherwise the laptop would cycle between hot,
    suspended, woken and hot again. A softer option is ≤ 70 °C for 10 min.
  - If no sensor is readable, treat temperature as unknown: allow hosting but log a warning. A
    stricter option refuses to host.
- Battery thresholds: only hold the lock on AC. On battery, never hold it while the lid is
  closed, so that a laptop in a bag cannot cook. An optional "lid open, battery ≥ 30 %" mode is
  only for when the lid is open.

## 3. Wake on demand

- **WoL (wired):** `ethtool -s IF wol g` needs root and does not persist (UNVERIFIED for
  persistence). `nmcli connection modify CONN 802-3-ethernet.wake-on-lan magic` persists in the
  NetworkManager profile. Changing a system connection needs
  `org.freedesktop.NetworkManager.settings.modify.system` (auth_admin_keep). **On Arch,
  `/usr/share/polkit-1/rules.d/org.freedesktop.NetworkManager.rules` grants this to local wheel
  users without a password** (VERIFIED, and this user is in wheel). Profiles with
  `permissions=user:me` use `modify.own`, which is `yes` for active users. So on Arch with NM the
  owner may need no sudo. Elsewhere it needs admin. Few laptops still have Ethernet.
- **WoWLAN:** `802-11-wireless.wake-on-wlan magic` (VERIFIED in `man nm-settings-nmcli`) needs
  driver and firmware support (`iw phy phyN wowlan show`, not installed here). Many chips and
  platforms lose association or drop WoWLAN under **s2idle** ("modern standby"). Reliability is
  poor and there is extra battery drain (UNVERIFIED, general knowledge).
- **Reach:** the magic packet is an L2 broadcast. It cannot traverse the iroh relay. Only a peer
  **on the same LAN** can wake a laptop. **Design implication:** the always-on desktop can offer
  `jaunt wake <laptop>`, which sends the magic packet to a MAC learned at pairing. This only works
  at home.
- **Scheduled wake:**
  - `WakeSystem=` "requires privileges and is thus generally only available in the system service
    manager" (VERIFIED, `man systemd.timer`), so a user timer cannot use it.
  - `/sys/class/rtc/rtc0/wakealarm` is `0644 root` (VERIFIED), and `rtcwake` needs root.
  - The owner could install a system timer, but scheduled wake ("wake every 30 min for 2 min to
    check in") is a poor substitute for presence.
- **s2idle reality:** this desktop has `mem_sleep = s2idle [deep]` (VERIFIED). Network wake from
  s2idle on Linux is hardware-specific and rarely works for Wi-Fi (UNVERIFIED).
- **Costs:** s2idle drains about 0.5–3 %/h depending on the platform, and S3 drains less.
  WoWLAN keeps the radio partly up, which adds an unknown amount. Staying awake idle with the lid
  closed draws about 3–8 W, so an average battery is gone in roughly 6–15 h. That is why host mode
  must be AC-only. All of these numbers are UNVERIFIED estimates.

## 4. macOS (all UNVERIFIED on hardware)

- `caffeinate` ([ss64 man page mirror](https://ss64.com/mac/caffeinate.html), read today):
  - `-i` prevents idle system sleep.
  - `-s` prevents system sleep and is "valid only when system is running on AC power".
  - `-d` keeps the display awake. `-u` declares user activity. `-w pid` ties the assertion to a
    process.

  These are IOKit power assertions (`IOPMAssertionCreateWithName`,
  `kIOPMAssertPreventUserIdleSystemSleep` / `PreventSystemSleep`), which an unprivileged app can
  create from Rust through IOKit FFI. **They do not override a lid close** when there is no
  external display (secondary sources:
  [Macworld](https://www.macworld.com/article/673295/how-to-use-macbook-with-lid-closed-stop-closed-mac-sleeping.html),
  [MachineFriendly](https://www.machinefriendly.com/blog/keep-macbook-awake-lid-closed-awaketoggle)).
- `pmset` ([ss64 mirror](https://ss64.com/mac/pmset.html)): "pmset must be run as root in order to
  modify any settings". Relevant settings are `sleep`, `displaysleep`, `womp` (wake on magic
  packet), `powernap`, `tcpkeepalive`, `lidwake`, `acwake` and `schedule`/`repeat` wake. The
  `-a/-b/-c` flags pick all, battery or charger. `disablesleep 1` (an undocumented-ish
  SleepDisabled flag) keeps the machine awake with the lid closed, even on battery, on Intel and
  Apple Silicon (secondary sources). It needs root and is dangerous in a bag.
- **Clamshell ("closed-display mode"):** this needs a power adapter, an external display and an
  external keyboard or mouse. I could not retrieve the Apple support page today (the URLs I tried
  redirect to other articles), so this rests on secondary sources.
- **"Wake for network access"** (womp) together with a Bonjour Sleep Proxy (Apple TV or HomePod)
  can wake a Mac for advertised services. jaunt over iroh/QUIC is not an advertised Bonjour
  service, so it will not trigger this.
- **launchd:**
  - `~/Library/LaunchAgents/*.plist` user agents run in the GUI (Aqua) session without root, so
    the daemon and its power assertion can live there.
  - LaunchDaemons (`/Library/LaunchDaemons`) need root.
- **Verdict:** with no root on macOS, host mode can only prevent idle sleep on AC while the lid is
  open, or while in real clamshell. If the owner wants a closed lid with no external display, the
  owner-run script would toggle `pmset -c disablesleep 1` (AC only? `disablesleep` is global:
  UNVERIFIED whether `-c` scoping is honoured). That needs the same AC and thermal guard, run as
  root, which defeats "no root". Recommend: **do not support closed-lid hosting on macOS in v1**.
- Windows, in case a laptop runs it (UNVERIFIED): the lid action is a power-plan setting
  (`powercfg /setacvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0`, then `powercfg /setactive
  SCHEME_CURRENT`). Whether that needs admin depends on policy. `SetThreadExecutionState` does not
  override the lid.

## 5. Owner-run root extras (`jaunt-host-setup.sh`, optional; each piece is reversible)

**Option A (preferred): a polkit rule.** It lets the guard take the lock even with no local login
(after a reboot, or over SSH). The AC and thermal guard stays in charge. Nothing changes when
jaunt is not running.

```js
// /etc/polkit-1/rules.d/49-jaunt-host.rules
polkit.addRule(function(action, subject) {
    if ((action.id == "org.freedesktop.login1.inhibit-handle-lid-switch" ||
         action.id == "org.freedesktop.login1.inhibit-block-sleep") &&
        subject.user == "OWNER_USERNAME") {
        return polkit.Result.YES;
    }
});
```

- Install: `install -m 0644 49-jaunt-host.rules /etc/polkit-1/rules.d/`. polkitd picks up rule
  changes automatically (UNVERIFIED; `systemctl restart polkit` otherwise).
- Undo: `rm /etc/polkit-1/rules.d/49-jaunt-host.rules`.
- Risk: any process of that user can now block sleep or the lid. That was already true while the
  user was logged in.

**Option B: a logind drop-in.** The lid is ignored whenever on AC, so no guard is needed, but
there is also no thermal protection.

```ini
# /etc/systemd/logind.conf.d/50-jaunt-lid-on-ac.conf
[Login]
HandleLidSwitchExternalPower=ignore
# HandleLidSwitch=suspend          (default; on battery, lid close still suspends)
# HandleLidSwitchDocked=ignore     (default)
```

- Apply: `systemctl reload systemd-logind`. VERIFIED: `CanReload=yes`, and
  `man systemd-logind.service` says SIGHUP reloads the configuration. Restarting logind kills
  sessions on some versions, so do not restart it.
- Undo: `rm` the file, then `systemctl reload systemd-logind`.
- Caveat: AC loss while the lid is closed relies on logind re-evaluating the lid, which happens on
  its next loop iteration (§1.4).

**Option C: WoL on the laptop's NIC.**
- Enable: `nmcli connection modify "<conn>" 802-3-ethernet.wake-on-lan magic` (or
  `802-11-wireless.wake-on-wlan magic`), then `nmcli connection up "<conn>"`.
- Undo: set it to `default`.
- Without NM: a udev rule
  `ACTION=="add", SUBSYSTEM=="net", NAME=="enp*", RUN+="/usr/bin/ethtool -s $name wol g"`
  in `/etc/udev/rules.d/81-jaunt-wol.rules`. Undo by `rm` + `udevadm control --reload`.
- Also enable WoL in the BIOS/UEFI (manual).

**Option D (macOS, owner choice):** `sudo pmset -c sleep 0` plus `sudo pmset -c womp 1`. Undo
with `sudo pmset -c sleep <previous>`; record the previous values with `pmset -g custom` first.
`disablesleep` is not recommended.

## 6. Recommended design: the "host mode" guard

The guard runs inside the jaunt user daemon as a systemd **user** unit with `Type=notify` and
`WatchdogSec=30s`. It is never started from SSH. Enable it with `jaunt host on`, a persistent
config flag. It is Linux-only in v1. On macOS it holds `PreventUserIdleSystemSleep` on AC only
and documents that a closed lid sleeps.

**Inputs, sampled every 5 s** (well under the 30 s resume holdoff), plus immediate re-evaluation
on these events:
- logind `PropertiesChanged(LidClosed)`
- `PrepareForSleep(false)` (resume)
- a config change

Each tick reads:
- `enabled`
- `ac`: the copy of `on_ac_power()`, AND NOT a battery discharging for more than 60 s, cross-checked
  with logind `OnExternalPower`
- `lid_closed`
- `docked`
- `temp_state`: ok, hot-sustained or critical
- `battery_pct`

**States:**

```
DISABLED ──enable──▶ ARMED
ARMED     : not holding. Enter HOLDING when ac && temp ok && !thermal_latch.
HOLDING   : holds Inhibit("handle-lid-switch:sleep", "jaunt", "host mode: reachable on AC", "block").
            → ARMED       on !ac (debounced 10 s, so a cable wiggle is ignored)
            → THERMAL     on hot ≥ 120 s or critical (immediate)
            → DISABLED    on disable
THERMAL   : release fd; set thermal_latch; log + desktop notify.
            latch clears on lid open (LidClosed false) [or ≤70 °C for 10 min if configured] → ARMED
on any release: if lid_closed && !docked, expect logind to suspend at once (§1.4).
            Fallback: if still awake 20 s later and lid_closed && !ac (or THERMAL), call
            Manager.Suspend(false) ourselves (polkit `suspend` is allowed for an active user; it
            may fail if another block inhibitor exists: log it).
```

**Acquire failure** (polkit denied because there is no display session, or SSH): stay in ARMED,
retry every 60 s, and surface "host mode inactive: log in locally once, or install the polkit
rule (§5A)" in `jaunt status`, which peers see.

**Failure safety:**
- A crash or kill closes the fd, which releases the lock, and logind suspends a closed-lid laptop
  at once. This is the safe direction. VERIFIED for fd release (`kill -9` test); logind's reaction
  is VERIFIED by source only.
- A **hang** keeps the fd open, which is unsafe. Mitigate it with `WatchdogSec=30s`: the
  evaluation loop pets `WATCHDOG=1` only after a full successful tick, and systemd SIGABRTs the
  hung process, which releases the lock.
- Never pass the fd to children (CLOEXEC). Never hold an `idle` lock.
- Unknown power state means AC, which is systemd's convention. Unknown temperature means warn but
  allow.

**Test plan (no laptop available):**
1. Make the guard's readers take roots: `power_supply_root`, `thermal_root`, `hwmon_root`, and an
   optional `lid_file`. Hide the logind and inhibitor behind traits (`LoginBackend`:
   `lid_closed()`, `docked()`, `on_external_power()`, `inhibit() -> Guard`, `suspend()`). Use an
   injectable clock.
2. Unit tests build fake trees in a tempdir. Fixtures:
   - an `AC` Mains supply with online 0/1
   - `BAT0` Charging, Discharging or Full
   - a `ucsi-source-psy-*` USB supply with online=2, sink and source roles (a fake `typec` parent)
   - the **hidpp mouse** case (`scope=Device` Discharging; must not mean battery)
   - no supplies at all (means AC)
   - an unreadable attribute

   Thermal fixtures: zones with trips, hwmon-only (like this desktop), and none.
3. A state-machine table test, with fake time advancing: cable wiggle under 10 s, AC loss with
   the lid closed, a sustained hot period vs a spike, the thermal latch clearing on lid open, a
   resume within the holdoff, and acquire denied.
4. An integration test against **real logind on this desktop** (safe: no lid, two displays so
   `Docked=true`, `HandleLidSwitchDocked=ignore`). Take and release `handle-lid-switch:sleep`.
   Assert `BlockInhibited` toggles. `kill -9` the test holder and assert that it clears. Run the
   holder under `systemd-run --user` to cover the user-service polkit path.

   Do **not** call Suspend here. Do not run it in Docker, because there is no logind in containers.
5. A hang test: a fake backend that blocks forever plus a `WatchdogSec=5` transient user unit.
   Assert the unit is killed and the inhibitor disappears.
6. Manual on the first real laptop, with the lid closed:
   - on AC, the machine stays reachable
   - pulling AC leads to suspend within 10–15 s
   - heating it with `stress-ng` for 2 min trips the thermal guard and suspends
   - opening the lid re-arms
   - `kill -9` jaunt with the lid closed leads to an immediate suspend
   - log out (no linger session) and check whether the lock persists, and whether re-acquire
     fails after a reboot

   Record the idle closed-lid temperature curve over 1 h.

Commands run today: `systemd-inhibit --list`; `systemd-inhibit --what=handle-lid-switch ...`;
`systemd-run --user --unit=jaunt-inhibit-test --wait ...` (auto-collected); `busctl
get-property/introspect org.freedesktop.login1`; `GetSessionByPID`; `loginctl
show-user/show-session/list-sessions`; parse of `org.freedesktop.login1.policy` and
`org.freedesktop.NetworkManager.policy`; `man logind.conf systemd-inhibit org.freedesktop.login1
systemd.timer rtcwake systemd-logind.service nm-settings-nmcli`. Source fetched from
`raw.githubusercontent.com/systemd/systemd/v261/src/login/{logind-button,logind-action,logind-inhibit,logind-dbus,logind-core}.c`,
`.../src/shared/battery-util.c` and `polkit-org/polkit/main/.../polkitbackendsessionmonitor-systemd.c`.
