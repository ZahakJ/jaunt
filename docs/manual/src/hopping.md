# Hopping

A hop is a terminal on another machine. Everything in this chapter assumes at least two linked machines; the examples sit at benzene and hop to toluene.

## The mesh view: `jaunt`

`jaunt` with no arguments, on a terminal, opens the live mesh view: this machine on the left, a line to every other member, and an event log underneath. The style of a line is its path: solid green for direct, dotted amber through the relay, dim for offline. Each line carries the latency of the last minute as a small sparkline.

| Key | Does |
| --- | --- |
| arrows, or `j` / `k` | choose a machine |
| Enter | hop to it |
| `l` | link a new machine |
| `?` | help |
| `q` | quit |

When the output is not a terminal (piped into another command), `jaunt` behaves like `jaunt ls`.

## Hop: `jaunt toluene`

```sh
jaunt toluene
```

This opens the tmux session called `jaunt` on toluene, creating it the first time. A one-line banner says how you got there, for example `benzene ━━▸ toluene · direct · 0.8 ms`, and you are in.

- **Same session every time.** Leave and come back, and you are on the same screen with the same programs running. The session lives in tmux on toluene; it outlives the connection, the client and the daemon.
- **Other sessions.** `jaunt toluene:work` opens or creates a session called `work`, separate from the default one.
- **Prefixes.** Any unambiguous start of a name works: `jaunt tol` is `jaunt toluene`. If a prefix matches two machines, jaunt lists them and asks you to type more of the name.
- **`jaunt hop toluene`** is the same command spelled out.

A hop needs a terminal. To run something from a script, use `jaunt run`.

## The other commands

| Command | Does |
| --- | --- |
| `jaunt ls` | every machine: direct, relayed or offline, latency, host state, sessions |
| `jaunt ls --json` | the same as JSON. Piped, plain `jaunt ls` prints tab-separated columns: name, state, latency, host, sessions, id, with this machine first as `self` |
| `jaunt run toluene -- uname -a` | run one command on toluene; its output and exit status come back |
| `jaunt cp notes.txt toluene:docs/` | copy a file or a directory. Either side can be `machine:path`; remote paths are relative to the home directory there |
| `jaunt fwd toluene 8080` | make toluene's `127.0.0.1:8080` available here at `127.0.0.1:8080`, until Ctrl-C |
| `jaunt fwd toluene 8080:9090` | the same, but here at port 9090 |
| `jaunt pipe toluene [port]` | connect stdin and stdout to a port on toluene's loopback (default 22) |

`fwd` reaches only toluene's loopback. The local end listens on `127.0.0.1`, so every user of this machine can use the port while it runs, as with `ssh -L`.

`jaunt pipe` carries OpenSSH. Where sshd listens on loopback, this in `~/.ssh/config` sends `ssh`, `scp`, `rsync` and `git` through jaunt, with SSH's key check as a second layer:

```
Host benzene toluene xylene phenol
    ProxyCommand jaunt pipe %h
```

The `Host` names must be jaunt machine names.

## Onward hops

Every machine is a full member, so you can hop from where you are. Inside a hop on toluene, `jaunt xylene` goes on from toluene to xylene. The shell in a hop has two variables set:

| Variable | Holds |
| --- | --- |
| `JAUNT_FROM` | the machine you came from |
| `JAUNT_MACHINE` | the machine you are on |

## Getting out and back

The escape keys are read by jaunt itself, before any tmux sees them, so they always work. They start with Enter, as in OpenSSH.

| You want to | Do | Session on the other machine |
| --- | --- | --- |
| leave a hop | Enter, then `~`, then `.` | kept |
| see the escape keys | Enter, then `~`, then `?` | |
| type a literal `~` at the start of a line | Enter, then `~`, then `~` | |
| detach from the remote tmux (not nested) | Ctrl-b, then `d` | kept |
| go back | `jaunt toluene` (same session, same screen) | |
| use a second session there | `jaunt toluene:work` | separate session |
| end the session for good | type `exit` in the remote shell | ended |
| quit the mesh view | `q` | |
| stop linking | Ctrl-c (or `n` at the question) | |

On the way out a banner says where you are again: `◂━━ back on benzene · session kept on toluene`.

## When the link drops

A change of network (Wi-Fi to a phone hotspot) is absorbed by QUIC: the connection moves to the new path, and typing resumes within about half a second. Nothing on your screen changes except the path in `jaunt ls`.

When the connection is really lost (the lid closed, the Wi-Fi gone), jaunt keeps your terminal and shows one status line at the bottom:

```
toluene ╌ link lost · retrying in 4s · ~. to give up
```

It tries again after 1 second, then 2, 4, 8, and every 10 seconds after that, and re-attaches by itself when toluene is back; tmux then repaints the screen as it was. `~.` gives up at any time, and the session stays on toluene.

There is a second, quieter line. If you type and nothing at all (no output, no acknowledgement, no keep-alive) has come back from toluene for 3 seconds, the bottom row says:

```
toluene ╌ no answer for 5s · waiting for the link · ~. to give up
```

It disappears as soon as output arrives. A program that is simply quiet on a live link, such as a password prompt, does not trigger it.

Without tmux on the far machine, a hop is a plain login shell, and it ends when the link drops. `jaunt check` on that machine warns about it.

## Using jaunt from tmux

A hop from inside tmux works without setup. The snippet below adds a popup picker that opens each machine in its own window, and a key that hands your keys to the tmux on the other machine.

{{#include ../../tmux.md:2:}}
