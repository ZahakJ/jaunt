# Try it on one machine


This runs two pretend machines on one computer, using a local test relay over plain HTTP. Nothing leaves the computer. It was run like this in a sandbox, with two differences: `iroh-relay` 1.3.0 was already installed (built with the same `cargo install` features), and the tmux socket had another name.

```sh
cargo install iroh-relay@1.3.0 --features server --locked     # the relay program
printf 'http_bind_addr = "127.0.0.1:3340"\nenable_metrics = false\n' > ~/relay-try.toml
iroh-relay --dev -c ~/relay-try.toml &                        # test relay, plain HTTP, local only

export JAUNT_TMUX_SOCKET=jaunt-try                            # keep test sessions off your own tmux
JAUNT_HOME=~/jaunt-try/benzene jaunt relay set http://127.0.0.1:3340
JAUNT_HOME=~/jaunt-try/benzene jaunt name benzene
JAUNT_HOME=~/jaunt-try/toluene  jaunt name toluene
JAUNT_HOME=~/jaunt-try/benzene jaunt link                       # terminal 1
JAUNT_HOME=~/jaunt-try/toluene  jaunt join --relay http://127.0.0.1:3340   # terminal 2 (same JAUNT_TMUX_SOCKET)
JAUNT_HOME=~/jaunt-try/benzene jaunt toluene                       # hop; Enter ~ . to leave
```

`JAUNT_HOME` keeps everything in one directory and away from your real configuration. To clean up: `JAUNT_HOME=~/jaunt-try/benzene jaunt down`, the same for toluene, stop the relay (`kill %1`), `tmux -L jaunt-try kill-server`, `rm -rf ~/jaunt-try ~/relay-try.toml`.

The same test relay can serve two machines on one home network if it binds to the LAN address instead of 127.0.0.1 (`http_bind_addr = "192.168.1.10:3340"`, each machine using `http://192.168.1.10:3340`). That variant has **not been tried**. A plain-HTTP relay is for trying out only: use the HTTPS relay from `deploy/relay/` for real.
