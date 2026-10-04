#!/usr/bin/env python3
"""Drive a `jaunt <machine>:<session>` hop under a pseudo-terminal, non-interactively.

  hop_pty.py [--between CMD] [--timeout S] -- jaunt-binary target

Scenario: type a marker command and wait for its output; optionally run CMD (a shell
command that kills and restarts the remote daemon) while the hop stays open and wait for
the client to re-attach (the marker must be repainted by tmux); type a second marker;
leave with Enter ~ . and check the exit. Prints PASS/FAIL lines; exit status 0 = all pass.
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

ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b[()][A-Za-z0-9]|\x1b[=>]|\r")


class Hop:
    def __init__(self, argv):
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.environ.setdefault("TERM", "xterm-256color")
            os.execvp(argv[0], argv)
        self.buf = b""
        self.mark = 0  # search position

    def pump(self, secs):
        end = time.time() + secs
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], max(0.0, end - time.time()))
            if not r:
                break
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            self.buf += data
        return True

    def text(self, start=0):
        return ANSI.sub(b"", self.buf[start:]).decode("utf-8", "replace")

    def expect(self, pattern, secs):
        """Wait until pattern appears after the current mark; move the mark past it."""
        end = time.time() + secs
        rx = re.compile(pattern)
        while time.time() < end:
            m = rx.search(self.text(self.mark))
            if m:
                self.mark = len(self.buf)
                return True
            if not self.pump(0.2):
                m = rx.search(self.text(self.mark))
                return bool(m)
        return False

    def send(self, data):
        for ch in data:
            os.write(self.fd, ch.encode())
            time.sleep(0.02)

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
    ap.add_argument("--between", action="append", default=[], help="shell command run while the hop is open (repeatable)")
    ap.add_argument("--timeout", type=float, default=20)
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
    time.sleep(1.5)
    hop.pump(0.5)
    hop.send("echo MARK$((40+2))\r")
    if not check(hop.expect(r"MARK42", a.timeout), "hop: command output arrives"):
        print(hop.text()[-2000:])
        os.kill(hop.pid, signal.SIGKILL)
        return 1

    for i, cmd in enumerate(a.between):
        r = subprocess.run(cmd, shell=True)
        check(r.returncode == 0, f"between-command {i + 1} ran")
        check(hop.expect(r"link lost", 45), f"[{i + 1}] client shows the link-lost line")
        # tmux repaints the whole screen on re-attach, so the old marker comes back.
        t0 = time.time()
        ok = hop.expect(r"MARK42", 90)
        check(ok, f"[{i + 1}] client re-attached and tmux repainted the session ({time.time() - t0:.1f}s)")
        time.sleep(0.5)
        hop.pump(0.3)
        hop.send(f"echo AFTER$(({i}+100))\r")
        check(hop.expect(rf"AFTER{i + 100}", a.timeout), f"[{i + 1}] typing works after re-attach")

    hop.send("\r~.")
    code = hop.wait_exit(10)
    check(code == 0, f"Enter ~ . disconnects with status 0 (got {code})")
    check("session kept" in hop.text(), "out-banner says the session is kept")
    if failures:
        print("---- last output ----")
        print(hop.text()[-3000:])
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
