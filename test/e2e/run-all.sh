#!/usr/bin/env bash
# Run the harness e2e scripts (01-10) against one harness: up once, each script in turn,
# down at the end, then a summary. Each script takes the harness lock and releases it
# when done; the harness stays up between scripts.
#   JAUNT_BIN=target/release/jaunt test/e2e/run-all.sh [04 07 ...]   (default: all)
set -uo pipefail
E2E_NAME=run-all
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
export JAUNT_BIN=$BIN
LOGS=$ROOT/target/tmp/run-all
mkdir -p "$LOGS"
want=("$@")
scripts=()
for s in "$E2E_DIR"/[0-9][0-9]-*.sh; do
  n=$(basename "$s"); n=${n%%-*}
  if [ ${#want[@]} -eq 0 ] || printf '%s\n' "${want[@]}" | grep -qx "$n"; then scripts+=("$s"); fi
done

summary=()
total_fail=0
for s in "${scripts[@]}"; do
  name=$(basename "$s" .sh)
  h_lock || { summary+=("$name	SKIPPED (harness lock busy)"); total_fail=$((total_fail + 1)); continue; }
  echo "==== $name"
  t0=$SECONDS
  E2E_LOCK_HELD=1 KEEP_HARNESS=1 bash "$s" 2>&1 | tee "$LOGS/$name.log"
  rc=${PIPESTATUS[0]}
  h_unlock
  line=$(grep '^==== .* passed, .* failed' "$LOGS/$name.log" | tail -1 | sed 's/^==== //')
  summary+=("$name	rc=$rc	${line:-no summary}	$((SECONDS - t0)) s")
  [ "$rc" = 0 ] || total_fail=$((total_fail + 1))
done

h_lock && { "$HARNESS/down.sh" >/dev/null 2>&1; h_unlock; }
echo
echo "==== summary (logs in $LOGS)"
printf '%s\n' "${summary[@]}" | column -t -s $'\t'
[ "$total_fail" = 0 ]
