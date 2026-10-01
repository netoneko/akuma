#!/usr/bin/env python3
"""Does the amd64 console deliver keystrokes to a shell? — local QEMU probe.

Boots `amd64/run.sh` (PVH, `-M microvm`) with the serial line on a pipe, waits
for the kernel to hand the console to `init`, types a few commands and reports
what came back.

What this proves: the console pump -> line discipline -> `/bin/sh` -> echo path,
for whatever `INIT=` selects (`/bin/herd` with a console service, or
`/bin/busybox sh` directly). Input arrives through the 16550, so it exercises
`console::pump_once`, `TerminalState` and the shell, NOT `kbd.rs` (microvm has no
i8042) and NOT the framebuffer.

What it does not prove: a real USB keyboard through the firmware's PS/2
emulation, or anything drawn on a real screen. Those are the metal's claims.

    python3 scripts/utils/amd64_console_probe.py                 # INIT=/bin/herd
    INIT=/bin/busybox python3 scripts/utils/amd64_console_probe.py
    python3 scripts/utils/amd64_console_probe.py --log /tmp/probe.log --boot-timeout 900

Exit status 0 only when every typed command's marker came back. A silent boot
is `NO BOOT`, never a pass.
"""

import argparse
import os
import select
import subprocess
import sys
import time

# A marker the shell prints and the *typed line* does not contain, so seeing it
# proves the command ran rather than that the echo reflected our own keystrokes.
# `echo AB""CD` types `AB""CD` and prints `ABCD`.
CASES = [
    ('echo AKUMA""PROBE$((6*7))', "AKUMAPROBE42"),
    ("echo $0 | tr a-z A-Z", None),  # informational: which shell answered
    ('ls / | head -3; echo END""MARK', "ENDMARK"),
    # Line editing: type BAD, erase it with DEL (0x7f — what kbd.rs's Backspace
    # sends), type GOOD. Only the edited line can print GOODX.
    ('echo BAD\x7f\x7f\x7fGOOD""X', "GOODX"),
]

# `^C` must kill the foreground job and leave the shell alive: start a 100 s
# sleep, interrupt it, and ask for a marker. A dead ISIG path means the marker
# arrives ~100 s later, i.e. never inside --cmd-timeout.
INTERRUPT = ("sleep 100", "echo AFT""ER-INT", "AFTER-INT")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--log", default="/tmp/amd64-console-probe.log")
    ap.add_argument("--boot-timeout", type=int, default=600)
    ap.add_argument("--cmd-timeout", type=int, default=60)
    ap.add_argument("--ready", default="-- running ", help="line that means init started")
    ap.add_argument("--settle", type=float, default=8.0, help="seconds to let init print before typing")
    args = ap.parse_args()

    repo = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    env = dict(os.environ)
    env.setdefault("INIT", "/bin/herd")
    p = subprocess.Popen(
        ["sh", "amd64/run.sh"], cwd=repo, env=env,
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    log = open(args.log, "wb")
    buf = bytearray()

    def pump(timeout: float) -> bool:
        r, _, _ = select.select([p.stdout], [], [], timeout)
        if not r:
            return False
        chunk = os.read(p.stdout.fileno(), 65536)
        if not chunk:
            return False
        log.write(chunk)
        log.flush()
        buf.extend(chunk)
        return True

    def wait_for(needle: bytes, budget: float, start: int = 0) -> int:
        end = time.time() + budget
        while time.time() < end:
            i = buf.find(needle, start)
            if i >= 0:
                return i
            if p.poll() is not None and not pump(0.2):
                return -1
            pump(0.5)
        return buf.find(needle, start)

    try:
        if wait_for(args.ready.encode(), args.boot_timeout) < 0:
            print(f"NO BOOT: never saw {args.ready!r} (log: {args.log})")
            return 2
        print(f"init started after the kernel's own line; log: {args.log}")
        time.sleep(args.settle)
        while pump(0.5):
            pass
        failures = 0
        for line, marker in CASES:
            mark = len(buf)
            p.stdin.write(line.encode().decode("unicode_escape").encode("latin1") + b"\r")
            p.stdin.flush()
            if marker is None:
                time.sleep(3)
                while pump(0.5):
                    pass
                print(f"  info  {line!r} -> {bytes(buf[mark:])[-80:]!r}")
                continue
            # The marker must appear AFTER the echo of what we typed.
            i = wait_for(marker.encode(), args.cmd_timeout, mark)
            ok = i >= 0
            print(f"  {'ok  ' if ok else 'FAIL'}  {line!r} -> {marker!r}")
            failures += 0 if ok else 1
        # ^C.
        job, after, marker = INTERRUPT
        p.stdin.write(job.encode() + b"\r")
        p.stdin.flush()
        time.sleep(4)
        mark = len(buf)
        p.stdin.write(b"\x03")
        p.stdin.flush()
        time.sleep(1)
        p.stdin.write(after.encode() + b"\r")
        p.stdin.flush()
        ok = wait_for(marker.encode(), args.cmd_timeout, mark) >= 0
        print(f"  {'ok  ' if ok else 'FAIL'}  ^C interrupts `{job}` and the shell survives")
        failures += 0 if ok else 1
        print("RESULT:", "PASS" if failures == 0 else f"FAIL ({failures})")
        return 0 if failures == 0 else 1
    finally:
        p.terminate()
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()


if __name__ == "__main__":
    sys.exit(main())
