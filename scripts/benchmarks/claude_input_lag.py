#!/usr/bin/env python3
"""Measure how long a TUI on an Akuma box takes to show typed input, over an ssh PTY.

    scripts/benchmarks/claude_input_lag.py --host 192.168.1.120 --cmd '/usr/local/bin/claude-glm' \\
        '@Syntax theme' '\\r' '@Security notes' '\\r' '@trust' '\\r' '@>' '~3' 'lag:20' 'dump'

Needs `pyte` (`python3 -m venv v && v/bin/pip install pyte`). The output of the remote command
is fed to a 120x40 virtual terminal, so "what is on the screen" is what a person would see, not
an ANSI-stripped byte stream (Ink redraws with cursor moves, and the stripped stream lies).

Steps, in order:

  @text     wait (up to --step-timeout) until the *screen* contains `text`
  ~N        sleep N seconds, still draining output
  lag:N[:T] type N distinct probe characters one at a time (T = seconds to wait for each, default 30); each is written only after the
            previous one showed, and the time from write to the screen showing one more of
            that character is recorded. Prints every sample and min/median/p90/max
  paste:S   write S wrapped in bracketed-paste markers, report the time until its last
            8 characters are on the screen (`~45` then `paste:` is the /login repro)
  resize    change the window size (sends SIGWINCH through sshd): a wakeup that does not depend on stdin
  dump      print the screen
  anything else is written to the PTY as-is (Python escapes: \\r, \\x1b[B, ...)

The point of `lag:` is the user's complaint that input "takes way too long": a keystroke that
echoes in tens of milliseconds on Linux and in seconds here is the bug, and this gives it a
number that can be compared between kernels, builds and Linux itself.
"""
import argparse
import fcntl
import os
import pty
import select
import statistics
import struct
import sys
import termios
import time

import pyte

PROBES = "QXJKZVWQXJKZVWQXJKZVW"  # capitals the TUIs do not draw as text; each is typed once


class Term:
    def __init__(self, cmd, host, port, term, cols=120, rows=40, lima=None):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        pid, fd = pty.fork()
        if pid == 0 and lima:
            # The Linux side of an A/B: the same command in a Lima VM, through limactl's ssh.
            os.execvp("limactl", ["limactl", "shell", lima, "--", "bash", "-c",
                                  f"stty cols {cols} rows {rows}; export TERM={term}; {cmd}"])
        if pid == 0:
            os.execvp("ssh", ["ssh", "-tt", "-F", "/dev/null", "-o", "StrictHostKeyChecking=no",
                              "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR",
                              "-i", os.path.expanduser("~/.ssh/id_ed25519"), "-p", str(port),
                              f"root@{host}",
                              f"stty cols {cols} rows {rows}; export TERM={term}; {cmd}"])
        self.pid, self.fd = pid, fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.t0 = time.time()
        self.alive = True

    def pump(self, secs):
        """Drain output for up to `secs` seconds."""
        end = time.time() + secs
        while time.time() < end and self.alive:
            r, _, _ = select.select([self.fd], [], [], min(0.02, max(0.0, end - time.time())))
            if r:
                try:
                    d = os.read(self.fd, 65536)
                except OSError:
                    self.alive = False
                    return
                if not d:
                    self.alive = False
                    return
                self.stream.feed(d)

    def text(self):
        return "\n".join(self.screen.display)

    def send(self, s):
        os.write(self.fd, s.encode() if isinstance(s, str) else s)


def wait_for(t, needle, timeout):
    start = time.time()
    while time.time() - start < timeout:
        if needle in t.text():
            return time.time() - start
        t.pump(0.05)
        if not t.alive:
            break
    return None


def lag(t, n, per_char_timeout):
    samples = []
    for i in range(n):
        c = PROBES[i % len(PROBES)]
        before = t.text().count(c)
        t.pump(0.3)  # let the previous redraw settle so it is not credited to this key
        before = t.text().count(c)
        t0 = time.time()
        t.send(c)
        got = None
        while time.time() - t0 < per_char_timeout:
            t.pump(0.01)
            if t.text().count(c) > before:
                got = time.time() - t0
                break
        samples.append(got)
        print(f"  key {i + 1:2d} {c!r}: " + (f"{got * 1000:8.1f} ms" if got is not None else f"NOT SEEN in {per_char_timeout:.0f}s"))
        if not t.alive:
            break
    ok = sorted(s for s in samples if s is not None)
    lost = sum(1 for s in samples if s is None)
    if ok:
        p90 = ok[min(len(ok) - 1, int(len(ok) * 0.9))]
        print(f"lag: n={len(samples)} lost={lost} min={ok[0]*1000:.1f} median={statistics.median(ok)*1000:.1f} "
              f"p90={p90*1000:.1f} max={ok[-1]*1000:.1f} ms")
    else:
        print(f"lag: n={len(samples)} lost={lost}: nothing ever showed")
    return samples


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--cmd", required=True)
    ap.add_argument("--lima", help="run --cmd in this Lima VM instead of ssh (Linux A/B)")
    ap.add_argument("--term", default="xterm-256color")
    ap.add_argument("--step-timeout", type=float, default=120)
    ap.add_argument("steps", nargs="*")
    a = ap.parse_args()

    t = Term(a.cmd, a.host, a.port, a.term, lima=a.lima)
    for step in a.steps:
        el = f"{time.time() - t.t0:6.1f}s"
        if step.startswith("@"):
            w = wait_for(t, step[1:], a.step_timeout)
            print(f"{el} wait {step[1:]!r}: " + (f"seen after {w:.1f}s" if w is not None else "TIMEOUT"))
            if w is None:
                print(t.text())
                break
        elif step.startswith("~"):
            t.pump(float(step[1:]))
            print(f"{el} slept {step[1:]}")
        elif step.startswith("lag:"):
            n, _, per = step[4:].partition(":")
            print(f"{el} lag probe")
            lag(t, int(n), float(per) if per else 30)
        elif step == "resize":
            fcntl.ioctl(t.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 41, 121, 0, 0))
            print(f"{el} resized to 121x41")
            t.pump(0.2)
        elif step.startswith("paste:"):
            body = step[6:].encode().decode("unicode_escape")
            tail = body[-8:]
            t.pump(0.3)
            t0 = time.time()
            t.send("\x1b[200~" + body + "\x1b[201~")
            got = wait_for(t, tail, 30)
            print(f"{el} paste {len(body)} chars: " + (f"shown after {got*1000:.0f} ms" if got is not None else "NOT SEEN in 30s"))
        elif step == "dump":
            print("\n".join(l.rstrip() for l in t.screen.display).rstrip())
        else:
            time.sleep(0.3)
            t.send(step.encode().decode("unicode_escape"))
            print(f"{el} sent {step!r}")
            t.pump(0.2)
        if not t.alive:
            print("(remote closed)")
            break
    try:
        os.kill(t.pid, 9)
    except OSError:
        pass


if __name__ == "__main__":
    main()
