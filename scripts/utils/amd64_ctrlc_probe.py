#!/usr/bin/env python3
"""Time whether `^C` over `ssh` actually kills a foreground job.

# Why a script

This measurement was run by hand on 2026-09-11 and produced the one number that
mattered — QEMU 4.8 s, bare metal 30.2 s, i.e. the metal never killed the job —
and then existed nowhere. It has to be repeatable on two machines that are
minutes apart, so it is a script.

# What it measures, and why it is a clock and not an eyeball

Reading the session's output tells you very little: a shell that ignored `^C`
and a shell that handled it both end up back at a prompt eventually, and
`NOTREACHED` can be swallowed by a torn console. So this times it:

    echo GO; sleep 30; echo NOTREACHED

wait for `GO`, send `0x03` after `--delay` seconds, then immediately type
`echo BACK`. `BACK` cannot be echoed until the shell is back in control, so the
interval from `GO` to `BACK` is exactly "how long the foreground job survived".

    killed   ~= delay + a round trip      (a few seconds)
    survived ~= the full `sleep` duration (~30 s)

`NOTREACHED` in the transcript is the corroborating observable, reported but not
relied upon.

A real pty is mandatory: `ssh -tt` sends the `pty-req` that sets
`SPAWN_FLAG_PTY`, and without it the session never gets a terminal-backed
channel, so the kernel's ISIG branch — the thing under test — is unreachable by
construction. Hence `pty.fork()` rather than `subprocess`.

# Usage

    python3 scripts/utils/amd64_ctrlc_probe.py --port 2224          # local QEMU
    python3 scripts/utils/amd64_ctrlc_probe.py --host 192.168.1.220 --port 2222
    python3 scripts/utils/amd64_ctrlc_probe.py -n 3                 # repeat

Exit status is 0 only if every trial killed the job.
"""

import argparse
import os
import pty
import re
import select
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

DEFAULT_KEY = os.path.join(
    REPO, "target", "x86_64-unknown-none", "release", "amd64-ssh-test-key"
)

# Escape sequences and the terminal's own echo make a plain `in` test unreliable
# — the marker can arrive split across reads, and it is also echoed back before
# the shell ever runs it. Both are handled by matching on the *accumulated*
# transcript and by counting occurrences: the echo is the first, the shell's
# output is the second.
GO = "GO"
BACK = "BACK"


def _drain(fd, buf, deadline):
    """Read whatever is available until `deadline`, appending to `buf`.

    Returns False once the child has closed the pty (ssh exited).
    """
    while time.monotonic() < deadline:
        r, _, _ = select.select([fd], [], [], 0.2)
        if not r:
            return True
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return False
        if not chunk:
            return False
        buf.append(chunk.decode("utf-8", "replace"))
        return True
    return True


def _count(transcript, marker):
    return len(re.findall(re.escape(marker), transcript))


def trial(host, port, key, delay, sleep_secs, timeout, verbose):
    """One measurement. Returns (killed, seconds, transcript)."""
    argv = [
        "ssh", "-tt",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-o", "IdentitiesOnly=yes",
        "-o", "ConnectTimeout=10",
        "-i", key,
        "-p", str(port),
        f"root@{host}",
    ]
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(argv[0], argv)
        os._exit(127)

    buf = []
    t_start = time.monotonic()
    hard_deadline = t_start + timeout
    try:
        # Let the session settle and the shell print its first prompt. There is
        # no reliable prompt string to match on (busybox `ash` under a pty may
        # print `~ #`, `#`, or nothing at all if the console tore), so this is a
        # fixed settle rather than a match.
        settle = time.monotonic() + 6
        while time.monotonic() < settle:
            if not _drain(fd, buf, settle):
                break

        cmd = f"echo {GO}; sleep {sleep_secs}; echo NOTREACHED\n"
        os.write(fd, cmd.encode())

        # `GO` twice: once echoed by the terminal as we typed it, once printed
        # by the shell. The second is the one that means the job started.
        t_go = None
        while time.monotonic() < hard_deadline:
            if _count("".join(buf), GO) >= 2:
                t_go = time.monotonic()
                break
            if not _drain(fd, buf, hard_deadline):
                break
        if t_go is None:
            return (False, float("nan"), "".join(buf))

        # Wait, then interrupt, then immediately ask the shell to speak. The
        # `echo BACK` is typed while the job still owns the terminal; it sits in
        # the line discipline until the shell reads again, which is the event
        # being timed.
        wake = t_go + delay
        while time.monotonic() < wake:
            _drain(fd, buf, wake)
        os.write(fd, b"\x03")
        time.sleep(0.2)
        os.write(fd, f"echo {BACK}\n".encode())

        t_back = None
        while time.monotonic() < hard_deadline:
            if _count("".join(buf), BACK) >= 2:
                t_back = time.monotonic()
                break
            if not _drain(fd, buf, hard_deadline):
                break

        transcript = "".join(buf)
        elapsed = (t_back - t_go) if t_back else float("nan")
        # The job was killed if the shell came back well before the sleep would
        # have ended. Half the sleep is a wide margin on purpose: the failing
        # case overshoots by ~25 s, so nothing near the boundary is ambiguous.
        killed = t_back is not None and elapsed < (sleep_secs / 2.0)
        if verbose:
            sys.stderr.write(transcript + "\n")
        return (killed, elapsed, transcript)
    finally:
        try:
            os.write(fd, b"\nexit\n")
        except OSError:
            pass
        try:
            os.close(fd)
        except OSError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--host", default="localhost")
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--key", default=DEFAULT_KEY,
                    help="ssh private key (default: the amd64 test key mkdisk.sh stages)")
    ap.add_argument("--delay", type=float, default=3.0,
                    help="seconds after GO before sending ^C")
    ap.add_argument("--sleep", type=int, default=30,
                    help="how long the foreground job sleeps")
    ap.add_argument("--timeout", type=float, default=90.0,
                    help="hard cap on one trial")
    ap.add_argument("-n", "--trials", type=int, default=1)
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="dump each transcript to stderr")
    args = ap.parse_args()

    if not os.path.exists(args.key):
        sys.stderr.write(f"no ssh key at {args.key}\n")
        return 2

    ok = True
    for i in range(args.trials):
        killed, secs, transcript = trial(
            args.host, args.port, args.key, args.delay, args.sleep,
            args.timeout, args.verbose)
        notreached = "NOTREACHED" in transcript.replace("echo NOTREACHED", "")
        verdict = "KILLED" if killed else "SURVIVED"
        isig = transcript.count("[ISIG]")
        print(f"trial {i + 1}/{args.trials}: {verdict} after {secs:.1f}s"
              f"  (NOTREACHED printed: {notreached}"
              + (f", [ISIG] lines seen: {isig}" if isig else "") + ")")
        ok = ok and killed
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
