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

Files here: `Dockerfile` (builds iroh-relay 1.3.0 from crates.io in debian:12, runs it in
debian:12-slim as an unprivileged user), `relay.toml` (Let's Encrypt, address discovery on),
`fly.toml`.

## Option 1: a small VPS (recommended)

Any Linux VPS with a public IPv4 address. The smallest plan at the usual providers is
enough (the relay idles at about 20 MB of RAM); expect roughly 4 to 6 US dollars a month.
Prices change; check the provider's page. Traffic is small unless most sessions are
relayed.

1. Point a DNS name at the VPS (A record, plus AAAA if it has IPv6): `relay.example.net`.
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

## Option 2: Fly.io (not recommended)

**Fly.io is not recommended for this relay, and the files here do not make it work.** On
Fly, UDP reaches an app only on a socket bound to the address of `fly-global-services`
(inside the machine), and replies must leave from that same socket. iroh-relay 1.3.0
binds its QUIC address-discovery socket to the literal `quic_bind_addr` in its config; it
cannot be given a host name. As shipped (`[::]:7842`) the relay gets no UDP on Fly at all,
so address discovery fails and **every session between machines behind NAT is relayed**.

The only way to bind it there would be a start script that resolves
`fly-global-services` inside the machine and writes that address into `quic_bind_addr`
before starting iroh-relay. That is not done here and has not been tried. Community
reports (Aug–Sep 2026) also describe UDP replies leaving from a different IP and packets
not arriving, either of which breaks address discovery even when bound correctly, and
Let's Encrypt through Fly's raw TCP services was not tried either.

`fly.toml` is kept as a starting point for someone who wants to do that work (a dedicated
IPv4, `fly ips allocate-v4`, is needed for UDP at all). If you try it, check it the same
way as any relay: `jaunt check` on two machines behind different NATs must report that
UDP address discovery works, and `jaunt ls` must show them `direct`. If they stay
`relay`, it is not working. Use a VPS (Option 1) instead.

## Option 3: the desktop, behind the existing Cloudflare tunnel (fallback only)

Run the relay in plain HTTP on the desktop and let the tunnel carry it. Costs nothing
extra. **Machines behind NAT will not find direct paths through it**: address discovery
needs UDP to the relay's own host name, and a Cloudflare hostname carries no UDP. Every
session between two NATed machines is relayed through Cloudflare's edge and the desktop;
a machine on the desktop's own LAN, or one with a public IPv6 address, may still go
direct.

Relay config (`relay-http.toml`, not in this directory, because it is three lines):

```toml
http_bind_addr = "127.0.0.1:3340"
enable_metrics = false
enable_quic_addr_discovery = false
```

Run `iroh-relay -c relay-http.toml` (as a user service), and add an ingress rule to the
tunnel: `hostname: relay.example.net` → `service: http://localhost:3340`. Cloudflare
terminates TLS, so Cloudflare sees relay metadata (ids, timing, sizes), never content.
This project does not touch the tunnel; the rule is yours to add and remove.

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
