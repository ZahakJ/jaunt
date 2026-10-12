# What was tested

jaunt has been tested a great deal, and almost all of it in simulation. This chapter says exactly what was measured, where, and what was never run, so you can decide how much to trust it and what to check yourself.

Three kinds of testing stand behind the claims in this manual:

- **A Docker lab on one computer.** Three machines, each behind its own simulated home router (one of them a hard, symmetric NAT), and a real `iroh-relay` 1.3 with TLS on a simulated internet. Nine end-to-end scripts drive the release binary through the whole flow: linking, hopping, onward hops, copies, forwards, revocation, relay outages, network drops, roams and frozen machines. 250 checks, all passing. The routers count every packet the daemons send, which is how the claim "jaunt contacts nothing but the relay and your machines" was checked.
- **A sandbox on the host.** The README's own commands, typed on a real terminal, with separate `JAUNT_HOME` directories, a local test relay and a private tmux socket. This is the same setup as [Try it on one machine](try-it.md).
- **Unit tests.** 133 of them, for the parts that are easiest to get subtly wrong: merging rosters (including hostile ones), attacks on pairing, frame size limits, the host-mode guard on fake power and temperature readings, and settling.

What none of this covers: a real laptop with a real lid, battery and heat; a real relay on a real server; real home networks, phone hotspots, carrier-grade NAT or hotel Wi-Fi; IPv6; macOS; the systemd user service under a real login; and the public `curl` install line. The lab's simulated delay has no jitter or loss, which real Wi-Fi and mobile links add. The tables below say this again row by row.

The numbers are honest about where the speed comes from. On a direct path jaunt is faster than SSH through a relay host because the path is shorter, not because the software is faster: through its own relay jaunt is as fast as SSH with OpenSSH's keystroke-timing obfuscation turned off, and no faster. A comparison with the real Cloudflare tunnel was not measured.

After the lab results comes the checklist for the first real-world test, to be done with both machines in sight and the old way in still working. Until it has been run and written down, everything here is a simulation. The full method and the reference round trips are on the next page, [Measurements](measurements.md).

{{#include ../../testing.md:2:}}

## The first real-world test

{{#include ../../first-real-test.md:2:}}
