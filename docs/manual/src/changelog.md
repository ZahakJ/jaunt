# Changelog

Releases: <https://github.com/ZahakJ/jaunt/releases>. Each has one static Linux binary,
`jaunt-x86_64-linux-musl`, and its `.sha256`.

## v0.2.0, 15 October 2026

- `jaunt ports` lists what listens on a machine's loopback; a key forwards one and opens it.
  `jaunt open toluene:7707` forwards a web port and opens it in one step.
- Held forwards: `jaunt fwd --keep`, `jaunt unfwd`, listed in `jaunt ls`, back after
  restarts; each connection dials afresh.
- `jaunt rfwd` (like `ssh -R`) and `jaunt socks` (a SOCKS5 proxy through another machine).
- `jaunt tabs` pulls another machine's open browser tabs; `jaunt send-tab` pushes URLs.
- Web links opened in a hop open in the browser of the machine you came from. Sessions
  jaunt starts see no display of the far machine, as with ssh.
- Clipboard sharing, opt-in on each machine: `jaunt clip on`.
- `jaunt notify` and `jaunt run --notify`.
- The install line no longer needs the GitHub CLI.

## v0.1.2, 5 October 2026

- `jaunt up` takes over from a daemon an earlier command had started, instead of failing.
- `deploy/relay/desktop-tunnel.sh`: run the relay on a desktop behind an existing
  Cloudflare tunnel.

## v0.1.1, 5 October 2026

- Demo machine names: benzene, toluene, xylene, phenol, styrene. No functional change.

## v0.1.0, 5 October 2026

- First release.
