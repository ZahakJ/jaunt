#!/usr/bin/env bash
# Host-only proof (no Docker): two daemons under separate JAUNT_HOME dirs and a local dev
# relay on 127.0.0.1. Nothing outside target/tmp/host-e2e is touched; tmux runs only on
# the private socket "jaunt-test".
#   JAUNT_BIN=target/debug/jaunt RELAY_PORT=3350 test/e2e/00-host.sh
set -uo pipefail
E2E_NAME=00-host
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/debug/jaunt}
RELAY_BIN=${RELAY_BIN:-$ROOT/spike/.tools/bin/iroh-relay}
PORT=${RELAY_PORT:-3350}
W=$ROOT/target/tmp/host-e2e
unset TMUX TMUX_PANE XDG_RUNTIME_DIR XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME
export JAUNT_TMUX_SOCKET=jaunt-test
# Belt and braces: anything that forgets its own JAUNT_HOME lands here, never in the real home.
export HOME=$W/home JAUNT_HOME=$W/stray

[ -x "$BIN" ] || { echo "no jaunt binary at $BIN"; exit 2; }
[ -x "$RELAY_BIN" ] || { echo "no iroh-relay at $RELAY_BIN"; exit 2; }
if ss -ltn | awk '{print $4}' | grep -q ":$PORT\$"; then echo "port $PORT is in use; set RELAY_PORT"; exit 2; fi

PIDS=()
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  for m in a b; do [ -f "$W/$m.pid" ] && kill "$(cat "$W/$m.pid")" 2>/dev/null; done
  tmux -L jaunt-test kill-session -t e2e 2>/dev/null
  true
}
trap cleanup EXIT

mkdir -p "$W" "$HOME"
for m in a b; do
  [ -d "$W/$m" ] && find "$W/$m" -mindepth 1 -delete
  mkdir -p "$W/$m/config"
  printf 'name = "host%s"\nrelays = ["http://127.0.0.1:%s"]\nportmapper = false\n' "$m" "$PORT" > "$W/$m/config/config.toml"
done
printf 'http_bind_addr = "127.0.0.1:%s"\nenable_metrics = false\n' "$PORT" > "$W/relay.toml"
"$RELAY_BIN" --dev -c "$W/relay.toml" > "$W/relay.log" 2>&1 &
PIDS+=($!)

ja() { JAUNT_HOME=$W/a "$BIN" "$@"; }
jb() { JAUNT_HOME=$W/b "$BIN" "$@"; }
start_daemon() { # m
  JAUNT_HOME=$W/$1 "$BIN" daemon >> "$W/$1.log" 2>&1 &
  echo $! > "$W/$1.pid"
}
state_of() { # m peer
  JAUNT_HOME=$W/$1 "$BIN" ls | awk -F'\t' -v p="$2" '$1==p {print $2}'
}
connected() { case "$(state_of "$1" "$2")" in direct|relay) return 0;; *) return 1;; esac; }

check "relay is up" wait_until 10 curl -sf "http://127.0.0.1:$PORT/ping"
start_daemon a
start_daemon b
check "daemons answer" wait_until 10 sh -c "JAUNT_HOME=$W/a $BIN ls >/dev/null && JAUNT_HOME=$W/b $BIN ls >/dev/null"
IA=$(ja debug id); IB=$(jb debug id)
check "trust a->b" ja debug add-peer "$IB" hostb
check "trust b->a" jb debug add-peer "$IA" hosta
check "a sees b connected" wait_until 20 connected a hostb
check "b sees a connected" wait_until 20 connected b hosta
timeout 3 env JAUNT_HOME="$W/a" "$BIN" debug events --history 20 > "$W/events.out"
check "event stream starts with a status snapshot" sh -c "head -1 $W/events.out | grep -q '\"kind\":\"status\"'"
check "event stream replays a handshake" grep -q '"event":"handshake"' "$W/events.out"
check "a sees b direct" wait_until 20 sh -c "[ \"\$(JAUNT_HOME=$W/a $BIN ls | awk -F'\t' '\$1==\"hostb\"{print \$2}')\" = direct ]"
note "ls on a:"; ja ls
note "ls on b:"; jb ls
rtt=$(ja ls --json | python3 -c 'import json,sys; s=json.load(sys.stdin); print(s["peers"][0]["rtt_ms"])')
check "ls --json carries an RTT ($rtt ms)" python3 -c "import sys; sys.exit(0 if float('$rtt') > 0 else 1)"

out=$(ja run hostb -- hostname); want=$(hostname)
check "run hostb -- hostname ($out)" [ "$out" = "$want" ]
out=$(echo stdin-ok | ja run hostb -- cat)
check "run passes stdin" [ "$out" = stdin-ok ]
ja run hostb -- sh -c 'echo to-stderr >&2; exit 3' 2>"$W/stderr"; code=$?
check "run passes the exit status (3)" [ "$code" = 3 ]
check "run passes stderr" grep -q to-stderr "$W/stderr"
ja run hos -- true; check "unambiguous prefix 'hos' resolves" [ $? = 0 ]
ja run nosuch -- true 2>/dev/null; check "unknown machine fails" [ $? != 0 ]

# Hop under a pty; restart b's daemon twice in the middle: SIGTERM (it says goodbye) and
# SIGKILL (it does not; the new connection from the restarted daemon replaces the dead one).
restart_cmd() { # signal
  echo "kill -$1 \$(cat $W/b.pid); sleep 1; JAUNT_HOME=$W/b $BIN daemon >> $W/b.log 2>&1 & echo \$! > $W/b.pid; sleep 1"
}
python3 "$E2E_DIR/hop_pty.py" --between "$(restart_cmd TERM)" --between "$(restart_cmd KILL)" \
  -- env JAUNT_HOME="$W/a" "$BIN" hostb:e2e > "$W/hop.out" 2>&1 < /dev/null
code=$?
sed 's/^/    /' "$W/hop.out" | grep -E 'PASS|FAIL'
check "hop survives a restart of the remote daemon" [ "$code" = 0 ]
check "tmux session e2e still exists after the hop" tmux -L jaunt-test has-session -t e2e
check "session content kept (MARK42 in the pane)" sh -c "tmux -L jaunt-test capture-pane -p -t e2e -S -50 | grep -q MARK42"

finish
