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
h_daemon_start() { docker exec -d -u jaunt "jaunt-$1" "${H_ENV[@]}" sh -c 'exec jaunt daemon >>/home/jaunt/jh/daemon.out 2>&1'; }

# h_peer_state <node> <peer-name>  -> prints direct|relay|offline|connecting
h_peer_state() { h_jaunt "$1" ls | awk -F'\t' -v p="$2" '$1==p {print $2}'; }
h_is_state() { [ "$(h_peer_state "$1" "$2")" = "$3" ]; }
h_is_connected() { case "$(h_peer_state "$1" "$2")" in direct|relay) return 0;; *) return 1;; esac; }

# The harness is a shared lab: take turns through a mkdir mutex.
LOCK_DIR=$ROOT/target/tmp/harness.lock
h_lock() {
  mkdir -p "$ROOT/target/tmp"
  local tries=0
  until mkdir "$LOCK_DIR" 2>/dev/null; do
    tries=$((tries + 1))
    [ "$tries" -gt 180 ] && { echo "harness lock busy for 15 min: $LOCK_DIR"; return 1; }
    sleep 5
  done
  echo "$$ $(date +%s) ${E2E_NAME:-e2e}" > "$LOCK_DIR/owner"
}
h_unlock() { rm -f "$LOCK_DIR/owner"; rmdir "$LOCK_DIR" 2>/dev/null || true; }
