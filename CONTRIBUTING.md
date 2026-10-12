# Contributing to jaunt

jaunt is a front door to every machine its owner has, so changes are judged first on what they do to the threat model and only then on everything else.

## Before you start

- Read `docs/DESIGN.md` (the contract the code is built against) and `docs/security.md` (what is and is not defended). If a change alters either, say so in the pull request and update the document in the same change.
- Read the [manual](https://zahakj.github.io/jaunt/manual/), especially *Internals* and *Decisions*. Several things are left out on purpose (port mapping, LAN pairing without a relay, predictive echo); reopen them in an issue before building them.
- For anything larger than a fix, open an issue first.

## Setting up

```sh
git clone https://github.com/ZahakJ/jaunt.git && cd jaunt
cargo build --release          # one static binary in target/release/jaunt
cargo test                     # unit tests
test/e2e/run-all.sh            # the Docker lab: three machines behind simulated NATs, no root
```

Captures for the manual and README come from `scripts/capture.sh`, which runs `jaunt demo` in a private tmux server; nothing it produces contains a real machine.

## Rules of the code

- No networking or cryptography of our own: the transport is iroh, the pairing is SPAKE2 and standard pieces. A change that adds a protocol needs a very good reason.
- The daemon contacts nothing but the configured relay and the peers on the roster; a test asserts this and must keep passing.
- Nothing in tests may touch the real home directory: use `JAUNT_HOME`.
- Every claim about what was tested goes in `docs/testing.md`, and anything untested stays marked as untested.

## Pull requests

One change per pull request, with the template filled in. Commit messages say what changed and why, in the imperative.

## Reporting problems

Bugs and feature requests go through the issue templates. Security problems go through [private vulnerability reporting](https://github.com/ZahakJ/jaunt/security/advisories/new); see `SECURITY.md`.
