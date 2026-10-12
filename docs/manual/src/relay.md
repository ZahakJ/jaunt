# The relay

Every jaunt mesh uses one relay, and you host it yourself. It is the one piece of jaunt that has to sit somewhere both of your machines can reach, which on today's internet usually means somewhere other than behind your home router.

## What it does

A machine behind a home router does not know its own public address, and nothing outside can dial it. The relay solves both problems:

1. **It introduces machines.** Each daemon keeps a connection to the relay. When benzene wants toluene, it asks for toluene by its public key, and the relay passes the first packets along.
2. **It tells each machine its public address**, over UDP port 7842. Two machines that know their public addresses can then punch a direct path through their routers. Once that works, traffic goes straight between them and the relay drops out of the picture.
3. **It carries the traffic** when no direct path can be made, for example when one side is behind a hard NAT or UDP is blocked.

The program is `iroh-relay` 1.3.0, unmodified. jaunt adds nothing to it.

## What it sees, and what it cannot do

The relay sees which machine ids are online, from which IP addresses, who talks to whom, when, and how much. It cannot read or change the traffic: that is QUIC with TLS 1.3 end to end between the machines' own keys, and the relay only forwards the encrypted packets. It cannot add a machine to your mesh, because every machine refuses keys that are not on its member list.

Anyone who learns the relay's URL can use it to reach machine ids they already know, which gets them nothing inside the mesh, and can occupy a pairing slot, which makes a pairing fail but leaks nothing. The relay is left open on purpose; the access section below explains why an allowlist would break pairing.

## Choosing where it runs

One relay serves every machine you will ever link, and it is small (about 20 MB of memory). What you choose decides whether machines on different networks get a direct path.

| Where it runs | Cost | What you get |
| --- | --- | --- |
| your desktop, behind a Cloudflare tunnel you already have | free | works everywhere. Away from home, sessions go through the relay (about as fast as SSH through the tunnel); at home a direct path is expected |
| a small VPS | a few dollars a month, or a free tier | direct paths from anywhere: the fast case |
| your desktop, with two ports forwarded on the home router | free | direct paths from anywhere, but your home address becomes public |
| Fly.io | a paid add-on needed | **no direct paths as shipped**: Fly delivers UDP only to a specially bound socket, and the relay cannot bind that way without a start script nobody has written or tested |

A sensible order is to start with the first row, which costs nothing and exposes nothing new, and move to a VPS when you want direct paths away from home. Switching later is one command, `jaunt relay set NEW_URL`, which every online machine picks up.

**None of these has been deployed as part of this project.** The lab ran the same `iroh-relay` 1.3.0 with a throwaway certificate; no real VPS, Let's Encrypt certificate, Dockerfile build, Fly.io app or Cloudflare-tunnel relay has been tried.

## Checking it

After `jaunt relay set URL` on the first machine, `jaunt check` reports on the relay. The lines to look for:

- `relay https://relay.example.net answers its web probe (… ms)`: the relay answers over HTTPS.
- `relay connected: other machines can reach this one`
- `UDP address discovery through the relay works; public address …`: UDP port 7842 gets through, so direct paths are possible.

If the last one is a warning instead, see [Troubleshooting](troubleshooting.md).

## Hosting notes

The rest of this chapter is `deploy/relay/README.md` from the repository. It names files (`Dockerfile`, `relay.toml`, `fly.toml`, `desktop-tunnel.sh`) that live in the [`deploy/relay/`](https://github.com/ZahakJ/jaunt/tree/main/deploy/relay) directory of the repository, and its commands are meant to be run from a checkout of it.

{{#include ../../../deploy/relay/README.md:2:}}
