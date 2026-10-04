# jaunt NAT simulation harness

Docker-based, rootless (no sudo on the host), tool-agnostic. Three "machines" sit behind their own NAT
routers on a fake internet that also holds a relay slot. Later integration tests install the real `jaunt` binary
into it and check direct-vs-relay behaviour, roaming, link drops and suspend.

Status: built and proven on 2026-10-04 (Arch host, kernel 7.1.8, Docker 29.7.2). `./prove.sh` gives **ALL PASS**.
The harness was left **down**. The image `jaunt-harness:latest` (577 MB) is still there.

## Quick start

```sh
cd test/harness
./up.sh                     # idempotent; builds jaunt-harness:latest if missing (about 1 min)
./prove.sh                  # self-test of the NAT behaviour, exit 0 = all good
./jh help                   # helpers
./relay.sh                  # real iroh-relay in jaunt-relay: https://relay.jaunt.test + QAD udp/7842,
                            # throwaway CA made now in .state/relay, copied to a b c as /opt/jaunt/relay-ca.crt
./jh install ../../target/release/jaunt          # copies into a b c relay at /opt/jaunt/bin (on PATH)
./jh exec a jaunt ...       # run things
./down.sh                   # removes exactly the 7 containers + 4 networks by name (--image also drops the image)
```

Knobs for `up.sh`: `HARD_MODE=symmetric|udpblock|both` (router c, default symmetric), `NAT_UDP_TIMEOUT=30`,
`NAT_UDP_STREAM_TIMEOUT=120` (conntrack UDP timeouts on all routers, which are the Linux defaults; lower them to mimic
stingy home routers).

## Topology

```
                         jaunt-inet 198.51.100.0/24  ("internet", TEST-NET-2, no real uplink)
   +-----------------+------------------+------------------+------------------+
   | jaunt-relay     | jaunt-rtr-a      | jaunt-rtr-b      | jaunt-rtr-c      |
   | .10             | .11  (cone NAT)  | .12  (cone NAT)  | .13  (HARD NAT)  |
   | sshd:22         |   eth1 10.231.1.2|   eth1 10.231.2.2|   eth1 10.231.3.2|
   | introducer      |        |         |        |         |        |         |
   |  udp/9999       |  jaunt-lan-a     |  jaunt-lan-b     |  jaunt-lan-c     |
   |                 |  10.231.1.0/24   |  10.231.2.0/24   |  10.231.3.0/24   |
   |                 |  jaunt-a .100    |  jaunt-b .100    |  jaunt-c .100    |
```

| container | networks / IPs | role |
|---|---|---|
| jaunt-relay | jaunt-inet 198.51.100.10 | slot for the self-hosted relay. Runs sshd (baseline) and the UDP introducer `rendezvous.py` on udp/9999 |
| jaunt-rtr-a | inet .11, lan-a 10.231.1.2 | home-router NAT (MASQUERADE) |
| jaunt-rtr-b | inet .12, lan-b 10.231.2.2 | home-router NAT (MASQUERADE) |
| jaunt-rtr-c | inet .13, lan-c 10.231.3.2 | hard NAT: `symmetric` (MASQUERADE --random-fully), `udpblock` (drops all forwarded UDP except to the relay IP), `both`, or `cone` |
| jaunt-a / b / c | 10.231.{1,2,3}.100 | machines. Default route via their router, nothing else. Roamed IPs are .111 (a), .121 (b), .131 (c) |

* All four networks are `--internal` with `com.docker.network.bridge.inhibit_ipv4=true`. Docker adds no MASQUERADE
  and the host gets **no IP** on any of them. So no container can reach the real internet, the host's services or
  host DNS (verified: `ping 1.1.1.1` fails, `curl example.com` → "Could not resolve host"). This means iroh's default
  n0 relays and DNS discovery **cannot** be reached. A test that passes here passed on self-hosted infrastructure only.
* Machines have **only** a lan-network interface (checked with `ip -br addr`, `ip route` and docker inspect). The
  internet is reachable only through their router.
* All nodes resolve `relay.jaunt.test` → 198.51.100.10 (`--add-host`). Docker DNS does not resolve container names
  across networks, so machines cannot use the name `jaunt-relay`.
* Subnets were picked after reading the existing docker networks (172.17–172.21/16) and the host LAN (10.0.0.0/24).
  Caveat: another agent's `jaunt-sp-*` containers were running at the same time. If anyone else uses
  198.51.100.0/24 or 10.231.0.0/16, `docker network create` will refuse.
* IPv6 is disabled in every container. The harness is IPv4 only.
* Containers get `NET_ADMIN`+`NET_RAW` (routers need them for NAT. Machines need them so the harness can set routes,
  netem and blackhole rules inside them. The jaunt binary itself should run as the unprivileged user `jaunt`). Memory
  limits: relay 512m, routers 128m, machines 768m.
* The harness dir is bind-mounted read-only at `/harness` in every container.

### Router NAT rules (`node/router.sh`)
FORWARD policy DROP. LAN→WAN is allowed and ESTABLISHED,RELATED comes back. Unsolicited packets to the WAN address are
**silently dropped** in INPUT (echo-request is allowed so `ping <router>` works). MASQUERADE on the WAN.
Linux MASQUERADE keeps the source port when it can, so the a/b routers behave as **endpoint-independent mapping +
address-and-port-dependent filtering** ("port-restricted cone"), which is the most common home-router behaviour.

## Helpers (`./jh`)

| command | what it does |
|---|---|
| `jh exec <node> cmd...` | `docker exec` (adds `-it` on a tty). Nodes: a b c relay rtr-a rtr-b rtr-c |
| `jh delay <ms> [relay_ms] [loss%]` | netem on **both** interfaces of every router (so +ms each way per uplink) and on the relay egress (+relay_ms). `jh delay 0` clears it. Path RTTs: a↔relay = 2·ms+r, direct a↔b = 4·ms, a→relay→b = 4·ms+2·r |
| `jh drop <m> <s> [blackhole\|link]` | blackhole (default): netem loss 100% on egress + iptables INPUT DROP, so sends still "succeed" and packets vanish (Wi-Fi that died silently). `link`: `ip link down`, so apps get ENETUNREACH. The route is restored afterwards |
| `jh roam <m> <lanA\|lanB\|hard>` | `docker network disconnect`/`connect` with a new IP and a new default route. Processes keep running. Verified: a long-running UDP prober's reflexive address changed 198.51.100.11:41500 → 198.51.100.12:41500 with no restart |
| `jh freeze <m> <s> [pause-only]` | blackhole + `docker pause`, then unpause and restore (suspend). `pause-only` skips the blackhole (see limitations) |
| `jh install <bin> [nodes]` | `docker cp` to `/opt/jaunt/bin/` (on PATH). Warns if `ldd` shows unresolved libs |
| `jh hardmode <cone\|symmetric\|udpblock\|both>` | re-configures router c live |
| `jh tunnel <m>` | baseline that mimics today's cloudflared setup: the machine keeps `ssh -N -R 220N:127.0.0.1:22 relay` open, so its sshd is at relay:127.0.0.1:220N (a=2201, b=2202, c=2203) |
| `jh sshvia <from> <to> [cmd]` | `ssh -J relay -p 220N jaunt@127.0.0.1` (ProxyJump through the relay) |
| `jh punch <m1> <m2> [port]` | UDP simultaneous-open hole punch through the introducer, with a fixed local port |
| `jh whoami <node> [port]` | the reflexive (post-NAT) address the relay sees |
| `jh nat <router>` | conntrack table (shows the NAT mappings) |
| `jh status` | containers and IPs |

SSH: user `jaunt` exists on the relay and machines. A throwaway ed25519 key is generated into `.state/` (deleted by
down.sh) and installed everywhere. Host keys are not checked inside the harness.

## Proof results (VERIFIED, `./prove.sh` plus the manual runs, 2026-10-04)

Observed mapped (reflexive) addresses, as seen by the introducer on the relay:

| machine | local | mapped |
|---|---|---|
| a (cone) | 10.231.1.100:41000 | 198.51.100.11:41000 (port preserved) |
| b (cone) | 10.231.2.100:41000 | 198.51.100.12:41000 (port preserved) |
| c (symmetric) | 10.231.3.100:41000 | 198.51.100.13:42457, then :25743 on the next run, then :4903. A new random port per destination/flow |
| c (udpblock only) | 10.231.3.100:41000 | 198.51.100.13:41000 |

| check | result |
|---|---|
| a, b, c ping relay | PASS (0.08 ms with no delay) |
| a → b's private IP 10.231.2.100 | unreachable (host unreachable from rtr-a) |
| a → 1.1.1.1 / example.com | unreachable / no DNS |
| b → rtr-a:2222 TCP (a listening) | connect timed out |
| relay → rtr-a:2222, relay → 10.231.1.100 even with a manual route via rtr-a | timed out (FORWARD drops NEW from WAN) |
| b, relay → rtr-a:40001 UDP (a listening) | nothing delivered (a local send does arrive, so the listener works) |
| punch a↔b (cone↔cone), ports 40000/40010/40050/40100/40300 | **punched=yes**, each side heard from the other's mapped addr |
| punch a↔c, b↔c with c=symmetric | **punched=no** (a aims at c's port seen by the introducer, but c's flow to a uses another random port, and a's router filters by port) |
| punch a↔c with c=udpblock, and with c=both | **punched=no**. c still reaches relay UDP (whoami ok) and relay TCP/ssh |
| punch a↔c with c switched to cone | punched=yes (sanity check of the control) |
| ssh a → relay → b (ProxyJump over b's `-R` tunnel) | works: `connected-to b from 127.0.0.1 ... 22` |

Latency with `jh delay 20` (20 ms per router hop each way, relay egress 20 ms):

| path | measured |
|---|---|
| ping a → relay | 60.08 ms |
| direct punched UDP a ↔ b (median) | 80.2 ms |
| **ssh keystroke RTT a→relay→b** (`keystroke_rtt.py`, 20–30 samples, `-o ObscureKeystrokeTiming=no`) | **median 120.6–120.8 ms** (min 120.6, max 122.8) |
| same, OpenSSH default (ObscureKeystrokeTiming on) | median 127.8, p90 129.7 ms |
| same, no netem | 0.5 ms with Obscure off. **9.8 ms** with the default on |

So with the default settings, the relayed path is 40 ms slower than a direct path, which is what a jaunt-direct vs
ssh-via-relay comparison should show.

### Findings worth knowing (VERIFIED here)
1. **OpenSSH 10.x keystroke-timing obfuscation** (`ObscureKeystrokeTiming`, default on, 20 ms chaff interval) adds
   about 0–20 ms (median about 7–10 ms) to every interactive keystroke. Any latency comparison against the ssh baseline
   must say which setting it used. jaunt should decide deliberately whether to copy this defence.
2. **Linux conntrack clash breaks hole punching** when the router does *not* drop unsolicited WAN packets. I removed
   rtr-b's INPUT DROP and had a's packets reach rtr-b before b had sent anything. rtr-b then created an UNREPLIED
   entry (`198.51.100.11:40060 → .12:40060`). b's later outbound to a was NAT-ed to **port 52293** instead of 40060,
   and the punch failed for its whole duration, because a's ongoing probes keep the stale entry alive. With the DROP
   (the harness default, and most real routers) the same early-arrival sequence punches fine. Real routers vary.
   jaunt/iroh must survive a mapping that differs per destination even behind a "cone" router.
3. Symmetric↔port-restricted fails. Symmetric↔full-cone (endpoint-independent filtering) would likely succeed, since
   the cone side accepts from any port. The harness has no full-cone mode (it could be added with a DNAT rule).
4. `docker exec ... ssh -f` / `nohup ... &` children die when the exec session ends. Long-running daemons in tests
   must be started with `docker exec -d` (as the harness does).
5. `tc ... netem` from inside a container auto-loaded `sch_netem` on the host kernel. Nothing needed root.

## glibc / binary portability (VERIFIED)
The image is **archlinux:latest** (glibc 2.44, the same as the host), so anything built on the host runs as is.
A trivial `cargo build --release` on the host only needs GLIBC_2.34 and ran on debian:12 (2.36) and ubuntu:22.04
(2.35), but failed on rockylinux:8 (2.28: "GLIBC_2.30 not found"). The glibc floor depends on the symbols used, not
on the build host. Check it with `objdump -T bin | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1`.
For the unknown-OS laptops, a static `x86_64-unknown-linux-musl` build is the safer artefact. Note that the host's
Rust is Arch's `rust` package (1.97.1) without rustup, and **only the gnu target is installed**. A musl build needs
the `rust-musl` package (root) or rustup in $HOME, or building in a container, or `cargo zigbuild`.
(Test artefact left: `target/release/jaunt-hello`, a few hundred KB.)

## Limitations vs. real networks (be careful drawing conclusions)
* **NAT is all Linux conntrack.** Real home routers (Broadcom/Realtek firmware, ISP boxes) differ in port allocation
  (sequential, random, port-preserving), in mapping lifetime (often 30–60 s for UDP and sometimes shorter) and in
  hairpinning. None of these is modelled beyond the two Linux modes and the timeout sysctls.
* **No CGNAT / double NAT.** There is one NAT layer per site. Real CGNAT (100.64/10) adds a second, often
  endpoint-dependent or port-block-allocating NAT with small port quotas. This can be approximated by putting the
  `hard` router behind another router, which is not built (limit of 7 containers).
* **No overlapping private subnets.** Real homes all use 192.168.1.0/24 and Docker refuses overlapping networks.
  So a roaming laptop never sees "same LAN prefix, different network", and a peer's private address never collides
  with a local one. jaunt must not assume that a matching private IP is the same LAN.
* **No hairpin/LAN-local case.** Two machines on the same LAN behind the same router are not in the topology (c could
  be roamed onto lanA to test it, since `jh roam c lanA` works).
* **No captive portals, DNS hijacking, TLS-intercepting middleboxes, MTU/PMTU problems (all links are 1500), or
  TCP-only / port-443-only egress** (hotel and corporate Wi-Fi). `udpblock` is the nearest: UDP is allowed only to the
  relay. A true "only TCP 443 out" mode would need an extra FORWARD rule.
* **No IPv6.** Many real networks (mobile, some ISPs) give working IPv6 that makes direct paths trivial, or give
  IPv6-only with NAT64. Neither is tested.
* **Latency is deterministic netem** with no jitter and no reordering (optional loss% only). Wi-Fi jitter and
  bufferbloat are absent.
* **The internet is one L2 segment.** Routers can ARP each other. Packets to a router's public IP never cross a real
  routed path, and there is no TTL or ICMP behaviour from intermediate hops.
* **freeze ≠ suspend.** `docker pause` freezes processes, but CLOCK_MONOTONIC keeps running (on a real suspend
  monotonic stops and only BOOTTIME advances), so timers fire immediately on thaw. The kernel network stack is not
  frozen either: TCP ACKs and ICMP would still be answered, which is why freeze also blackholes by default.
  The NAT state on the router is not cleared (a real router may have expired the mapping. Wait longer than
  NAT_UDP_TIMEOUT to get that).
* **drop/roam happen without Wi-Fi association events or DHCP.** There are no netlink link-change storms of the kind
  a laptop produces, and roam is instant.
* **No real relay or DERP/QUIC specifics.** The slot only runs sshd and the toy introducer. The real iroh-relay (TLS
  cert for `relay.jaunt.test`, HTTPS port, QUIC address discovery port) is installed later with `jh install`. Its
  UDP ports are reachable from every NAT mode, including udpblock (all traffic to the relay IP is allowed).
* Host netfilter: `br_netfilter` is not loaded on this host, so bridged traffic bypasses host iptables. If it gets
  loaded, Docker's rules apply to intra-bridge traffic too, which should still be allowed but has not been tested.

## Files
* `Dockerfile`: jaunt-harness:latest (archlinux + iproute2, iptables-nft, nftables, conntrack-tools, tmux,
  openssh, socat, iputils, curl, procps-ng, python, bind (dig), less)
* `lib.sh`: names, subnets, IPs (one place)
* `up.sh`, `down.sh`, `jh`, `prove.sh`
* `node/router.sh`, `node/machine.sh`: run inside containers
* `node/rendezvous.py` (introducer), `node/punch.py` (hole-punch client), `node/keystroke_rtt.py` (pty echo timer)
