#!/usr/bin/env python3
"""Load test for the process-per-session `sshd` (userspace/sshd).

Exercises the four properties the fork-per-connection model is supposed to buy,
against a running Akuma VM with sshd on port 2222:

  A. isolation    — N concurrent sessions each get *their own* output back, with
                    no cross-talk between connections.
  B. starvation   — long-lived sessions keep making steady progress while short
                    ones churn alongside them. This is the one the old
                    cooperative multiplexer could fail silently: a session that
                    blocked stalled every peer, and the only symptom was latency.
  C. cap          — more connections than `max_sessions` are refused cleanly,
                    and (critically) the server still serves after the flood.
  D. fault        — SIGKILLing one session's process leaves every other session
                    running. Under the old single-process design this was
                    structurally impossible.

Usage:
    python3 scripts/sshd_concurrency_test.py [--port 2222] [--max-sessions 24]
                                             [--only A,B,C,D]

Exit code is 0 only if every selected test passes.
"""

import argparse
import concurrent.futures
import re
import subprocess
import sys
import time

SSH_BASE = [
    "ssh",
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR",
    "-o", "BatchMode=yes",
]


def ssh(cmd, port, timeout=60):
    """Run one command over SSH. Returns (rc, stdout, stderr)."""
    argv = SSH_BASE + ["-o", f"ConnectTimeout={min(timeout, 30)}",
                       "-p", str(port), "root@localhost", cmd]
    try:
        p = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return -1, "", f"TIMEOUT after {timeout}s"


def hr(title):
    print(f"\n{'=' * 70}\n{title}\n{'=' * 70}")


# ---------------------------------------------------------------------------
# A. Concurrent sessions do not cross-talk
# ---------------------------------------------------------------------------

def test_isolation(port, n=16):
    hr(f"A. isolation — {n} concurrent sessions, each must get its own output")

    def one(i):
        token = f"TOKEN-{i:03d}-{i * 7919 % 100000}"
        # Two echoes with a gap, so the sessions genuinely overlap in time
        # rather than each finishing before the next one starts.
        rc, out, err = ssh(f"echo {token}-a; sleep 2; echo {token}-b", port, timeout=90)
        return i, token, rc, out, err

    t0 = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=n) as ex:
        results = list(ex.map(one, range(n)))
    elapsed = time.time() - t0

    failures = []
    for i, token, rc, out, err in results:
        if rc != 0:
            failures.append(f"session {i}: rc={rc} err={err.strip()[:120]}")
            continue
        if f"{token}-a" not in out or f"{token}-b" not in out:
            failures.append(f"session {i}: missing own token in {out!r}")
            continue
        # Cross-talk check: no other session's token may appear here.
        for j in range(n):
            if j != i and f"TOKEN-{j:03d}-" in out:
                failures.append(f"session {i}: saw session {j}'s output")
                break

    print(f"  {n} sessions finished in {elapsed:.1f}s "
          f"(serial would be ~{2 * n}s+)")
    for f in failures:
        print(f"  FAIL: {f}")

    # Each session sleeps 2s. If sessions were serialized, this could not
    # finish in much under 2*n seconds; a healthy parallel run is ~2-10s.
    if elapsed > 2 * n:
        print(f"  FAIL: took {elapsed:.1f}s — sessions appear serialized")
        failures.append("serialized")

    ok = not failures
    print(f"  A. isolation: {'PASS' if ok else 'FAIL'}")
    return ok


# ---------------------------------------------------------------------------
# B. Long sessions are not starved by short ones
# ---------------------------------------------------------------------------

def test_starvation(port, long_n=6, ticks=15, short_n=40):
    hr(f"B. starvation — {long_n} long sessions ({ticks} ticks each) "
       f"vs {short_n} short ones")

    stop = time.time() + ticks + 25

    def long_session(i):
        # One line per second. The *client* checks they all arrive; a starved
        # session would come back short or time out.
        cmd = f"i=0; while [ $i -lt {ticks} ]; do echo TICK-{i}-$i; i=$((i+1)); sleep 1; done"
        rc, out, err = ssh(cmd, port, timeout=ticks + 60)
        got = len(re.findall(rf"^TICK-{i}-\d+$", out, re.M))
        return ("long", i, rc, got, err)

    def short_session(i):
        # Churn: connect, do a trivial thing, disconnect. These are what would
        # starve the long ones if the server serialized work.
        while time.time() < stop:
            rc, out, err = ssh(f"echo SHORT-{i}", port, timeout=45)
            if rc != 0 or f"SHORT-{i}" not in out:
                return ("short", i, rc, 0, err)
            time.sleep(0.2)
            break  # one round trip each; the pool provides the churn
        return ("short", i, 0, 1, "")

    t0 = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=long_n + 12) as ex:
        futs = [ex.submit(long_session, i) for i in range(long_n)]
        # Stagger the short ones so they arrive *during* the long sessions.
        time.sleep(1.5)
        futs += [ex.submit(short_session, i) for i in range(short_n)]
        results = [f.result() for f in concurrent.futures.as_completed(futs)]
    elapsed = time.time() - t0

    failures = []
    for kind, i, rc, got, err in results:
        if kind == "long":
            if rc != 0:
                failures.append(f"long {i}: rc={rc} err={err.strip()[:120]}")
            elif got != ticks:
                failures.append(f"long {i}: only {got}/{ticks} ticks — starved")
        else:
            if rc != 0:
                failures.append(f"short {i}: rc={rc} err={err.strip()[:120]}")

    longs = [r for r in results if r[0] == "long"]
    print(f"  long sessions completing all {ticks} ticks: "
          f"{sum(1 for r in longs if r[3] == ticks)}/{len(longs)}")
    print(f"  short sessions ok: "
          f"{sum(1 for r in results if r[0] == 'short' and r[2] == 0)}/{short_n}")
    print(f"  wall clock {elapsed:.1f}s (floor is ~{ticks}s)")
    for f in failures:
        print(f"  FAIL: {f}")

    ok = not failures
    print(f"  B. starvation: {'PASS' if ok else 'FAIL'}")
    return ok


# ---------------------------------------------------------------------------
# C. max_sessions cap holds, and the server survives hitting it
# ---------------------------------------------------------------------------

def test_cap(port, max_sessions):
    over = max_sessions + 8
    hr(f"C. cap — {over} simultaneous connections against max_sessions={max_sessions}")

    def hold(i):
        # Hold the session open long enough that they genuinely overlap.
        rc, out, err = ssh(f"sleep 12; echo HELD-{i}", port, timeout=90)
        return i, rc, (f"HELD-{i}" in out)

    with concurrent.futures.ThreadPoolExecutor(max_workers=over) as ex:
        results = list(ex.map(hold, range(over)))

    served = sum(1 for _, rc, ok in results if rc == 0 and ok)
    refused = over - served
    print(f"  served {served}, refused/failed {refused} (of {over})")

    failures = []
    # The cap must actually bind: serving *everything* means it is not enforced.
    if served > max_sessions:
        failures.append(f"served {served} > max_sessions {max_sessions} — cap not enforced")
    # And it must not over-refuse: the server should get close to its own limit.
    if served < max_sessions // 2:
        failures.append(f"served only {served}, expected near {max_sessions}")

    # The part that actually matters: is the server still alive and serving?
    time.sleep(3)
    rc, out, err = ssh("echo AFTER-FLOOD", port, timeout=60)
    if rc != 0 or "AFTER-FLOOD" not in out:
        failures.append(f"server not serving after the flood: rc={rc} {err.strip()[:160]}")
    else:
        print("  server still serving after the flood")

    for f in failures:
        print(f"  FAIL: {f}")
    ok = not failures
    print(f"  C. cap: {'PASS' if ok else 'FAIL'}")
    return ok


# ---------------------------------------------------------------------------
# D. Killing one session does not disturb the others
# ---------------------------------------------------------------------------

def test_fault_isolation(port, peers=4, ticks=20):
    hr(f"D. fault isolation — kill one session, {peers} peers must survive")

    def peer(i):
        cmd = f"i=0; while [ $i -lt {ticks} ]; do echo P{i}-$i; i=$((i+1)); sleep 1; done"
        rc, out, err = ssh(cmd, port, timeout=ticks + 60)
        got = len(re.findall(rf"^P{i}-\d+$", out, re.M))
        return i, rc, got, err

    with concurrent.futures.ThreadPoolExecutor(max_workers=peers + 2) as ex:
        futs = [ex.submit(peer, i) for i in range(peers)]
        time.sleep(4)  # let everyone get established

        # Find a session process to kill. Every forked child is also named
        # /bin/sshd, so the pids must be picked apart by position:
        #   - lowest  = the listener (forked first, at boot). Killing it would
        #               end the server, which is not what this tests.
        #   - highest = *this* connection, the one running `ps` — killing it
        #               just kills the shell asking the question (that is why an
        #               earlier version of this test reported `kill` rc=1: the
        #               session was already gone by the time kill ran).
        # So take the second-lowest: an established peer, forked before us.
        rc, out, _ = ssh("ps | grep sshd | grep -v grep", port, timeout=45)
        pids = sorted(int(m) for m in re.findall(r"^\s*(\d+)", out, re.M))
        print(f"  sshd pids seen: {pids}")

        killed = None
        if len(pids) >= 3:
            killed = pids[1]
            rc, out, err = ssh(f"kill -9 {killed}", port, timeout=45)
            print(f"  killed session pid {killed} (rc={rc})")
            if rc != 0:
                print(f"  WARN: kill reported rc={rc}: {err.strip()[:120]}")
        else:
            print("  WARN: could not identify a session pid to kill")

        peer_results = [f.result() for f in futs]

    failures = []
    if killed is None:
        failures.append("no session pid to kill — test inconclusive")

    survived = [r for r in peer_results if r[1] == 0 and r[2] == ticks]
    died = [r for r in peer_results if r not in survived]
    print(f"  peers completing all {ticks} ticks: {len(survived)}/{peers}")
    print(f"  peers cut short: {[r[0] for r in died]}")

    # Exactly one peer was killed, so exactly one is expected to be cut short.
    # More than one means the kill took out bystanders — the failure mode the
    # single-process design had by construction.
    if len(died) == 0:
        failures.append("nobody died — the kill did not land, test inconclusive")
    elif len(died) > 1:
        failures.append(
            f"{len(died)} peers ended early after killing one — blast radius "
            f"beyond the target"
        )

    # The server itself must still be up.
    rc, out, _ = ssh("echo AFTER-KILL", port, timeout=60)
    if rc != 0 or "AFTER-KILL" not in out:
        failures.append("server not serving after a session was killed")
    else:
        print("  server still serving after the kill")

    for f in failures:
        print(f"  FAIL: {f}")
    ok = not failures
    print(f"  D. fault isolation: {'PASS' if ok else 'FAIL'}")
    return ok


# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--max-sessions", type=int, default=24)
    ap.add_argument("--only", default="A,B,C,D",
                    help="comma-separated subset of tests to run")
    args = ap.parse_args()

    selected = {s.strip().upper() for s in args.only.split(",") if s.strip()}

    # Fail fast with a clear message rather than N confusing timeouts.
    rc, out, err = ssh("echo PROBE", args.port, timeout=45)
    if rc != 0 or "PROBE" not in out:
        print(f"Cannot reach sshd on port {args.port}: rc={rc} {err.strip()[:200]}")
        return 2

    results = {}
    if "A" in selected:
        results["A"] = test_isolation(args.port)
    if "B" in selected:
        results["B"] = test_starvation(args.port)
    if "C" in selected:
        results["C"] = test_cap(args.port, args.max_sessions)
    if "D" in selected:
        results["D"] = test_fault_isolation(args.port)

    hr("SUMMARY")
    for k in sorted(results):
        print(f"  {k}: {'PASS' if results[k] else 'FAIL'}")
    ok = all(results.values())
    print(f"\n{'ALL PASS' if ok else 'FAILURES PRESENT'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
