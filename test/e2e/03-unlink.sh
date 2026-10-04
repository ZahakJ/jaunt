#!/usr/bin/env bash
# Revocation (DESIGN §7) in the Docker NAT harness. a, b, c are linked with the ritual
# (c through b). a unlinks c: c's shell and command on b are ended at once, a and b refuse
# c, c cannot come back by syncing or by pairing with its old key. Then c is linked again
# with a new key, b is frozen, a unlinks c meanwhile, and b learns of it on return; the
# time that takes is printed (the window in which b still accepts c).
#   JAUNT_BIN=target/release/jaunt test/e2e/03-unlink.sh       (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=03-unlink
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-*

h_lock || exit 2
cleanup() {
  docker unpause jaunt-b >/dev/null 2>&1
  for m in a b c; do h_root "$m" pkill -u jaunt -x jaunt 2>/dev/null; done
  if [ "${KEEP_HARNESS:-0}" != 1 ]; then "$HARNESS/down.sh" >/dev/null 2>&1; fi
  h_unlock
}
trap cleanup EXIT

all_connected() {
  for pair in "a b" "a c" "b a" "b c" "c a" "c b"; do
    set -- $pair
    h_is_connected "$1" "$2" || return 1
  done
}
fresh_all() { h_fresh a with-relay && h_fresh b && h_fresh c; }
on_b_sleeping() { h_exec b pgrep -u jaunt -x sleep >/dev/null; }
on_b_clients() { h_exec b tmux -L jaunt-test list-clients -t victim 2>/dev/null | wc -l; }
c_hop_attached() { [ "$(on_b_clients)" -ge 1 ]; }
c_hop_detached() { [ "$(on_b_clients)" = 0 ]; }
not_connected() { ! h_is_connected "$@"; }
c_refused_by() { ! h_jaunt c run "$1" -- true >/dev/null 2>&1; }
revoked_on() { # node -> c is gone from ls and listed as revoked in ls --json
  h_not_knows "$1" c && h_jaunt "$1" ls --json | python3 -c "import json,sys; s=json.load(sys.stdin); assert any(r['name']=='c' for r in s['revoked'])"
}

note "harness up"
"$HARNESS/up.sh" >/dev/null 2>&1 || { fail "harness up.sh"; finish; exit 1; }
check "relay (TLS + QAD) up" "$HARNESS/relay.sh"
"$HARNESS/jh" install "$BIN" a b c >/dev/null
check "fresh a b c" fresh_all
check "relay connected on a" wait_until 20 sh -c 'docker exec -u jaunt jaunt-a env JAUNT_HOME=/home/jaunt/jh PATH=/opt/jaunt/bin:/usr/bin jaunt check | grep -q "relay connected"'
check "link a <- b" h_link a b
check "link b <- c" h_link b c
check "a, b, c all connected" wait_until 120 all_connected

note "phase 1: c holds a shell and a command on b; a unlinks c"
docker exec -d -u jaunt jaunt-c "${H_ENV[@]}" TERM=xterm python3 -c 'import pty; pty.spawn(["jaunt", "b:victim"])'
docker exec -d -u jaunt jaunt-c "${H_ENV[@]}" jaunt run b -- sleep 600
check "c's shell is attached to tmux session 'victim' on b" wait_until 20 c_hop_attached
check "c's 'sleep 600' runs on b" wait_until 20 on_b_sleeping
h_jaunt a unlink c > "$OUT-unlink1.out" 2>&1
check "a: jaunt unlink c succeeds" [ $? = 0 ]
sed 's/^/    a| /' "$OUT-unlink1.out"
check "a: b confirmed" grep -q "confirmed by: b" "$OUT-unlink1.out"
check "b: c's command was ended" wait_until 5 sh -c '! docker exec jaunt-b pgrep -u jaunt -x sleep'
check "b: c's shell was detached" wait_until 5 c_hop_detached
check "b: the tmux session itself is kept (it is the owner's)" h_exec b tmux -L jaunt-test has-session -t victim
h_exec b timeout 2 jaunt debug events --history 300 > "$OUT-events-b.jsonl" 2>/dev/null
check "b: a 'revoked' event for c with 2 processes ended" \
  python3 -c "import json,sys
ev=[e for e in map(json.loads, open(sys.argv[1])) if e.get('event')=='revoked']
assert any(e['peer']['name']=='c' and e['sessions_killed']==2 for e in ev), ev" "$OUT-events-b.jsonl"
check "a lists c as revoked" revoked_on a
check "b lists c as revoked" revoked_on b
check "c is refused by a" c_refused_by a
check "c is refused by b" c_refused_by b
h_jaunt c run b -- true > "$OUT-refused.out" 2>&1; sed 's/^/    c| /' "$OUT-refused.out"

note "c tries to come back by syncing (daemon restart, it dials a and b for 40 s)"
h_root c pkill -u jaunt -x jaunt; sleep 1; h_daemon_start c
sleep 40
check "a still has c revoked" revoked_on a
check "b still has c revoked" revoked_on b
check "c still cannot connect to a" not_connected c a
check "c still cannot connect to b" not_connected c b

note "c tries to come back by pairing with its old key (roster deleted, key kept)"
h_root c pkill -u jaunt -x jaunt; sleep 1
h_exec c rm -f /home/jaunt/jh/data/roster.json; h_daemon_start c
wait_until 15 docker exec jaunt-c test -S /home/jaunt/jh/run/sock
rejoin_fails() { ! h_link a c; }
check "a refuses c's old key in the ritual" rejoin_fails
sed 's/^/    c| /' "$OUT-link-a-c.join"
check "c is told its key was unlinked" grep -q "unlinked" "$OUT-link-a-c.join"

note "phase 2: c gets a new key and is linked again; b is frozen while a unlinks c"
check "c fresh (new key)" h_fresh c
check "link a <- c (new key)" h_link a c
check "a, b, c all connected again" wait_until 120 all_connected
"$HARNESS/jh" freeze b 45 > "$OUT-freeze.log" 2>&1 &
FREEZE=$!
sleep 3
h_jaunt a unlink c > "$OUT-unlink2.out" 2>&1
check "a: jaunt unlink c succeeds while b is frozen" [ $? = 0 ]
sed 's/^/    a| /' "$OUT-unlink2.out"
check "a: b has not heard yet" grep -q "not heard yet: b" "$OUT-unlink2.out"
wait "$FREEZE"
t0=$SECONDS
note "b thawed; c tries b right away (it may still get in: that is the window)"
if h_jaunt c run b -- true >/dev/null 2>&1; then note "c reached b before b heard"; else note "c did not reach b"; fi
# Both c's share the name "c"; once b has the new tombstone, no member called c is left.
check "b learns of the revocation after its return" wait_until 120 h_not_knows b c
note "b learned $((SECONDS - t0)) s after thawing"
echo "$((SECONDS - t0))" > "$OUT-window-s"
check "c is refused by b afterwards" c_refused_by b
check "c is refused by a" c_refused_by a
for m in a b; do note "jaunt ls on $m"; h_jaunt "$m" ls | cut -f1-5 | sed 's/^/    /'; done

finish
