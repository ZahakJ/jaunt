#!/usr/bin/env bash
# Host-only proof (no Docker): two daemons under separate JAUNT_HOME dirs and a local dev
# relay on 127.0.0.1. Nothing outside target/tmp/host-e2e is touched; tmux runs only on
# the private socket "jaunt-test".
#   JAUNT_BIN=target/debug/jaunt RELAY_PORT=3350 test/e2e/00-host.sh   (HOST_E2E_DIR=<dir> moves it)
set -uo pipefail
E2E_NAME=00-host
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/debug/jaunt}
RELAY_BIN=${RELAY_BIN:-$ROOT/spike/.tools/bin/iroh-relay}
PORT=${RELAY_PORT:-3350}
W=${HOST_E2E_DIR:-$ROOT/target/tmp/host-e2e}
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
  for m in a b c d; do [ -f "$W/$m.pid" ] && kill "$(cat "$W/$m.pid")" 2>/dev/null; done
  for m in c d; do [ -f "$W/$m/data/daemon.lock" ] && JAUNT_HOME=$W/$m "$BIN" down >/dev/null 2>&1; done
  tmux -L jaunt-test kill-session -t e2e 2>/dev/null
  true
}
trap cleanup EXIT

mkdir -p "$W" "$HOME"
for m in a b c d; do
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

# A daemon that started with no relay at all, then 'jaunt relay set': it must reach the
# relay and link without being restarted (once a bug: "not connected to any relay" until
# a restart, and a link failed with "no answer within 20 s").
note "c starts with no relay; jaunt relay set; then c links d"
jc() { JAUNT_HOME=$W/c "$BIN" "$@"; }
jd() { JAUNT_HOME=$W/d "$BIN" "$@"; }
for m in c d; do printf 'name = "host%s"\nportmapper = false\n' "$m" > "$W/$m/config/config.toml"; done
start_daemon c
check "c answers" wait_until 10 sh -c "JAUNT_HOME=$W/c $BIN ls >/dev/null"
pid_c=$(jc ls --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["daemon_pid"])')
jc check > "$W/c-check-before.out" 2>&1
check "c without a relay: check says so" grep -q "no relay configured" "$W/c-check-before.out"
jc relay set "http://127.0.0.1:$PORT" > "$W/c-relay-set.out" 2>&1
check "jaunt relay set succeeds" [ $? = 0 ]
sed 's/^/    c| /' "$W/c-relay-set.out"
check "relay set reports the relay connected" grep -q "connected to the relay now" "$W/c-relay-set.out"
check "c: jaunt check finds the relay connected" wait_until 15 sh -c "JAUNT_HOME=$W/c $BIN check 2>&1 | grep -q 'relay connected'"
pid_c2=$(jc ls --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["daemon_pid"])')
note "c's daemon pid before $pid_c, after $pid_c2"
rm -f "$W/link-c.out"
JAUNT_HOME=$W/c "$BIN" link --yes > "$W/link-c.out" 2>&1 &
LINK=$!
wait_until 20 grep -q '^code:' "$W/link-c.out"
code=$(awk '/^code:/{print $2; exit}' "$W/link-c.out")
printf '%s\n' "$code" | jd join --relay "http://127.0.0.1:$PORT" --yes > "$W/join-d.out" 2>&1
rc=$?
wait "$LINK"; rc_link=$?
sed 's/^/    d| /' "$W/join-d.out" | tail -3
check "d joins c (no daemon restart on c)" [ "$rc" = 0 ]
check "c's link succeeds" [ "$rc_link" = 0 ]
check "c sees d connected" wait_until 30 connected c hostd
check "d sees c connected" wait_until 30 connected d hostc
jd down >/dev/null 2>&1; jc down >/dev/null 2>&1

finish
