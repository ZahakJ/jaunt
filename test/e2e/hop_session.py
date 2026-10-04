#!/usr/bin/env python3
"""Drive one interactive hop under a pseudo-terminal and run disruptions while it is open.

  hop_session.py [options] -- <command that hops, e.g. docker exec -it ... jaunt b:s>

On attach it marks the shell (`KEEP=k42`) so that later probes prove it is the same shell,
not a new one. Each --step NAME=CMD runs CMD (a host shell command, e.g. "jh drop a 60")
while the hop stays open, then probes every 2 s by typing `echo R<n>_$((k+1000))_$KEEP`
until the expanded answer appears; the time from CMD's end to that answer is the recovery
time. Output: PASS/FAIL lines and one `STEP name=<n> recovered_s=<x> link_lost=<yes|no> stall_shown=<yes|no>`
line per step. Exit status 0 = all passed.

  --expect-first RE   before typing anything, RE must appear (a re-hop repaints old output)
  --onward TARGET     from inside the hop, hop on to TARGET, check JAUNT_FROM/JAUNT_MACHINE
                      (--onward-from NAME is the expected JAUNT_FROM), leave it with Enter ~~ .
"""
import argparse
import os
import pty
import re
import select
import signal
import subprocess
import sys
import time

ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[()][A-Za-z0-9]|\x1b[=>]|\r")


class Hop:
    def __init__(self, argv):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ.setdefault("TERM", "xterm-256color")
            os.execvp(argv[0], argv)
        self.buf = b""
        self.mark = 0
        self.alive = True

    def pump(self, secs):
        end = time.time() + secs
        while True:
            left = end - time.time()
            if left <= 0:
                return self.alive
            r, _, _ = select.select([self.fd], [], [], left)
            if not r:
                return self.alive
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                data = b""
            if not data:
                self.alive = False
                return False
            self.buf += data

    def text(self, start=0):
        return ANSI.sub(b"", self.buf[start:]).decode("utf-8", "replace")

    def find(self, pattern, start):
        return re.search(pattern, self.text(start))

    def expect(self, pattern, secs):
        end = time.time() + secs
        while time.time() < end:
            if self.find(pattern, self.mark):
                self.mark = len(self.buf)
                return True
            if not self.pump(0.2):
                break
        if self.find(pattern, self.mark):
            self.mark = len(self.buf)
            return True
        return False

    def send(self, data):
        for ch in data:
            try:
                os.write(self.fd, ch.encode())
            except OSError:
                return
            time.sleep(0.015)

    def wait_exit(self, secs):
        end = time.time() + secs
        while time.time() < end:
            self.pump(0.1)
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                return os.waitstatus_to_exitcode(status)
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--step", action="append", default=[], help="NAME=shell command run while the hop is open")
    ap.add_argument("--expect-first")
    ap.add_argument("--onward")
    ap.add_argument("--onward-from")
    ap.add_argument("--recover-timeout", type=float, default=150)
    ap.add_argument("argv", nargs="+")
    a = ap.parse_args()
    failures = 0

    def check(ok, what):
        nonlocal failures
        print(("PASS " if ok else "FAIL ") + what, flush=True)
        if not ok:
            failures += 1
        return ok

    hop = Hop(a.argv)
    hop.pump(2.0)
    if a.expect_first:
        check(hop.expect(a.expect_first, 15), f"re-hop shows the old session contents ({a.expect_first})")
    hop.send("export KEEP=k$((6*7)); echo MARK$((40+2))\r")
    if not check(hop.expect(r"MARK42", 30), "hop: typed command's output arrives"):
        print(hop.text()[-2000:])
        os.kill(hop.pid, signal.SIGKILL)
        return 1

    if a.onward:
        hop.send(f"jaunt {a.onward}\r")
        hop.pump(3.0)
        hop.send("echo FROM=$JAUNT_FROM-AT=$JAUNT_MACHINE\r")
        at = a.onward.split(":")[0]
        ok = hop.expect(rf"FROM={re.escape(a.onward_from or '')}-AT={re.escape(at)}\b", 30)
        m = re.findall(r"FROM=([\w.-]*)-AT=(\w+)", hop.text())
        check(ok, f"onward hop: inner shell reports JAUNT_FROM, JAUNT_MACHINE = {m[-1] if m else None}")
        hop.send("\r~~.")
        check(hop.expect(r"kept", 15), "onward hop left with Enter ~~ . (inner session kept)")
        time.sleep(0.5)
        hop.send("echo BACK=$JAUNT_MACHINE\r")
        check(hop.expect(rf"BACK={re.escape(a.onward_from or '')}\b", 15), "back in the first hop's shell")

    for i, spec in enumerate(a.step):
        name, _, cmd = spec.partition("=")
        start = len(hop.buf)
        p = subprocess.Popen(cmd, shell=True)
        while p.poll() is None:
            hop.pump(0.2)
        check(p.returncode == 0, f"[{name}] disruption command ran")
        t0 = time.time()
        k = 0
        recovered = None
        want = rf"R{i}_1\d\d\d_k42"
        next_probe = 0.0
        while time.time() - t0 < a.recover_timeout:
            if time.time() >= next_probe:
                hop.send(f"echo R{i}_$(({k}+1000))_$KEEP\r")
                k += 1
                next_probe = time.time() + 2.0
            hop.pump(0.1)
            if hop.find(want, start):
                recovered = time.time() - t0
                break
            if not hop.alive:
                break
        lost = "yes" if hop.find(r"link lost", start) else "no"
        stall = "yes" if hop.find(r"no answer for \d+s", start) else "no"
        hop.mark = len(hop.buf)
        rs = f"{recovered:.1f}" if recovered is not None else "none"
        print(f"STEP name={name} recovered_s={rs} link_lost={lost} stall_shown={stall}", flush=True)
        check(recovered is not None, f"[{name}] same shell answers again ({rs} s after the disruption ended, link lost shown: {lost})")
        if recovered is None:
            print(hop.text(start)[-1500:])

    time.sleep(0.5)
    hop.send("\r~.")
    code = hop.wait_exit(15)
    check(code == 0, f"Enter ~ . disconnects with status 0 (got {code})")
    check("kept" in hop.text(), "out-banner says the session is kept")
    if failures:
        print("---- last output ----")
        print(hop.text()[-3000:])
    if code is None:
        os.kill(hop.pid, signal.SIGKILL)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
