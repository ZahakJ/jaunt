# jaunt — design

This is the contract the implementation is built against. Research behind it is in
`docs/research/` (`survey.md`, `iroh.md`, `awake.md`) and `test/harness/README.md`; where
this file is silent on a detail, those files decide.

## 1. What it is

`jaunt` links a handful of machines that belong to one person and lets them sit at any one
and work on any other: `jaunt fern` drops you into a persistent tmux session on the machine
called fern, over a direct encrypted peer-to-peer path when one can be made and through a
relay you host when it cannot. One static binary per machine, no root, no accounts, no
third-party service.

The name: a jaunt is a short trip, and in Alfred Bester's *The Stars My Destination*
"jaunting" is teleporting yourself somewhere by wanting to be there. It is five letters and
is not taken as a command on Arch or as a crate.

## 2. What was surveyed, and what that decided

Full survey: `docs/research/survey.md`. The conclusions that shape the build:

| Finding | Consequence |
| --- | --- |
| The slowness of the current setup is structural: every keystroke goes desktop ⇄ Cloudflare edge ⇄ laptop over TCP, even on the same LAN. Any direct-path tool removes it. | The speed win is real but is not unique to jaunt; Tailscale would give it too. We say so. |
| Tailscale (free plan) + tmux is the strongest alternative: audited WireGuard data plane, >90 % direct paths. It needs an account on a third-party control plane, root and a system VPN device on every machine. Headscale removes the third party but adds a server to run. Nebula and ZeroTier likewise need root and a coordinating node. | jaunt's reason to exist is: no root, no account, no VPN device, a pairing ritual instead of a control plane, hopping and laptop-awake built in. It does not claim to be more mature than WireGuard, and the README says when Tailscale is the better choice. |
| iroh 1.3 (stable since 1.0, June 2026) gives exactly the transport needed, in userspace: dial by ed25519 public key, QUIC with TLS 1.3, hole punching, relay fallback through a relay you can self-host, connection migration when the network changes. | **Reuse.** jaunt writes no transport, no NAT traversal and no cryptographic protocol of its own. |
| iroh's defaults use n0's public relays and lookup servers, and its DNS resolver falls back to public resolvers. | jaunt builds the endpoint with `presets::Minimal`, a relay list that comes only from jaunt's config, and an OS resolver. It must contact nothing but the configured relay and the peers. A test asserts this. |
| A machine cannot be dialled by key alone; it needs key + relay URL. All machines share one relay list. | The relay is both introducer and fallback. The relay URL is part of the fleet settings every machine holds. |
| The relay needs public UDP (7842) for address discovery, or peers behind NAT never go direct. Behind a Cloudflare tunnel it works as fallback only. | Hosting the relay is the owner's decision; `deploy/relay/` documents the options (§10). |
| mosh's local echo is the one thing a direct QUIC stream does not give. Eternal Terminal and mosh both need an open port on the host and SSH for auth. | Out of scope for v1: no predictive echo. On a direct path it is not needed; on a relayed path jaunt is honest that typing feels like the relay. |
| There is no published third-party audit of iroh or its QUIC library (noq, n0's fork of quinn). rustls was audited in 2020. The RustCrypto `spake2` crate is unaudited (it is what magic-wormhole.rs uses). | Stated plainly in the threat model. Mitigations: pre-auth surface is iroh's handshake only; optional second layer by running OpenSSH over a jaunt stream (`jaunt pipe`); pairing has a second, human check (§6). |
| `iroh-ssh`, `dumbpipe` exist. Neither has pairing, a fleet roster, revocation, a mesh view or laptop-awake. | jaunt's own code is those things. |

**Built here:** the daemon, the roster and its sync, the pairing ritual (composition of
standard pieces), the PTY/exec/TCP/copy services, the awake guard, the CLI and its look.
**Reused:** iroh + iroh-relay (transport, traversal, relay), rustls/ring (TLS 1.3), SPAKE2
(RFC 9382, RustCrypto crate), ChaCha20-Poly1305 and HKDF-SHA256 (RustCrypto), tmux
(session persistence), systemd-logind inhibitors (awake), OpenSSH (optional second layer).

## 3. Vocabulary

- **machine**: one computer running the jaunt daemon. It has a **name** (default: hostname,
  lowercase) and an **id** (its iroh `EndpointId`, an ed25519 public key).
- **mesh**: the set of machines linked to each other. Membership is the **roster**.
- **link / unlink**: add a machine to the mesh / revoke it.
- **relay**: an `iroh-relay` server the owner hosts. Introduces peers, relays when no
  direct path exists, sees ciphertext only.
- **hop**: open a session on another machine.
- **host mode**: a laptop's promise to stay awake and reachable while on mains power.

## 4. Architecture

One binary, `jaunt`. One long-lived process per machine, the **daemon** (`jaunt daemon`,
normally a systemd user service), which owns the machine's key and its single iroh
endpoint. Every other invocation is a thin client that talks to the local daemon over a
Unix socket at `$XDG_RUNTIME_DIR/jaunt/sock` (directory 0700, socket 0600; fallback
`~/.local/state/jaunt/run/`). If the daemon is not running, client commands start it
(systemd user unit if installed, otherwise a detached process) and say so.

Why a daemon: one endpoint per identity; connections to the other machines are kept warm so
a hop is instant and `jaunt` can show live paths and latency; the same process accepts
incoming hops.

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
| config | `~/.config/jaunt/config.toml` (name, relays, default session, host-mode settings) |
| logs, event history | `~/.local/state/jaunt/` |
| systemd user unit | `~/.config/systemd/user/jaunt.service` (`KillMode=process`, so restarting jaunt never kills tmux) |

All paths honour `XDG_*` and a `JAUNT_HOME` override that relocates everything under one
directory (tests and containers use it; nothing in tests may touch the real home).

### Source layout (ownership boundaries for parallel work)

```
src/main.rs        CLI parsing (clap), dispatch
src/paths.rs config.rs identity.rs
src/roster.rs      roster data type, merge, persistence
src/net.rs         endpoint construction, OS resolver, allowlist hook, path/RTT observation
src/proto.rs       wire types for every stream kind and for the IPC socket
src/daemon.rs      accept loop, peer manager (warm connections, sync, events)
src/ipc.rs         unix-socket server and client
src/pair.rs        pairing ritual (both roles)
src/shell.rs       PTY server side, raw-terminal client side, reconnect loop
src/services.rs    exec, tcp (fwd/pipe), cp
src/awake/         host-mode guard (state machine, sensors, inhibitor)
src/ui/            theme, glyphs, status table, mesh view (TUI), pairing view, banners, demo data
src/check.rs       `jaunt check`
```

## 5. Wire protocol (application level)

Two ALPNs.

**`jaunt/1`** on the machine's real endpoint. The endpoint's after-handshake hook rejects
any remote id that is not in the roster or is tombstoned, before any stream is accepted;
the check compares the raw 32 key bytes. Every stream additionally re-checks membership
when it opens. Each request is one bidirectional QUIC stream whose first frame is a
length-prefixed header naming the service:

| Service | Purpose |
| --- | --- |
| `Sync` | exchange rosters and fleet settings; run first on every new connection and whenever the roster changes |
| `Ping` | liveness/RTT, machine facts for the mesh view (host mode, session count, version) |
| `Shell { session, term, cols, rows, env }` | PTY running `tmux new-session -A -s <session>`; frames: data, resize, exit status |
| `Exec { argv }` | run a command without a PTY; stdin/stdout/stderr frames, exit status |
| `Tcp { port }` | connect to `127.0.0.1:<port>` on the host and splice (loopback only) |
| `Copy { .. }` | send or fetch files (tar stream) |

Frames are length-prefixed (u32) with a one-byte type; maximum frame 1 MiB; headers are
serde structures with a size cap; unknown services are refused. Version mismatch is
reported in words, not as a hang.

**`jaunt-pair/1`** on a short-lived pairing endpoint (§6). Nothing else is reachable there.

QUIC settings: keep-alive 5 s, idle timeout 30 s. iroh migrates the connection across
network changes by itself; when a connection is truly lost the idle timeout frees the
host's side (the tmux session stays).

## 6. Linking (pairing)

The ritual: on a machine already in the mesh (or the first machine) run `jaunt link`; it
shows a code like `4127-tremor-kudos`. On the new machine run `jaunt join` and type the code
when asked (it is read from the terminal or stdin, not taken from the command line, where
every local user could read it while the join runs; the command-line form still works and
warns).
Both screens show the same sigil and three words; confirm on both; done. The new machine
receives the roster, and every other machine in the mesh learns of it without being
touched.

The first time, the new machine has no relay configured: `jaunt join` asks for it (or
takes `--relay URL` / `JAUNT_RELAY`, or the install line carries it). Two machines on the
same LAN can link with no relay at all through mDNS (best effort).

How it works. This is the magic-wormhole construction, composed from standard parts; no
new cryptography:

1. **Code** = nameplate (4 random digits) + two words from the EFF large wordlist drawn
   with the OS CSPRNG (25.8 bits of secret). The nameplate is not secret.
2. **Rendezvous.** The inviter's daemon opens a second, temporary iroh endpoint whose key
   is derived from the nameplate alone (HKDF-SHA256, label `jaunt/pair/1/nameplate`). The
   joiner derives the same public id and dials it through the relay. Anyone who guesses
   the nameplate can reach or impersonate this endpoint; that gives them nothing but the
   chance to make one guess at step 3. The TLS on this connection is treated as
   unauthenticated transport.
3. **PAKE.** SPAKE2 (RFC 9382, Ed25519 group, asymmetric roles, identities
   `jaunt-inviter` / `jaunt-joiner`) with the full code as the password. An attacker in
   the middle gets exactly one online guess (1 in 60 million); a wrong guess burns the
   code. Codes expire after 5 minutes and work once.
4. **Sealed exchange.** From the SPAKE2 key, HKDF-SHA256 derives one ChaCha20-Poly1305 key
   per direction. Each side sends its real id and name, and an ed25519 signature with its
   own machine key over a value derived from this session's key (proof that it holds the
   id it claims; the same key and signature scheme the machine already uses). A failed
   open is a wrong code; a missing or wrong proof ends the pairing.
5. **Human check.** Both sides derive a sigil (a small symmetric glyph) and three words
   from SHA-256 over the SPAKE2 key alone (the key covers the whole SPAKE2 transcript; the
   ids are bound by the proofs, so no field chosen after the key is known feeds the
   check), display them, and ask for confirmation. This is a second line in case the PAKE
   implementation is flawed; it is not the primary defence (without a commitment step a
   determined attacker with the code could grind SPAKE2 shares for a matching sigil; the
   three words are the harder part to match), and the docs do not oversell it.
6. Only after both confirmations (sent sealed) does the inviter send the roster and fleet
   settings (sealed); the joiner checks them and acknowledges; the inviter writes its
   roster and says so; then the joiner writes its own. The pairing endpoint is closed and
   the new entry pushed to the rest of the mesh. If the last message is lost, the joiner
   simply joins again with a new code (a member that proves its key may link again).
7. A joining machine tries each code once: whatever went wrong, the same code is not
   tried again from that machine (a man in the middle would get another guess).

`--yes` skips the confirmation prompt for scripted tests only.

## 7. Roster, trust and revocation

The roster is a small replicated set:

- **entries**: `{ id, name, added_at, added_by }`
- **tombstones**: `{ id, revoked_at, revoked_by }`, permanent
- **fleet settings**: relay list, last-writer-wins by timestamp

A machine is a member iff it has an entry and no tombstone. Merge is union of entries and
union of tombstones; tombstones always win, so a revoked key can never come back (a
recovered laptop gets a new key with `jaunt reset` and is linked again). Sync runs on
every new connection in both directions and is pushed on every change; the daemon also
retries all known peers every 30 s, which is what spreads a revocation to machines that
were offline.

**Trust is flat.** Every member can open a shell on every other member, so every member
may also add and revoke members; there is no fleet master key to lose. The cost, stated in
the README: a stolen, unlocked, not-yet-revoked laptop is the whole mesh until it is
revoked, and it could link an attacker's machine or revoke the others first. Therefore:

- `jaunt unlink <name>` tombstones the machine, immediately closes its connections and
  kills its sessions on this machine, pushes the tombstone to every reachable machine and
  **prints which machines confirmed and which have not yet heard**. A machine that has not
  heard still accepts the revoked key until it syncs with any informed machine.
- `jaunt unlink <name> --cascade` also revokes every machine that `<name>` added
  (`added_by`), and plain `unlink` warns when such machines exist.
- If the others were revoked by a thief, recovery is local: `jaunt reset` on each machine
  and link again. The Cloudflare path remains as the way in meanwhile.
- `jaunt leave` removes this machine from the mesh (tombstones itself, pushes, wipes local
  roster and key).

## 8. Hopping

| Command | Does |
| --- | --- |
| `jaunt` | on a terminal: the live mesh view (§11); arrow keys or `j`/`k` to choose, Enter to hop, `q` to quit. Piped: same as `jaunt ls`. |
| `jaunt <machine>[:session]` | hop: attach (creating if needed) tmux session `session` (default `jaunt`) on that machine. Unambiguous prefixes of names work. |
| `jaunt ls [--json]` | machines, state (direct / relay / offline), latency, host mode, sessions. Plain tab-separated text when piped. |
| `jaunt run <machine> -- cmd…` | run one command there; exit status and stdio pass through. |
| `jaunt cp <src> <dst>` | copy files; either side may be `machine:path`. |
| `jaunt fwd <machine> <port>[:<localport>]` | forward a local port to that machine's loopback. |
| `jaunt pipe <machine> [port]` | stdio ⇄ that machine's loopback port (default 22): `ProxyCommand jaunt pipe %h` puts real OpenSSH (scp, rsync, git, editors' remote modes) on top of jaunt as a second, audited layer where sshd exists. |
| `jaunt link`, `jaunt join <code>`, `jaunt unlink <name>`, `jaunt leave`, `jaunt reset` | §6, §7 |
| `jaunt host on\|off\|status` | §9 |
| `jaunt up` / `jaunt down` | install+start / stop the user service. |
| `jaunt check` | diagnose: daemon, relay reachability, UDP, tmux, host-mode prerequisites. Never called "doctor". |
| `jaunt name <new>` | rename this machine. |
| `jaunt demo` | the mesh view and pairing ritual against simulated data, for trying the look and for captures. |

**Session continuity.** Persistence is tmux on the host: the session outlives the
connection, the client, and the daemon. The client side:

- A network change (Wi-Fi to hotspot) is absorbed by QUIC migration; nothing visible
  happens beyond the path indicator.
- When the connection is lost (lid closed, long outage), the client keeps the terminal,
  shows one status line (`fern ╌ link lost · retrying · ~. to give up`), retries with
  backoff, and re-attaches when the machine is back; tmux repaints the screen.
- `Enter ~ .` disconnects (session kept), `Enter ~ ?` lists escapes, as in OpenSSH.
- Without tmux on the host the hop is a plain login shell and jaunt says it will not
  survive a disconnect.

**Onward hops.** Each machine is a full member, so `jaunt moss` typed inside a session on
fern hops onward from fern; from the first machine you can also hop to moss directly in
another window. The remote shell gets `JAUNT_FROM=<name>` and `JAUNT_MACHINE=<name>`.

**tmux fit** (`contrib/tmux/jaunt.tmux`, optional, installed by `jaunt tmux-setup` into
`~/.config/jaunt/` and sourced by one line the owner adds; for rihla users the line goes in
`~/.config/rihla/tmux/local.conf`): `prefix + J` opens the mesh picker in a popup and puts
the hop in a new local window named after the machine, so each machine is a window and
none is lost; `F12` turns the local tmux's keys off and on so the prefix reaches the
remote tmux (the status bar shows when it is off). It does not bind `Ctrl-h/j/k/l` or
change the prefix. Without the snippet, a hop from inside tmux still works; jaunt prints
the one-time hint about `F12`/prefix-twice.

**Clipboard** needs nothing: OSC 52 from the remote tmux passes through the PTY.

## 9. Laptops that stay reachable (host mode)

Full research: `docs/research/awake.md`. The design:

- `jaunt host on` (persisted in config) asks the daemon to keep this machine reachable.
  The guard holds a logind **block inhibitor for `handle-lid-switch:sleep`** (never
  `idle`, so the screen still locks) **only while all hold**: host mode is on, the
  machine is on mains power, and it is not too hot.
- Mains power is detected the way systemd does (power supplies of type Mains/USB, online;
  batteries of peripherals ignored). Lid state from logind/procfs. Temperature from
  thermal zones and hwmon.
- **Safety rules** (the closed bag): on losing mains power, or ≥ 90 °C sustained 120 s (or
  within 5 °C of a sensor's critical point), the guard releases the lock and, if the lid
  is closed, calls `systemctl suspend` itself; after a thermal trip it stays released
  until the lid is opened. The lock is a file descriptor held through a child whose life
  is tied to the daemon, so a crash releases it (the safe direction); the unit uses
  `WatchdogSec` so a hang does too. Poll every 5 s.
- Works without root for a logged-in local session. After a reboot with no local login,
  logind refuses the lock; `jaunt check` says so and `scripts/root-extras/` holds the
  small owner-run polkit rule (and the optional logind drop-in and Wake-on-LAN helper),
  each with what it changes and how to undo it.
- `jaunt ls` shows each machine's host state (`awake`, `will sleep on lid close`, `on
  battery`) so the owner knows before leaving a laptop behind.
- Waking a sleeping laptop remotely is not promised: Wake-on-LAN works only from the same
  LAN and scheduled wake needs root. Documented, with the helper, not in the core.
- macOS: the daemon and hopping are expected to build and work (unverified); closed-lid
  hosting is not possible without root (`pmset disablesleep`), so `host on` on macOS only
  prevents idle sleep with the lid open (`caffeinate`) and says so. Everything about
  macOS is marked unverified.

## 10. The relay

`iroh-relay`, unmodified, hosted by the owner. It needs HTTPS (443, its own certificate)
and UDP 7842 reachable from the internet. `deploy/relay/` contains a Dockerfile, a config
and step-by-step notes for: a small VPS (recommended), Fly.io (not recommended: its UDP
needs a bind iroh-relay cannot be configured for as shipped), and "desktop behind the
existing Cloudflare tunnel" as a fallback-only relay
(works for relaying, but machines behind NAT will not find direct paths through it).
Nothing is deployed by this project. The relay sees: which ids are online, from which IP
addresses, who talks to whom and how much. It cannot read or alter traffic and cannot join
the mesh. The relay's own access control stays open: an id allowlist would break pairing
(its endpoints have ids nobody knows in advance) and jaunt does not send the relay a token.

## 11. The look

The terminal presence is a requirement. Principles:

- **One visual language.** A machine is a node (`●` up, `○` down). A link is a line whose
  style is its path: solid `━` green for direct, dashed `╌` amber for relayed, nothing
  and dim for offline, red for revoked. The same vocabulary appears in the mesh view,
  `jaunt ls`, the hop banner and the pairing ritual.
- **Palette.** Works in 16 colours; upgrades with `COLORTERM=truecolor` to: accent violet
  `#b4a0ff` → cyan `#6fe3e1` (wordmark gradient, selection), direct green `#8ee6a0`,
  relay amber `#f2c97d`, alert red `#f28b9a`, dim `#6c7086`, text default foreground.
  Never set a background colour except for the selection bar.
- **Glyphs** every monospace font has (box drawing, block elements `▁▂▃▄▅▆▇█`, `●○◆▸`,
  braille for spinners). No Nerd Font glyphs. `JAUNT_ASCII=1` or a non-UTF-8 locale
  switches to an ASCII set. `NO_COLOR`, `--no-color` and non-TTY output are honoured:
  piped output is plain, stable, tab-separated, with no escape codes or animation.
- **Motion** is information: a pulse travels along a link when traffic or a handshake
  happens; latency is a sparkline of the last minute; a path change redraws the line
  style. Nothing blocks: animations never delay a hop, and cap at ~15 fps.
- **Mesh view** (`jaunt`): header with wordmark, this machine, relay state; the mesh drawn
  as this machine on the left with a link to each peer (style = path, label = latency +
  sparkline + remote address kind), peer column with name, host state, sessions; an event
  log underneath (handshakes, path upgrades relay→direct, roams, syncs, link/unlink). A
  key bar at the bottom. Degrades cleanly to 80×24 and to narrow panes (tmux popup).
- **`jaunt ls`**: the same information as a static, screenshot-worthy block.
- **Hop**: a one-line banner on the way in (`forge ━━▸ fern · direct · 0.8 ms`) and on the
  way out (`◂━━ back on forge · session kept on fern`).
- **Pairing ritual**: two nodes with a gap between them; the code shown large and
  speakable; steps tick as they happen (rendezvous, key exchange, identities sealed);
  the link closes with a pulse; the sigil and three words appear on both; then the roster
  fan-out (`told moss ✓ tin …`).
- Looks are verified from real captures (rendered to PNG from terminal output at 80×24
  and 120×36, truecolor and 16-colour and no-colour) and iterated.

## 12. Threat model

**Assets:** a shell as the owner on every machine. **Trusted:** the machines in the
roster and the owner's user account on each. **Not trusted:** the network, the relay and
whoever hosts it, any machine not in the roster.

| Defended | How |
| --- | --- |
| Eavesdropping or tampering on any path, including by the relay | QUIC/TLS 1.3 end to end between machine keys; the relay forwards ciphertext |
| An unknown machine connecting | the handshake authenticates the peer's key; non-members are rejected before any service code runs |
| Someone who learns the relay URL or a machine id | ids are public keys; knowing them grants nothing |
| An attacker racing or intercepting a pairing | PAKE: one online guess per code, 1 in 6×10⁷, code burnt on failure, 5-minute life; plus the on-screen sigil check |
| A lost laptop, after `unlink` | tombstone is permanent, pushed to all reachable machines, spread by sync; sessions killed |
| Other local users on a machine | socket and key are 0600 in 0700 directories |
| A laptop overheating in a bag | host mode holds the lock only on mains power and below the thermal limit; fails towards sleep |

| Not defended | Why / what to do |
| --- | --- |
| A stolen laptop that is unlocked and not yet revoked | it is a member; revoke from any other machine. Use disk encryption and a screen lock; the key file is not passphrase-protected (the daemon must start unattended) |
| A machine that was offline when the revocation was issued | it honours the revoked key until it syncs with any informed machine; `unlink` lists who has not heard |
| A compromised member machine | flat trust: it can reach every other member, add members and revoke |
| Traffic analysis | the relay and network observers see who talks to whom, when, how much; the machine id is visible in the TLS handshake, so an observer can recognise a machine across networks |
| Bugs in iroh/noq, in jaunt itself, or in the SPAKE2 crate | none has a published third-party audit. The pre-authentication surface is iroh's handshake. For a second independent layer run OpenSSH over `jaunt pipe` |
| Denial of service | the relay can drop traffic; anyone can occupy a pairing nameplate (pairing fails, nothing leaks) |
| root on the host, malware in the owner's account | out of scope |

**Compared with SSH through the Cloudflare tunnel.** Speed: Cloudflare carries every byte
through its edge over TCP (measured here: tens of ms to the nearest edge, with jitter;
twice per keystroke echo); jaunt's direct path is the network distance between the two
machines and nothing else, and its relayed path is comparable to Cloudflare's. Security:
the Cloudflare setup rests on OpenSSH (decades of review) and can add an Access policy
with SSO/MFA, central audit logs, account recovery and DDoS shielding; it exposes no port
at all; those are real strengths jaunt does not have. Its costs: a third party is in the
path and in control of reachability (it can observe metadata and cut access, and whoever
takes over the Cloudflare account can re-route the tunnel, though they would still need
an SSH key); setup per machine. jaunt removes the third party and the per-machine setup
and encrypts end to end between the owner's own keys, but it is new code on a younger
stack, a single layer by default, and it listens on a UDP port for handshakes. Hence: keep
the Cloudflare path until jaunt has proven itself, and use `jaunt pipe` + SSH where two
layers are wanted.

## 13. Constraints on the build

Nothing on the real system is changed: no sshd, tunnel, firewall or system unit; the tool
is not enrolled on the owner's machines. Tests run in the Docker harness
(`test/harness`) or under `JAUNT_HOME` with `tmux -L jaunt-test`. Release artefact: a
static x86_64 Linux binary (musl, built in a container); other targets build from source.

## 14. Verification (what "done" is measured by)

`test/e2e/` scripts over the harness (relay + three NATed machines): link a⇄b with the
ritual; b appears on a third machine after linking it; `ls` shows paths; hop and run a
command; onward hop a→b→c; direct path between a and b; relay path to c behind the hard
NAT; session survives a link drop, a roam to another LAN and a freeze; unlink locks a
machine out (new connections refused, existing sessions killed, a machine that was offline
learns on return); no packets go anywhere but the relay and peers; keystroke latency of
jaunt direct and jaunt relayed against SSH through the relay host, with the harness
delays stated. Unit tests cover roster merge, pairing (wrong code, replay, expiry, burn),
frame parsing limits, and the awake state machine on fake sysfs trees.

## As built: deviations

The body above is the contract as agreed (§6 and §10 were edited in place during hardening
to match the code). This list records where the built program differs from it or goes
beyond it, and why. Sources: `docs/internals.md`, the fixer and verifier reports, the code.

| § | Contract | As built | Why |
| --- | --- | --- | --- |
| 2, 6 | two machines on one LAN can link without a relay through mDNS | not built: linking always needs a relay (a local test relay over plain HTTP works) | stage 1 had no mDNS; the endpoint contacts only the relay and the peers, which `08-nocontact` checks |
| 4 | files as in the table | the single-instance lock is `data/daemon.lock` next to the key and records the socket path; a client that finds no daemon at its own socket tries the recorded one. Extra state: `state/addrs.json` (last direct addresses per peer), `state/tmux-owners.json` (who created which session). Extra config: `portmap`, `port`, `relay_ca`, `tmux_socket` | one daemon per identity even when two contexts disagree about `XDG_RUNTIME_DIR`; address hints and session owners (below) |
| 4 | client commands start the daemon | as specified, and the detached daemon runs in `$HOME` with an allowlisted environment and a PATH of absolute entries only; the client talks only to a socket in its own directory whose server has its uid (`SO_PEERCRED`) | found by the local security review: a planted `tmux` or a foreign socket |
| 5 | `Copy` is a tar stream | own framing: a JSON plan, one JSON entry per file followed by its bytes as data frames, `skip` and `abort` messages; receiving checks every path component and never writes through a symlink | control over path safety and per-entry error reporting |
| 5 | services: Sync, Ping, Shell, Exec, Tcp, Copy | also `kill_sessions` (ends the tmux sessions a revoked machine created, on request of `unlink --kill-sessions`) | H9 |
| 6 | pairing as in steps 1 to 7 | as in the edited text: pairing version 2 (a version 1 peer is told to update), the check over the SPAKE2 key alone, signature proofs of the machine keys, roster only after both confirmations, a final acknowledgement, each code tried once per joining machine (kept in memory only, so a daemon restart forgets it), up to 16 incoming rendezvous connections handled side by side. A code on the command line still works, with a warning | adversarial review PAIR-1 to PAIR-9 |
| 7 | `unlink` kills the revoked machine's sessions on this machine | it ends every process started for that machine (shell clients, commands; SIGHUP, SIGKILL after 1 s, with up to 3 s grace for the serving stream so the SIGKILL lands) and every stream it has open, but keeps the tmux sessions unless `--kill-sessions` is given; it always prints what revocation does not undo | the sessions are the owner's and may hold work; review R3, H9 |
| 7 | a machine that has not heard accepts the revoked key until it syncs | **settling** added: after an absence (daemon start, the monitor not running for 5 s, or no contact with any member for 60 s, including a network-only outage with the daemon running) a daemon is unsettled: it serves Sync and Ping, but a peer's shell, command, port, copy or session kill only after a Sync with a member other than that peer, or when no such member exists, or (provisionally, until the next absence) after a settling round that started after the requester's connection was established reached no other member; rounds during an outage never count and no timer settles it. The first such request waits for that round (every other member, relay and direct hints, at most 5 s); later ones do not. Residual window: a revoked machine is served only if no informed member answered within 5 s of its first request, and then only until an informed member becomes reachable; an outage under a minute with no freeze is not an absence | H1; the network-only case found in verification |
| 7 | roster: entries, tombstones, fleet settings | also limits (256 members, 1024 entries, 512 tombstones for unknown ids, 8 relays) and a bounded, item-by-item merge that takes tombstones first, so a junk-stuffed roster cannot block a revocation. A compromised member can still fill the entry limit (new links then fail until `jaunt reset`) and set `added_by` so that `--cascade` misses machines it added | review R1 |
| 7 | `--cascade` revokes what `<name>` added | as specified, also after `<name>` was revoked: with `--cascade` or `--kill-sessions`, a name that matches no member is looked up among revoked machines (their roster entries keep the name, so the roster format is unchanged; a tombstone without an entry and same-named revoked machines are found by id prefix) | found while writing the README |
| 8 | `jaunt join <code>` | `jaunt join` reads the code from the terminal or stdin | review PAIR-2 |
| 8 | command table | also `jaunt relay [set URL… --ca FILE]`, `jaunt uninstall`, `jaunt hop`, `jaunt daemon`, and hidden `jaunt pick` (the tmux popup) and `jaunt debug` (test bootstrap, refused unless `JAUNT_HOME` is set) | needed in use and in tests |
| 8 | a network change is absorbed; nothing visible happens | true for a plain roam (0.5 s in 10 of 10 runs). A roam onto a hard NAT right after an outage took 17 to 56 s, because iroh 1.3's relay reconnect backoff is not cut short by a network change; the client shows a "no answer for Ns · waiting for the link" line meanwhile | H3, not fixed: no sound fix inside jaunt with iroh 1.3 |
| 8, 10 | (not specified) | port mapping (UPnP, NAT-PMP, PCP) is off by default; `portmap = true` turns it on. Each peer's last direct addresses are kept and used as dial hints, and the daemon re-binds its last UDP port, so a restarted daemon finds a peer directly while the relay is down | it changes the owner's router (H4); H2 |
| 9 | durations, debounce, failsafe as sketched | durations are leaky accumulators (time out of a condition pays back at half speed); after a power-caused release the lock is taken again only once mains has been steady 20 s to 2 min; sensor reads have a 2 s deadline; if the guard stalls for 25 s the daemon kills the lock holder by pid; once CPU sensors have been seen, losing them counts as unknown heat | review L3, L4, L12 |
| 9 | macOS expected to build and work (unverified) | never built | no Mac available |
| 10 | relay list from the fleet settings | the endpoint always has a relay transport (an empty relay map rather than relays disabled), so a relay set on a running daemon, or received by roster sync, is used at once; `jaunt relay set` reports whether the daemon connected | a daemon started without a relay never connected to one set later |
| 10 | relay access left open | as specified; iroh-relay's shared-token mode is not supported (jaunt sends no token, so every machine would be refused). `http://` relay URLs are accepted, for the local test relay | H7 |
| 12 | threat model | additional defences: strings from other machines are printed without control characters; frames are read incrementally; the client checks the daemon socket's owner. Additional gaps: the install checksum comes from the same place as the binary (no signature); `jaunt fwd` listens on 127.0.0.1, so every local user can use a forward while it runs | reviews L8, L9, L13, N1, R5 |
| 13 | static musl release | built (`scripts/build-release.sh`, 10,712,904 bytes); `install.sh` also falls back to `gh release download` for a private repository | |
| 14 | verification | all listed e2e scenarios pass (9 scripts, 250 checks, with the release binary; `00-host.sh` on the host, 28 checks) and the unit tests listed exist. Not covered: a real systemd user service, any real laptop or network | see README "What was tested" |
