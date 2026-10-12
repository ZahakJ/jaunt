# Internals

This chapter is the reference for how jaunt is built: where it keeps its files, what goes over the wire, what the daemon does on its own, how the roster merges, how pairing and revocation are implemented, what `jaunt check` looks at, and how the tests are organised. You do not need any of it to use jaunt. It is here for reading the code, for judging the security claims made elsewhere in this manual, and for debugging something the [Troubleshooting](troubleshooting.md) chapter does not cover.

A map of the pieces before the detail:

- **One binary, two roles.** `jaunt daemon` is the long-lived process, normally a systemd user service. It owns this machine's key and its single iroh endpoint, keeps warm connections to the other members, and serves incoming hops. Every other `jaunt` command is a thin client that talks to it over a Unix socket in a private directory, and starts it if it is not running.
- **Two wire protocols.** `jaunt/1` runs on the machine's real endpoint and carries every service (sync, ping, shell, exec, port forwarding, copy). `jaunt-pair/1` runs only on the short-lived pairing endpoint, and nothing else is reachable there.
- **The roster** is a small replicated set of member entries and permanent tombstones. Merging is a union in which tombstones always win, so no machine, clock or ordering can bring a revoked key back.
- **Settling** is the rule that closes most of the revocation window: after an absence (a start, a freeze, a long outage) a daemon first tries to sync with the other members before it serves a shell, command, port or copy.
- **The look** is drawn by hand on a cell grid, with no terminal UI library, and degrades to 16 colours, no colour and ASCII.

The design contract these follow is `docs/DESIGN.md` in the repository, which this text refers to by section number (§4 and so on); its last part lists every place where the built program differs from the contract. The text also names source files (`src/daemon.rs`, `src/pair/` and so on) and test scripts (`test/e2e/`); all of them are in the [repository](https://github.com/ZahakJ/jaunt).

The file is long and kept whole on purpose: it is the reference. The rest of this chapter is `docs/internals.md` from the repository.

{{#include ../../internals.md:2:}}
