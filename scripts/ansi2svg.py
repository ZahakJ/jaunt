#!/usr/bin/env python3
"""Render `tmux capture-pane -e -p` output (SGR colours) to an SVG terminal window.

usage: ansi2svg.py IN.ans OUT.svg COLS ROWS [TITLE]
The 16 ANSI colours use a Catppuccin-Mocha-like palette so 16-colour captures look like
a typical dark terminal; truecolor values are drawn as given.
"""
import html
import os
import re
import sys

PAL = [
    "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
    "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]
BG = "#1b1b26"
FG = "#cdd6f4"
FONT = "JetBrainsMono Nerd Font Mono"
SIZE = 15
CW = SIZE * 0.6
LH = round(SIZE * 1.3)
PAD = 18
BAR = 30


def xterm256(n):
    if n < 16:
        return PAL[n]
    if n < 232:
        n -= 16
        v = [0, 95, 135, 175, 215, 255]
        return "#%02x%02x%02x" % (v[n // 36], v[(n // 6) % 6], v[n % 6])
    g = 8 + (n - 232) * 10
    return "#%02x%02x%02x" % (g, g, g)


def parse(text, cols, rows):
    grid = [[(" ", None, None, False) for _ in range(cols)] for _ in range(rows)]
    for y, line in enumerate(text.split("\n")[:rows]):
        fg, bg, bold, rev = None, None, False, False
        x = 0
        i = 0
        while i < len(line):
            m = re.match(r"\x1b\[([0-9;:]*)m", line[i:])
            if m:
                ps = [p for p in re.split("[;:]", m.group(1))] or ["0"]
                ps = [int(p) if p else 0 for p in ps]
                k = 0
                while k < len(ps):
                    p = ps[k]
                    if p == 0:
                        fg, bg, bold, rev = None, None, False, False
                    elif p == 1:
                        bold = True
                    elif p == 22:
                        bold = False
                    elif p == 7:
                        rev = True
                    elif p == 27:
                        rev = False
                    elif 30 <= p <= 37:
                        fg = PAL[p - 30]
                    elif 90 <= p <= 97:
                        fg = PAL[p - 90 + 8]
                    elif p == 39:
                        fg = None
                    elif 40 <= p <= 47:
                        bg = PAL[p - 40]
                    elif 100 <= p <= 107:
                        bg = PAL[p - 100 + 8]
                    elif p == 49:
                        bg = None
                    elif p in (38, 48) and k + 1 < len(ps):
                        if ps[k + 1] == 2:
                            # 38;2;r;g;b or 38:2::r:g:b (colourspace slot may be empty)
                            rest = ps[k + 2:k + 6]
                            if len(rest) == 4 and m.group(1).count(":") >= 4:
                                rest = rest[1:]
                            c = "#%02x%02x%02x" % tuple(rest[:3])
                            k += 4 if len(rest) == 3 else 5
                        else:
                            c = xterm256(ps[k + 2])
                            k += 2
                        if p == 38:
                            fg = c
                        else:
                            bg = c
                    k += 1
                i += len(m.group(0))
                continue
            if line[i] == "\x1b":
                i += 1
                continue
            if x < cols:
                f, b = fg or FG, bg
                if rev:
                    f, b = (bg or BG), (fg or FG)
                grid[y][x] = (line[i], f, b, bold)
            x += 1
            i += 1
    return grid


def render(grid, cols, rows, title):
    w = int(PAD * 2 + cols * CW)
    h = int(PAD * 2 + BAR + rows * LH)
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">',
        f'<rect x="0" y="0" width="{w}" height="{h}" rx="10" fill="{BG}"/>',
        f'<rect x="0.5" y="0.5" width="{w-1}" height="{h-1}" rx="10" fill="none" stroke="#313244"/>',
    ]
    for k, c in enumerate(["#f38ba8", "#f9e2af", "#a6e3a1"]):
        out.append(f'<circle cx="{PAD + 6 + k * 20}" cy="{PAD + 4}" r="6" fill="{c}" opacity="0.8"/>')
    if title:
        out.append(f'<text x="{w/2}" y="{PAD + 9}" fill="#6c7086" font-family="{FONT}" font-size="13" text-anchor="middle">{html.escape(title)}</text>')
    top = PAD + BAR
    for y, row in enumerate(grid):
        x = 0
        while x < cols:
            b = row[x][2]
            if b:
                x0 = x
                while x < cols and row[x][2] == b:
                    x += 1
                out.append(f'<rect x="{PAD + x0 * CW:.2f}" y="{top + y * LH}" width="{(x - x0) * CW:.2f}" height="{LH}" fill="{b}"/>')
            else:
                x += 1
    base = LH * 0.78
    # Block elements as rectangles: a real terminal draws them edge to edge.
    blocks = {"█": (0, 1), "▀": (0, 0.5), "▄": (0.5, 1)}
    for k, ch in enumerate("▁▂▃▄▅▆▇"):
        blocks.setdefault(ch, (1 - (k + 1) / 8, 1))
    for y, row in enumerate(grid):
        for x, (ch, f, b, bold) in enumerate(row):
            if ch in blocks:
                t, btm = blocks[ch]
                out.append(f'<rect x="{PAD + x * CW:.2f}" y="{top + y * LH + t * LH:.2f}" width="{CW + 0.3:.2f}" height="{(btm - t) * LH + 0.3:.2f}" fill="{f}"/>')
    for y, row in enumerate(grid):
        x = 0
        while x < cols:
            ch, f, b, bold = row[x]
            if ch == " " or ch in blocks:
                x += 1
                continue
            x0 = x
            s = ""
            while x < cols and row[x][1] == f and row[x][3] == bold and row[x][0] not in blocks:
                s += row[x][0]
                x += 1
            s = s.rstrip(" ")
            wt = ' font-weight="bold"' if bold else ""
            out.append(
                f'<text x="{PAD + x0 * CW:.2f}" y="{top + y * LH + base:.2f}" fill="{f}"{wt} '
                f'textLength="{len(s) * CW:.2f}" lengthAdjust="spacingAndGlyphs" xml:space="preserve">{html.escape(s)}</text>'
            )
    out.append("</svg>")
    return "\n".join(out)


def main():
    src, dst, cols, rows = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    title = sys.argv[5] if len(sys.argv) > 5 else ""
    text = open(src, encoding="utf-8", errors="replace").read()
    grid = parse(text, cols, rows)
    if os.environ.get("TRIM"):
        # Static output: drop the empty rows below it (keep one blank row of margin).
        used = [i for i, row in enumerate(grid) if any(c[0] != " " or c[2] for c in row)]
        rows = min(rows, (used[-1] + 2) if used else 1)
        grid = grid[:rows]
    svg = render(grid, cols, rows, title)
    svg = svg.replace("<svg ", f'<svg font-family="{FONT}" font-size="{SIZE}" ', 1)
    open(dst, "w", encoding="utf-8").write(svg)


if __name__ == "__main__":
    main()
