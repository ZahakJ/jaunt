# Troubleshooting

This chapter is organised by what you see. Most answers start with the same command:

```sh
jaunt check
```

It prints one line per finding, each starting with `ok`, `warn`, `fail` or `info`, and exits with status 1 if any line is `fail`. It never starts the daemon, so it describes the machine as it is. Run it on both ends of a problem.

The quoted lines below are what `check` prints, word for word; `…` stands for the part that changes.

## The daemon is not running

`check` says:

```
warn	the daemon is not running (any jaunt command starts it; 'jaunt up' installs the service)
```

Any other `jaunt` command starts the daemon when it finds none, and says `jaunt: started the daemon (pid …, log …)`. Without the user service, nothing starts it again at the next login or after a reboot; `jaunt up` installs the service. Two more lines point the same way:

```
warn	no systemd user service: the daemon does not start at login ('jaunt up' installs it)
warn	systemd user service jaunt.service is … and … ('jaunt up' enables and starts it)
```

If the daemon is running but is an older binary than the command (after an upgrade), the daemon line ends with `a different version from this command: restart it with 'jaunt down'`. Run `jaunt down`, then `jaunt up`.

If starting fails, the client says `the jaunt daemon did not come up; see …/daemon.log`. That log is the place to look (see [Where the logs are](#where-the-logs-are)).

## The relay does not answer

```
fail	no relay configured: other machines cannot find this one (jaunt relay set URL)
fail	relay … does not answer: check the URL, DNS and that its HTTPS port is reachable
fail	not connected to any relay: other machines cannot reach this one
```

The first means this machine has never been given a relay: `jaunt relay set URL` on the first machine, or `jaunt join --relay URL` on a new one. For the other two, check the relay itself from any computer:

```sh
curl -fsS https://relay.example.net/ping
```

If that fails, the problem is the relay host, its DNS name or its firewall (443/tcp must be open), not jaunt. The [relay chapter](relay.md) lists the ports. A healthy relay gives `relay … answers its web probe (… ms)` and `relay connected: other machines can reach this one`.

While the relay is down, machines with a direct path keep working (a daemon remembers its peers' last direct addresses); new machines cannot link, and relayed-only machines are unreachable.

## UDP is blocked and every path is relayed

```
warn	UDP address discovery got no answer (relay UDP port 7842 blocked, or UDP blocked here): direct paths through NAT are unlikely
warn	no direct path to any machine: everything goes through the relay
```

Machines learn their public address from the relay over **UDP port 7842**; without it they cannot punch a direct path. jaunt still works, through the relay. Check, in order:

1. **The relay's port.** 7842/udp must be open in the provider's firewall and on the host. A relay behind a Cloudflare tunnel never receives UDP: that is the expected result of that option, not a fault.
2. **This network.** Some networks block outgoing UDP. Run `check` from another network to compare.
3. **The kind of NAT.** `this network's NAT uses a different public port per destination (hard NAT): direct paths are unlikely and traffic will go through the relay` means the router here makes direct paths unlikely even with UDP open.

`check` then also mentions port mapping: `portmap = true` in `config.toml` lets jaunt ask the router (UPnP, NAT-PMP, PCP) to forward a UDP port. It is off by default because it changes your router's settings.

## A machine shows offline

`jaunt ls` says `offline` for toluene, and `check` prints a `warn` line for it, with the last error in brackets. Go through these:

- **It is asleep.** A laptop with the lid closed suspends unless host mode holds it awake, and jaunt does not wake machines. While a machine is online, `jaunt ls` shows its host state; `will sleep on lid close` or `on battery` mean exactly that.
- **Its daemon is not running.** After a reboot without the user service, nothing starts the daemon until a `jaunt` command runs there. `jaunt up` on that machine fixes this for good.
- **It cannot reach the relay**, and has no direct path to you. Run `jaunt check` on it.

## "link lost · retrying"

```
toluene ╌ link lost · retrying in 4s · ~. to give up
```

The connection is gone. jaunt keeps your terminal, retries after 1, 2, 4 and 8 seconds and then every 10, and re-attaches when toluene answers; the tmux session there is untouched. Wait, or press `~.` and come back later with `jaunt toluene`. If it never returns, see the section above.

## A slow recovery after an outage

Moving to a new network right after an outage (in the lab: onto a hard NAT, straight after a 60-second drop) can leave a hop frozen with the line `no answer for …s · waiting for the link` for much longer than usual: 17 and 49 seconds in two lab runs. This is a known limit of iroh 1.3: after an outage it backs off reconnecting to the relay for up to about 16 seconds plus jitter, and a network change does not cut that short. jaunt cannot shorten the wait; the hop recovers by itself.

## The first hop after sleep waits a few seconds

The first hop, command or copy into a machine that has just come back from sleep (or a restart, a freeze, or an outage of a minute or more) can wait up to 5 seconds before anything happens. The next ones do not.

This is deliberate. A machine that was away may have missed a revocation, so before serving a shell, command, port or copy it first tries to sync with the other members, for at most 5 seconds, once per absence. The rule is called settling; [Revoking a lost laptop](security.md#revoking-a-lost-laptop) explains the window it leaves.

## tmux is missing on the host

```
warn	tmux is not installed: hops to this machine get a plain shell that ends when the link drops
```

The hop still works, but with no tmux to keep the session, closing the link ends the shell and whatever ran in it. Install tmux on the machine you hop *into*. With tmux there, the line is `tmux at … (sessions here survive disconnects)`.

## The lid closes and the laptop sleeps anyway

Host mode holds the laptop awake only while every condition holds; anything else means sleep, on purpose. `jaunt host status` on the laptop says what is true now, what closing the lid would do, and what is missing. The usual causes:

| Cause | What you see, what to do |
| --- | --- |
| on battery, or a charger too weak to keep up | released after 10 s on battery, or after the battery has drained for 60 s on a weak USB-C charger. Use a proper charger |
| too hot | released after 120 s at 90 °C or more, or at once within 5 °C of a sensor's critical point. It stays released until the lid is opened |
| power or temperature unreadable | unknown means sleep |
| no local login since boot | `no local graphical session: logind refuses the lid lock until you log in at this machine once after each boot, or until scripts/root-extras/host-polkit-rule.sh is installed` |
| the daemon was started from an SSH login | `this is an SSH login and the jaunt service is not installed: a daemon started from here belongs to the SSH session and logind refuses it the lid lock; run 'jaunt up' so it runs as a user service` |
| plugged back in with the lid closed | it stays asleep until the lid is opened; jaunt does not wake machines |

The full rules are in [Host mode](awake.md). Remember that host mode has not yet been run on a real laptop.

## `jaunt join` says the code is wrong or expired

Codes work once, for 5 minutes, a wrong guess burns them, and each joining machine tries a code only once. The fix is always a fresh code from `jaunt link`. The exact messages are in [When linking fails](pairing.md#when-linking-fails).

## Two contexts disagree about `XDG_RUNTIME_DIR`

The client finds the daemon through a Unix socket at `$XDG_RUNTIME_DIR/jaunt/sock` (or under `~/.local/state/jaunt/run/` when that variable is not set). A desktop session and an SSH login can see different values, and so look in different places.

jaunt handles this with a lock file, `daemon.lock`, next to the key in `~/.local/share/jaunt/`. Only one daemon per key can hold it, and it records that daemon's socket path. A client that finds no daemon at its own socket path tries the recorded one before starting a daemon, so a second context reaches the running daemon instead of starting another. A client only talks to a socket in a directory it owns, served by a process of the same user.

## Where the logs are

| What | Where |
| --- | --- |
| the daemon's log, when it was started by a `jaunt` command | `~/.local/state/jaunt/daemon.log` (rotated to `daemon.log.1` past 1 MiB) |
| the daemon's log, under the systemd user service | the user journal: `journalctl --user -u jaunt` (the real systemd user service was never run in the tests) |
| the event history (handshakes, path changes, syncs, links) | `~/.local/state/jaunt/events.jsonl`, also shown live in the mesh view |
| every address the daemon has talked to | the end of `jaunt check`; anything that is not the relay or a linked machine is a `warn` |

With `JAUNT_HOME` set, all of these live under `$JAUNT_HOME/state/` instead.
