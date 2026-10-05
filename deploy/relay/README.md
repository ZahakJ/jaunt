# Hosting the jaunt relay

Every machine in a mesh uses one relay. It introduces machines to each other, tells a
machine behind NAT its public address (so two machines can punch a direct path), and
carries the traffic when no direct path exists. It is `iroh-relay` 1.3.0, unmodified.

It sees which machine ids are online, from which IP addresses, who talks to whom, when and
how much. It cannot read or alter traffic (QUIC/TLS 1.3 end to end between the machines'
own keys) and it cannot join the mesh. Nothing here is deployed by this project.

What it needs:

| Port | For |
| --- | --- |
| 443/tcp | the relay itself: HTTPS + WebSocket at `/relay`, with its own certificate |
| 7842/udp | QUIC address discovery. **Without it machines behind NAT never go direct**; everything is relayed |
| 80/tcp | Let's Encrypt HTTP challenge and `/generate_204` (not needed with a manual certificate) |

Once it runs, on the first machine: `jaunt relay set https://relay.example.net` (every
other machine gets it through the roster, or from `install.sh --relay URL` /
`jaunt join --relay URL`). Check it with `jaunt check`; it reports whether the relay
answers and whether UDP address discovery worked.

## Do I have to pay? Which option?

You need **one** relay for the whole mesh, however many machines you have. It is a small
program (about 20 MB of memory) and it can run for free. What you choose decides whether
machines on different networks get a direct path or go through the relay.

| Where the relay runs | Cost | Direct paths between different networks | New exposure |
| --- | --- | --- | --- |
| **A. Your desktop, behind the Cloudflare tunnel you already have** | free | **no**: away from home every session is relayed (Cloudflare, then the desktop), about as fast as SSH is today. On the same LAN a direct path is expected (untested) | none |
| **B. Your desktop, with two ports forwarded on the home router** | free | yes | your home IP address is published in DNS and the router forwards 443/tcp and 7842/udp to the desktop |
| **C. A small VPS** (recommended) | about 4 to 6 US dollars a month, or nothing on a provider's free tier | yes | none at home |
| **D. Fly.io** | a dedicated IPv4 address is a paid extra | **not as shipped** (see below) | none at home |

A sensible order: start with **A** (free, nothing new exposed, good enough to try jaunt and
to use it at home), and move to **C** when you want direct paths from outside. Changing
relay later is one command (`jaunt relay set NEW_URL`); every machine that is online picks it
up, so keep the old relay running until `jaunt ls` has shown each machine once.

Why the difference: a machine behind a home router does not know its own public address.
The relay tells it, over **UDP port 7842**, and two machines that know their public
addresses can then punch a direct path. A relay that cannot receive UDP from the internet
(option A: a Cloudflare hostname carries only HTTP) still relays, but nobody learns their
public address, so nothing goes direct across networks.

Nothing here has been deployed or run on a real server; see "What was tested" at the end.

Files here: `Dockerfile` (builds iroh-relay 1.3.0 from crates.io in debian:12, runs it in
debian:12-slim as an unprivileged user), `relay.toml` (Let's Encrypt, address discovery on),
`fly.toml`.

## Option C: a small VPS (recommended)

Any Linux VPS with a public IPv4 address. The smallest plan is enough. Traffic is small
unless most sessions are relayed.

1. Point a DNS name at the VPS (A record, plus AAAA if it has IPv6): `relay.example.net`.
   On Cloudflare DNS set the record to "DNS only" (grey cloud), not proxied.
2. Open 80/tcp, 443/tcp and 7842/udp in the provider's firewall and on the host.
3. Copy this directory to the VPS, edit `relay.toml` (`hostname`, `contact`), then:

   ```sh
   docker build -t jaunt-relay .
   docker volume create jaunt-relay-certs
   docker run -d --name jaunt-relay --restart unless-stopped \
     -p 80:80/tcp -p 443:443/tcp -p 7842:7842/udp \
     -v jaunt-relay-certs:/var/lib/iroh-relay jaunt-relay
   curl -fsS https://relay.example.net/ping   # answers once the certificate is issued
   ```

   The container runs as an unprivileged user; binding 80/443 inside it works because
   Docker publishes the ports. Without Docker: `cargo install iroh-relay@1.3.0 --features
   server --locked`, the same config, and a systemd unit with
   `AmbientCapabilities=CAP_NET_BIND_SERVICE`.

## Option B: the desktop, with ports forwarded on the home router

The same relay as option C, running on the always-on desktop. Free, and it gives direct
paths, but it undoes part of what the Cloudflare tunnel was for: the home IP address
becomes public and the router lets traffic in.

It needs: a public IPv4 address at home (not carrier-grade NAT; if the router's WAN address
differs from what a "what is my IP" site shows, this option is out); a router where you
can forward 80/tcp, 443/tcp and 7842/udp to the desktop; a DNS name pointing at the home
address, set to "DNS only", and kept up to date if the address changes. Then follow option
C's step 3 on the desktop (check `ss -ltn` first: 80 and 443 must be free there).

Not tried. One known catch: laptops at home reach the relay through the router's public
address, which only works if the router supports "NAT loopback" (hairpin NAT).

## Option D: Fly.io (does not work as shipped)

Fly.io is not ruled out for good; it needs work that has not been done or tested.

- **What goes wrong.** On Fly, UDP reaches an app only on a socket bound to a special
  internal address (the one the name `fly-global-services` resolves to inside the
  machine). iroh-relay 1.3.0 binds its UDP socket to the literal address in its config
  (`quic_bind_addr`, here `[::]:7842`) and cannot be given a name. So with the files here
  the relay on Fly receives **no UDP**. It still relays over HTTPS, but no machine learns
  its public address, and nothing goes direct: the same result as option A, which is free.
- **What would fix it.** A start script in the container that looks up
  `fly-global-services` and writes that address into `quic_bind_addr` before starting
  iroh-relay, plus a dedicated IPv4 address for the app (`fly ips allocate-v4`, a paid
  extra, required for UDP). That script is not written and has not been tried.
- **Why it may still not work.** Reports from August and September 2026 describe UDP
  replies on Fly leaving from a different address and packets not arriving; either breaks
  address discovery even with the right binding. Let's Encrypt through Fly's raw TCP
  services was not tried either.

`fly.toml` is kept as a starting point. If you try it, the test is the same as for any
relay: `jaunt check` on two machines behind different routers must say UDP address
discovery works, and `jaunt ls` must show them `direct`.

## Option A: the desktop, behind the existing Cloudflare tunnel (free)

Run the relay in plain HTTP on the desktop and let the tunnel carry it. Costs nothing. **Machines behind NAT will not find direct paths through it**: address discovery
needs UDP to the relay's own host name, and a Cloudflare hostname carries no UDP. Every
session between two NATed machines is relayed through Cloudflare's edge and the desktop;
a machine on the desktop's own LAN, or one with a public IPv6 address, may still go
direct.

`desktop-tunnel.sh` in this directory does the desktop side (no root, everything in your
home directory, `undo` removes it):

```sh
sh deploy/relay/desktop-tunnel.sh install      # relay as a user service on 127.0.0.1:3340
```

Then add a public hostname to one of the tunnels that already run on the desktop, in the
Cloudflare Zero Trust dashboard (Networks > Tunnels > the tunnel > Public Hostname):
`relay.example.net`, service `HTTP`, `127.0.0.1:3340`. Check with
`curl -fsS https://relay.example.net/ping`, then `jaunt relay set https://relay.example.net`.
Cloudflare terminates TLS, so Cloudflare sees relay metadata (ids, timing, sizes), never
content. The script does not touch the tunnel; the hostname is yours to add and remove.

Untested: WebSocket upgrade through the tunnel for `/relay` (expected to work; Cloudflare
proxies WebSockets), and how long Cloudflare keeps an idle WebSocket open.

A mesh can list two relays (`jaunt relay set URL1 URL2`): e.g. a VPS relay first and the
tunnel relay as a second way in. iroh picks each machine's home relay by measured latency;
a mesh with two relays was not tested.

## Access

By default anyone who knows the URL can use the relay to reach ids they already know. That
gives them nothing inside the mesh (every machine refuses keys not in its roster), but it
lets strangers use your bandwidth, and it lets anyone occupy a pairing nameplate: pairing
over an open relay can be denied by anyone who wants to (it fails; nothing leaks, and a
joining machine never tries the same code twice).

Leave `[access]` at its default. Do **not** use iroh-relay's id allowlist: pairing
(`jaunt link`/`join`) needs two temporary endpoints whose ids are random (the joiner's) or
derived from the code (the inviter's), so no list can name them in advance, and a new
machine's own id is not known before it links. With an allowlist on, linking new machines
breaks.

iroh-relay also has a shared-token mode (`access.shared_token`). **jaunt does not support
it**: jaunt does not send a token to the relay, so with that mode on every jaunt machine is
refused and nothing connects.

## What was tested

- The harness (`test/harness/relay.sh`) runs this same iroh-relay 1.3.0 with a manual
  certificate from a throwaway CA and address discovery on; the whole e2e suite runs
  through it, including the relayed path and relay outages.
- `relay.toml` was parsed by iroh-relay 1.3.0 (with the certificate switched to manual).
- Not tested: the Dockerfile build, Let's Encrypt, any real VPS, Fly.io, the Cloudflare
  tunnel.
