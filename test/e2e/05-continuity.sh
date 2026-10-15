#!/usr/bin/env bash
# Session continuity (design §8) in the Docker NAT harness. One interactive hop a -> b stays
# open while the lab disrupts it: the client's link blackholed for 10 s and for 60 s, the
# client roamed to another LAN and back, the client frozen for 90 s, the host frozen for
# 90 s, and the host's daemon restarted. After each, the same shell (a variable set at the
# start) must answer typed commands again; the time from the end of the disruption to that
# answer is recorded in target/tmp/05-continuity.tsv.
#   JAUNT_BIN=target/release/jaunt test/e2e/05-continuity.sh   (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=05-continuity
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-* "$OUT.tsv"

h_begin
check "mesh a b c linked and connected" h_mesh
check "a -> b is direct" wait_until 30 h_is_state a b direct

restart_b="bash -c '. $E2E_DIR/lib.sh; h_daemon_restart b'"
steps=(
  "drop-client-10s=$H_JH drop a 10"
  "drop-client-60s=$H_JH drop a 60"
  "roam-client-to-hard-nat=$H_JH roam a hard"
  "roam-client-back=$H_JH roam a lanA"
  "freeze-client-90s=$H_JH freeze a 90"
  "freeze-host-90s=$H_JH freeze b 90"
  "restart-host-daemon=$restart_b"
)
args=()
for s in "${steps[@]}"; do args+=(--step "$s"); done
note "one hop a -> b:cont kept open through ${#steps[@]} disruptions (about 8 minutes)"
t0=$SECONDS
# shellcheck disable=SC2046
python3 "$E2E_DIR/hop_session.py" "${args[@]}" -- $(h_tty_cmd a b:cont) > "$OUT-hop.out" 2>&1
code=$?
sed 's/^/    /' "$OUT-hop.out" | grep -E '^    (PASS|FAIL|STEP)'
check "the hop survived every disruption ($((SECONDS - t0)) s in all)" [ "$code" = 0 ]
grep '^STEP' "$OUT-hop.out" | sed 's/^STEP //; s/ /\t/g' > "$OUT.tsv"
check "tmux session cont is still on b" h_exec b tmux -L jaunt-test has-session -t cont
for m in a b; do note "jaunt ls on $m"; h_jaunt "$m" ls | cut -f1-5 | sed 's/^/    /'; done

finish
