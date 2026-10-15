# Security

jaunt gives a shell as you on every machine in the mesh.

**Trusted:** the machines on the roster and your user account on each. **Not trusted:** the network, the relay and whoever runs it, any machine not on the roster.

## What is defended

| Threat | How |
| --- | --- |
| someone reading or changing traffic, including the relay | QUIC with TLS 1.3, end to end between the machines' own keys; the relay forwards ciphertext |
| an unknown machine connecting | the handshake proves the peer's key; non-members are refused before any service code runs, and every stream checks again |
| someone who knows the relay URL or a machine id | ids are public keys; knowing one grants nothing |
| someone intercepting a pairing | SPAKE2: one guess per code (1 in 60 million), the code dies on a wrong guess, lives 5 minutes, works once; each side proves it holds the key it claims; the roster is sent only after both people said yes; a joining machine never tries the same code twice |
| a lost laptop, after `jaunt unlink` | permanent tombstone, pushed to every reachable machine and spread by sync; its connections are closed, its commands and shell clients ended; its tmux sessions too with `--kill-sessions` |
| a revoked machine racing a machine that was asleep, frozen, restarted or off the network | after such an absence a daemon first tries to sync with every other member (up to 5 s, once per absence) before it serves a machine shells, commands, ports or copies. |
| a revoked or broken member stuffing the roster to block revocations | roster merges are bounded and take tombstones first |
| other users on the same machine | key and socket are private (0600 in 0700 directories); the client checks the socket's owner; the pairing code is typed, not passed on the command line |
| strings from other machines messing up your terminal | control characters are removed before printing |
| a laptop overheating in a bag | host mode only on mains and below the heat limit; anything unknown means sleep |

## What is not defended

| Threat | What it means, what to do |
| --- | --- |
| a stolen laptop that is unlocked and not yet revoked | it is a full member until you revoke it. Use disk encryption and a screen lock. The key file has no passphrase (the daemon must start unattended) |
| a member that missed the revocation and cannot reach a machine that knows | when it is back it serves the revoked machine only if no machine that knows answered within 5 s of the revoked machine's first request, and then only until one becomes reachable (the revocation then ends its connections and processes). An outage shorter than a minute, with no sleep or freeze, does not count as an absence. `unlink` lists who has not heard |
| a compromised member | trust is flat: it can reach every machine, link new ones and revoke the others. It can also fill the roster's entry limit (new links then fail until `jaunt reset`) and lie about which machines it added, so `--cascade` misses them |
| traffic analysis | the relay and the network see who talks to whom, when and how much; the machine id is visible in the handshake, so an observer can recognise a machine across networks |
| denial of service | the relay can drop traffic; anyone who knows the relay URL can block pairing (the pairing fails, nothing leaks) |
| a tampered release | `install.sh` checks the binary only against a checksum from the same place; there is no signature |
| root on a machine, or malware in your account | out of scope |

**Second layer.** Where sshd listens on loopback, run OpenSSH through jaunt; SSH's key check then stands behind jaunt's:

```
# ~/.ssh/config: the Host names must be jaunt machine names
Host benzene toluene xylene phenol
    ProxyCommand jaunt pipe %h
```

## Revoking a lost laptop

From any other machine in the mesh:

```sh
jaunt unlink toluene --kill-sessions
```

- toluene is refused here at once. Its connections are closed, every command and shell client it was running here is ended, and with `--kill-sessions` the tmux sessions it created are ended here and on every machine that confirms.
- The revocation is pushed to every machine that can be reached. The output lists **who confirmed** and **who has not heard yet**.
- If toluene linked other machines and you want them gone too: `jaunt unlink toluene --cascade`. Plain `unlink` warns when such machines exist. This also works after toluene was revoked. If several revoked machines share the name, give the start of the id instead (`jaunt unlink 1a013197 --cascade`; `jaunt ls --json` lists revoked ids).

**The window.** A machine that has not heard still accepts toluene until it syncs with one that has (daemons retry every 30 s and on every reconnect). A machine coming back from sleep, a freeze, a restart or a network outage of a minute or more first tries the other machines; it lets toluene in only if none that knows answered within 5 s of toluene's first request, and only until one is reachable. The first hop into a machine back from sleep can therefore wait up to 5 s. Get every machine on the "not heard yet" list online, and check `jaunt unlink` or `jaunt ls` on it.

**What revocation does not undo** (jaunt prints this reminder too): processes toluene started outside its own tmux sessions, files it wrote or copied, cron jobs, SSH keys or config it changed. If the laptop may be in someone else's hands, check the machines it could reach and replace any secrets it held.

If the laptop comes back: run `jaunt reset` on it (new key) and link it again; its old key stays revoked forever.

If a thief revoked your other machines first: run `jaunt reset` on each of yours and link them again.
