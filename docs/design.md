# jaunt design

The code refers to this document by section (§4 and so on). Implementation detail is
in [internals.md](internals.md).

## 1. What it is

`jaunt` links a handful of machines that belong to one person and lets them work on any
one from any other: `jaunt toluene` attaches a persistent tmux session on the machine
called toluene, over a direct encrypted peer-to-peer path when one can be made and through
a self-hosted relay when it cannot. One static binary per machine, no root, no accounts,
no third-party service.

The name: a jaunt is a short trip, and in Alfred Bester's *The Stars My Destination*
"jaunting" is teleporting yourself somewhere by wanting to be there.

## 2. Transport

jaunt writes no transport, NAT traversal or cryptographic protocol of its own.

- **iroh 1.3** gives the transport in userspace: dial by ed25519 public key, QUIC with TLS
  1.3, hole punching, relay fallback through a self-hosted relay, connection migration.
- iroh's defaults use n0's public relays and lookup servers and fall back to public DNS
  resolvers. jaunt builds the endpoint with `presets::Minimal`, a relay list taken only
  from its own config, and the OS resolver. It contacts nothing but the configured relay
  and the peers; `test/e2e/08-nocontact.sh` checks this from outside.
- A machine is dialled by key plus relay URL, so every machine holds the same relay list
  (the fleet settings, §7).
- Peers behind NAT find direct paths only if the relay's UDP address discovery (7842) is
  reachable (§10).
- Port mapping (UPnP, NAT-PMP, PCP) is off by default because it changes the router's
  configuration; `portmap = true` enables it. Each peer's last direct addresses are kept
  and used as dial hints, and the daemon re-binds its last UDP port, so a restarted daemon
  finds a peer directly while the relay is down.
- No predictive local echo: on a relayed path typing feels like the relay round trip.

**Built here:** the daemon, the roster and its sync, the pairing ritual (composed of
standard pieces), the PTY/exec/TCP/copy services, the awake guard, the CLI and its look.
**Reused:** iroh and iroh-relay, rustls/ring (TLS 1.3), SPAKE2 (RFC 9382, RustCrypto),
ChaCha20-Poly1305 and HKDF-SHA256 (RustCrypto), tmux, systemd-logind inhibitors, and
OpenSSH as an optional second layer.

## 3. Vocabulary

- **machine**: one computer running the jaunt daemon. It has a **name** (default: hostname,
  lowercase) and an **id** (its iroh `EndpointId`, an ed25519 public key).
- **mesh**: the set of machines linked to each other. Membership is the **roster**.
- **link / unlink**: add a machine to the mesh / revoke it.
- **relay**: a self-hosted `iroh-relay` server. Introduces peers, relays when no direct
  path exists, sees ciphertext only.
- **hop**: open a session on another machine.
- **host mode**: a laptop staying awake and reachable while on mains power.

## 4. Architecture

One binary, `jaunt`. One long-lived process per machine, the **daemon** (`jaunt daemon`,
normally a systemd user service), owns the machine's key and its single iroh endpoint.
Every other invocation is a thin client that talks to the local daemon over a Unix socket
at `$XDG_RUNTIME_DIR/jaunt/sock` (directory 0700, socket 0600; fallback
`~/.local/state/jaunt/run/`). If the daemon is not running, client commands start it
(systemd user unit if installed, otherwise a detached process) and say so. A detached
daemon runs in `$HOME` with an allowlisted environment and a PATH of absolute entries
only; the client talks only to a socket in its own directory whose server has its uid
(`SO_PEERCRED`).

The daemon exists so that there is one endpoint per identity, connections to the other
machines stay warm (a hop is instant, and `jaunt` can show live paths and latency), and the
same process accepts incoming hops.

```
 terminal ── jaunt (client) ──unix socket── jaunt daemon ══ QUIC/TLS1.3 (iroh) ══ jaunt daemon ── pty ── tmux
                                                │   direct UDP when possible            │
                                                └────── else via the relay (WSS) ───────┘
```

### Files

| What | Where |
| --- | --- |
| binary | `~/.local/bin/jaunt` |
| secret key (32 bytes, 0600) | `~/.local/share/jaunt/key` |
| roster (0600, atomic writes) | `~/.local/share/jaunt/roster.json` |
| single-instance lock, with the socket path | `~/.local/share/jaunt/daemon.lock` |
| config | `~/.config/jaunt/config.toml` (name, relays, default session, host mode, `portmap`, `port`, `relay_ca`, `tmux_socket`) |
| logs, event history, address hints, session owners | `~/.local/state/jaunt/` |
| systemd user unit | `~/.config/systemd/user/jaunt.service` (`KillMode=process`, so restarting jaunt never kills tmux) |

All paths honour `XDG_*` and a `JAUNT_HOME` override that relocates everything under one
directory. A client that finds no daemon at its own socket tries the one recorded in the
lock, so two contexts that disagree about `XDG_RUNTIME_DIR` share one daemon.

### Source layout

```
src/main.rs        CLI parsing (clap), dispatch
src/paths.rs config.rs identity.rs
src/roster.rs      roster data type, merge, persistence
src/net.rs         endpoint construction, OS resolver, allowlist hook, path/RTT observation
src/proto.rs       wire types for every stream kind and for the IPC socket
src/daemon.rs      accept loop, peer manager (warm connections, sync, events)
src/ipc.rs         unix-socket server and client
src/pair/          pairing ritual (both roles)
src/trust.rs       revocation
src/shell.rs       PTY server side, raw-terminal client side, reconnect loop
src/services/      exec, tcp (fwd/pipe), copy
src/awake/         host-mode guard (state machine, sensors, inhibitor)
src/ui/            theme, glyphs, status table, mesh view, pairing view, banners, demo data
src/check.rs       `jaunt check`
```

## 5. Wire protocol

Two ALPNs.

**`jaunt/1`** on the machine's real endpoint. The endpoint's after-handshake hook rejects
any remote id that is not in the roster or is tombstoned, before any stream is accepted;
the check compares the raw 32 key bytes. Every stream re-checks membership when it opens.
Each request is one bidirectional QUIC stream whose first frame is a length-prefixed header
naming the service:

| Service | Purpose |
| --- | --- |
| `Sync` | exchange rosters and fleet settings; run first on every new connection and whenever the roster changes |
| `Ping` | liveness/RTT, machine facts for the mesh view (host mode, session count, version) |
| `Shell { session, term, cols, rows, env }` | PTY running `tmux new-session -A -s <session>`; frames: data, resize, exit status |
| `Exec { argv }` | run a command without a PTY; stdin/stdout/stderr frames, exit status |
| `Tcp { port }` | connect to `127.0.0.1:<port>` on the host and splice (loopback only) |
| `Copy { op, path }` | send or fetch files: a JSON plan, one JSON entry per file followed by its bytes, `skip` and `abort` messages. The receiver checks every path component and never writes through a symlink |
| `KillSessions { of }` | end the tmux sessions revoked machines created (`unlink --kill-sessions`) |

Frames are length-prefixed (u32) with a one-byte type, read incrementally; maximum frame
1 MiB; headers are serde structures with a size cap; unknown services are refused. A
version mismatch is reported in words, not as a hang.

**`jaunt-pair/1`** on a short-lived pairing endpoint (§6). Nothing else is reachable there.

QUIC settings: keep-alive 5 s, idle timeout 30 s. iroh migrates the connection across
network changes; when a connection is truly lost the idle timeout frees the host's side
(the tmux session stays).

## 6. Linking (pairing)

On a machine already in the mesh (or the first machine) run `jaunt link`; it shows a code
like `4127-tremor-kudos`. On the new machine run `jaunt join` and type the code when asked
(it is read from the terminal or stdin, not the command line, where every local user could
read it; the command-line form still works and warns). Both screens show the same sigil
and three words; confirm on both. The new machine receives the roster, and every other
machine in the mesh learns of it without being touched.

The first time, the new machine has no relay configured: `jaunt join` asks for it, or takes
`--relay URL` / `JAUNT_RELAY`, or the install line carries it. Linking always goes through
a relay; a local test relay over plain HTTP works.

This is the magic-wormhole construction, composed from standard parts:

1. **Code** = nameplate (4 random digits) + two words from the EFF large wordlist drawn
   with the OS CSPRNG (25.8 bits of secret). The nameplate is not secret.
2. **Rendezvous.** The inviter's daemon opens a second, temporary iroh endpoint whose key
   is derived from the nameplate alone (HKDF-SHA256, label `jaunt/pair/1/nameplate`). The
   joiner derives the same public id and dials it through the relay. Anyone who guesses
   the nameplate can reach or impersonate this endpoint; that gives them nothing but the
   chance to make one guess at step 3. The TLS on this connection is treated as
   unauthenticated transport. Up to 16 incoming rendezvous connections are handled side by
   side.
3. **PAKE.** SPAKE2 (RFC 9382, Ed25519 group, asymmetric roles, identities
   `jaunt-inviter` / `jaunt-joiner`) with the full code as the password. An attacker in
   the middle gets exactly one online guess (1 in 60 million); a wrong guess burns the
   code. Codes expire after 5 minutes and work once.
4. **Sealed exchange.** From the SPAKE2 key, HKDF-SHA256 derives one ChaCha20-Poly1305 key
   per direction. Each side sends its real id and name, and an ed25519 signature with its
   own machine key over a value derived from this session's key, proving that it holds
   the id it claims. A failed open is a wrong code; a missing or wrong proof ends the
   pairing.
5. **Human check.** Both sides derive a sigil (a small symmetric glyph) and three words
   from SHA-256 over the SPAKE2 key alone (the key covers the whole SPAKE2 transcript; the
   ids are bound by the proofs, so no field chosen after the key is known feeds the
   check), display them, and ask for confirmation. This is a second line in case the PAKE
   implementation is flawed, not the primary defence: without a commitment step a
   determined attacker with the code could grind SPAKE2 shares for a matching sigil; the
   three words are the harder part to match.
6. Only after both confirmations (sent sealed) does the inviter send the roster and fleet
   settings (sealed); the joiner checks them and acknowledges; the inviter writes its
   roster and says so; then the joiner writes its own. The pairing endpoint is closed and
   the new entry pushed to the rest of the mesh. If the last message is lost, the joiner
   joins again with a new code (a member that proves its key may link again).
7. A joining machine tries each code once: whatever went wrong, the same code is not
   tried again from that machine. This is kept in memory only, so a daemon restart
   forgets it.

This is pairing version 2; a version 1 peer is told to update. `--yes` skips the
confirmation prompt for scripted tests.

## 7. Roster, trust and revocation

The roster is a small replicated set:

- **entries**: `{ id, name, added_at, added_by }`
- **tombstones**: `{ id, revoked_at, revoked_by }`, permanent
- **fleet settings**: relay list, last-writer-wins by timestamp

A machine is a member iff it has an entry and no tombstone. Merge is union of entries and
union of tombstones; tombstones always win, so a revoked key can never come back (a
recovered laptop gets a new key with `jaunt reset` and is linked again). Sync runs on
every new connection in both directions and is pushed on every change; the daemon also
retries all known peers every 30 s, which spreads a revocation to machines that were
offline.

Limits: 256 members, 1024 entries, 512 tombstones for unknown ids, 8 relays. The merge is
bounded, item by item, and takes tombstones first, so a junk-stuffed roster cannot block a
revocation.

**Trust is flat.** Every member can open a shell on every other member, so every member
may also add and revoke members; there is no fleet master key to lose. The cost: a stolen,
unlocked, not-yet-revoked laptop is the whole mesh until it is revoked, and it could link
another machine or revoke the others first.

- `jaunt unlink <name>` tombstones the machine, closes its connections, ends every process
  started for it on this machine (shell clients and commands: SIGHUP, SIGKILL after 1 s),
  pushes the tombstone to every reachable machine and prints which machines confirmed and
  which have not yet heard. Its tmux sessions are kept unless `--kill-sessions` is given.
  It always prints what revocation does not undo.
- `jaunt unlink <name> --cascade` also revokes every machine that `<name>` added
  (`added_by`); plain `unlink` warns when such machines exist. With `--cascade` or
  `--kill-sessions`, a name that matches no member is looked up among revoked machines.
- If the others were revoked by a thief, recovery is local: `jaunt reset` on each machine
  and link again.
- `jaunt leave` removes this machine from the mesh (tombstones itself, pushes, wipes local
  roster and key).

**Settling.** A machine that has not heard of a revocation accepts the revoked key until
it syncs with an informed machine. To narrow that window, a daemon is *unsettled* after an
absence: daemon start, the monitor not running for 5 s, or no contact with any member for
60 s. While unsettled it serves Sync and Ping, but serves a peer's shell, command, port,
copy or session kill only after one of:

- a Sync with a member other than that peer;
- there is no such member;
- a settling round that started after the requester's connection was established reached
  no other member (provisional, until the next absence). Rounds during an outage never
  count, and no timer settles it.

The first such request waits for that round (every other member, relay and direct hints,
at most 5 s); later ones do not. A revoked machine is therefore served only if no informed
member answered within 5 s of its first request, and only until an informed member becomes
reachable.

## 8. Hopping

| Command | Does |
| --- | --- |
| `jaunt` | on a terminal: the live mesh view (§11); arrow keys or `j`/`k` to choose, Enter to hop, `q` to quit. Piped: same as `jaunt ls`. |
| `jaunt <machine>[:session]` | attach (creating if needed) tmux session `session` (default `jaunt`) on that machine. Unambiguous prefixes of names work. |
| `jaunt ls [--json]` | machines, state (direct / relay / offline), latency, host mode, sessions. Tab-separated when piped. |
| `jaunt run <machine> -- cmd…` | run one command there; exit status and stdio pass through. |
| `jaunt cp <src> <dst>` | copy files; either side may be `machine:path`. |
| `jaunt fwd <machine> <port>[:<localport>]` | forward a local port to that machine's loopback. |
| `jaunt pipe <machine> [port]` | stdio ⇄ that machine's loopback port (default 22), for `ProxyCommand jaunt pipe %h`. |
| `jaunt link`, `jaunt join`, `jaunt unlink <name>`, `jaunt leave`, `jaunt reset` | §6, §7 |
| `jaunt host on\|off\|status` | §9 |
| `jaunt up` / `jaunt down` | install and start / stop the user service. |
| `jaunt relay [set URL… --ca FILE]` | show or set the relay list. |
| `jaunt check` | diagnose: daemon, relay reachability, UDP, tmux, host-mode prerequisites. |
| `jaunt name <new>` | rename this machine. |
| `jaunt demo` | the mesh view and pairing ritual on simulated data. |
| `jaunt uninstall` | remove jaunt from this machine. |

Also `jaunt hop`, `jaunt daemon`, and hidden `jaunt pick` (the tmux popup) and
`jaunt debug` (test bootstrap, refused unless `JAUNT_HOME` is set).

**Session continuity.** Persistence is tmux on the host: the session outlives the
connection, the client and the daemon. On the client:

- A network change is absorbed by QUIC migration. After an outage, iroh 1.3's relay
  reconnect backoff is not cut short by a network change, so a roam onto a hard NAT can
  take tens of seconds; the client shows "no answer for Ns · waiting for the link"
  meanwhile.
- When the connection is lost, the client keeps the terminal, shows one status line
  (`toluene ╌ link lost · retrying · ~. to give up`), retries with backoff, and
  re-attaches when the machine is back; tmux repaints the screen.
- `Enter ~ .` disconnects (session kept), `Enter ~ ?` lists escapes, as in OpenSSH.
- Without tmux on the host the hop is a plain login shell, and jaunt says it will not
  survive a disconnect.

**Onward hops.** Each machine is a full member, so `jaunt xylene` typed inside a session on
toluene hops onward from toluene. The remote shell gets `JAUNT_FROM=<name>` and
`JAUNT_MACHINE=<name>`.

**tmux** (`contrib/tmux/jaunt.tmux`, installed by `jaunt tmux-setup` into
`~/.config/jaunt/` and sourced from the user's tmux config): `prefix + J` opens the mesh
picker in a popup and puts the hop in a new local window named after the machine; `F12`
turns the local tmux's keys off and on so the prefix reaches the remote tmux. It does not
bind `Ctrl-h/j/k/l` or change the prefix. Without the snippet, a hop from inside tmux
prints a one-time hint about `F12`/prefix-twice.

**Clipboard**: OSC 52 from the remote tmux passes through the PTY.

## 9. Host mode

- `jaunt host on` (persisted in config) asks the daemon to keep this machine reachable.
  The guard holds a logind **block inhibitor for `handle-lid-switch:sleep`** (never
  `idle`, so the screen still locks) **only while all hold**: host mode is on, the machine
  is on mains power, and it is not too hot.
- Mains power is detected the way systemd does (power supplies of type Mains/USB, online;
  batteries of peripherals ignored). Lid state from logind/procfs. Temperature from
  thermal zones and hwmon.
- **Safety rules.** On losing mains power, or at ≥ 90 °C sustained 120 s (or within 5 °C
  of a sensor's critical point), the guard releases the lock and, if the lid is closed,
  calls `systemctl suspend` itself; after a thermal trip it stays released until the lid
  is opened. Durations are leaky accumulators (time out of a condition pays back at half
  speed). After a power-caused release the lock is taken again only once mains has been
  steady 20 s to 2 min. Sensor reads have a 2 s deadline. Once CPU sensors have been seen,
  losing them counts as unknown heat.
- The lock is a file descriptor held through a child whose life is tied to the daemon, so
  a crash releases it; the unit uses `WatchdogSec` so a hang does too; if the guard stalls
  for 25 s the daemon kills the lock holder by pid. Poll every 5 s.
- Works without root for a logged-in local session. After a reboot with no local login,
  logind refuses the lock; `jaunt check` says so, and `scripts/root-extras/` has an
  optional polkit rule, logind drop-in and Wake-on-LAN helper, each with what it changes
  and how to undo it.
- `jaunt ls` shows each machine's host state (`awake`, `will sleep on lid close`,
  `on battery`).
- Waking a sleeping laptop remotely is not part of jaunt: Wake-on-LAN works only from the
  same LAN and scheduled wake needs root.
- macOS: closed-lid hosting needs root (`pmset disablesleep`), so `host on` on macOS only
  prevents idle sleep with the lid open (`caffeinate`).

## 10. The relay

`iroh-relay`, unmodified. It needs HTTPS (443, its own certificate) and UDP 7842
reachable from the internet. `deploy/relay/` has a Dockerfile, a config and notes for a
small VPS, Fly.io (no direct paths as shipped),
and a desktop behind an existing Cloudflare tunnel (relaying only; machines behind NAT
find no direct paths through it). The relay sees which ids are online, from which IP
addresses, who talks to whom and how much. It cannot read or alter traffic and cannot
join the mesh.

The relay's access control stays open: an id allowlist would break pairing (its
endpoints have ids nobody knows in advance), and iroh-relay's shared-token mode is not
supported because jaunt sends no token. `http://` relay URLs are accepted for a local test
relay.

The endpoint always has a relay transport (an empty relay map rather than relays
disabled), so a relay set on a running daemon, or received by roster sync, is used at
once; `jaunt relay set` reports whether the daemon connected.

## 11. The look

- **One visual language.** A machine is a node (`●` up, `○` down). A link is a line whose
  style is its path: solid `━` green for direct, dashed `╌` amber for relayed, dim for
  offline, red for revoked. The same vocabulary appears in the mesh view, `jaunt ls`, the
  hop banner and the pairing ritual.
- **Palette.** Works in 16 colours; with `COLORTERM=truecolor`: accent violet `#b4a0ff` →
  cyan `#6fe3e1` (wordmark gradient, selection), direct green `#8ee6a0`, relay amber
  `#f2c97d`, alert red `#f28b9a`, dim `#6c7086`, text in the default foreground. No
  background colour except the selection bar.
- **Glyphs** every monospace font has (box drawing, block elements `▁▂▃▄▅▆▇█`, `●○◆▸`,
  braille for spinners). No Nerd Font glyphs. `JAUNT_ASCII=1` or a non-UTF-8 locale
  switches to an ASCII set. `NO_COLOR`, `--no-color` and non-TTY output are honoured:
  piped output is plain, stable, tab-separated, with no escape codes or animation.
- **Motion** carries information: a pulse travels along a link on traffic or a handshake;
  latency is a sparkline of the last minute; a path change redraws the line style.
  Animations never delay a hop and cap at ~15 fps.
- **Mesh view** (`jaunt`): header with wordmark, this machine, relay state; this machine
  on the left with a link to each peer (style = path, label = latency + sparkline +
  remote address kind), a peer column with name, host state, sessions; an event log
  underneath; a key bar at the bottom. Degrades to 80×24 and to narrow panes.
- **`jaunt ls`**: the same information as a static block.
- **Hop**: a one-line banner on the way in (`benzene ━━▸ toluene · direct · 0.8 ms`) and on
  the way out (`◂━━ back on benzene · session kept on toluene`).
- **Pairing ritual**: two nodes with a gap between them; the code shown large; steps tick
  as they happen (rendezvous, key exchange, identities sealed); the link closes with a
  pulse; the sigil and three words appear on both; then the roster fan-out
  (`told xylene ✓ phenol …`).
- Strings from other machines are printed without control characters.

## 12. Threat model

**Assets:** a shell as the user on every machine. **Trusted:** the machines in the roster
and the user's account on each. **Not trusted:** the network, the relay and whoever hosts
it, any machine not in the roster.

| Defended | How |
| --- | --- |
| Eavesdropping or tampering on any path, including by the relay | QUIC/TLS 1.3 end to end between machine keys; the relay forwards ciphertext |
| An unknown machine connecting | the handshake authenticates the peer's key; non-members are rejected before any service code runs |
| Someone who learns the relay URL or a machine id | ids are public keys; knowing them grants nothing |
| An attacker racing or intercepting a pairing | PAKE: one online guess per code, 1 in 6×10⁷, code burnt on failure, 5-minute life; plus the on-screen sigil check |
| A lost laptop, after `unlink` | tombstone is permanent, pushed to all reachable machines, spread by sync; its processes ended |
| Other local users on a machine | socket and key are 0600 in 0700 directories; the client checks the socket's owner |
| A laptop overheating in a bag | host mode holds the lock only on mains power and below the thermal limit; fails towards sleep |

| Not defended | Why / what to do |
| --- | --- |
| A stolen laptop that is unlocked and not yet revoked | it is a member; revoke from any other machine. The key file has no passphrase (the daemon must start unattended); use disk encryption and a screen lock |
| A machine that was offline when the revocation was issued | the settling window in §7 |
| A compromised member machine | flat trust: it can reach every other member, add members and revoke. It can also fill the entry limit (new links then fail until `jaunt reset`) and set `added_by` so that `--cascade` misses machines it added |
| Traffic analysis | the relay and network observers see who talks to whom, when, how much; the machine id is visible in the TLS handshake |
| Bugs in iroh/noq, jaunt or the SPAKE2 crate | the pre-authentication surface is iroh's handshake; for a second layer run OpenSSH over `jaunt pipe` |
| Denial of service | the relay can drop traffic; anyone can occupy a pairing nameplate (pairing fails, nothing leaks) |
| A tampered release | the install checksum comes from the same place as the binary; there is no signature |
| Other local users and `jaunt fwd` | the forward listens on 127.0.0.1 while it runs |
| root on the host, malware in the user's account | out of scope |

## 13. Release

A static x86_64 Linux binary (musl), built by `scripts/build-release.sh` in a container.
`install.sh` downloads it with its `.sha256` and falls back to `gh release download` for a
private fork. Other targets build from source.

## 14. Tests

Unit tests cover roster merge, pairing (wrong code, replay, expiry, burn), frame parsing
limits, settling, and the awake state machine on fake sysfs trees.

`test/e2e/` runs over the Docker harness (`test/harness`: a relay and three NATed
machines): link with the ritual; a third machine learns of a link; `ls` shows paths; hop
and run a command; onward hop a→b→c; a direct path between a and b; a relay path to c
behind the hard NAT; a session survives a link drop, a roam and a freeze; unlink locks a
machine out and a machine that was offline learns on return; no packets go anywhere but
the relay and peers; keystroke latency (written to [measurements.md](measurements.md)).
