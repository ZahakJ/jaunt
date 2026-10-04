#!/usr/bin/env bash
# Self-test of the harness NAT behaviour with plain tools. Requires ./up.sh first.
# Exit 0 only if every expectation holds. Leaves delay at 0 and router c in its HARD_MODE.
set -uo pipefail
cd "$(dirname "$0")" || exit 1; . ./lib.sh
fail=0
ok()  { echo "PASS  $*"; }
bad() { echo "FAIL  $*"; fail=1; }
expect() { local want=$1; shift; local desc=$1; shift
  if "$@" >"$HARNESS_DIR/.state/prove.out" 2>&1; then got=0; else got=1; fi
  if [ "$got" = "$want" ]; then ok "$desc"; else bad "$desc"; sed 's/^/      /' "$HARNESS_DIR/.state/prove.out"; fi
  rm -f "$HARNESS_DIR/.state/prove.out"; }

./jh delay 0 >/dev/null 2>&1
for m in a b c; do expect 0 "$m reaches relay (icmp)" ./jh exec $m ping -c1 -W2 "$IP_RELAY"; done
for m in a b c; do echo "      $m reflexive: $(./jh whoami $m 41000)"; done
expect 1 "a has no route to lanB private addr" ./jh exec a ping -c1 -W1 "$IP_B"
expect 1 "a cannot reach outside world (1.1.1.1)" ./jh exec a ping -c1 -W1 1.1.1.1

docker exec -d "$C_A" socat TCP-LISTEN:2222,reuseaddr EXEC:"echo hi"
docker exec -d "$C_A" socat -u UDP-RECV:40001 OPEN:/tmp/prove-udp,creat,trunc
sleep 0.5
expect 1 "b cannot open TCP to a (via rtr-a:2222) unsolicited" ./jh exec b socat -T2 - TCP:$WAN_RTRA:2222,connect-timeout=2
./jh exec b sh -c "echo from-b | socat -u - UDP:$WAN_RTRA:40001"; sleep 0.5
expect 1 "b cannot deliver UDP to a (via rtr-a:40001) unsolicited" ./jh exec a test -s /tmp/prove-udp
for p in $(docker exec "$C_A" pgrep socat); do docker exec "$C_A" kill "$p"; done

expect 0 "UDP hole punch a<->b succeeds (both cone)"     ./jh punch a b 40300
expect 1 "UDP hole punch a<->c fails (c hard: $HARD_MODE)" ./jh punch a c 40310
expect 1 "UDP hole punch b<->c fails (c hard: $HARD_MODE)" ./jh punch b c 40320

./jh tunnel b >/dev/null 2>&1
expect 0 "ssh a -> relay -> b (ProxyJump over b's reverse tunnel)" \
  ./jh exec a su - jaunt -c "ssh -J relay -p 2202 jaunt@127.0.0.1 true"

./jh delay 20 >/dev/null 2>&1
echo "      with 20ms/hop: ping a->relay: $(./jh exec a ping -c3 -i0.2 -q $IP_RELAY | tail -1)"
echo "      direct punched a<->b: $(./jh punch a b 40330 | grep -o 'rtt_ms=[^ ]*' | head -1)"
echo "      ssh-via-relay $(./jh exec a su - jaunt -c "python3 /harness/node/keystroke_rtt.py 20 -- ssh -tt -o ObscureKeystrokeTiming=no -J relay -p 2202 jaunt@127.0.0.1 'echo READY; exec cat'")"
./jh delay 0 >/dev/null 2>&1
[ $fail = 0 ] && echo "ALL PASS" || echo "SOME CHECKS FAILED"
exit $fail
