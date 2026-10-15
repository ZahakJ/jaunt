# Contributing to jaunt

jaunt gives a shell on every machine in a mesh; changes are judged first by the threat model.

## Before you start

- Read `docs/design.md` and `docs/security.md`. If a change alters either, update the document in the same change.
- Port mapping by default, LAN pairing without a relay and predictive echo are out of scope; open an issue before building them.
- For anything larger than a fix, open an issue first.

## Setting up

```sh
git clone https://github.com/ZahakJ/jaunt.git && cd jaunt
cargo build --release          # target/release/jaunt
cargo test                     # unit tests
test/e2e/run-all.sh            # the Docker lab: three machines behind simulated NATs, no root
```

Screenshots come from `scripts/capture.sh`, which runs `jaunt demo` in a private tmux server.

## Rules of the code

- No networking or cryptography of our own: the transport is iroh, the pairing is SPAKE2 and standard pieces. A change that adds a protocol needs a very good reason.
- The daemon contacts nothing but the configured relay and the peers on the roster; a test asserts this and must keep passing.
- Nothing in tests may touch the real home directory: use `JAUNT_HOME`.

## Pull requests

One change per pull request, with the template filled in. Commit messages say what changed and why, in the imperative.

## Reporting problems

Bugs and feature requests go through the issue templates. Security problems go through [private vulnerability reporting](https://github.com/ZahakJ/jaunt/security/advisories/new); see `SECURITY.md`.
