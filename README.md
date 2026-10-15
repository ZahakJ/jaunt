<p align="center"><img src="docs/media/banner.png" alt="jaunt" width="820"></p>

# jaunt

Open a terminal on any of your machines from any other. `jaunt toluene` attaches
a tmux session on the machine called toluene, over a direct encrypted path when
one can be made and through a relay you host when it cannot.

![Mesh view](captures/mesh-120x36-truecolor.png)

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/ZahakJ/jaunt/main/install.sh | sh
```

Installs one static binary to `~/.local/bin`; no root. Options go after
`sh -s --`, e.g. `| sh -s -- --name benzene`. Linux x86_64 has a release binary;
elsewhere, `cargo build --release`.

`jaunt demo` shows the interface on made-up machines.

## Usage

Every mesh needs one relay (`iroh-relay`); see
[deploy/relay/README.md](deploy/relay/README.md), or [docs/try-it.md](docs/try-it.md)
for a local test relay.

On the first machine:

```sh
jaunt relay set https://relay.example.net
jaunt up                 # install and start the user service
jaunt link               # prints a code like 4127-tremor-kudos
```

On the next:

```sh
jaunt up
jaunt join --relay https://relay.example.net    # type the code
```

Both screens show the same sigil and three words; press `y` on both if they
match. Further machines join from any linked one.

| Command | |
| --- | --- |
| `jaunt` | mesh view; Enter hops, `q` quits |
| `jaunt toluene[:session]` | hop to toluene |
| `jaunt ls` | machines, paths, latency, sessions |
| `jaunt run toluene -- cmd` | run a command there |
| `jaunt cp src toluene:dst` | copy files, either direction |
| `jaunt fwd toluene 8080 [--keep]` | forward a port; `--keep` holds it across restarts |
| `jaunt rfwd toluene 3000` | reverse forward, like `ssh -R` |
| `jaunt socks toluene` | SOCKS5 proxy through toluene |
| `jaunt ports toluene` | what listens there; a key forwards and opens it |
| `jaunt open toluene:7707` | forward a web port and open it in the browser |
| `jaunt tabs toluene` | pull its open browser tabs |
| `jaunt send-tab toluene URL` | push a URL to its browser |
| `jaunt clip on` | share the clipboard with machines that share theirs |
| `jaunt notify done` | notify the machine you hopped in from |
| `jaunt host on` | keep a laptop awake with the lid closed, on mains and below a heat limit |
| `jaunt unlink toluene` | revoke a machine |
| `jaunt check` | diagnose |

Enter `~` `.` leaves a hop; the session stays. A dropped link is retried and
re-attached. Links opened in a hop open in the browser you hopped from.

Documentation: [manual](https://zahakj.github.io/jaunt/manual/),
[commands](docs/commands.md), [ports, tabs and the clipboard](docs/tools.md), [tmux](docs/tmux.md), [host mode](docs/awake.md),
[security](docs/security.md), [design](docs/design.md).

## Build

```sh
cargo build --release
cargo test
test/e2e/run-all.sh      # end-to-end tests in a Docker NAT lab
```

## License

MIT
