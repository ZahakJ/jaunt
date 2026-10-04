#!/usr/bin/env bash
# Core proof in the Docker NAT harness (test/harness): a real iroh-relay with TLS + QAD,
# three machines behind NATs (a, b: cone; c: hard/symmetric). Checks: trust bootstrap and
# roster spread, a<->b DIRECT, c RELAYED, `jaunt run` both ways, a hop over the relay.
#   JAUNT_BIN=target/release/jaunt test/e2e/01-core.sh       (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=01-core
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }

h_lock || exit 2
cleanup() {
  for m in a b c; do h_root "$m" pkill -u jaunt -x jaunt 2>/dev/null; done
  if [ "${KEEP_HARNESS:-0}" != 1 ]; then "$HARNESS/down.sh" >/dev/null 2>&1; fi
  h_unlock
}
trap cleanup EXIT

note "harness up"
"$HARNESS/up.sh" >/dev/null 2>&1 || { fail "harness up.sh"; finish; exit 1; }
check "relay (TLS + QAD) up" "$HARNESS/relay.sh"
"$HARNESS/jh" install "$BIN" a b c >/dev/null
check "jaunt installed in a b c" h_exec a jaunt --version

for m in a b c; do
  h_exec "$m" sh -c "pkill -x jaunt; mkdir -p /home/jaunt/jh && find /home/jaunt/jh -mindepth 1 -delete; mkdir -p /home/jaunt/jh/config && printf '%s\n' 'name = \"$m\"' 'relays = [\"https://relay.jaunt.test\"]' 'relay_ca = \"/opt/jaunt/relay-ca.crt\"' > /home/jaunt/jh/config/config.toml"
  h_daemon_start "$m"
done
# Wait for the sockets rather than calling jaunt (which would start a second daemon).
check "daemons answer on a b c" wait_until 15 sh -c 'for m in a b c; do docker exec jaunt-$m test -S /home/jaunt/jh/run/sock || exit 1; done'
IA=$(h_jaunt a debug id); IB=$(h_jaunt b debug id); IC=$(h_jaunt c debug id)

# Trust bootstrap: a trusts b and c; b and c trust only a. b and c must learn each other
# through roster sync with a.
check "a trusts b" h_jaunt a debug add-peer "$IB" b
check "a trusts c" h_jaunt a debug add-peer "$IC" c
check "b trusts a" h_jaunt b debug add-peer "$IA" a
check "c trusts a" h_jaunt c debug add-peer "$IA" a
check "relay connected on a" wait_until 20 sh -c 'docker exec -u jaunt jaunt-a env JAUNT_HOME=/home/jaunt/jh PATH=/opt/jaunt/bin:/usr/bin jaunt check | grep -q "relay connected"'

check "a <-> b connected" wait_until 40 h_is_connected a b
check "a -> b DIRECT" wait_until 30 h_is_state a b direct
check "b -> a DIRECT" wait_until 30 h_is_state b a direct
check "a -> c connected" wait_until 40 h_is_connected a c
check "b learned c by roster sync and connects" wait_until 75 h_is_connected b c
check "c learned b by roster sync and connects" wait_until 75 h_is_connected c b
# Give hole punching a fair chance before asserting it failed.
sleep 10
check "a -> c RELAYED (c is behind the hard NAT)" h_is_state a c relay
check "c -> a RELAYED" h_is_state c a relay
check "b -> c RELAYED" h_is_state b c relay
for m in a b c; do note "jaunt ls on $m"; h_jaunt "$m" ls | cut -f1-5 | sed 's/^/    /'; done

out=$(h_jaunt a run b -- cat /etc/hostname); check "a: run b -- cat /etc/hostname = '$out' (direct)" [ "$out" = b ]
out=$(h_jaunt a run c -- cat /etc/hostname); check "a: run c -- cat /etc/hostname = '$out' (relayed)" [ "$out" = c ]
out=$(h_jaunt c run b -- cat /etc/hostname); check "c: run b -- cat /etc/hostname = '$out' (relayed)" [ "$out" = b ]
out=$(echo through-relay | h_exec_i c jaunt run a -- cat); check "c: run a -- cat passes stdin" [ "$out" = through-relay ]
h_jaunt a run c -- sh -c 'exit 7'; check "a: run c passes exit status 7" [ $? = 7 ]
out=$(h_jaunt b run c -- sh -c 'echo $JAUNT_FROM $JAUNT_MACHINE'); check "remote env JAUNT_FROM/JAUNT_MACHINE = '$out'" [ "$out" = "b c" ]

docker cp -q "$E2E_DIR/hop_pty.py" jaunt-a:/tmp/hop_pty.py
h_exec a python3 /tmp/hop_pty.py -- jaunt c:e2e > "$ROOT/target/tmp/01-core-hop.out" 2>&1
code=$?
sed 's/^/    /' "$ROOT/target/tmp/01-core-hop.out" | grep -E '^    (PASS|FAIL)'
check "hop a -> c over the relay, Enter ~ . leaves" [ "$code" = 0 ]
check "tmux session e2e lives on c" h_exec c tmux -L jaunt-test has-session -t e2e

finish
