# shellcheck shell=bash
# Shared helpers for the jaunt end-to-end scripts. Source it; then use pass/fail/check
# and finish at the end. Harness helpers (h_*) drive test/harness containers.

E2E_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$E2E_DIR/../.." && pwd)
HARNESS=$ROOT/test/harness
E2E_PASS=0
E2E_FAIL=0

pass() { E2E_PASS=$((E2E_PASS + 1)); echo "PASS $*"; }
fail() { E2E_FAIL=$((E2E_FAIL + 1)); echo "FAIL $*"; }
note() { echo "---- $*"; }

# check "<description>" cmd args...   (PASS if the command succeeds)
check() {
  local what=$1; shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}

# wait_until <seconds> cmd args...   (polls every 0.5 s)
wait_until() {
  local secs=$1; shift
  local end=$((SECONDS + secs))
  while [ "$SECONDS" -lt "$end" ]; do
    if "$@" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}

finish() {
  echo "==== $E2E_PASS passed, $E2E_FAIL failed"
  [ "$E2E_FAIL" -eq 0 ]
}

# ---- harness ------------------------------------------------------------------------
# Machines run jaunt as the unprivileged user 'jaunt' with JAUNT_HOME=/home/jaunt/jh and
# tmux socket 'jaunt-test'.
H_ENV=(env JAUNT_HOME=/home/jaunt/jh JAUNT_TMUX_SOCKET=jaunt-test HOME=/home/jaunt PATH=/opt/jaunt/bin:/usr/bin)

h_exec() { local n=$1; shift; docker exec -u jaunt "jaunt-$n" "${H_ENV[@]}" "$@"; }
h_exec_i() { local n=$1; shift; docker exec -i -u jaunt "jaunt-$n" "${H_ENV[@]}" "$@"; }
h_root() { local n=$1; shift; docker exec "jaunt-$n" "$@"; }
h_jaunt() { local n=$1; shift; h_exec "$n" jaunt "$@"; }
# E2E_DAEMON_LOG=<filter> (e.g. info,iroh=debug) turns on more daemon logging.
h_daemon_start() {
  docker exec -d -u jaunt "jaunt-$1" "${H_ENV[@]}" ${E2E_DAEMON_LOG:+JAUNT_LOG=$E2E_DAEMON_LOG} sh -c 'exec jaunt daemon >>/home/jaunt/jh/daemon.out 2>&1'
}
# Stop / restart only the daemon (not jaunt clients on that machine), waiting for the old
# one to exit: it can take a few seconds to close, and until then a new one cannot start.
# Started by the harness as "jaunt daemon", or by a client as "/opt/jaunt/bin/jaunt daemon".
H_DAEMON_RE='^(/opt/jaunt/bin/)?jaunt daemon$'
h_daemon_running() { docker exec "jaunt-$1" pgrep -u jaunt -f "$H_DAEMON_RE" >/dev/null; }
h_daemon_stop() {
  local p; p=$(docker exec "jaunt-$1" pgrep -u jaunt -f "$H_DAEMON_RE")
  [ -n "$p" ] && docker exec "jaunt-$1" kill $p
  wait_until 15 eval "! h_daemon_running $1"
}
h_daemon_restart() {
  h_daemon_stop "$1" || return 1
  h_daemon_start "$1"
  wait_until 15 eval "h_daemon_running $1 && docker exec jaunt-$1 test -S /home/jaunt/jh/run/sock"
}
# Save each machine's daemon log next to the script's other output.
h_save_logs() { local m; for m in a b c; do docker exec "jaunt-$m" cat /home/jaunt/jh/daemon.out > "$ROOT/target/tmp/$E2E_NAME-daemon-$m.log" 2>/dev/null; done; }

# h_peer_state <node> <peer-name>  -> prints direct|relay|offline|connecting
h_peer_state() { h_jaunt "$1" ls | awk -F'\t' -v p="$2" '$1==p {print $2}'; }
h_is_state() { [ "$(h_peer_state "$1" "$2")" = "$3" ]; }
h_is_connected() { case "$(h_peer_state "$1" "$2")" in direct|relay) return 0;; *) return 1;; esac; }

# The harness is a shared lab: take turns through a mkdir mutex.
LOCK_DIR=$ROOT/target/tmp/harness.lock
h_lock() {
  # run-all.sh holds the lock for the script it runs.
  [ "${E2E_LOCK_HELD:-0}" = 1 ] && return 0
  mkdir -p "$ROOT/target/tmp"
  local tries=0
  until mkdir "$LOCK_DIR" 2>/dev/null; do
    tries=$((tries + 1))
    [ "$tries" -gt 180 ] && { echo "harness lock busy for 15 min: $LOCK_DIR"; return 1; }
    sleep 5
  done
  echo "$$ $(date +%s) ${E2E_NAME:-e2e}" > "$LOCK_DIR/owner"
}
h_unlock() {
  [ "${E2E_LOCK_HELD:-0}" = 1 ] && return 0
  rm -f "$LOCK_DIR/owner"; rmdir "$LOCK_DIR" 2>/dev/null || true
}

# ---- trust --------------------------------------------------------------------------
RELAY_URL=https://relay.jaunt.test

# h_fresh <node> [with-relay]: stop jaunt, wipe its JAUNT_HOME, write a config (name and
# the harness relay CA; the relay URL only with "with-relay", so a fresh joiner has to be
# told it), start the daemon and wait for its socket.
h_fresh() {
  local m=$1 relay=""
  [ "${2:-}" = with-relay ] && relay="relays = [\"$RELAY_URL\"]"
  h_root "$m" pkill -u jaunt -x jaunt 2>/dev/null
  # A dying daemon removes its socket on exit: let it finish before a new one starts.
  wait_until 15 eval "! docker exec jaunt-$m pgrep -u jaunt -x jaunt"
  h_exec "$m" sh -c "mkdir -p /home/jaunt/jh && find /home/jaunt/jh -mindepth 1 -delete; mkdir -p /home/jaunt/jh/config && printf '%s\n' 'name = \"$m\"' 'relay_ca = \"/opt/jaunt/relay-ca.crt\"' '$relay' > /home/jaunt/jh/config/config.toml"
  h_daemon_start "$m"
  wait_until 15 docker exec "jaunt-$m" test -S /home/jaunt/jh/run/sock
}

# h_link <inviter> <joiner> [code-transform]: the ritual, scripted with --yes. The
# inviter's output goes to $OUT-link-<inv>-<joi>.inv, the joiner's to .join. The optional
# transform (a sed expression) is applied to the code before the joiner types it.
# Returns 0 when both sides say linked.
h_link() {
  local inv=$1 joi=$2 tf=${3:-} out=$ROOT/target/tmp/$E2E_NAME-link-$1-$2 code rc
  h_exec "$inv" rm -f /home/jaunt/link.out
  docker exec -d -u jaunt "jaunt-$inv" "${H_ENV[@]}" sh -c 'jaunt link --yes > /home/jaunt/link.out 2>&1; echo "exit=$?" >> /home/jaunt/link.out'
  wait_until 20 h_exec "$inv" grep -q '^code:' /home/jaunt/link.out || { h_exec "$inv" cat /home/jaunt/link.out > "$out.inv"; return 1; }
  code=$(h_exec "$inv" awk '/^code:/{print $2; exit}' /home/jaunt/link.out)
  echo "$code" > "$out.raw"
  [ -n "$tf" ] && code=$(printf '%s' "$code" | sed "$tf")
  echo "$code" > "$out.code"
  # The code goes in on stdin, never in argv (where every local user could read it).
  printf '%s\n' "$code" | h_exec_i "$joi" jaunt join --relay "$RELAY_URL" --yes > "$out.join" 2>&1
  rc=$?
  wait_until 45 h_exec "$inv" grep -q '^exit=' /home/jaunt/link.out
  h_exec "$inv" cat /home/jaunt/link.out > "$out.inv"
  [ "$rc" = 0 ] && grep -q '^exit=0' "$out.inv"
}

# The sigil and words both sides printed (the lines between "compare" and "confirmed").
h_check_lines() { sed -n '/^compare with/,/^confirmed/p' "$1" | grep -v -e '^compare' -e '^confirmed'; }

# h_members <node>: names in its ls, one per line, without itself.
h_members() { h_jaunt "$1" ls | awk -F'\t' '$2!="self" {print $1}' | sort | tr '\n' ' '; }
h_knows() { h_jaunt "$1" ls | awk -F'\t' -v p="$2" '$1==p {f=1} END {exit !f}'; }
h_not_knows() { ! h_knows "$@"; }

# ---- shared setup for 04-09 ---------------------------------------------------------
H_JH=$HARNESS/jh

# Put the lab's network back the way up.sh leaves it: no delay, c's NAT hard, every
# machine on its own LAN, no leftover jaunt-test rules on the routers.
h_reset_net() {
  "$H_JH" delay 0 >/dev/null 2>&1
  "$H_JH" hardmode "${HARD_MODE:-symmetric}" >/dev/null 2>&1
  "$H_JH" roam a lanA >/dev/null 2>&1; "$H_JH" roam b lanB >/dev/null 2>&1; "$H_JH" roam c hard >/dev/null 2>&1
  for c in jaunt-a jaunt-b jaunt-c; do docker unpause "$c" >/dev/null 2>&1; done
}

# h_begin: lock, harness up (idempotent), relay, the binary in a b c, a clean network.
h_begin() {
  h_lock || exit 2
  trap h_end EXIT
  note "harness up"
  "$HARNESS/up.sh" >/dev/null 2>&1 || { fail "harness up.sh"; finish; exit 1; }
  "$HARNESS/relay.sh" >/dev/null 2>&1 || { fail "relay.sh"; finish; exit 1; }
  "$H_JH" install "$BIN" a b c >/dev/null 2>&1
  h_reset_net
}

# Scripts may define h_cleanup_extra for their own leftovers.
h_end() {
  type h_cleanup_extra >/dev/null 2>&1 && h_cleanup_extra
  for c in jaunt-a jaunt-b jaunt-c; do docker unpause "$c" >/dev/null 2>&1; done
  for m in a b c; do h_root "$m" pkill -u jaunt -x jaunt 2>/dev/null; done
  if [ "${KEEP_HARNESS:-0}" != 1 ]; then "$HARNESS/down.sh" >/dev/null 2>&1; else h_reset_net; fi
  h_unlock
}

h_all_connected() {
  local pair
  for pair in "a b" "a c" "b a" "b c" "c a" "c b"; do
    # shellcheck disable=SC2086
    set -- $pair
    h_is_connected "$1" "$2" || return 1
  done
}

# h_mesh: fresh a b c that trust each other (debug add-peer, no ritual), all connected.
h_mesh() {
  local m n id
  for m in a b c; do h_fresh "$m" with-relay || return 1; done
  declare -A ids
  for m in a b c; do ids[$m]=$(h_jaunt "$m" debug id); done
  for m in a b c; do for n in a b c; do
    [ "$m" = "$n" ] && continue
    h_jaunt "$m" debug add-peer "${ids[$n]}" "$n" >/dev/null || return 1
  done; done
  wait_until 120 h_all_connected
}

# Copy the e2e helpers into a machine (as /tmp/<name>).
h_put() { docker cp -q "$E2E_DIR/$2" "jaunt-$1:/tmp/$2"; }

# The command line that runs `jaunt <args>` as user jaunt inside a machine with a tty
# (for the PTY drivers, which run on the host).
h_tty_cmd() { local n=$1; shift; echo docker exec -it -u jaunt "jaunt-$n" "${H_ENV[@]}" TERM=xterm-256color jaunt "$@"; }

# Record one result row: h_row <file> <cells...> (tab-separated).
h_row() { local f=$1; shift; (IFS=$'\t'; echo "$*") >> "$f"; }
