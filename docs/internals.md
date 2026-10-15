# jaunt internals

How jaunt is implemented, and the APIs modules consume from each other. Section numbers
(§4 and so on) refer to `docs/design.md`.

## Files and paths (`src/paths.rs`)

With `JAUNT_HOME=<dir>` (must be absolute) everything lives under it:
`config/config.toml`, `data/{key,roster.json,daemon.lock}`,
`state/{daemon.log,events.jsonl,addrs.json,tmux-owners.json}`, `run/sock`,
`config/systemd/user/jaunt.service`. Without it, the XDG paths of §4
(`$XDG_RUNTIME_DIR/jaunt/sock`, fallback `~/.local/state/jaunt/run/`).
Directories are 0700 and must be real directories owned by the user (a symlink or a
foreign owner is refused); key/roster/config/socket and the state files 0600. Writes are
atomic (temp + fsync + rename). The single-instance lock sits next to the key (one daemon
per identity even when two contexts disagree about `XDG_RUNTIME_DIR`) and records the
running daemon's socket path; a client that finds no daemon at its own socket path tries
the recorded one before starting a daemon. A client only talks to a socket whose directory
is its own and whose server runs as its uid (`SO_PEERCRED`). A detached daemon's
`daemon.log` is rotated to `daemon.log.1` past 1 MiB.

`config.toml` keys: `name`, `relays` (bootstrap list), `relay_ca` (PEM path),
`default_session`, `tmux_socket` (env `JAUNT_TMUX_SOCKET` wins), `portmap` (UPnP / NAT-PMP /
PCP; off by default; `portmapper` is
read as an alias; `jaunt check` mentions it when no peer is direct), `port` (fixed UDP
port; without it the daemon re-binds the port it used last time when that is free).
`JAUNT_RELAY` (comma list) is used when the config has no relays.

Relays in effect: the roster's fleet settings once anyone ran `jaunt relay set`, else the
config's. Fleet relay changes (`jaunt relay set`, or a list that arrives by roster sync)
are applied live (`Endpoint::insert_relay/remove_relay`); a new `relay_ca` needs a daemon
restart. The endpoint is always built with a relay transport (`RelayMode::custom` with the
list, empty if there is none, never `RelayMode::Disabled`), so a daemon that started with
no relay connects as soon as one is set; `jaunt relay set` waits up to 10 s and says
whether the daemon is connected to the relay.

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
| `tcp {port}` | host connects to 127.0.0.1:port (then ::1) *before* replying (a closed port is a refusal in words); then `Data` both ways, `Eof` = half-close each way; a side finishes the stream only when both directions are done (`src/services/tcp.rs`) |
| `copy {op: push\|pull, path}` | the sender (client for push, host for pull) sends `Json` messages tagged `m`: `plan {name, files, bytes}`, then per entry `entry {path, kind: file\|dir\|symlink, mode, size, mtime, target}` followed for a file by exactly `size` bytes of `Data`, `skip {path, reason}` for what it could not send, `done` (or `abort {reason}`); for a push the host answers `written {files, bytes}` or `abort` (`src/services/copy.rs`) |
| `kill_sessions {of}` | host ends the tmux sessions those (revoked) machines created; `Json` count |
| `tabs` | `Json` `[Tab {browser, title, url}]` from the browsers' session files (`src/services/tabs.rs`) |
| `ports` | `Json` `[Port {port, bind, process, what}]`: listening TCP sockets loopback reaches (`src/services/ports.rs`) |
| `open_url {url}` | host opens an `http(s)` URL in its browser; `Json` `null` |
| `notify {title, body}` | host shows a notification titled `<sender> · <title>`; `Json` `null` |
| `clip {op: get\|set}` | refused unless `clipboard = true` on the host. `get`: `Json` text. `set`: client sends the text as one `Json` frame, host answers `Json` `Reply` (`src/clipsync.rs`) |
| `dial {host, port}` | as `tcp`, to any address the host reaches (`jaunt socks`) |
| `listen {port, to}` | host listens on 127.0.0.1:port while the stream is open, and opens a `tcp {port: to}` stream back to the client for each connection (`jaunt rfwd`) |

Copy receiving (`src/services/tree.rs`): entry paths are relative, `/`-separated, plain
components only (no `..`, `.`, empty, absolute, NUL; ≤ 4096 bytes, ≤ 128 deep, ≤ 2 million
entries); before each write every directory between the copy's root and the entry is
checked with lstat to be a real directory, so a received symlink is never written through;
files go to a temp file in the same directory (0600) and are renamed into place with the
sent mode (permission bits only, setuid/setgid/sticky dropped) and mtime; directory modes
are applied last. If the destination is an existing directory the tree goes inside it
under the plan's `name`; on a pull the client requires that name to equal the basename it
asked for. Remote paths: relative and `~/…` are under the daemon's `$HOME`.

If the stream dies before `Exit`, the host kills the process group (SIGHUP, SIGKILL after 2 s);
for shells that is the tmux client, so the tmux session survives.

## IPC API (unix socket, `src/ipc.rs`)

Same framing; every message is one `Json` frame. The server checks the peer's uid. One
request per connection. Requests (`IpcRequest`, tagged by `op`):

| request | response(s) |
| --- | --- |
| `{"op":"status"}` | `{"kind":"status", ...StatusSnapshot}` |
| `{"op":"subscribe","history":N}` | `status` snapshot, then up to N past `event`s (oldest first, max 500), then live `event` and `rtt` messages until the client closes; after a lag a fresh `status` |
| `{"op":"open","target":"toluene","service":{...}}` | `{"kind":"opened","peer":{id,name},"path":"direct"\|"relay"\|"unknown","rtt_ms":0.8}` and from then on the socket carries the QUIC stream's frames verbatim (both directions; closing the socket ends the stream); or `{"kind":"error","message":...}` |
| `{"op":"add_peer","id":"<64 hex>","name":"b"}` | `ok` / `error` (test bootstrap: `jaunt debug add-peer`; refused unless the daemon runs under `JAUNT_HOME`) |
| `{"op":"quiet","id":"<64 hex>"}` | `{"kind":"quiet","quiet_ms":N\|null}`: ms since any datagram (data, acks, keep-alives) arrived from that peer on its live connection; `null` without one. The hop client asks it when typed input has had no answer for 3 s |
| `{"op":"rename","name":"x"}` | `ok` / `error` |
| `{"op":"set_relays","relays":["https://..."]}` | `ok` / `error` |
| `{"op":"shutdown"}` | `ok`, then the daemon exits |
| `{"op":"set_host","on":true}` | `ok`; host mode saved in config.toml and applied at once (`src/awake/`). `StatusSnapshot.host` is a `HostReport {state, reason, on_lid_close, lock_held, lid, power, hottest}`; `state` is `off`/`awake`/`on_battery`/`too_hot`/`will_sleep`, and peers get `HostState::label()` in `PingInfo.host` |
| `{"op":"set_clipboard","on":true}` | `ok`; saved as `clipboard` in config.toml |
| `{"op":"forward","machine":"b","port":8000,"local":0,"name":null,"keep":false}` | `{"kind":"forward", ...ForwardStatus}` once listening (`local` 0: the same port if free, else any); `keep` saves it as a `[[forward]]` table. `StatusSnapshot.forwards` lists them (`src/forwards.rs`) |
| `{"op":"unforward","name":"site"}` | `ok` / `error`; by name or local port |
| `{"op":"visitor"}` | `ok` with the name of the machine most recently hopped in whose shell is still open, else the last one that hopped in; `error` if none |

| `{"op":"link","yes":false}` | a stream of `pair` updates (below); at step `confirm` the daemon waits for `{"op":"pair_answer","accept":true\|false}` on the same socket (not with `yes`); ends with `ok` or `error`. Closing the socket cancels |
| `{"op":"join","code":"4127-tremor-kudos","relays":[...],"yes":false}` | same conversation as `link` |
| `{"op":"unlink","target":"c","cascade":false,"kill_sessions":false}` | one `told` per other member as it answers, then `unlinked` (report) or `error`. With `kill_sessions`, the tmux sessions the revoked machine created are ended here and, through the `kill_sessions` service, on every member that confirmed (each machine records which peer created which session in `state/tmux-owners.json`, and ends only sessions of machines it has revoked); the report's `tmux_sessions_killed` counts them |
| `{"op":"leave"}` | `told`s, then `unlinked`; the client then stops the daemon and deletes key and roster |

`target` is resolved by the daemon: exact display name, then unambiguous name prefix, then
id-hex prefix (≥ 4 chars); this machine is never a target. Errors say "could mean x or y".
For `unlink` with `cascade` or `kill_sessions`, a target that matches no member is looked
up among revoked machines the same way (their entries, with the name, stay in the roster;
a tombstone without an entry is found by id prefix; same-named revoked machines are told
apart by id), so `jaunt unlink toluene --cascade` still works after toluene was unlinked; the
report then lists it in `already_revoked`. Without those flags it says the machine is
already unlinked.

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
Peers are roster members other than this machine, sorted by name. A peer's `host` is the
`HostState::label()` it reported in its last `PingInfo` (`off`, `awake`, `on battery`, `too
hot`, `will sleep on lid close`); `sessions` is the number of tmux sessions on its jaunt tmux
server (cached 5 s). `StatusSnapshot.host` is this machine's own `HostReport`.

`Event` = `{seq, t_ms, peer?: {id,name}, event: <kind>, ...fields}`; kinds:
`daemon_started {version}`, `handshake {incoming, path}` (for a connection this machine
dialled: after its first sync went through), `path_changed {from, to,
remote_addr}`, `disconnected {reason}`, `dial_failed {error}` (only when the error text
changes; also, once per streak, when a peer refuses a connection we dialled as "not a
member", the usual case while a new link spreads), `sync {changed, outgoing}`, `roster_changed {what}`, `served {service}` (shell/exec
opened here by a peer), `refused {reason}` (a stream from a peer was refused: bad header, version, membership). `rtt` messages: `{"kind":"rtt",
"peer":"<id>","sample":{t_ms, rtt_ms, path}}` once per second per connected peer; they are
live-only, not history. Events are also appended to `state/events.jsonl` (rotated at 1 MiB).

Trust kinds: `pair {role, step, ...}` (the same steps as the IPC `pair` messages; the
`waiting` step's code is logged as `NNNN-…`, never the words), `revoked {by, sessions_killed,
added}` with `peer` = the revoked machine (emitted on every machine as the tombstone arrives,
locally or by sync; `added` = members it added, still linked), `told {ok, detail}` with `peer`
= the machine that answered a pushed unlink/leave.

### Pairing and revocation messages

`pair` message / event: `{"kind":"pair","role":"inviter"|"joiner","step":<step>,...}`. Steps in order
(fields inside a step are never called `peer`, which belongs to the enclosing event):

| step | fields | when |
| --- | --- | --- |
| `waiting` | `code, expires_at_ms, relays, first_machine` | inviter: the code is live (5 min, one attempt) |
| `dialing` | `relays` | joiner: looking for the inviter's rendezvous endpoint |
| `rendezvous` | | the two temporary endpoints are connected (unauthenticated) |
| `key_exchange` | | SPAKE2 done (a wrong code shows only at the next step) |
| `sealed` | `other {id,name}` | real ids and names opened under the PAKE key: the code was right |
| `confirm` | `other, sigil[5 strings of 5 '#'/'.'], hue 0..5, words[3]` | show them; ask the human (the event history keeps `other` only, not the sigil and words) |
| `confirmed` | `here: bool` | one side said yes (both arrive before `linked`) |
| `linked` | `other, members` | both said yes; the roster is written on this side |
| `fan_out` | `machine {id,name}, ok, detail` | inviter: one other member's acknowledgement of the new entry |
| `fan_out_done` | `told, not_yet` | inviter: every other member was tried (10 s each, in parallel) |
| `failed` | `reason` | the pairing ended without linking (the final `error` repeats it) |

`told`: `{"kind":"told","peer":{id,name},"ok":true,"detail":"confirmed"}`.
`unlinked`: `{"kind":"unlinked","revoked":[PeerRef],"confirmed":[PeerRef],"not_heard":[PeerRef],
"added_by_revoked":[PeerRef],"sessions_killed":N}`. `confirmed` = answered with a roster
holding the tombstone(s); `added_by_revoked` is empty with `cascade`.

`jaunt debug events [--history N]` prints the subscription as JSON lines; it is the easiest
way to see the stream.

## Daemon behaviour (`src/daemon.rs`)

- Endpoint: `presets::Minimal`, relays only from settings, OS resolver (no public DNS
  fallback), no address lookup, no captive-portal probe, keep-alive 5 s, idle 30 s.
  The after-handshake hook admits only current members (raw 32-byte ids); every stream
  re-checks membership.
- One keeper task per member: dial if no live connection, then wait 30 s / a roster change /
  a kick (2 s after a loss; 1 s after a "not a member" refusal, for the first 15). A dial
  is `id + every relay URL + the peer's last working direct addresses` (`state/addrs.json`,
  at most 4 per peer, saved whenever a direct path's address is new). The addresses are
  only hints: the peer is still authenticated by its key, so a stale one costs a failed
  attempt. With them, and with the daemon re-binding its last UDP port (so its NAT
  mapping and the address the others remember stay the same), a restarted daemon finds a
  peer on the same direct path while the relay is down.
- Every adopted connection is tracked per peer until it closes; a revocation closes all of
  them. A new connection from a peer replaces (and closes) any older one except inside the
  simultaneous-dial race below.
- **Settling.** An absence is a daemon start, the 1 s monitor not having run for 5 s
  (a freeze or suspend), or no authenticated contact with any member for 60 s (contact is a
  sync either way or a ping answered or served). From then on the daemon is *unsettled*.
  It serves Sync and Ping always; it serves a peer's Shell, Exec, Tcp, Copy or
  KillSessions at once if a Sync (either direction, a settling round's or any other) has
  completed since the absence with a member other than that peer, or if no member other
  than that peer exists (nobody else could know of its revocation); it is settled once that
  holds for every member. Otherwise the request waits for a settling round that started
  at or after the moment the requester's connection was established and after the absence
  began (joining a running one, else starting one): that connection proves the network
  was up while the round ran, so rounds that ran during an outage never count. If the round
  syncs with another member, settled as above; if it completes without reaching any, the
  daemon is **provisionally settled** and serves everyone without waiting until the next
  absence. Then the requester's membership is checked again and it is served. So the wait
  is at most once per absence (up to 5 s). A round syncs with every other member at once,
  dialling through the relay and the remembered direct addresses; it ends when every
  attempt has ended (failed dials are retried while time is left; a connection from before
  that does not answer in 2.5 s is closed and dialled again), at most 5 s. While unsettled
  the monitor also starts a round every 30 s; the keepers keep dialling every 30 s in any
  case, and a tombstone that arrives later ends the revoked machine's connections and
  processes as usual.
- Every served stream (shell, exec, tcp, copy) ends when its peer stops being a member,
  besides the process kills below.
- Every adopted connection (either direction) accepts streams, samples the selected path
  every second, pings every 10 s. The dialler runs Sync on every new connection; any roster
  change (local or merged) is pushed to every connected peer.
- Simultaneous dials: both sides keep the connection dialled by the smaller id and close the
  other after 3 s. A new connection from a peer outside that race replaces the old one at
  once (the peer only dials when its side of the old one is gone), so a killed and restarted
  peer is back in about a second instead of after the 30 s idle timeout.
- SIGTERM/SIGINT/SIGHUP or `shutdown`: close every connection with a reason, close the
  endpoint (3 s cap). sd_notify READY/WATCHDOG/STOPPING when run by systemd.
- Single instance per identity: `flock` on `data/daemon.lock` (next to the key; see Files).

## Roster (`src/roster.rs`)

`entries{id → {id, name, added_at, added_by, named_at}}`, `tombstones{id → {id,
revoked_at, revoked_by}}`, `fleet {relays, updated_at, updated_by}`. Merge: entries by max
`(named_at, name, added_at, added_by)`, tombstones by min `(revoked_at, revoked_by)`, fleet
by max `(updated_at, updated_by, relays)`; a tombstone beats any entry. Limits: 256
members, 1024 entries (revoked machines' entries count here, not as members), 512
tombstones for ids without an entry (a tombstone for a known id is always taken), 8
relays, names `[a-z0-9-]{1,32}`. A roster from a peer is merged item by item
(`merge_bounded`): malformed entries (bad name, id not an ed25519 key) are dropped
singly, tombstones go first, and only new items past the limits are skipped, so a full or
junk-stuffed roster never blocks a revocation. A merge that still fails is refused in
words (`sync refused: ...`), which `unlink` reports as such. Same-name machines display as
`name` (earliest added) and `name-<4 hex of id>`.

## Pairing (`src/pair/`)

`code.rs`: codes `NNNN-word-word`, nameplate and words from the OS CSPRNG (`getrandom`,
rejection sampling), the EFF large wordlist embedded (`eff_large_wordlist.txt`, words only).
Parsing is forgiving (case, spaces/underscores/dots for dashes, dashed list words like
`t-shirt`); a misspelt word is caught on the joiner before anything is sent.
`crypto.rs`: rendezvous key = HKDF-SHA256(ikm = nameplate, info `jaunt/pair/1/nameplate`);
SPAKE2 (`spake2` 0.4, Ed25519, `start_a` inviter / `start_b` joiner, identities
`jaunt-inviter` / `jaunt-joiner`, password = the canonical code); per-direction keys =
HKDF-SHA256(salt `jaunt/pair/1`, ikm = SPAKE2 key, info `jaunt/pair/1/inviter-to-joiner` /
`.../joiner-to-inviter`); ChaCha20-Poly1305 with a counter nonce per direction; check =
SHA-256(`jaunt/pair/2/check` ‖ key) → sigil (15 bits, mirrored 5×5), hue, three EFF words
(the key alone: a claimed id, chosen after the key is known, must not feed it); proof of
key = ed25519 signature by the machine key over `jaunt/pair/2/proof/<role>` ‖
HKDF-SHA256(salt `jaunt/pair/1`, ikm = key, info `jaunt/pair/2/proof`), checked against
the claimed id (which must be a valid ed25519 key). `flow.rs` (pairing version 2): the
conversation on one stream (diagram in the file): joiner `spake` → inviter `spake` →
joiner sealed `{id,name,proof}` → inviter sealed `{id,name,proof}` → both sealed `{ok}` →
inviter sealed `{roster,your_entry}` → joiner sealed `done{ok}` → inviter writes its roster,
sealed `done{ok}` → joiner writes its roster. The roster reaches the joiner only after both
humans said yes. Plain `{t:"abort",reason}` frames carry refusals (wrong code, revoked
key); they are unauthenticated courtesy and are shown only cleaned of control characters,
cut to 200 characters and marked "not verified". A member that proves its key may link
again (how a pairing whose last message was lost is finished).
`mod.rs`: the inviter's temporary endpoint (ALPN `jaunt-pair/1`, accepts any remote, no
port mapping) lives until the code's single attempt or its 5 minutes; incoming connections
are handled side by side (up to 16) until one sends a well-formed first message, so one
that never speaks holds up nobody and does not use up the code; the first SPAKE message
burns it. The joiner dials from a throwaway random key and sends its real id only sealed;
it records each code it sends a first message for (as a hash, 5 minutes) and never tries
the same code again. The client reads the code from the terminal or stdin (`jaunt join`
with no argument); a code given on the command line works but warns that the process list
shows it. `join` refuses on a machine that already has other members (use `leave` first). If the
fleet relay list was never set, `link` sets it from the inviter's relays so the joiner
receives it. A `join` on a machine whose daemon runs without a relay saves the relay to
config; the running daemon takes the fleet relay list from the roster it receives. There is no LAN pairing without a relay (no mDNS).

## Revocation (`src/trust.rs`, `src/children.rs`, `Daemon::roster_changed`)

On every roster change, in this order: the handshake allowlist is replaced (a revoked id is
refused by the hook and by every stream open from then on); every process started for a
non-member (shell pty clients, exec commands; `children.rs` registry) gets SIGHUP to its
process group, SIGKILL after 1 s if still registered (the serving future is kept alive up to
3 s after the revocation, `REVOKED_GRACE`, so that SIGKILL still reaches a command that ignores
SIGHUP); connections to non-members are
closed; a `revoked` event is emitted per new tombstone; the change is pushed to members only.
tmux sessions themselves are kept (they are the user's; a revoked machine's shell client is
detached). `unlink` applies the tombstone(s) locally first, then syncs with every other
member in parallel (10 s each) and reports. `leave` pushes a roster with this machine's own
tombstone first (the receiver spares the sender's connection until its answer is read) and
applies it locally last. `reset` does the same when still a member, then wipes.

Roster defences: remote rosters are sanitised before merging (entries and fleet settings
with timestamps past 2100 are dropped; tombstones are never dropped, only clamped);
membership ignores timestamps, so no clock can un-revoke; local renames and relay changes
use `max(now, previous + 1)` with saturation. A machine whose own id is tombstoned talks to
nobody. Accepted by design (flat trust): any member may tombstone any other, including the
receiver.

**Window for a machine that was away during an unlink.** A machine back from an absence
serves a peer no shell, command, port or copy until it has synced with some other member
or a settling round run with the network up reached none (settling, above), whether the
absence was a freeze, a suspend, a restart or a network-only outage with the daemon
running. **What is left, exactly:** a machine returning from an absence serves a revoked
machine only if no informed member answered within 5 s of the revoked machine's first
request (the others offline, the relay down and no direct path to them, or the member that
answered had not heard either), and then only until an informed member becomes reachable
(keepers dial every 30 s and on every reconnect; the tombstone then ends its connections
and processes). Absences shorter than a minute without a freeze are not noticed. A machine that
never left learns of a revocation by the push or by its next sync. `03-unlink.sh` checks
the freeze case (b frozen 60 s) once and the network-only case (b blackholed 90 s, daemon
running) three times: each time c sends five commands to b the moment b is back, and none
gets through.

## `jaunt check` (`src/check.rs`)

IPC `{"op":"check"}` → `{"kind":"check", net, contacts, relay_ips}`: `net` is iroh's latest
net report (QAD over IPv4/IPv6, public address, whether the mapping varies per destination,
per-relay probe latencies for `https` / `qad_ipv4` / `qad_ipv6`); `contacts` lists every
remote address of a QUIC path the daemon has used (recorded once a second per live
connection, with the peer's name) plus the daemon's open TCP/UDP sockets with a remote end
(from `/proc/self/fd` and `/proc/self/net/{tcp,udp}[6]`); `relay_ips` is what the relay host
names resolve to now. The client prints `ok` / `warn` / `fail` / `info` lines: version, key
and directory permissions, tmux, the systemd user unit (not consulted under `JAUNT_HOME`),
daemon, relay probe and connection, UDP address discovery and hard-NAT warning, each peer's
path, the contact audit (anything that is not the relay, a linked machine, loopback or the
system resolver is a warning), and the awake guard's prerequisites. Exit 1 if any `fail`.
The daemon is never started by `check`.

## Client side

- Auto-start: if the socket does not answer, `jaunt` starts `jaunt daemon` detached
  (`setsid`, log to `state/daemon.log`) or `systemctl --user start jaunt` if the unit is
  installed and `JAUNT_HOME` is unset. The detached daemon runs in `$HOME` (not the
  directory the command ran in) with an allowlisted environment (HOME, USER, LOGNAME,
  SHELL, LANG, LC_*, TZ, XDG_*, DBUS_SESSION_BUS_ADDRESS, TMUX_TMPDIR, JAUNT_*) and a PATH
  of absolute directories only; every daemon, however started, also leaves its start
  directory and drops empty or relative PATH entries, so no helper (tmux,
  systemd-inhibit, busctl, systemctl) is found relative to a directory.
- Hop (`src/shell.rs`, `src/term.rs`): raw mode restored on drop, on panic (hook) and on
  SIGTERM/SIGHUP; SIGWINCH → `Resize`; `Enter ~ .` / `Enter ~ ?` / `~~`; when the stream
  dies the client keeps the terminal, shows one status line and retries with backoff
  1–10 s; `~.` gives up during retry too. While the stream is up, typed input that has
  had no output back for 3 s makes the client ask the daemon (`quiet`) whether anything at
  all (data, acks, keep-alives) arrived from the machine in the last 3 s; if not, the
  bottom row shows `toluene ╌ no answer for Ns · waiting for the link · ~. to give up` until
  output arrives (then the remote screen is redrawn). A quiet program (a password prompt)
  on a live link shows nothing.
- Strings from other machines (refusal reasons, ping facts, notices, abort reasons, error
  messages) are printed without control characters (`term::safe_text`).
- UI (`src/ui/`): drawn by hand on a cell grid (`canvas.rs`, per-cell diffs, synchronized
  updates), no TUI crate. `theme.rs` picks the colour mode (`JAUNT_COLOR=truecolor|16|none`
  forces one; `NO_COLOR`/`--no-color`; off a terminal no escapes at all) and the glyph set
  (`JAUNT_ASCII=1` or a non-UTF-8 locale → ASCII). `model.rs` turns a `StatusSnapshot` plus
  the event stream into what is drawn; `mesh.rs` draws the tree (this machine as hub, a
  link per peer styled by path, latency and sparkline, the peer column with host state and
  sessions); a relayed link is drawn through a visible relay waypoint, all sparklines share one
  latency scale, repeated events fold into one line (`in step ×6`), and the live view has a
  detail panel for the selected machine (path, last minute of latency, last path change,
  handshake, uptime, id); `view.rs`/`live.rs` are the live view (bare `jaunt` on a terminal, and the
  hidden `jaunt pick` for the tmux popup: Enter opens the hop in a new tmux window);
  `ls.rs` is `jaunt ls` on a terminal; `ritual.rs` is what `jaunt link`/`join` show on a
  terminal (off a terminal they print the plain lines `02-link.sh` parses); `demo.rs` is
  `jaunt demo mesh|link|ls|hop [--at S] [--join]` on simulated data; `tmux.rs` is
  `jaunt tmux-setup`. `ls_plain` stays stable: tab-separated name, state, rtt, host,
  sessions, id; this machine first with state `self`.
- A hop from inside tmux prints a one-time hint about the prefix (marker
  `state/tmux-hint-shown`), unless the local tmux has sourced `jaunt.tmux` (`@jaunt_tmux`).
- Host mode (`src/awake/`, `jaunt host on|off|status`): see `docs/awake.md`. The lock is a
  `systemd-inhibit` child on a pipe from the daemon, so any daemon death releases it.

## Install, uninstall, release

- `scripts/build-release.sh`: static x86_64 musl binary built in a throwaway `debian:12`
  container with rustup (toolchain pinned, default 1.97.1); caches under
  `target/musl-build/` (about 1.8 GB); output `dist/jaunt-x86_64-linux-musl` and `.sha256`.
  Static-pie, about 10.7 MB. The harness installs a binary under its file name, so copy it
  to a file called `jaunt` before running the e2e suite with it.
- `install.sh` (POSIX sh, no root): downloads the release asset and its `.sha256` from
  GitHub (`JAUNT_REPO`, `--version TAG`, `JAUNT_BASE_URL` for another base, `file://`
  works), falls back to `gh release download` on a 404 (private repository), or takes
  `--from-file`. Verifies the checksum, writes `~/.local/bin/jaunt` atomically (`--bin-dir`),
  writes `name`/`relays` into config.toml for `--name`/`--relay` (keeps values already
  there), and starts nothing unless `--up`; otherwise it prints the next two commands.
- `jaunt uninstall [--yes|--keep-key]`: stops the service or daemon, removes the unit,
  config, state and runtime directories and the binary (only `~/.local/bin/jaunt`, or
  `$JAUNT_HOME/bin/jaunt`, and only if it is the running binary); asks before deleting the
  key and roster (keeps them off a terminal); says to `jaunt unlink <name>` from another
  machine. tmux sessions are left alone.
- `deploy/relay/`: Dockerfile, `relay.toml`, `fly.toml` and the three hosting options.

## Tests

- `cargo test`: roster merge laws (also over generated hostile rosters), tombstones, clock
  skew, sanitising, cascade closure, collisions, frame limits and malformed input, escapes,
  argv/env limits, key file permissions; pairing: codes and forgiving entry, wrong code
  (fails both sides, burns), expiry and single use, replayed first messages, a
  man-in-the-middle with the nameplate (one guess, opens nothing), tampered sealed
  payload, reflection/reordering, a "no" on either side, a revoked joiner, a code holder
  claiming an invalid or someone else's key (refused), no roster before both confirmed, a
  forged inviter id (refused), abort reasons cleaned and cut, an existing member linking
  again, the joiner trying a code once; roster: bounded merge equals merge below the
  limits, a junk-stuffed full roster never blocks a tombstone, malformed items dropped
  singly; host mode: heat dipping for one sample still trips, a flapping cable and an
  alternating weak charger release, a hung sensor read (a FIFO) releases, lost CPU
  sensors are unknown heat; local: uninstall removes only jaunt's files, the daemon's
  PATH and environment, systemd unit quoting, settling (what starts an absence, that a
  sync with a member other than the requester ends it, that rounds reaching nobody and time
  alone do not, that a round started before the requester's connection does not count, and
  provisional settling after one that started later), revoked machines resolving for `unlink --cascade`, address hints.
- `test/e2e/00-host.sh`: two daemons on the host (JAUNT_HOME under `target/tmp`, or
  `HOST_E2E_DIR`; dev relay on 127.0.0.1, tmux `-L jaunt-test`), run, events, and a pty hop
  that survives SIGTERM and SIGKILL restarts of the remote daemon; then a daemon started
  with no relay gets one by `jaunt relay set`, reaches it without a restart (same pid) and
  links a fourth machine.
- `test/e2e/02-link.sh`: harness: a starts the mesh, b joins (code typed with spaces and
  capitals), identical sigils, wrong code fails and burns, c joins through b, a learns of c,
  all connected, pairing events present and free of the code words.
- `test/e2e/03-unlink.sh`: harness: c's shell and command on b ended by `a unlink c`, c
  refused by a and b, cannot come back by syncing or by pairing with its old key; with b
  frozen for 60 s, `a unlink c --kill-sessions` ends the session c created on a and prints
  the reminder; the moment b thaws c sends five commands to b and none gets through; b learns of the unlink (time printed).
  Then three rounds of the network-only case: c linked again, b blackholed 90 s with its
  daemon running while a unlinks c, c sends five commands the moment b's network is back,
  none may get through; then `unlink <id prefix of a revoked c> --cascade` works; last,
  with a newly linked c offline and b frozen 15 s, a's first `run b` takes at most ~7 s
  and the next two under 1.5 s (one settling wait per absence).
- `test/e2e/06-paths.sh` also asserts that a daemon restarted while the relay is down is
  reached again on the direct path (address hints), and `05-continuity.sh`'s driver
  reports for each disruption whether the stall line was shown.
- `test/e2e/04-09`, `run-all.sh`: hop, onward hop and re-hop; continuity through
  link drops, roams, freezes and a daemon restart; paths (hard NAT modes, relay down and
  back); services (run, cp, fwd, pipe + OpenSSH, throughput); no-contact audit from outside
  (conntrack and iptables counters); keystroke latency (writes `docs/measurements.md`).
  Helpers: `hop_session.py` (PTY driver run on the host through `docker exec -it`, so
  disruptions run on the host meanwhile), `keystroke.py`, and in `lib.sh` `h_mesh`,
  `h_daemon_restart` (waits for the old daemon to exit: it holds the lock for up to 3 s).
- `scripts/capture.sh [--readme]`: demo views × sizes × colour modes rendered to PNG;
  `scripts/capture-live.sh`: the real mesh view on harness machine a (b direct, c relayed).
- `test/e2e/01-core.sh`: Docker harness with a TLS+QAD iroh-relay (`test/harness/relay.sh`,
  throwaway CA made at run time): a⇄b direct, c relayed, roster spread, run, relayed hop.
