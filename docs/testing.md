# What was measured and tested

## Measured


All of this comes from the Docker test lab on one computer (`test/harness`): machines a and b each behind their own home-router NAT, machine c behind a hard (symmetric) NAT, and a real `iroh-relay` 1.3 with TLS on a simulated internet. **It is a simulation.** The relay host stands in for Cloudflare's edge; real numbers depend on real distances, and the simulated delay has no jitter or loss, which Wi-Fi and mobile links add. Full method: [`docs/measurements.md`](measurements.md).

### Keystroke round trip

One letter typed into a tmux session on b running `cat`, timed until it is echoed back. 200 samples per row, three full runs (run 3, after the last fixes, is the one in `docs/measurements.md`). "Harness delay" is added on every router hop, so a direct path has 4× that delay and a relayed one 6×.

| harness delay | path | run 1 median / p95 (ms) | run 2 median / p95 (ms) | run 3 median / p95 (ms) |
| --- | --- | --- | --- | --- |
| 0 ms | jaunt direct | 0.42 / 0.68 | 0.6 / 1.75 | 0.39 / 1.13 |
| 0 ms | jaunt relayed | 1.0 / 1.52 | 0.98 / 2.33 | 0.95 / 1.17 |
| 0 ms | ssh through the relay host (default) | 7.0 / 9.09 | 10.0 / 11.87 | 8.49 / 10.55 |
| 0 ms | ssh through the relay host (`ObscureKeystrokeTiming=no`) | 0.7 / 0.87 | 0.51 / 0.74 | 0.69 / 0.91 |
| 20 ms | jaunt direct | 80.82 / 81.69 | 80.84 / 81.88 | 80.84 / 80.97 |
| 20 ms | jaunt relayed | 120.97 / 121.57 | 121.17 / 121.94 | 121.05 / 121.38 |
| 20 ms | ssh through the relay host (default) | 128.48 / 131.52 | 138.63 / 141.54 | 123.5 / 127.44 |
| 20 ms | ssh through the relay host (`ObscureKeystrokeTiming=no`) | 120.73 / 121.46 | 120.87 / 121.48 | 120.85 / 123.09 |

At 0 ms the numbers show the software's own cost. jaunt has no predictive echo (mosh's trick), so on a relayed path every keystroke waits for the relay round trip, just as with SSH.

### Recovering an open session (seconds until typing works again)

| disruption | run 1 | run 2 |
| --- | --- | --- |
| client's network silently dropped for 10 s | 0.5 | 0.5 |
| client's network dropped for 60 s | 1.8 | 1.7 |
| client roams onto the hard NAT (right after the 60 s drop) | **17.3** | **48.7** |
| roam back | 0.5 | 0.5 |
| client frozen 90 s (like a suspend) | 2.9 | 2.9 |
| host frozen 90 s | 4.6 | 0.8 |
| host daemon restarted | 2.9 | 2.9 |

The slow roam is a known problem: after an outage, iroh 1.3 backs off reconnecting to the relay for up to about 16 s plus jitter, and a network change does not cut that short. A roam with no outage before it recovered in 0.5 s in 10 of 10 runs. jaunt shows the "waiting for the link" line meanwhile, but does not shorten the wait.

### Throughput and size

| What | Result |
| --- | --- |
| `jaunt cp`, 200 MB, direct | 296 MB/s push, 128 MB/s pull |
| `jaunt cp`, 200 MB, relayed | 224 MB/s push, 237 MB/s pull |
| 50 MB piped through `ssh` over `jaunt pipe` | 130 MB/s direct, 118 MB/s relayed |
| release binary | 10,712,904 bytes, static (musl), x86_64 Linux |

The throughput rows are from one computer with no bandwidth limit, so they show that jaunt is not the bottleneck, not what your network will do. Relayed is as fast as direct here only because the relay runs on the same computer.

## What was tested and what was not


| Tested | Where |
| --- | --- |
| the whole flow: link, join, hop, onward hop, run, cp, fwd, pipe with OpenSSH, unlink and lock-out, a revoked machine racing a frozen one and one cut off from the network, direct and relayed paths, hard NAT, relay down and back, drops, roams, freezes, daemon restarts, latency | Docker NAT lab on one Arch Linux computer: 9 scripts, 250 checks, all passing with the current release binary (01-09 in one full run before the last settling change, then 03 and 05 again; the build before passed its 217 checks twice) |
| that the daemon contacts nothing but the relay and linked machines | lab, counted from outside on the routers |
| this README's commands (install from file, up, relay set, link and join typed on a terminal, hop, Enter ~ ., tmux detach and re-attach, `toluene:work`, the mesh view, `ls`, `run`, `cp` both ways, `fwd`, `pipe`, `check`, `host status`, `name`, `unlink --kill-sessions` and lock-out, `--cascade`, `leave`, `reset`, `down`, `uninstall`, the tmux snippet's prefix J popup and F12) | sandbox on the host (separate `JAUNT_HOME` directories, a local test relay, a private tmux socket) |
| roster merging, pairing attacks, frame limits, host-mode logic, settling, local hardening | 133 unit tests |
| host mode's real logind lock | the desktop (no lid, no battery) |

| Only simulated | |
| --- | --- |
| NAT, roaming, link drops, suspend | containers, `docker pause` and packet rules standing in for real routers, Wi-Fi and sleep |
| laptop power and heat | fake sysfs trees |

| Never run | |
| --- | --- |
| a real laptop, a real lid, real battery or heat | |
| macOS (never even built) | |
| a real relay deployment (VPS, Let's Encrypt, the Dockerfile in `deploy/relay`, Fly.io, the Cloudflare-tunnel relay) | |
| real networks: two homes, a phone hotspot, CGNAT, hotel Wi-Fi with captive portals | |
| IPv6 (the lab is IPv4 only) | |
| `jaunt up` with the real systemd user manager, and its watchdog | tests ran the daemon directly so they never touched the real user services |
| the public `curl` install line, `--up`, the root extras, the one-LAN test relay | |
| two relays in one mesh | |
