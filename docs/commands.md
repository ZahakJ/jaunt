# Commands

| Command | Does |
| --- | --- |
| `jaunt` | live mesh view. Arrows or `j`/`k` to choose, Enter to hop, `l` to link, `?` help, `q` quit |
| `jaunt toluene` | hop to toluene, session `jaunt`. Prefixes work: `jaunt tol` |
| `jaunt toluene:work` | hop to toluene, session `work` (created if needed) |
| `jaunt ls` / `jaunt ls --json` | machines, path, latency, host state, sessions. Tab-separated when piped |
| `jaunt run toluene -- uname -a` | run one command there; output and exit status come back |
| `jaunt cp notes.txt toluene:docs/` | copy a file or directory; either side can be `machine:path`. Remote paths are relative to the home directory there |
| `jaunt fwd toluene 8080:9090` | make toluene's `127.0.0.1:8080` available here at `127.0.0.1:9090` until Ctrl-C. Every user of this machine can use the forwarded port while it runs |
| `jaunt pipe toluene [port]` | connect stdin/stdout to a port on toluene's loopback (default 22); for `ProxyCommand` |
| `jaunt link` / `jaunt join` | add a machine |
| `jaunt unlink toluene` | revoke toluene (see [Revoking a lost laptop](security.md#revoking-a-lost-laptop)) |
| `jaunt leave` | take this machine out of the mesh and delete its key |
| `jaunt reset` | new key, empty roster (link again afterwards) |
| `jaunt name xylene` | rename this machine; the others see the new name |
| `jaunt relay` / `jaunt relay set URL [URL2]` | show or set the relay for the whole mesh. `--ca FILE` trusts a relay certificate from your own CA |
| `jaunt host on` / `off` / `status` | host mode ([awake.md](awake.md)) |
| `jaunt check` | diagnose: key permissions, daemon, relay, UDP, tmux, each peer's path, and a list of every address the daemon has talked to. Never starts the daemon |
| `jaunt up` / `jaunt down` | install and start / stop the user service |
| `jaunt demo` | the mesh view and pairing screens on made-up data |
| `jaunt tmux-setup` | install the tmux snippet ([tmux.md](tmux.md)) |
| `jaunt uninstall` | remove jaunt from this machine |

Inside a hop, the shell gets `JAUNT_FROM` (where you came from) and `JAUNT_MACHINE` (where you are). Typing `jaunt xylene` there hops onward from toluene to xylene.

## Getting out and back

| You want to | Do | Session on the other machine |
| --- | --- | --- |
| leave a hop | Enter, then `~`, then `.` | kept |
| see the escape keys | Enter, then `~`, then `?` | |
| detach from the remote tmux (not nested) | Ctrl-b, then `d` | kept |
| go back | `jaunt toluene` (same session, same screen) | |
| use a second session there | `jaunt toluene:work` | separate session |
| end the session for good | type `exit` in the remote shell | ended |
| quit the mesh view | `q` | |
| stop linking | Ctrl-c (or `n` at the question) | |

If the link drops (lid closed, Wi-Fi gone), jaunt keeps your terminal, shows one status line such as `toluene ╌ link lost · retrying · ~. to give up`, and re-attaches when toluene is back. If typed input gets no answer for 3 seconds and nothing at all has arrived from toluene, the bottom line says `no answer for Ns · waiting for the link`.
