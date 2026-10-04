#!/usr/bin/env bash
# Capture the real mesh view on machine a of the Docker harness: a, b, c linked, a <-> b
# direct, c behind the hard NAT (relayed). Writes captures/live-mesh-<size>-truecolor.png.
#   JAUNT_BIN=target/release/jaunt scripts/capture-live.sh
# Takes the harness lock, brings the harness up, and down again at the end.
set -uo pipefail
E2E_NAME=capture-live
. "$(dirname "$0")/../test/e2e/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
WORK=$ROOT/target/tmp/capture-live
mkdir -p "$WORK" "$ROOT/captures"

h_begin
check "mesh a b c linked and connected" h_mesh
check "a -> b direct" wait_until 30 h_is_state a b direct
check "a -> c relayed" wait_until 30 h_is_state a c relay
# Some traffic, so the event log and pulses have something to show.
h_jaunt a run b -- true >/dev/null 2>&1
h_jaunt a run c -- true >/dev/null 2>&1
h_jaunt b run a -- true >/dev/null 2>&1

TM=(tmux -L jaunt-cap -f /dev/null)
for size in 120x36 80x24; do
  cols=${size%x*}; rows=${size#*x}
  h_exec a "${TM[@]}" kill-server >/dev/null 2>&1
  h_exec a "${TM[@]}" new-session -d -s cap -x "$cols" -y "$rows" \
    -e JAUNT_COLOR=truecolor -e LANG=C.UTF-8 -e TERM=tmux-256color jaunt
  # Let a minute of latency samples and a few pings accumulate in the view.
  sleep 25
  h_exec a "${TM[@]}" capture-pane -e -p -t cap > "$WORK/live-mesh-$size.ans"
  h_exec a "${TM[@]}" kill-server >/dev/null 2>&1
  python3 "$ROOT/scripts/ansi2svg.py" "$WORK/live-mesh-$size.ans" "$WORK/live-mesh-$size.svg" "$cols" "$rows" \
    "jaunt (live, harness: a, b direct, c relayed) · ${cols}x${rows}"
  rsvg-convert "$WORK/live-mesh-$size.svg" -o "$ROOT/captures/live-mesh-$size-truecolor.png"
  check "captured $size" test -s "$ROOT/captures/live-mesh-$size-truecolor.png"
done
h_jaunt a ls > "$WORK/ls-a.txt" 2>&1
finish
