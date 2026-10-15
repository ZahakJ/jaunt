#!/bin/sh
# Run the jaunt relay on this machine, on loopback only, as a systemd user service.
# An existing Cloudflare tunnel then carries it (see README.md).
#
#   sh desktop-tunnel.sh install [PATH-TO-iroh-relay]   set up and start
#   sh desktop-tunnel.sh status                         is it running, does it answer
#   sh desktop-tunnel.sh undo                           stop and remove everything it made
#
# What install changes, all inside your home directory, no root:
#   ~/.local/bin/iroh-relay                         the relay program (copied)
#   ~/.config/jaunt-relay/relay.toml                its config (plain HTTP on 127.0.0.1:3340)
#   ~/.config/systemd/user/jaunt-relay.service      the service (enabled and started)
# It does not touch the tunnel: you add the public hostname in the Cloudflare dashboard.
set -eu

PORT=3340
BIN=$HOME/.local/bin/iroh-relay
CONF=$HOME/.config/jaunt-relay
UNIT=$HOME/.config/systemd/user/jaunt-relay.service

say() { printf '%s\n' "$*"; }
fail() { say "error: $*" >&2; exit 1; }

find_relay() {
	if [ -n "${1:-}" ]; then
		[ -x "$1" ] || fail "$1 is not an executable file"
		say "$1"
	elif [ -x "$BIN" ]; then
		say "$BIN"
	elif command -v iroh-relay >/dev/null 2>&1; then
		command -v iroh-relay
	else
		fail "no iroh-relay found. Build it with:
  cargo install iroh-relay@1.3.0 --features server --locked
or pass the path of an existing one: sh desktop-tunnel.sh install /path/to/iroh-relay"
	fi
}

status() {
	systemctl --user is-active jaunt-relay.service || true
	if curl -fsS -m 5 -o /dev/null "http://127.0.0.1:$PORT/ping"; then
		say "the relay answers on http://127.0.0.1:$PORT"
	else
		say "the relay does not answer on http://127.0.0.1:$PORT"
		return 1
	fi
}

case "${1:-}" in
install)
	src=$(find_relay "${2:-}")
	if ss -ltn 2>/dev/null | grep -q ":$PORT " && ! systemctl --user is-active -q jaunt-relay.service; then
		fail "port $PORT is already in use by something else"
	fi
	mkdir -p "$HOME/.local/bin" "$CONF" "$(dirname "$UNIT")"
	[ "$src" = "$BIN" ] || install -m 755 "$src" "$BIN"
	cat >"$CONF/relay.toml" <<EOF
# jaunt relay on this desktop: plain HTTP, loopback only.
# The Cloudflare tunnel terminates TLS and forwards to it.
http_bind_addr = "127.0.0.1:$PORT"
enable_relay = true
enable_metrics = false
enable_quic_addr_discovery = false
EOF
	cat >"$UNIT" <<'EOF'
[Unit]
Description=jaunt relay (iroh-relay on loopback, reached through the Cloudflare tunnel)
After=network-online.target

[Service]
ExecStart=%h/.local/bin/iroh-relay --config-path %h/.config/jaunt-relay/relay.toml
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
	systemctl --user daemon-reload
	systemctl --user enable --now jaunt-relay.service
	sleep 2
	status || fail "see: journalctl --user -u jaunt-relay.service -n 30"
	say
	say "Next, in the Cloudflare Zero Trust dashboard: Networks > Tunnels > one of your"
	say "tunnels on this machine > Public Hostname > Add:"
	say "  hostname: relay.<your domain>     service: HTTP   127.0.0.1:$PORT"
	say "Then check from anywhere:  curl -fsS https://relay.<your domain>/ping"
	;;
status)
	status
	;;
undo)
	systemctl --user disable --now jaunt-relay.service 2>/dev/null || true
	rm -f "$UNIT" "$CONF/relay.toml" "$BIN"
	rmdir "$CONF" 2>/dev/null || true
	systemctl --user daemon-reload
	say "removed. Delete the public hostname in the Cloudflare dashboard too."
	;;
*)
	sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'
	exit 2
	;;
esac
