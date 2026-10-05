# Security


What jaunt protects: a shell as you on every one of your machines. That makes it a front door to everything, so this section is long on purpose.

**Trusted:** the machines on the roster and your user account on each. **Not trusted:** the network, the relay and whoever runs it, any machine not on the roster.

### What is defended

| Threat | How |
| --- | --- |
| someone reading or changing traffic, including the relay | QUIC with TLS 1.3, end to end between the machines' own keys; the relay forwards ciphertext |
| an unknown machine connecting | the handshake proves the peer's key; non-members are refused before any service code runs, and every stream checks again |
| someone who knows the relay URL or a machine id | ids are public keys; knowing one grants nothing |
| someone intercepting a pairing | SPAKE2: one guess per code (1 in 60 million), the code dies on a wrong guess, lives 5 minutes, works once; each side proves it holds the key it claims; the roster is sent only after both people said yes; a joining machine never tries the same code twice |
| a lost laptop, after `jaunt unlink` | permanent tombstone, pushed to every reachable machine and spread by sync; its connections are closed, its commands and shell clients ended; its tmux sessions too with `--kill-sessions` |
| a revoked machine racing a machine that was asleep, frozen, restarted or off the network | after such an absence a daemon first tries to sync with every other member (up to 5 s, once per absence) before it serves a machine shells, commands, ports or copies. Tested in the lab: a freeze, and three network outages with the daemon running |
| a revoked or broken member stuffing the roster to block revocations | roster merges are bounded and take tombstones first |
| other users on the same machine | key and socket are private (0600 in 0700 directories); the client checks the socket's owner; the pairing code is typed, not passed on the command line |
| strings from other machines messing up your terminal | control characters are removed before printing |
| a laptop overheating in a bag | host mode only on mains and below the heat limit; anything unknown means sleep |

### What is not defended

| Threat | What it means, what to do |
| --- | --- |
| a stolen laptop that is unlocked and not yet revoked | it is a full member until you revoke it. Use disk encryption and a screen lock. The key file has no passphrase (the daemon must start unattended) |
| a member that missed the revocation and cannot reach a machine that knows | when it is back it serves the revoked machine only if no machine that knows answered within 5 s of the revoked machine's first request, and then only until one becomes reachable (the revocation then ends its connections and processes). An outage shorter than a minute, with no sleep or freeze, does not count as an absence. `unlink` lists who has not heard |
| a compromised member | trust is flat: it can reach every machine, link new ones and revoke the others. It can also fill the roster's entry limit (new links then fail until `jaunt reset`) and lie about which machines it added, so `--cascade` misses them |
| traffic analysis | the relay and the network see who talks to whom, when and how much; the machine id is visible in the handshake, so an observer can recognise a machine across networks |
| denial of service | the relay can drop traffic; anyone who knows the relay URL can block pairing (the pairing fails, nothing leaks) |
| a tampered release | `install.sh` checks the binary only against a checksum from the same place; there is no signature |
| bugs in iroh, in the SPAKE2 crate or in jaunt | see below |
| root on a machine, or malware in your account | out of scope |

**No audit.** iroh and its QUIC library (noq, n0's own fork of quinn) have no published third-party security audit. The RustCrypto `spake2` crate says plainly that it has had no formal review. jaunt itself has had one adversarial review by AI agents in this project, which found and fixed real problems (pairing, revocation races, roster limits, local socket and environment handling); no human security reviewer has looked at it. By default jaunt is a **single layer**: if the handshake code in iroh or jaunt's own check has a bug, there is nothing behind it.

**Two layers, if you want them.** Where a machine has sshd listening on its loopback, run real OpenSSH through jaunt. jaunt then only carries the bytes, and SSH's own key check stands behind it. Tested in the lab with `ssh` and `scp`:

```
# ~/.ssh/config: the Host names must be jaunt machine names
Host benzene toluene xylene phenol
    ProxyCommand jaunt pipe %h
```

### Compared with SSH through the Cloudflare tunnel

**Speed.** Through the tunnel every keystroke goes out to Cloudflare's edge and back over TCP, even when the laptop is next to the desktop. jaunt's direct path is just the distance between your two machines. In the lab (numbers below), jaunt on a direct path was 80.8 ms against 138.6 ms for SSH through a relay host, with a simulated 20 ms per router hop. jaunt through its relay (121.2 ms) is as fast as SSH through a relay with OpenSSH's keystroke-timing obfuscation turned off (120.9 ms), so the gain comes from the direct path, not from jaunt being faster software. That obfuscation, on by default in OpenSSH, adds the other 18 ms; jaunt does not do it. A real Cloudflare comparison was **not** measured: the tunnel was not touched. An earlier probe from the desktop measured 35 to 46 ms just to connect to the nearest Cloudflare edge.

**Security: where Cloudflare is stronger.**
- It rests on OpenSSH, with decades of review, and can add an Access policy with single sign-on and two-factor login. A stolen laptop with an SSH key still needs that login; a stolen, unlocked laptop with jaunt needs nothing until you revoke it.
- It has central logs, account recovery and DDoS protection.
- It opens no port on your machines. jaunt opens none on your router either, but its daemon listens on a UDP port for handshakes.
- Revocation is one dashboard click that applies everywhere at once. jaunt's revocation has to spread from machine to machine and has the offline-member window above.

**Where jaunt is stronger.** No third party in the path: Cloudflare can see who connects when, can cut access, and whoever takes over the Cloudflare account can re-route the tunnel (they would still need an SSH key). jaunt encrypts end to end between your own keys, and the relay, which you run, can never add a machine. No per-machine dashboard setup.

**Hence: keep the Cloudflare path** until jaunt has a track record on your machines, and use `jaunt pipe` with SSH where two layers matter.

### When Tailscale is simply the better choice

The research for this project ([`docs/research/survey.md`](research/survey.md)) says it plainly: Tailscale's free plan plus tmux (and mosh, for typing over bad links) gives the same speed win in about 15 minutes, on a WireGuard data plane that is formally verified and maintained by a company whose job it is, with phone, Windows and macOS clients and a better direct-path rate. Choose Tailscale if you want an audited, mature tool now, need phones or other operating systems, or do not mind root and an account. Turn on Tailnet Lock so Tailscale's servers cannot add machines.

jaunt makes sense if you want: no root, no system VPN device, no account and no company that can add machines to your network; a pairing ritual instead of a control plane; and the hop view, onward hops and lid-awake behaviour built in. It is not more mature than WireGuard and does not claim to be.

## Revoking a lost laptop


From any other machine in the mesh:

```sh
jaunt unlink toluene --kill-sessions
```

- toluene is refused here at once. Its connections are closed, every command and shell client it was running here is ended, and with `--kill-sessions` the tmux sessions it created are ended here and on every machine that confirms.
- The revocation is pushed to every machine that can be reached. The output lists **who confirmed** and **who has not heard yet**.
- If toluene linked other machines and you want them gone too: `jaunt unlink toluene --cascade`. Plain `unlink` warns when such machines exist. This also works after toluene was revoked. If several revoked machines share the name, give the start of the id instead (`jaunt unlink 1a013197 --cascade`; `jaunt ls --json` lists revoked ids).

**The window.** A machine that has not heard still accepts toluene until it syncs with one that has (daemons retry every 30 s and on every reconnect). A machine coming back from sleep, a freeze, a restart or a network outage of a minute or more first tries the other machines; it lets toluene in only if none that knows answered within 5 s of toluene's first request, and only until one is reachable. The cost: the first hop into a machine back from sleep can wait up to 5 s while the others sleep; the next hops do not. So: get every machine on the "not heard yet" list online, and check `jaunt unlink` or `jaunt ls` on it.

**What revocation does not undo** (jaunt prints this reminder too): processes toluene started outside its own tmux sessions, files it wrote or copied, cron jobs, SSH keys or config it changed. If the laptop may be in someone else's hands, check the machines it could reach and replace any secrets it held.

If the laptop comes back: run `jaunt reset` on it (new key) and link it again; its old key stays revoked forever.

If a thief revoked your other machines first: run `jaunt reset` on each of yours and link them again. The Cloudflare path is your way in meanwhile.
