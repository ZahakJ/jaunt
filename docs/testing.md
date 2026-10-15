# Testing

```sh
cargo test                  # unit tests
test/e2e/run-all.sh         # end-to-end, in the Docker NAT lab (no root)
```

Unit tests cover roster merging (including hostile rosters), attacks on pairing, frame
limits, the host-mode guard on fake sysfs trees, settling and local hardening.

The lab (`test/harness`) runs machines a and b each behind their own home-router NAT,
machine c behind a hard (symmetric) NAT, and a real `iroh-relay` 1.3 with TLS on a
simulated internet, all in containers on one host. The e2e scripts drive the release
binary through link, join, hop, onward hop, run, cp, fwd, pipe with OpenSSH, unlink and
lock-out, a revoked machine racing a frozen or disconnected one, direct and relayed paths,
the relay going down, drops, roams, freezes, daemon restarts and latency. `08-nocontact.sh`
checks from the routers that the daemon contacts nothing but the relay and its peers.
`09-latency.sh` writes keystroke round trips to [docs/measurements.md](https://github.com/ZahakJ/jaunt/blob/main/docs/measurements.md).
