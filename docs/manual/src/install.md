# Install

jaunt is one static binary. Installing it means putting that file in `~/.local/bin` on each machine you want to link. Nothing else is written unless you ask for it, and nothing needs root.

## The one-line install

```sh
curl -fsSL https://raw.githubusercontent.com/ZahakJ/jaunt/main/install.sh | sh
```

Run it as the user who will use jaunt, not as root; the script refuses to run as root. It does this, in order:

1. Picks the release asset for this machine. Only Linux on x86_64 has one (`jaunt-x86_64-linux-musl`); on anything else it stops and tells you to build from source.
2. Downloads the binary and its `.sha256` file from the latest GitHub release, over HTTPS only (it refuses plain `http://`, including a redirect to it).
3. Checks the binary against the checksum, runs it once with `--version` to make sure it works on this machine, and writes it to `~/.local/bin/jaunt`. The write is a copy into the same directory followed by a rename, so a daemon that is already running keeps its old file until it restarts.
4. Writes the machine name and the relay into jaunt's config file, if you gave `--name` or `--relay`. Values already in the config are kept.
5. Tells you whether `~/.local/bin` is on your `PATH`, and prints the next two commands. It starts nothing unless you pass `--up`.

The script is POSIX `sh`, and everything in it sits inside one function that runs on the last line, so a download cut off half way runs nothing.

Releases are not signed; the checksum catches a corrupted download.

### Passing options through the one-liner

To give the script options when it arrives through a pipe, pass them after `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/ZahakJ/jaunt/main/install.sh | sh -s -- --name toluene --relay https://relay.example.net
```

or download it first and run it as a file:

```sh
sh install.sh --relay https://relay.example.net --name toluene
```

## Every option

These come from the header of `install.sh`; `sh install.sh --help` prints the same list.

| Option | Does |
| --- | --- |
| `--version TAG` | install this release, for example `v0.1.2`, instead of the latest |
| `--relay URL` | write the relay URL into jaunt's config (`https://host[:port][/]`) |
| `--name NAME` | write this machine's name into jaunt's config: lowercase letters, digits and `-`, at most 32 characters, not starting or ending with `-` |
| `--up` | also install and start the user service, as `jaunt up` does |
| `--no-service` | do not suggest the service; the daemon then starts on first use. Cannot be combined with `--up` |
| `--from-file FILE` | install this binary instead of downloading; `FILE.sha256` is checked if it is next to it |
| `--bin-dir DIR` | where to put the binary (default `~/.local/bin`) |

Two environment variables change where the download comes from:

| Variable | Does |
| --- | --- |
| `JAUNT_REPO` | the GitHub repository as `owner/name` (default `ZahakJ/jaunt`) |
| `JAUNT_BASE_URL` | another download base. It must be `https://` or `file://` |

The script also reads `JAUNT_BIN_DIR` as the default for `--bin-dir`. If you point `JAUNT_REPO` at a private fork, the script falls back to `gh release download` when the plain download answers 404; for the public repository the GitHub CLI is not needed.

The config file it writes is `~/.config/jaunt/config.toml` (or `$XDG_CONFIG_HOME/jaunt/config.toml`), created with mode 0600 in a private directory.

## Installing without the network: `--from-file`

On a machine with no internet, or when you want to check the file yourself first, copy the binary over and install it from disk:

```sh
sh install.sh --from-file ./jaunt-x86_64-linux-musl --name xylene
```

If `jaunt-x86_64-linux-musl.sha256` sits next to it, the checksum is checked. If not, the script installs the binary unchecked and says so.

## Building from source

Every platform other than Linux x86_64 builds from source, with a Rust toolchain:

```sh
git clone https://github.com/ZahakJ/jaunt
cd jaunt
cargo build --release
sh install.sh --from-file target/release/jaunt
```

`cargo test` runs the unit tests. The release binary itself is built by `scripts/build-release.sh` in a throwaway `debian:12` container, as a static musl binary of about 10.7 MB.

## Supported platforms

| Platform | Status |
| --- | --- |
| Linux x86_64 | release binary |
| other Linux | build from source |
| macOS | build from source; host mode is limited (see [Host mode](awake.md)) |

The machine you hop *into* should have tmux. Without it a hop gets a plain shell that ends when the link drops; `jaunt check` warns about this.

## Demo

`jaunt demo` runs the interface on made-up machines:

```sh
jaunt demo               # the live mesh view on made-up machines
jaunt demo ls            # the same machines as jaunt ls prints them
jaunt demo link          # the pairing screens, inviting side
jaunt demo link --join   # the pairing screens, joining side
```

No daemon, network or relay is involved. Press `q` to leave the mesh view. Colour follows your terminal: `NO_COLOR` or `--no-color` turns it off, and `JAUNT_ASCII=1` swaps the box-drawing glyphs for plain ASCII.

## Next steps

On the first machine:

```sh
jaunt relay set https://relay.example.net   # once, on the first machine
jaunt up                                     # install and start the user service
jaunt link                                   # shows a code for the next machine
```

The relay comes first; its chapter is [The relay](relay.md). Then [Pairing](pairing.md).

`jaunt up` installs a systemd user service (`~/.config/systemd/user/jaunt.service`) so the daemon starts at login and survives logout. It uses `KillMode=process`, so restarting jaunt never kills your tmux sessions. Without the service, any `jaunt` command starts the daemon by itself when it needs it, and says so. `jaunt down` stops it.

## Uninstall

```sh
jaunt uninstall
```

This stops the service or the daemon, removes the unit, the config, the state and runtime directories, and the binary (only `~/.local/bin/jaunt`, and only if it is the binary that is running). It asks before deleting the key and the member list; `--yes` deletes them without asking, `--keep-key` keeps them without asking. Your tmux sessions are left alone.

Uninstalling does not tell the other machines. To revoke this one, run from another machine in the mesh:

```sh
jaunt unlink toluene
```

To take a machine out of the mesh in one step while it is still running, use `jaunt leave` instead (see [Pairing](pairing.md)).
