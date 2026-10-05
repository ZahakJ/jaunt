# The first real-world test


Do this with both machines in sight, and with the Cloudflare path still working.

1. **Same LAN.** Desktop and laptop at home. `jaunt ls` should say `direct` with a latency of a few ms. Hop, type, leave with Enter `~` `.`, come back.
2. **Phone hotspot.** Move the laptop to a phone hotspot while a hop is open. It should keep going (maybe a short pause). `jaunt ls` shows whether the path is direct or relayed; note which. `jaunt check` on both machines should say UDP address discovery works.
3. **Lid closed, on mains.** On the laptop: `jaunt up`, log in locally, `jaunt host on`, `jaunt host status` (expect `awake`). Close the lid. From the desktop: `jaunt ls` shows `awake`; `jaunt toluene` works.
4. **Lid closed, on battery.** Pull the charger with the lid closed. It should be asleep within about 15 s, and `jaunt ls` on the desktop should show it offline.
5. **Plug in again with the lid still closed.** Expect it to stay asleep until you open the lid: jaunt does not wake machines. Open the lid; `jaunt host status` should say `awake` again.
6. **Heat (optional, watch it).** Lid closed on mains, run `stress-ng --cpu 0 --timeout 300s` through `jaunt run toluene -- ...`. It should release and sleep within 2 to 3 minutes of reaching 90 °C.
7. **Unlink and lock-out.** Link a third machine (or a second user account), then `jaunt unlink <it> --kill-sessions` from the desktop. Check that every machine is listed under "confirmed by", that `jaunt run desktop -- true` from the unlinked one fails, and that its tmux sessions are gone.
8. **Relay down.** Stop the relay. Machines that already have a direct path should keep working, and a restarted daemon should find its peer again directly. New machines cannot link and relayed-only machines are unreachable until the relay is back.

Write down what happened at each step, especially anything that took longer than a few seconds.
