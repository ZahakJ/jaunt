# Ports, tabs and the clipboard

What reaches past the terminal: web ports, browser tabs, links, the clipboard and notifications. The examples sit at benzene; toluene is the other machine.

## Web ports

```sh
jaunt ports toluene           # what listens on toluene's loopback
jaunt open toluene:7707       # forward it and open it in the browser here
```

`jaunt ports` lists every port that toluene's loopback reaches, with the program and its working directory:

```
 7707  node edgewake
 5432  postgres
 8000  python3 site
```

On a terminal each row has a key; press it to forward that port and open it in the browser. Piped, it prints tab-separated columns: port, bind address, program, directory. `--json` gives the full list.

`jaunt open toluene:7707/admin` forwards toluene's port 7707 to the same port here (or any free one, if that is taken), opens `http://127.0.0.1:7707/admin`, and returns. The forward is held by the background service until `jaunt unfwd toluene:7707` or the service stops; `--keep` keeps it across restarts.

## Forwards

| Command | Does |
| --- | --- |
| `jaunt fwd toluene 8080` | toluene's `127.0.0.1:8080` here at `127.0.0.1:8080`, until Ctrl-C |
| `jaunt fwd toluene 8080:9090` | the same, here at port 9090 |
| `jaunt fwd toluene 5432 --keep --name db` | held by the service, kept in `config.toml`, back after every restart |
| `jaunt unfwd db` | stop a held forward, by name or local port |
| `jaunt rfwd toluene 3000` | the reverse, like `ssh -R`: toluene's `127.0.0.1:3000` reaches port 3000 here, until Ctrl-C |
| `jaunt socks toluene [1080]` | a SOCKS5 proxy here whose connections leave from toluene |

Held forwards show in `jaunt ls` with their open connections and last error. Each connection dials afresh, so a held forward never needs restarting: it works again as soon as toluene is reachable, after sleep, a new network or a restart on either side. Connections that are open while a laptop roams carry on (the lab test moves a download across two network changes).

`jaunt socks` resolves names on toluene, so `socks5h://` reaches toluene's LAN by name:

```sh
jaunt socks toluene 1080 &
curl --proxy socks5h://127.0.0.1:1080 http://router.lan/
chromium --proxy-server=socks5://127.0.0.1:1080
```

## Browser tabs

```sh
jaunt tabs toluene            # pick some of toluene's open tabs; Enter opens them here
jaunt tabs toluene --all      # open every one
jaunt tabs toluene --list     # print them: browser, title, URL
jaunt send-tab toluene URL... # open URLs in toluene's browser
```

In the picker, type to filter, Tab marks a tab, Ctrl-A marks every tab shown, Enter opens what is marked (or the tab under the cursor).

Tabs are read from the session files the browsers keep on disk, never from the running browser: Firefox, LibreWolf, Waterfox, Zen and Floorp (`recovery.jsonlz4`); Chromium, Chrome, Brave, Edge and Vivaldi (`Session_*`); native, Flatpak and Snap installs, Linux and macOS. A browser writes this file every few seconds, so a tab opened a moment ago may be missing. Files untouched for two weeks are skipped. Only `http` and `https` tabs are listed.

## Links from the other machine

In a session jaunt starts, a program that opens a web link opens it in the browser of the machine you hopped in from: `gh auth login`, a dev server's `--open`, Python's `webbrowser`, `xdg-open https://…`. This works through `BROWSER` and an `xdg-open` placed first on the session's `PATH`; links still open on toluene when nobody is hopped in.

Like an ssh login, these sessions see no display on toluene (`DISPLAY` and `WAYLAND_DISPLAY` are empty), so nothing opens a window on a screen nobody is watching. Sessions created before an upgrade keep their old environment; `bridge_urls = false` in toluene's `config.toml` turns both off.

`jaunt send-tab URL` with no machine sends to the machine hopped in here.

## Clipboard

```sh
jaunt clip on                 # on each machine that should share
jaunt clip off
jaunt clip get toluene        # print toluene's clipboard
jaunt clip set toluene < f    # replace it
```

With sharing on at both ends, text copied on one machine is on the clipboard of the other within two seconds, in both directions, on every linked machine that shares and is online. Only text is shared, up to 512 KiB. A copy that a password manager marks as secret (KeePassXC, Bitwarden and others set a hint for this) stays where it is. A machine that does not share refuses `get` and `set`.

Linux needs `wl-clipboard` on Wayland, or `xclip` or `xsel` on X11; macOS has what it needs.

## Notifications

```sh
make; jaunt notify build done       # on toluene, in a hop: shows on benzene
jaunt notify --to toluene "lunch"   # on a named machine
jaunt run --notify toluene -- make  # here, when the command ends: how and after how long
```

`jaunt notify` with no `--to` goes to the machine hopped in here. The notification is titled with the sending machine's name. For every long command, in `~/.zshrc` on the far machine:

```sh
autoload -Uz add-zsh-hook
_jt_start() { _jt=$SECONDS }
_jt_done() { (( ${_jt:-$SECONDS} + 30 < SECONDS )) && jaunt notify "$(fc -ln -1)"; unset _jt }
[[ -n $JAUNT_FROM ]] && { add-zsh-hook preexec _jt_start; add-zsh-hook precmd _jt_done }
```

## Trust

Every linked machine can already run any command on every other one (`jaunt run`), so none of this gives a machine a power it lacked. What each tool does, and who can ask for it:

| Tool | The far machine | The local end |
| --- | --- | --- |
| `ports` | reads its own socket table; names only this user's processes | prints |
| `tabs` | reads session files, never writes; lists `http`/`https` only | opens what you choose |
| `send-tab`, links | opens only `http`/`https` URLs, never `file:` or other schemes | |
| `notify` | shows text, titled with the sender's name, control characters removed | |
| `clip` | refuses unless its owner ran `jaunt clip on` | |
| `fwd`, `open` | connects to its own loopback | listens on `127.0.0.1` only |
| `rfwd` | listens on its own `127.0.0.1` only, while the command runs | |
| `socks` | connects where it can reach | listens on `127.0.0.1` only |

Only linked machines can ask, over the same authenticated connection as a hop, and every request shows in the event log. A local end on `127.0.0.1` can be used by every user of the machine while it is open, as with `ssh -L`; on a shared machine forward only what anyone there may reach.
