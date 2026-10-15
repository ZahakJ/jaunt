# Introduction

jaunt lets you sit at any of your own machines and open a terminal on any other one. Type `jaunt toluene` and you are in a tmux session on the machine called toluene. The connection is a direct, encrypted link between the two machines when one can be made, and goes through a relay you run when it cannot. Leave and come back later: the session lives in tmux on the far machine and is where you left it.

![The jaunt mesh view: benzene on the left with solid green lines to toluene and xylene (direct), a dotted amber line through the relay to phenol, styrene greyed out as offline, and the event log underneath](../captures/mesh-100x28-truecolor.png)

*`jaunt` with no arguments: the live mesh. A solid green line is a direct path, a dotted amber one goes through the relay. The picture is `jaunt demo`.*

## Overview

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

The machine names in this manual (benzene, toluene, xylene, phenol, styrene) are the demo machines.

The source is at [github.com/ZahakJ/jaunt](https://github.com/ZahakJ/jaunt). MIT licensed.
