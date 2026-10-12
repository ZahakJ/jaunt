# Host mode

A laptop is often the machine you want to reach and the one least likely to be awake. Close the lid and it suspends; a suspended machine answers nothing, and `jaunt ls` on the desktop shows it offline. Host mode is jaunt's answer: with `jaunt host on`, the laptop stays awake with the lid closed, but **only while it is plugged in and not too hot**.

The design starts from the worst case: a laptop closed and pushed into a bag. A program that keeps such a laptop awake can cook it. So every rule below leans the same way. When jaunt cannot tell whether the machine is on mains power, or cannot read a temperature, or its own guard stops answering, the answer is "sleep". If the daemon crashes or is killed, the lock it holds disappears with it within milliseconds, and the laptop suspends as if jaunt were not there.

In practice:

```sh
jaunt host on        # on the laptop
jaunt host status    # what closing the lid would do right now, and what is missing
jaunt ls             # on any machine: each one's host state
```

`jaunt ls` shows the host state of every machine: `awake`, `will sleep on lid close`, `on battery` or `too hot`, so you know before you walk away from the laptop whether it will still be there.

What host mode does not do:

- It does not wake a sleeping laptop. Once asleep, it stays asleep until someone opens the lid. Wake-on-LAN works only from the same network and needs firmware support; a helper script is offered, nothing more.
- It does not turn the display off. That is your compositor's job, and an example for Hyprland is below.
- It does not work without a local login since boot, unless you install an optional rule as root.

How far this has been verified matters here more than anywhere else. The guard's logic was unit-tested against fake power and temperature readings, and the real lid lock was taken and released on a desktop with no lid, no battery and no temperature sensors. **No real laptop has run host mode.** The first test on one is the checklist at the end of this chapter, best done on a desk, in sight, with the laptop's normal behaviour as the fallback.

On macOS (never built) a closed lid cannot be kept awake without root, so host mode there would only keep the machine awake with the lid open, and says so.

The rest of this chapter is `docs/awake.md` from the repository.

{{#include ../../awake.md:2:}}
