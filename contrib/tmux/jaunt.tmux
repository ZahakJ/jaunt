# jaunt for tmux. `jaunt tmux-setup` copies this file to ~/.config/jaunt/jaunt.tmux;
# add one line to your tmux.conf (rihla users: ~/.config/rihla/tmux/local.conf):
#
#   source-file ~/.config/jaunt/jaunt.tmux
#
#   prefix + J   the mesh in a popup; Enter opens the hop in a new window named after
#                the machine, so each machine is a window and none is lost.
#   F12          hand the keys to the tmux on the far side (this tmux's prefix and
#                bindings go quiet, the status bar says so); F12 again to take them back.
#
# Nothing else is bound; the prefix is not changed; Ctrl-h/j/k/l are left alone.

bind-key J display-popup -E -w 90% -h 80% -T " jaunt " "jaunt pick"

bind-key -T root F12 set-option prefix None \; set-option key-table jaunt-off \; refresh-client -S
bind-key -T jaunt-off F12 set-option -u prefix \; set-option -u key-table \; refresh-client -S

# The indicator goes in front of status-right (so a long status line cannot cut it off),
# once even if this file is sourced again. Under -F, `##` keeps a `#` for later.
if-shell -F '#{!=:#{@jaunt_tmux},1}' {
  set-option -g @jaunt_tmux 1
  set-option -gF status-right '##{?##{==:##{client_key_table},jaunt-off},#[fg=yellow##,bold]F12 keys to remote #[default],}#{status-right}'
}
