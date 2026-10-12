# Security

jaunt gives a shell as you on every machine in your mesh, so its security model is written down in full: [`docs/security.md`](docs/security.md) lists what is defended, what is not, and how jaunt compares with SSH through a tunnel and with Tailscale. Read that before relying on it.

**What you should know.** jaunt has had no outside security audit, and neither have the iroh library and the SPAKE2 crate under it. Releases are not signed; `install.sh` checks a checksum that comes from the same place as the binary, which catches a corrupted download and nothing more. A stolen, unlocked laptop is a full member of the mesh until you revoke it.

## Reporting a vulnerability

Please report privately through [GitHub's vulnerability reporting](https://github.com/ZahakJ/jaunt/security/advisories/new) rather than in a public issue. Include the version, how your machines are connected (relay, direct, same LAN), the steps, and what an attacker gains. You will get an acknowledgement within a week and a fix or a reasoned answer as soon as one exists. There is no bounty programme.

## Supported versions

The latest release only.
