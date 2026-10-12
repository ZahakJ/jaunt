# Decisions


### Made during the build (you should know)

| Decision | Why |
| --- | --- |
| The name **jaunt** | a short trip; in Bester's *The Stars My Destination*, "jaunting" is teleporting by wanting to be there. Free as a command on Arch and as a crate |
| **iroh** for the transport | dialling by key, hole punching and a self-hostable relay without root; jaunt writes no networking or crypto of its own. Cost: young code with no audit |
| a **built-in shell server**, single layer by default | works on machines without sshd and keeps hops instant. The research recommended carrying SSH by default; that is available as `jaunt pipe` instead |
| **flat trust**: every member can add and revoke | no master key to lose; the cost is that one stolen, unlocked machine is the whole mesh until revoked |
| **port mapping off** (UPnP, NAT-PMP, PCP) | it changes your router's settings. `portmap = true` in `config.toml` turns it on; `jaunt check` mentions it when no path is direct |
| **tmux sessions kept** on unlink unless `--kill-sessions` | they are yours and may hold your work; the revoked machine is detached from them |
| **heat limit** 90 °C for 120 s, or 5 °C below a sensor's critical point | conservative for laptop CPUs; adjustable in `config.toml` (`hot_c`, `hot_secs`, `crit_margin_c`) |
| no pairing without a relay | the design mentioned LAN pairing via mDNS; it was not built |
| the relay is left open (no id allowlist, no token) | an allowlist breaks pairing; jaunt does not send relay tokens |

### Yours to make

- **Where to host the relay.** A small VPS is recommended ([`deploy/relay/README.md`](https://github.com/ZahakJ/jaunt/blob/main/deploy/relay/README.md)). Fly.io does not work for this as shipped. The desktop behind the Cloudflare tunnel works only as a relay of last resort (no direct paths through it).
- **Making the repository public.** It has been public at `github.com/ZahakJ/jaunt` since 11 October 2026. The published history contains no path, hostname or address of your machines (one early commit did; it was rewritten before the first push).
- **When to retire the Cloudflare path.** Not before the real-world test above has passed and jaunt has been used for a while.
- Whether the remaining gaps matter enough to fix: the revocation window when no informed machine is reachable, the slow roam after an outage, signed releases.
