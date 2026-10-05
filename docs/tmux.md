# jaunt and tmux

## The tmux snippet


```sh
jaunt tmux-setup
```

then add this one line to your `~/.tmux.conf` (rihla users: `~/.config/rihla/tmux/local.conf`) and reload tmux:

```
source-file ~/.config/jaunt/jaunt.tmux
```

| Key | Does |
| --- | --- |
| prefix `J` (default prefix is Ctrl-b, so Ctrl-b then Shift-j) | the mesh view in a popup. Enter opens the hop in a **new window named after the machine**, so each machine gets its own window |
| `F12` | hand your keys to the tmux on the other machine. The status bar shows `F12 keys to remote` while this is on. Press `F12` again to get them back |

Nothing else is bound; your prefix and `Ctrl-h/j/k/l` are untouched.

## Nested tmux, for a tmux beginner


If you hop from inside tmux, there are now two tmux programs on your screen: yours (the outer one) and the one on the other machine (inside the window). Your prefix key (Ctrl-b) always goes to the **outer** one first.

- To talk to the **remote** tmux: press `F12` once (with the snippet), then use Ctrl-b as usual; press `F12` again to come back. Without the snippet: press the prefix twice (Ctrl-b Ctrl-b) then the key, which is standard tmux behaviour (not tested with jaunt).
- To detach from the remote tmux: `F12`, then Ctrl-b `d`. You are back in your outer tmux, and the remote session stays.
- When in doubt, `Enter ~ .` always works: it is read by jaunt itself, not by either tmux.
