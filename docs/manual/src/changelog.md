# Changelog

Each release has one asset, the static Linux binary `jaunt-x86_64-linux-musl`, and its `.sha256` file, on the [releases page](https://github.com/ZahakJ/jaunt/releases). Releases are not signed; see [Install](install.md) for what the checksum does and does not prove. Dates are the publication dates GitHub records for each release (UTC). Under each release are its release notes and the subjects of the commits since the previous tag.

The version you have is printed by `jaunt --version`, and `jaunt check` reports it together with the version of the running daemon, which can differ after an upgrade until the daemon is restarted.

## Since v0.1.2 (no new release)

- 11 October 2026: the repository is public. The install line is now `curl -fsSL https://raw.githubusercontent.com/ZahakJ/jaunt/main/install.sh | sh` and no longer needs the GitHub CLI. The binary is unchanged.

## v0.1.2, 5 October 2026

Release notes: *Fix: jaunt up failed when an earlier command had already started the daemon by hand.*

Commits since v0.1.1:

- `7438136` jaunt up hands over from a daemon started outside systemd; v0.1.2
- `b55deb6` Relay on the desktop behind an existing Cloudflare tunnel: owner-run setup script

In practice: if any `jaunt` command had started the daemon before `jaunt up` was run, `jaunt up` failed. From this release it takes over from that daemon. The second commit added `deploy/relay/desktop-tunnel.sh`, which sets up the relay on a desktop behind an existing Cloudflare tunnel.

## v0.1.1, 5 October 2026

Release notes: *Demo machine names changed (benzene, toluene, xylene, phenol, styrene). No functional change from 0.1.0.*

Commits since v0.1.0:

- `cee980c` README made short; details moved to docs/; relay options and costs made plain
- `ce140c3` Demo names: benzene, toluene, xylene, phenol, styrene; v0.1.1
- `32e2eb4` README: install line verified against the private release

## v0.1.0, 5 October 2026

Release notes: *First build. Static x86_64 Linux binary. Tested only in a simulated NAT lab; see the README for what was and was not verified.*

Commits up to v0.1.0, oldest last:

- `15e6c71` Settling: one wait per absence, provisional settle after a round with the network up
- `e118cef` Settling v2, live relay changes, unlink --cascade on revoked machines
- `f26a870` README from verified facts; MIT license; DESIGN as-built deviations; docs tidied
- `68582be` Verification fixes: R1 tombstone overflow, revoked SIGHUP-immune exec, join hint
- `40c1811` e2e: settle race, kill-sessions, relay-down restart, stall line, code on stdin
- `fe5e9ad` Docs: pairing v2, settling, hints; relay notes corrected; no owner details
- `47e30f3` Harden pairing, revocation, roster sync and the local daemon
- `3104f7f` Host mode: leaky debounce, sensor read deadline, failsafe holder kill
- `fae792f` UI: visible relay waypoint, shared sparkline scale, folded event log, detail panel
- `de5ff59` Integrate look, host mode and services; release build, install, relay deploy notes
- `090c13f` Trust: pairing ritual, roster propagation, unlink/leave/reset
- `d6e33c8` Core: daemon, roster sync, net, wire/IPC protocol, shell and exec
- `044539c` Design contract, research notes and NAT test harness

The order of the work is visible in these subjects: the design contract and the test lab came first, then the core daemon, then trust (pairing, roster, revocation), then the look, host mode and the services, then a round of hardening after an adversarial review, and last the settling rule that closes most of the revocation window after a machine has been away.
