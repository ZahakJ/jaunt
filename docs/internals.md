# jaunt internals (stage 1: core)

What the core provides for the next stages to build on. DESIGN.md is the contract; this
file records how stage 1 implemented it and the APIs other modules consume.

## Files and paths (`src/paths.rs`)

With `JAUNT_HOME=<dir>` everything lives under it:
`config/config.toml`, `data/key`, `data/roster.json`, `state/{daemon.log,events.jsonl}`,
`run/{sock,daemon.lock}`, `config/systemd/user/jaunt.service`. Without it, the XDG paths of
DESIGN §4 (`$XDG_RUNTIME_DIR/jaunt/sock`, fallback `~/.local/state/jaunt/run/`).
Directories are 0700, key/roster/config/socket 0600. Writes are atomic (temp + fsync + rename).

`config.toml` keys: `name`, `relays` (bootstrap list), `relay_ca` (PEM path),
`default_session`, `tmux_socket` (env `JAUNT_TMUX_SOCKET` wins), `portmapper` (default on,
**off under `JAUNT_HOME`**), `port` (fixed UDP port). `JAUNT_RELAY` (comma list) is used when
the config has no relays.

Relays in effect: the roster's fleet settings once anyone ran `jaunt relay set`, else the
config's. Fleet relay changes are applied live (`Endpoint::insert_relay/remove_relay`); a
new `relay_ca` needs a daemon restart.

## Wire (`src/proto.rs`)

Frame: `[u32 BE payload length][u8 type][payload]`, payload ≤ 1 MiB. Types:
`1 Header` (JSON `Hello`), `2 Reply` (JSON `Reply`), `3 Data`, `4 Stderr`, `5 Eof`,
`6 Resize` (u16 cols, u16 rows BE), `7 Exit` (JSON `{code, signal}`), `8 Notice` (UTF-8),
`9 Json`. JSON headers are capped at 64 KiB.

ALPN `jaunt/1`. Each request is one bi-stream: client sends `Header {proto: 1, service}`;
host answers `Reply {kind: ok}` or `{kind: refused, reason}` (version mismatch, unknown
service, non-member, not implemented, bad session name… all in words). Services
(`Service`, tagged by `kind`):

| kind | after the reply |
| --- | --- |
| `sync` | client sent one `Json` roster right after the header; host replies `Json` with its merged roster |
| `ping` | host sends `Json` `PingInfo {name, version, sessions, host, time_ms}` |
| `shell {session, term, cols, rows, env}` | `Data` both ways, `Resize` c→h, `Notice` h→c, final `Exit` h→c |
| `exec {argv}` | `Data` (stdin c→h / stdout h→c), `Stderr` h→c, `Eof` c→h, final `Exit` |
| `tcp {port}`, `copy {}` | refused: "not implemented yet" |

If the stream dies before `Exit`, the host kills the process group (SIGHUP, SIGKILL after 2 s);
for shells that is the tmux client, so the tmux session survives.

## IPC API (unix socket, `src/ipc.rs`)

Same framing; every message is one `Json` frame. The server checks the peer's uid. One
request per connection. Requests (`IpcRequest`, tagged by `op`):

| request | response(s) |
| --- | --- |
| `{"op":"status"}` | `{"kind":"status", ...StatusSnapshot}` |
| `{"op":"subscribe","history":N}` | `status` snapshot, then up to N past `event`s (oldest first, max 500), then live `event` and `rtt` messages until the client closes; after a lag a fresh `status` |
| `{"op":"open","target":"fern","service":{...}}` | `{"kind":"opened","peer":{id,name},"path":"direct"\|"relay"\|"unknown","rtt_ms":0.8}` and from then on the socket carries the QUIC stream's frames verbatim (both directions; closing the socket ends the stream); or `{"kind":"error","message":...}` |
| `{"op":"add_peer","id":"<64 hex>","name":"b"}` | `ok` / `error` (test bootstrap: `jaunt debug add-peer`) |
| `{"op":"rename","name":"x"}` | `ok` / `error` |
| `{"op":"set_relays","relays":["https://..."]}` | `ok` / `error` |
| `{"op":"shutdown"}` | `ok`, then the daemon exits |

`target` is resolved by the daemon: exact display name, then unambiguous name prefix, then
id-hex prefix (≥ 4 chars); this machine is never a target. Errors say "could mean x or y".

`StatusSnapshot`:
```
me {id, name}, version, daemon_pid, started_at_ms,
relays [url], relay_connected (home relay; becomes true ~3 s after start), bound [sockaddr],
peers [PeerStatus], revoked [{id, name?, revoked_at_ms}]
PeerStatus { id, name, state: direct|relay|connecting|offline, remote_addr?, addr_kind?
  (lan|public|cgnat|loopback|relay|other), rtt_ms?, rtt_history [{t_ms, rtt_ms, path}]
  (1 sample/s, last 60), connected_since_ms?, last_seen_ms?, version?, sessions?, host?,
  last_error? }
```
Peers are roster members other than this machine, sorted by name. `host` is always null
until the awake stage fills `PingInfo.host`.

`Event` = `{seq, t_ms, peer?: {id,name}, event: <kind>, ...fields}`; kinds:
`daemon_started {version}`, `handshake {incoming, path}`, `path_changed {from, to,
remote_addr}`, `disconnected {reason}`, `dial_failed {error}` (only when the error text
changes), `sync {changed, outgoing}`, `roster_changed {what}`, `served {service}` (shell/exec
opened here by a peer), `refused {reason}` (a stream from a peer was refused: bad header, version, membership). `rtt` messages: `{"kind":"rtt",
"peer":"<id>","sample":{t_ms, rtt_ms, path}}` once per second per connected peer; they are
live-only, not history. Events are also appended to `state/events.jsonl` (rotated at 1 MiB).

`jaunt debug events [--history N]` prints the subscription as JSON lines; it is the easiest
way to see the stream.

## Daemon behaviour (`src/daemon.rs`)

- Endpoint: `presets::Minimal`, relays only from settings, OS resolver (no public DNS
  fallback), no address lookup, no captive-portal probe, keep-alive 5 s, idle 30 s.
  The after-handshake hook admits only current members (raw 32-byte ids); every stream
  re-checks membership.
- One keeper task per member: dial if no live connection, then wait 30 s / a roster change /
  a kick (2 s after a loss). A dial is `id + every relay URL`.
- Every adopted connection (either direction) accepts streams, samples the selected path
  every second, pings every 10 s. The dialler runs Sync on every new connection; any roster
  change (local or merged) is pushed to every connected peer.
- Simultaneous dials: both sides keep the connection dialled by the smaller id and close the
  other after 3 s. A new connection from a peer outside that race replaces the old one at
  once (the peer only dials when its side of the old one is gone), so a killed and restarted
  peer is back in about a second instead of after the 30 s idle timeout.
- SIGTERM/SIGINT/SIGHUP or `shutdown`: close every connection with a reason, close the
  endpoint (3 s cap). sd_notify READY/WATCHDOG/STOPPING when run by systemd.
- Single instance per home: `flock` on `run/daemon.lock`.

## Roster (`src/roster.rs`)

`entries{id → {id, name, added_at, added_by, named_at}}`, `tombstones{id → {id,
revoked_at, revoked_by}}`, `fleet {relays, updated_at, updated_by}`. Merge: entries by max
`(named_at, name, added_at, added_by)`, tombstones by min `(revoked_at, revoked_by)`, fleet
by max `(updated_at, updated_by, relays)`; a tombstone beats any entry. Limits: 256 entries,
4096 tombstones, 8 relays, names `[a-z0-9-]{1,32}`. Same-name machines display as
`name` (earliest added) and `name-<4 hex of id>`. `revoke()` exists for the unlink stage.

## Client side

- Auto-start: if the socket does not answer, `jaunt` starts `jaunt daemon` detached
  (`setsid`, log to `state/daemon.log`) or `systemctl --user start jaunt` if the unit is
  installed and `JAUNT_HOME` is unset.
- Hop (`src/shell.rs`, `src/term.rs`): raw mode restored on drop, on panic (hook) and on
  SIGTERM/SIGHUP; SIGWINCH → `Resize`; `Enter ~ .` / `Enter ~ ?` / `~~`; when the stream
  dies the client keeps the terminal, shows one status line and retries with backoff
  1–10 s; `~.` gives up during retry too.
- UI seam (`src/ui/mod.rs`): `ls_plain` (stable tab-separated: name, state, rtt, host,
  sessions, id; this machine first with state `self`), `ls_pretty` (placeholder = plain),
  `hop_banner_in/out`, `link_lost_line`. The UI stage replaces bodies, not signatures.

## Tests

- `cargo test`: roster merge laws, tombstones, collisions, frame limits and malformed
  input, escapes, argv/env limits, key file permissions.
- `test/e2e/00-host.sh`: two daemons on the host (JAUNT_HOME under `target/tmp`, dev relay
  on 127.0.0.1, tmux `-L jaunt-test`), run, events, and a pty hop that survives SIGTERM and
  SIGKILL restarts of the remote daemon.
- `test/e2e/01-core.sh`: Docker harness with a TLS+QAD iroh-relay (`test/harness/relay.sh`,
  throwaway CA made at run time): a⇄b direct, c relayed, roster spread, run, relayed hop.
