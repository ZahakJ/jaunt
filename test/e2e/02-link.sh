#!/usr/bin/env bash
# Linking with the ritual (design §6) in the Docker NAT harness. a starts the mesh, b
# joins with a code (typed with spaces and capitals), a wrong code fails and burns the
# code, c joins through b and a learns of c without being touched; all three see each
# other. Every link is scripted with --yes (the sigil check is compared from the outputs).
#   JAUNT_BIN=target/release/jaunt test/e2e/02-link.sh       (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=02-link
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-*

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
fresh_all() { h_fresh a with-relay && h_fresh b && h_fresh c; }
wrong_guess() { ! h_link b c 's/-[a-z-]*-[a-z-]*$/-zoom-zoom/'; }
alone() { [ -z "$(h_members "$1")" ]; }
check "fresh a (knows the relay), b and c (do not)" fresh_all
check "relay connected on a" wait_until 20 sh -c 'docker exec -u jaunt jaunt-a env JAUNT_HOME=/home/jaunt/jh PATH=/opt/jaunt/bin:/usr/bin jaunt check | grep -q "relay connected"'

note "a starts the mesh, b joins (code typed as 'NNNN WORD WORD')"
check "a link / b join succeed" h_link a b 's/-/ /g; s/.*/\U&/'
sed 's/^/    a| /' "$OUT-link-a-b.inv"; sed 's/^/    b| /' "$OUT-link-a-b.join"
check "a said it is the first machine" grep -q "first machine" "$OUT-link-a-b.inv"
check "steps on a: rendezvous, key exchange, sealed, confirmed, linked" \
  sh -c 'for s in "^rendezvous" "^key exchange" "^identities sealed: b" "^confirmed on the other" "^linked: b joined"; do grep -q "$s" "$0" || exit 1; done' "$OUT-link-a-b.inv"
h_check_lines "$OUT-link-a-b.inv" > "$OUT-check-a"; h_check_lines "$OUT-link-a-b.join" > "$OUT-check-b"
check "both screens show the same sigil and words" sh -c '[ -s "$0" ] && cmp -s "$0" "$1"' "$OUT-check-a" "$OUT-check-b"
check "b saved the relay it was given" h_exec b grep -q relay.jaunt.test /home/jaunt/jh/config/config.toml
check "a <-> b connected" wait_until 40 h_is_connected a b
check "b <-> a connected" wait_until 40 h_is_connected b a

note "wrong code: b invites, c guesses the words"
check "c's wrong guess fails" wrong_guess
sed 's/^/    b| /' "$OUT-link-b-c.inv"; sed 's/^/    c| /' "$OUT-link-b-c.join"
check "c is told it was the wrong code" grep -q "wrong code" "$OUT-link-b-c.join"
check "b says the code is burnt" grep -q "burnt" "$OUT-link-b-c.inv"
check "no sigil was shown for the wrong code" sh -c '! grep -q "^compare" "$0" "$1"' "$OUT-link-b-c.inv" "$OUT-link-b-c.join"
# The right code no longer works: b's pairing endpoint is gone (the dial times out).
right=$(h_exec b awk '/^code:/{print $2; exit}' /home/jaunt/link.out)
h_exec c jaunt join "$right" --relay "$RELAY_URL" --yes > "$OUT-burnt.join" 2>&1
check "the right code fails after the wrong guess" [ $? != 0 ]
sed 's/^/    c| /' "$OUT-burnt.join"
check "a code given in argv draws a warning about the process list" grep -q "visible to other users" "$OUT-burnt.join"
check "c is still alone" alone c

note "c joins through b; a must learn of c without being touched"
check "b link / c join succeed" h_link b c
sed 's/^/    b| /' "$OUT-link-b-c.inv"; sed 's/^/    c| /' "$OUT-link-b-c.join"
check "b reported that a acknowledged the new machine" grep -q "^  told a" "$OUT-link-b-c.inv"
check "a knows c" wait_until 30 h_knows a c
check "a's entry for c says b added it" sh -c "docker exec jaunt-a python3 -c \"
import json; r=json.load(open('/home/jaunt/jh/data/roster.json'))
c=[e for e in r['entries'].values() if e['name']=='c'][0]
b=[e for e in r['entries'].values() if e['name']=='b'][0]
assert c['added_by']==b['id'], c\""
for pair in "a b" "a c" "b a" "b c" "c a" "c b"; do
  set -- $pair
  check "$1 sees $2 connected" wait_until 90 h_is_connected "$1" "$2"
done
for m in a b c; do note "jaunt ls on $m"; h_jaunt "$m" ls | cut -f1-5 | sed 's/^/    /'; done
out=$(h_jaunt c run a -- cat /etc/hostname); check "c: run a works after linking ('$out')" [ "$out" = a ]

note "pairing events for the UI"
h_exec a timeout 2 jaunt debug events --history 200 > "$OUT-events-a.jsonl" 2>/dev/null
check "a's event stream has the pair steps" sh -c 'for s in waiting rendezvous key_exchange sealed confirm confirmed linked fan_out_done; do grep -q "\"step\":\"$s\"" "$0" || exit 1; done' "$OUT-events-a.jsonl"
check "the event log never carries the code words" sh -c '! grep -q "$(cut -d- -f2 "$1")" "$0"' "$OUT-events-a.jsonl" "$OUT-link-a-b.raw"

finish
