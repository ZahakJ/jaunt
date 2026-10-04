#!/usr/bin/env python3
"""Keystroke round-trip time through an interactive session, the same way for every tool.

  keystroke.py <samples> -- <command that opens an interactive session on the far machine>

Runs the command under a pty, waits 3 s, then types a line that starts `cat` in the remote
shell behind a READY marker. Then, <samples> times: write one letter, time until that
letter comes back (the far pty echoes it, the far tmux redraws it, the bytes return). The
first 10 samples are warm-up and dropped. Prints one JSON object: samples (ms), median,
p95, min, max.
"""
import json
import os
import pty
import re
import select
import statistics
import sys
import time

ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[()][A-Za-z0-9]|\x1b[=>]")
WARMUP = 10


def main():
    n = int(sys.argv[1])
    cmd = sys.argv[sys.argv.index("--") + 1:]
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.execvp(cmd[0], cmd)

    def drain(t):
        end = time.time() + t
        buf = b""
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    buf += os.read(fd, 65536)
                except OSError:
                    break
        return buf

    buf = drain(3.0)
    os.write(fd, b"clear; echo RE''ADY; exec cat\r")
    end = time.time() + 30
    while b"READY" not in ANSI.sub(b"", buf) and time.time() < end:
        buf += drain(0.2)
    if b"READY" not in ANSI.sub(b"", buf):
        print(json.dumps({"error": "no READY marker", "tail": buf[-400:].decode("utf-8", "replace")}))
        return 1
    drain(1.0)
    samples = []
    for i in range(n + WARMUP):
        ch = bytes([ord("a") + i % 26])
        t0 = time.perf_counter()
        os.write(fd, ch)
        got = b""
        while ch not in ANSI.sub(b"", got) and time.perf_counter() - t0 < 5:
            r, _, _ = select.select([fd], [], [], 5)
            if r:
                try:
                    got += os.read(fd, 65536)
                except OSError:
                    break
        dt = (time.perf_counter() - t0) * 1000
        if i >= WARMUP:
            samples.append(dt)
        time.sleep(0.05)
        if i % 60 == 59:  # keep the line short
            os.write(fd, b"\r")
            drain(0.3)
    try:
        os.kill(pid, 9)
    except OSError:
        pass
    q = sorted(samples)
    print(json.dumps({
        "n": len(q),
        "median": round(statistics.median(q), 2),
        "p95": round(q[int(0.95 * (len(q) - 1))], 2),
        "min": round(q[0], 2),
        "max": round(q[-1], 2),
        "timeouts": sum(1 for s in q if s >= 5000),
        "samples": [round(s, 3) for s in samples],
    }))
    return 0


if __name__ == "__main__":
    sys.exit(main())
