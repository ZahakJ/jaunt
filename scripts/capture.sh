#!/usr/bin/env bash
# Capture jaunt's views from a real terminal (tmux) and render them to PNG.
#
#   scripts/capture.sh [OUT_DIR]        every view x size x colour mode (default OUT_DIR:
#                                       target/tmp/captures)
#   scripts/capture.sh --readme         the chosen few, into captures/ (they go in the README)
#
# Each view runs in `tmux -L jaunt-test` at 120x36, 80x24 and 60x20 (a tmux popup), in
# truecolor, 16 colours and no colour; `tmux capture-pane -e -p` grabs the cells and
# scripts/ansi2svg.py + rsvg-convert draw them. Uses the demo (simulated data) only and
# a throwaway JAUNT_HOME, so it touches no daemon and nothing outside target/tmp.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
README=""
if [ "${1:-}" = --readme ]; then README=1; shift; fi
OUT="${1:-$ROOT/target/tmp/captures}"
BIN="${JAUNT_BIN:-$ROOT/target/debug/jaunt}"
WORK="$ROOT/target/tmp/capture-work"
SIZES="${SIZES:-120x36 80x24 60x20}"
MODES="${MODES:-truecolor 16 none}"
VIEWS="${VIEWS:-mesh mesh-help ls link-code link-sigil link-close link-done join hop}"
mkdir -p "$OUT" "$WORK/home"
unset TMUX
T() { tmux -L jaunt-test -f /dev/null "$@"; }

cmd_for() {
    case "$1" in
        mesh) echo "$BIN demo mesh --at 9.3" ;;
        mesh-help) echo "$BIN demo mesh --at 9.3" ;;
        ls) echo "$BIN demo ls --at 20; sleep 60" ;;
        link-code) echo "$BIN demo link --at 4.6; sleep 60" ;;
        link-sigil) echo "$BIN demo link --at 7.0; sleep 60" ;;
        link-done) echo "$BIN demo link --at 13.6; sleep 60" ;;
        link-close) echo "$BIN demo link --at 11.6; sleep 60" ;;
        join) echo "$BIN demo link --join --at 7.0; sleep 60" ;;
        hop) echo "$BIN demo hop; sleep 60" ;;
    esac
}

for view in $VIEWS; do
    for size in $SIZES; do
        cols=${size%x*}; rows=${size#*x}
        for mode in $MODES; do
            name="$view-$size-$mode"
            T kill-session -t jaunt-cap 2>/dev/null || true
            T new-session -d -s jaunt-cap -x "$cols" -y "$rows" \
                -e JAUNT_HOME="$WORK/home" -e JAUNT_COLOR="$mode" -e LANG=C.UTF-8 -e TERM=tmux-256color \
                "$(cmd_for "$view")"
            sleep 0.6
            if [ "$view" = mesh-help ]; then T send-keys -t jaunt-cap '?'; sleep 0.3; fi
            T capture-pane -e -p -t jaunt-cap > "$WORK/$name.ans"
            T kill-session -t jaunt-cap 2>/dev/null || true
            case "$view" in mesh*) trim="" ;; *) trim=1 ;; esac
            TRIM=$trim python3 "$ROOT/scripts/ansi2svg.py" "$WORK/$name.ans" "$WORK/$name.svg" "$cols" "$rows" "jaunt $view · ${cols}x${rows} · $mode"
            rsvg-convert "$WORK/$name.svg" -o "$OUT/$name.png"
        done
    done
done
# The tmux server may be shared with other tests: only our session is removed.
if [ -n "$README" ]; then
    mkdir -p "$ROOT/captures"
    for f in mesh-120x36-truecolor mesh-80x24-truecolor mesh-60x20-truecolor mesh-80x24-16 mesh-80x24-none \
             mesh-help-80x24-truecolor ls-120x36-truecolor ls-80x24-16 ls-80x24-none \
             link-code-80x24-truecolor link-code-60x20-truecolor link-sigil-80x24-truecolor \
             link-close-80x24-truecolor link-done-80x24-truecolor join-80x24-truecolor \
             hop-80x24-truecolor hop-80x24-16; do
        cp "$OUT/$f.png" "$ROOT/captures/$f.png"
    done
    OUT="$ROOT/captures"
fi
echo "captures in $OUT"
