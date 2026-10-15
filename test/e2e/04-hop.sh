#!/usr/bin/env bash
# Hopping (design §8) in the Docker NAT harness: a scripted PTY hop a -> b, a typed command
# and its output, an onward hop b -> c from inside that session (JAUNT_FROM/JAUNT_MACHINE),
# Enter ~~ . for the inner hop and Enter ~ . for the outer one, then a second hop to the
# same session that must show the first one's output (tmux kept it).
#   JAUNT_BIN=target/release/jaunt test/e2e/04-hop.sh       (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=04-hop
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-*

h_begin
check "mesh a b c linked and connected" h_mesh
check "a -> b is direct" wait_until 30 h_is_state a b direct

note "hop a -> b, onward to c, back, leave"
# shellcheck disable=SC2046
python3 "$E2E_DIR/hop_session.py" --onward c:onward --onward-from b -- $(h_tty_cmd a b:e2e) > "$OUT-hop1.out" 2>&1
code=$?
sed 's/^/    /' "$OUT-hop1.out" | grep -E '^    (PASS|FAIL)'
check "hop a -> b -> c scenario" [ "$code" = 0 ]
check "tmux session e2e lives on b" h_exec b tmux -L jaunt-test has-session -t e2e
check "tmux session onward lives on c" h_exec c tmux -L jaunt-test has-session -t onward
check "no client is attached on b after ~." wait_until 10 sh -c '[ -z "$(docker exec -u jaunt jaunt-b tmux -L jaunt-test list-clients -t e2e)" ]'

note "hop a -> b again: the session still holds the first hop's output"
# shellcheck disable=SC2046
python3 "$E2E_DIR/hop_session.py" --expect-first 'BACK=b' -- $(h_tty_cmd a b:e2e) > "$OUT-hop2.out" 2>&1
code=$?
sed 's/^/    /' "$OUT-hop2.out" | grep -E '^    (PASS|FAIL)'
check "re-hop finds the session contents" [ "$code" = 0 ]
# The same shell: the variable set in the first hop is still there.
out=$(h_exec b tmux -L jaunt-test capture-pane -p -t e2e -S -200 | grep -c 'MARK42')
check "b's pane history holds the first hop's output ($out lines)" [ "$out" -ge 1 ]

note "hop to c (relayed, hard NAT) and to an unknown machine"
# shellcheck disable=SC2046
python3 "$E2E_DIR/hop_session.py" -- $(h_tty_cmd a c:e2e) > "$OUT-hop3.out" 2>&1
check "hop a -> c over the relay" [ $? = 0 ]
out=$(h_jaunt a run nosuch -- true 2>&1)
check "an unknown machine name is refused in words: $out" grep -q 'no machine called' <<<"$out"

finish
