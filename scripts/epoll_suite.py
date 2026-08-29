#!/usr/bin/env python3
"""Push and run the epoll/poll/select correctness probe against a running guest.

`epollops` is this family's regression gate, and until it existed there was
none: every incident it covers was found by pointing bun, tokio, nginx or cargo
at a live socket and waiting to see whether it hung. It probes op by op —
EPOLLET re-arm on a drained read and on a blocked write, the pipe EOF edge,
epoll_ctl's errno set, a zero timeout being non-blocking, level-triggered
repetition, poll(2)'s unrequested POLLHUP, select(2) overwriting exceptfds and
counting bits rather than fds, and the TCP group (a listener reporting EPOLLIN,
and a peer close reporting the buffered data and then the hangup).

It lives in `userspace/forktest/c_stress/` beside `futexops.c`, which it is
modelled on; this builds it from source and pushes it, so what runs in the guest
is what is in the tree right now.

The same static musl binary runs on Linux, which is what proves the probe
itself is right — `--linux` does that through Docker. Every FAIL in the guest
should be a PASS there.

Usage:
  scripts/epoll_suite.py --port 2322              # build, push, run
  scripts/epoll_suite.py --port 2322 --no-build   # reuse what is already there
  scripts/epoll_suite.py --linux                  # run it on Linux instead

Exit status is 0 only if the probe passed, so it can gate an A/B arm. A DIVERGE
line is a *known*, documented difference from Linux (see "Known divergences" in
docs/reference/subsystems/syscalls/poll.md) and does not fail the run; a FAIL
does, and so does a probe that printed nothing.
"""
import argparse
import base64
import pathlib
import re
import subprocess
import sys

PROBE = "epollops"
SRC = pathlib.Path(__file__).resolve().parent.parent / "userspace/forktest/c_stress"


def ssh(port, cmd, timeout=900):
    p = subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-p", str(port), "root@localhost", cmd],
        capture_output=True, timeout=timeout)
    return p.stdout.decode(errors="replace") + p.stderr.decode(errors="replace")


def push(port, path, dest):
    data = base64.b64encode(path.read_bytes())
    subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-p", str(port), "root@localhost",
         f"base64 -d > {dest} && chmod +x {dest}"],
        input=data, capture_output=True, timeout=600)


def verdict(out):
    """The probe passed only if it said so.

    Absence of FAIL is not success: a probe that died before printing anything,
    or one whose output never reached us, would otherwise read as clean. This is
    the property `futex_suite.py` has and the reason it is copied here rather
    than simplified — a silent probe scored as a pass turns an A/B arm into a
    coin flip.
    """
    passes = len(re.findall(r"^PASS ", out, re.M))
    fails = len(re.findall(r"^FAIL ", out, re.M))
    diverges = len(re.findall(r"^DIVERGE ", out, re.M))
    skips = len(re.findall(r"^SKIP ", out, re.M))
    summary = f"{passes} PASS, {fails} FAIL, {diverges} DIVERGE, {skips} SKIP"
    if not re.search(r"^epollops: \d+ FAIL", out, re.M):
        return False, summary + " — the probe never printed its summary line"
    return fails == 0 and passes > 0, summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--linux", action="store_true",
                    help="run the same binary on Linux (Docker) instead of the guest")
    ap.add_argument("--timeout", type=int, default=600)
    a = ap.parse_args()

    binary = SRC / PROBE
    if not a.no_build:
        subprocess.run(
            ["aarch64-linux-musl-gcc", "-O2", "-static", "-o", str(binary),
             str(SRC / f"{PROBE}.c")], check=True)
        print(f"built {PROBE}")

    if a.linux:
        out = subprocess.run(
            ["docker", "run", "--rm", "--platform", "linux/arm64",
             "-v", f"{SRC}:/w", "-w", "/w", "alpine:3.20", f"./{PROBE}"],
            capture_output=True, timeout=a.timeout)
        out = out.stdout.decode(errors="replace") + out.stderr.decode(errors="replace")
    else:
        push(a.port, binary, f"/tmp/{PROBE}")
        out = ssh(a.port, f"/tmp/{PROBE} 2>&1", timeout=a.timeout)

    ok, summary = verdict(out)
    where = "linux" if a.linux else f"guest :{a.port}"
    print(f"\n===== {PROBE} on {where}: {'PASS' if ok else 'FAIL'} ({summary}) =====")
    print(out.strip()[-8000:])
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
