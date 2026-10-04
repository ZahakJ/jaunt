#!/usr/bin/env python3
"""Crude keystroke round-trip: keystroke_rtt.py <n> -- <command...>
Runs <command> (e.g. ssh -tt ... cat) under a pty, waits for it to settle, then n times writes one
byte and times how long until that byte is echoed back by the remote pty. Prints min/median/p90/max ms."""
import os, pty, sys, time, select, statistics
n = int(sys.argv[1]); cmd = sys.argv[sys.argv.index("--") + 1:]
pid, fd = pty.fork()
if pid == 0:
    os.execvp(cmd[0], cmd)
def drain(t):
    end = time.time() + t; buf = b""
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try: buf += os.read(fd, 4096)
            except OSError: break
    return buf
# wait for remote 'READY' marker
buf = b""; end = time.time() + 20
while b"READY" not in buf and time.time() < end:
    buf += drain(0.2)
if b"READY" not in buf:
    print("no READY marker; output was:", buf[-500:]); sys.exit(1)
drain(0.5)
samples = []
for i in range(n):
    ch = bytes([ord("a") + i % 26])
    t0 = time.perf_counter(); os.write(fd, ch); got = b""
    while ch not in got and time.perf_counter() - t0 < 5:
        r, _, _ = select.select([fd], [], [], 5)
        if r: got += os.read(fd, 4096)
    samples.append((time.perf_counter() - t0) * 1000); time.sleep(0.05)
os.write(fd, b"\x04"); drain(0.3)
try: os.kill(pid, 9)
except Exception: pass
q = sorted(samples)
print(f"keystroke_rtt n={n} min={q[0]:.1f} median={statistics.median(q):.1f} p90={q[int(0.9*(n-1))]:.1f} max={q[-1]:.1f} ms")
