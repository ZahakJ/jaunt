# Introduction

jaunt lets you sit at any of your own machines and open a terminal on any other one. Type `jaunt toluene` and you are in a tmux session on the machine called toluene. The connection is a direct, encrypted link between the two machines when one can be made, and goes through a relay you run when it cannot. Leave, come back later, and the session is exactly where you left it, because it lives in tmux on the far machine.

![The jaunt mesh view: benzene on the left with solid green lines to toluene and xylene (direct), a dotted amber line through the relay to phenol, styrene greyed out as offline, and the event log underneath](../captures/mesh-100x28-truecolor.png)

*`jaunt` with no arguments: the live mesh. A solid green line is a direct path, a dotted amber one goes through the relay. This picture is `jaunt demo`, made-up machines on made-up data, which you can run before setting anything up.*

## What it is, in one screen

- **One static binary per machine**, in `~/.local/bin`. No root, no accounts, no system VPN device, no third-party service.
- **A small background service on each machine** (the daemon) holds that machine's own key. Every other `jaunt` command is a thin client that talks to it over a private Unix socket.
- **The network and the encryption come from the [iroh](https://iroh.computer) library**, not from jaunt: QUIC with TLS 1.3, dialling a machine by its public key, punching a direct path through home routers, and falling back to a relay. jaunt writes no transport and no cryptography of its own.
- **jaunt adds** the linking ritual (a short spoken code, then a picture and three words to compare), the shared member list and revocation, the hop commands, the mesh view, and a guard that keeps a laptop awake with the lid closed only while that is safe.
- **The relay** is one small program, `iroh-relay`, that you host. It introduces your machines to each other and carries traffic when no direct path exists. It sees only encrypted data and can never add a machine.

```
 your terminal                                               the other machine
 jaunt toluene ── daemon ════ QUIC + TLS 1.3 ════ daemon ── tmux session
                     │   direct when possible        │
                     └──── else through the relay ───┘
```

The names in this manual (benzene, toluene, xylene, phenol, styrene) are the demo machines. Read them as "your desktop", "your laptop" and so on.

## Before you trust it

> **Keep your existing SSH path for now.** jaunt is new, has had no outside security review, and has only been tested in a simulated network lab. Leave the way you reach your machines today (for example SSH through a Cloudflare tunnel) in place until jaunt has proven itself on your real machines.

What that means in detail:

- **Tested only in simulation.** A Docker lab with three machines behind simulated home routers passes 250 end-to-end checks. No real laptop, lid, relay server, home network or Mac has run it. See [What was tested](testing.md) and the first real-world test checklist in the same chapter.
- **No outside audit.** Not of jaunt, not of the iroh library under it, and not of the SPAKE2 crate the pairing uses. A stolen, unlocked laptop is a full member until you revoke it. See the [Security model](security.md).
- **Unsigned releases.** The installer checks the binary against a checksum that comes from the same place as the binary. That catches a broken download, not a malicious release.
- **Speed.** In the lab a keystroke took 81 ms on jaunt's direct path against 121 to 139 ms for SSH through a relay host, with 20 ms of simulated delay per router. The gain is the direct path; through the relay jaunt is no faster than SSH.

## When Tailscale is the better choice

The research behind jaunt says it plainly. Tailscale's free plan plus tmux (and mosh, for typing over bad links) gives the same speed win in about fifteen minutes, on a WireGuard data plane that is formally verified and maintained by a company whose job it is, with phone, Windows and macOS clients and a better rate of direct paths. Choose Tailscale if you want an audited, mature tool now, need phones or other operating systems, or do not mind root and an account. Turn on Tailnet Lock so Tailscale's servers cannot add machines.

jaunt makes sense if you want no root, no system VPN device, no account and no company that can add machines to your network; a pairing ritual instead of a control plane; and the hop view, onward hops and lid-awake behaviour built in. It is not more mature than WireGuard and does not claim to be.

## How this manual is laid out

The chapters follow the order you meet things in:

1. [Install](install.md) the binary, then run `jaunt demo` to see what it looks like.
2. Set up [the relay](relay.md), then [pair](pairing.md) your machines.
3. [Hop](hopping.md) between them, from a plain terminal or from inside tmux.
4. Keep a laptop reachable with its lid closed: [Host mode](awake.md).

After that come the reference chapters: the [Security model](security.md), the [Internals](internals.md), [Troubleshooting](troubleshooting.md) organised by symptom, [What was tested](testing.md), the [Decisions](decisions.md) made during the build and the ones left to you, and the [Changelog](changelog.md).

Several chapters include the repository's own documents word for word, so the manual and the repository say the same thing. The source is at [github.com/ZahakJ/jaunt](https://github.com/ZahakJ/jaunt), and the [jaunt home page](../) has the short version.

jaunt is MIT licensed.
