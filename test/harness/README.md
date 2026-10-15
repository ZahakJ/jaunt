# NAT lab

Docker-based and rootless. Three machines sit behind their own NAT routers on a simulated
internet that also holds the relay. The e2e tests in `test/e2e/` install the `jaunt`
binary into it and check direct and relayed paths, roaming, link drops and suspend.

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
  host DNS so iroh's default n0 relays and DNS discovery cannot be reached.
* Machines have only a LAN interface. The internet is reachable only through their router.
* All nodes resolve `relay.jaunt.test` → 198.51.100.10 (`--add-host`). Docker DNS does not resolve container names
  across networks, so machines cannot use the name `jaunt-relay`.
* If another Docker network already uses 198.51.100.0/24 or 10.231.0.0/16, `docker network create` refuses;
  the subnets are set in `lib.sh`.
* IPv6 is disabled in every container. The harness is IPv4 only.
* Containers get `NET_ADMIN`+`NET_RAW` (routers for NAT; machines so the harness can set routes, netem and blackhole
  rules inside them; jaunt itself runs as the unprivileged user `jaunt`). Memory
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
| `jh roam <m> <lanA\|lanB\|hard>` | `docker network disconnect`/`connect` with a new IP and a new default route. Processes keep running |
| `jh freeze <m> <s> [pause-only]` | blackhole + `docker pause`, then unpause and restore (suspend). `pause-only` skips the blackhole (see limitations) |
| `jh install <bin> [nodes]` | `docker cp` to `/opt/jaunt/bin/` (on PATH). Warns if `ldd` shows unresolved libs |
| `jh hardmode <cone\|symmetric\|udpblock\|both>` | re-configures router c live |
| `jh tunnel <m>` | SSH baseline, like a reverse tunnel: the machine keeps `ssh -N -R 220N:127.0.0.1:22 relay` open, so its sshd is at relay:127.0.0.1:220N (a=2201, b=2202, c=2203) |
| `jh sshvia <from> <to> [cmd]` | `ssh -J relay -p 220N jaunt@127.0.0.1` (ProxyJump through the relay) |
| `jh punch <m1> <m2> [port]` | UDP simultaneous-open hole punch through the introducer, with a fixed local port |
| `jh whoami <node> [port]` | the reflexive (post-NAT) address the relay sees |
| `jh nat <router>` | conntrack table (shows the NAT mappings) |
| `jh status` | containers and IPs |

SSH: user `jaunt` exists on the relay and machines. A throwaway ed25519 key is generated into `.state/` (deleted by
down.sh) and installed everywhere. Host keys are not checked inside the harness.

## Notes

* `./prove.sh` checks the NAT behaviour: cone routers preserve the source port, the symmetric router picks a new
  port per flow, unsolicited packets to a router are dropped, a↔b hole punching succeeds and a↔c fails with c in
  `symmetric`, `udpblock` or `both`, and SSH through the relay over b's reverse tunnel works.
* OpenSSH's `ObscureKeystrokeTiming` (default on, 20 ms chaff interval) adds about 0–20 ms to every interactive
  keystroke, so SSH baselines state which setting they used.
* If a router does not drop unsolicited WAN packets, a peer's packets arriving before the local side has sent create
  an UNREPLIED conntrack entry and the later outbound flow gets a different port, which breaks the punch. With the
  harness default (drop), the same sequence punches fine.
* Children of `docker exec ... &` die when the exec session ends; long-running daemons are started with
  `docker exec -d`.
* `tc ... netem` inside a container loads `sch_netem` on the host kernel without root.

## Not modelled

* NAT is Linux conntrack only: no vendor router quirks, CGNAT or double NAT.
* No overlapping private subnets, and no two machines behind the same router.
* No captive portals, DNS hijacking, TLS interception, MTU problems or TCP-443-only egress
  (`udpblock` is the nearest).
* No IPv6.
* Delay is fixed netem: no jitter or reordering (optional loss% only).
* The internet is one L2 segment.
* `freeze` is `docker pause`: CLOCK_MONOTONIC keeps running and router NAT state is kept,
  so freeze also blackholes by default.
* Drops and roams have no Wi-Fi association or DHCP events.
* Without `./relay.sh` the relay slot runs only sshd and the toy introducer.
* With `br_netfilter` loaded on the host, Docker's rules also apply to intra-bridge traffic.

## Files
* `Dockerfile`: jaunt-harness:latest (archlinux + iproute2, iptables-nft, nftables, conntrack-tools, tmux,
  openssh, socat, iputils, curl, procps-ng, python, bind (dig), less)
* `lib.sh`: names, subnets, IPs (one place)
* `up.sh`, `down.sh`, `jh`, `prove.sh`
* `node/router.sh`, `node/machine.sh`: run inside containers
* `node/rendezvous.py` (introducer), `node/punch.py` (hole-punch client), `node/keystroke_rtt.py` (pty echo timer)
