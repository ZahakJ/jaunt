#!/usr/bin/env bash
# Paths (DESIGN §2, §5) in the Docker NAT harness: a <-> b direct; c behind the hard NAT is
# relayed in both hard modes (symmetric NAT, and UDP blocked except to the relay); with the
# relay down an existing direct stream keeps flowing, and what happens to new connections
# is recorded; with the relay back everything recovers (times recorded).
#   JAUNT_BIN=target/release/jaunt test/e2e/06-paths.sh       (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=06-paths
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-* "$OUT.tsv"
h_cleanup_extra() { "$HARNESS/relay.sh" >/dev/null 2>&1; }

h_begin
check "mesh a b c linked and connected" h_mesh
check "a -> b DIRECT" wait_until 30 h_is_state a b direct
check "b -> a DIRECT" wait_until 30 h_is_state b a direct
row() { h_row "$OUT.tsv" "$@"; }
restart() { h_daemon_restart "$1"; }
ran_on() { [ "$(h_jaunt "$1" run "$2" -- cat /etc/hostname 2>/dev/null)" = "$2" ]; }

for mode in symmetric udpblock; do
  note "router c in hard mode '$mode'"
  "$H_JH" hardmode "$mode" >/dev/null
  restart c
  t0=$SECONDS
  check "[$mode] a -> c connected" wait_until 60 h_is_connected a c
  check "[$mode] c -> b connected" wait_until 60 h_is_connected c b
  row "$mode" "connect_s" "$((SECONDS - t0))"
  sleep 10 # give hole punching a fair chance before asserting it failed
  check "[$mode] a -> c RELAYED" h_is_state a c relay
  check "[$mode] b -> c RELAYED" h_is_state b c relay
  check "[$mode] c -> a RELAYED" h_is_state c a relay
  check "[$mode] a -> b still DIRECT" h_is_state a b direct
  check "[$mode] run a -> c works" ran_on a c
  check "[$mode] run c -> b works" ran_on c b
  h_jaunt c check > "$OUT-check-c-$mode.out" 2>&1
  sed 's/^/    c| /' "$OUT-check-c-$mode.out" | grep -iE 'udp|nat|direct' || true
done
"$H_JH" hardmode symmetric >/dev/null

note "relay down"
# An existing direct stream: b counts once a second into a file on a.
docker exec -d -u jaunt jaunt-a "${H_ENV[@]}" sh -c 'jaunt run b -- sh -c "i=0; while :; do echo tick \$i; i=\$((i+1)); sleep 1; done" > /tmp/ticks 2>&1'
ticks() { docker exec jaunt-a sh -c 'wc -l < /tmp/ticks'; }
check "stream a <- b is flowing" wait_until 15 sh -c '[ "$(docker exec jaunt-a sh -c "wc -l < /tmp/ticks")" -ge 2 ]'
h_root relay pkill -x iroh-relay
t_down=$SECONDS
check "relay process stopped" wait_until 5 sh -c '! docker exec jaunt-relay pgrep -x iroh-relay'
n0=$(ticks)
sleep 20
n1=$(ticks)
check "existing direct stream kept flowing with the relay down ($n0 -> $n1 lines in 20 s)" [ $((n1 - n0)) -ge 15 ]
check "new command a -> b over the warm direct connection works" ran_on a b
if ran_on a c; then note "a -> c (relayed) still answered right after the relay died"; row relay_down a_to_c_new_command works; else note "a -> c (relayed) fails with the relay down, as expected"; row relay_down a_to_c_new_command fails; fi
h_jaunt a run c -- true > "$OUT-relaydown-c.out" 2>&1; sed 's/^/    a| /' "$OUT-relaydown-c.out"
h_jaunt a check > "$OUT-check-a-relaydown.out" 2>&1
check "jaunt check on a reports the relay problem (exit 1)" [ $? = 1 ]
sed 's/^/    a| /' "$OUT-check-a-relaydown.out" | grep -E '^    a\| fail' || true
note "b's daemon restarts while the relay is down: can a reach it again without the relay?"
restart b
if wait_until 40 ran_on a b; then note "a reached the restarted b without the relay"; row relay_down a_to_restarted_b works; else note "a cannot reach the restarted b without the relay (no address lookup but the relay)"; row relay_down a_to_restarted_b fails; fi
for m in a; do note "jaunt ls on $m (relay down)"; h_jaunt "$m" ls | cut -f1-5 | sed 's/^/    /'; done
row relay_down down_for_s "$((SECONDS - t_down))"

note "relay back"
"$HARNESS/relay.sh" >/dev/null 2>&1
t1=$SECONDS
check "relay connected again on a" wait_until 60 sh -c 'docker exec -u jaunt jaunt-a env JAUNT_HOME=/home/jaunt/jh PATH=/opt/jaunt/bin:/usr/bin jaunt check | grep -q "relay connected"'
row relay_back a_relay_connected_s "$((SECONDS - t1))"
check "a -> c answers again" wait_until 90 ran_on a c
row relay_back a_to_c_s "$((SECONDS - t1))"
check "a -> b answers again" wait_until 90 ran_on a b
row relay_back a_to_b_s "$((SECONDS - t1))"
check "all six directions connected again" wait_until 120 h_all_connected
row relay_back all_connected_s "$((SECONDS - t1))"
check "a -> b back to DIRECT" wait_until 60 h_is_state a b direct
row relay_back a_b_direct_s "$((SECONDS - t1))"
h_save_logs
note "timings"; sed 's/^/    /' "$OUT.tsv"

finish
