# Hosting the relay

A mesh uses one relay: `iroh-relay` 1.3.0, unmodified. It introduces machines, tells a
machine behind NAT its public address so two machines can punch a direct path, and
carries traffic when no direct path exists. It sees which ids are online, from which
addresses, who talks to whom and how much; it cannot read or alter traffic and cannot
join the mesh.

| Port | For |
| --- | --- |
| 443/tcp | HTTPS + WebSocket at `/relay`, with its own certificate |
| 7842/udp | QUIC address discovery. Without it, machines behind NAT never go direct |
| 80/tcp | Let's Encrypt HTTP challenge and `/generate_204` (not needed with a manual certificate) |

On the first machine: `jaunt relay set https://relay.example.net`. Other machines get it
through the roster, or from `install.sh --relay URL` / `jaunt join --relay URL`.
`jaunt check` reports whether the relay answers and whether UDP address discovery works.
Changing relay later is `jaunt relay set NEW_URL`; keep the old one running until every
machine has picked it up.

Files: `Dockerfile` (builds iroh-relay 1.3.0 in debian:12, runs it in debian:12-slim as an
unprivileged user), `relay.toml` (Let's Encrypt, address discovery on), `fly.toml`,
`desktop-tunnel.sh`.

| Where | Cost | Direct paths across networks |
| --- | --- | --- |
| a small VPS | a few dollars a month | yes |
| a home machine with 443/tcp and 7842/udp forwarded | free | yes; the home address is public |
| a home machine behind a Cloudflare tunnel | free | no; everything away from home is relayed |
| Fly.io | dedicated IPv4 is paid | no, as shipped |

## VPS

Any Linux VPS with a public IPv4 address.

1. Point a DNS name at it (A, plus AAAA with IPv6). With Cloudflare DNS, set the record to
   "DNS only", not proxied.
2. Open 80/tcp, 443/tcp and 7842/udp.
3. Copy this directory over, set `hostname` and `contact` in `relay.toml`, then:

   ```sh
   docker build -t jaunt-relay .
   docker volume create jaunt-relay-certs
   docker run -d --name jaunt-relay --restart unless-stopped \
     -p 80:80/tcp -p 443:443/tcp -p 7842:7842/udp \
     -v jaunt-relay-certs:/var/lib/iroh-relay jaunt-relay
   curl -fsS https://relay.example.net/ping
   ```

   Without Docker: `cargo install iroh-relay@1.3.0 --features server --locked`, the same
   config, and a systemd unit with `AmbientCapabilities=CAP_NET_BIND_SERVICE`.

## Home machine, ports forwarded

The VPS setup on an always-on home machine. Needs a public IPv4 address (not carrier-grade
NAT), forwarding of 80/tcp, 443/tcp and 7842/udp, and a DNS name kept pointing at the home
address. Machines at home reach the relay through the public address, so the router must
support hairpin NAT.

## Home machine, Cloudflare tunnel

The relay runs in plain HTTP on loopback and an existing Cloudflare tunnel carries it.
A Cloudflare hostname carries no UDP, so address discovery fails and sessions between
machines on different networks are relayed through Cloudflare and the home machine.

```sh
sh deploy/relay/desktop-tunnel.sh install      # user service on 127.0.0.1:3340; `undo` removes it
```

Then add a public hostname to the tunnel (Zero Trust dashboard: Networks > Tunnels >
Public Hostname): `relay.example.net`, service `HTTP`, `127.0.0.1:3340`. Check with
`curl -fsS https://relay.example.net/ping`. Cloudflare terminates TLS, so it sees relay
metadata, never content.

A mesh can list two relays (`jaunt relay set URL1 URL2`), e.g. a VPS first and a tunnel
relay second; iroh picks each machine's home relay by latency.

## Fly.io

Fly delivers UDP only to a socket bound to the address `fly-global-services` resolves
to; iroh-relay 1.3.0 binds `quic_bind_addr` to a literal address, so it receives no UDP
and nothing goes direct. Working around it needs a start script that resolves
`fly-global-services` into `quic_bind_addr` and a dedicated IPv4 (`fly ips allocate-v4`).
`fly.toml` is a starting point.

## Access

Leave `[access]` at its default. An id allowlist breaks pairing (pairing endpoints have
ids nobody knows in advance), and `access.shared_token` is not supported.

Anyone who knows the URL of an open relay can use its bandwidth and occupy pairing
nameplates (pairing fails, nothing leaks); ids grant nothing inside the mesh.
