#!/usr/bin/env python3
"""Boot Akuma at each core count, sweep Redis concurrency, capture `[NICSTAT]`.

Answers "why is Akuma's Redis ~4x slower than Docker's" by separating the two
things a throughput number confuses:

- **per-round-trip latency** — what one client sees with nothing else running.
- **concurrency scaling** — what happens when 32 clients ask at once.

A path that is latency-bound scales with clients until it saturates something.
A path that services round trips *in series* is flat: 32 clients get what 1
client got, each waiting 32x longer. `BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §4
found flatness on the in-guest arm and never swept the forwarded arm; that is
the gap this closes, at three core counts, against a Docker control.

Each arm is a full boot so the core count is real (`SMP=N` is a QEMU flag, not
a runtime knob) and so no arm inherits the previous one's socket-table state
(ATTEMPT_0 §7: Akuma's socket budget depends on how recently the last run
ended). Arms run **one at a time** — two arms at once measure each other.

Usage:
    scripts/benchmarks/redis_smp_sweep.py --smp 1,2,4 --out logs/redis_why
    scripts/benchmarks/redis_smp_sweep.py --smp 4 --keep-up   # leave it booted
"""

import argparse
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SSH_PORT = 2222
REDIS_PORT = 4444
FEATURES = "devbox-smoltcp,no-tests,net-profile"
# The upstream redis:alpine rootfs unpacked by ATTEMPT_0 §2's workaround. Using
# the box (not the guest's own apk redis) keeps the binary identical to the one
# in the Docker control container.
REDIS_BOX = "redisbench"
# `box open` takes the root on EVERY open — the name alone does not remember it.
# Opening without it registers the box at `/`, where the binary does not exist,
# and the only symptom is `box open: failed to spawn`.
REDIS_ROOT = "/root/redisimg"


def sh(cmd, **kw):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, **kw)


def gssh(cmd, port=SSH_PORT, timeout=60):
    """Run a command in the guest. The `ssh` CLI is blocked, Python is not."""
    return subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
         "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10",
         "-p", str(port), "root@localhost", cmd],
        capture_output=True, text=True, timeout=timeout,
    )


def kill_existing_qemu():
    out = sh("pgrep -f qemu-system-aarch64").stdout.split()
    for pid in out:
        print(f"  stopping existing qemu pid={pid}")
        os.kill(int(pid), signal.SIGTERM)
    if out:
        time.sleep(3)
        for pid in sh("pgrep -f qemu-system-aarch64").stdout.split():
            os.kill(int(pid), signal.SIGKILL)
        time.sleep(1)


def boot(smp, logpath, memory=4096, tree=None, features=FEATURES):
    """Boot devbox-smoltcp at SMP=smp, detached, logging to `logpath`.

    Deliberately does NOT use overlays/devbox/run-smoltcp.sh: that script runs
    its own `cargo run --features ...`, which silently rebuilds over whatever
    feature set was just built (here: net-profile). Invoke cargo directly with
    the features this harness needs.

    `tree` builds and runs from another checkout (e.g. a `main` worktree) for a
    branch A/B. It is a real `cwd`, never `--manifest-path`: cargo resolves
    `.cargo/config.toml` and `rust-toolchain.toml` relative to the working
    directory, so `--manifest-path` into another tree silently builds for the
    HOST target and produces a stub kernel that boots to nothing.

    The disk stays the primary tree's (absolute path) so both arms run the same
    rootfs, the same redis binary and the same box registry.
    """
    root = Path(tree) if tree else REPO
    env = dict(os.environ, DISK=str(REPO / "devbox.img"),
               MEMORY=str(memory), SMP=str(smp))
    log = open(logpath, "wb")
    return subprocess.Popen(
        ["cargo", "run", "--release", "--features", features],
        cwd=root, stdout=log, stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL, env=env, start_new_session=True,
    )


def wait_for_sshd(logpath, timeout=240):
    """Wait for the guest to answer a real ssh round trip.

    Deliberately does NOT gate on the console marker. Both markers
    (`sshd started` / `Started sshd`, the two startup paths in CLAUDE.md
    § VM Access) arrive torn by console interleaving often enough to cost an
    arm — `AKUMA_NET_ISSUES.md` §12's harness note records the same failure,
    and it cost the `main` arm here: sshd was up and serving, the log said
    only `Started`, and the arm was skipped as dead.

    A successful ssh command is the thing we actually need, so test for that
    directly. The log is still watched, but only to fail fast on a kernel that
    panicked instead of waiting out the whole timeout.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if gssh("true", timeout=10).returncode == 0:
            return True
        try:
            txt = Path(logpath).read_text(errors="replace")
            if "KERNEL PANIC" in txt or "Kernel panic" in txt:
                print("  guest panicked during boot")
                return False
        except FileNotFoundError:
            pass
        time.sleep(3)
    return False


def start_redis(server_args="--save ''"):
    """(Re)open the redis box. Idempotent across boots — the rootfs is on disk."""
    gssh(f"box close {REDIS_BOX} 2>/dev/null; true")
    time.sleep(1)
    r = gssh(
        f"box open {REDIS_BOX} --root {REDIS_ROOT} -d /usr/local/bin/redis-server "
        f"--port {REDIS_PORT} --protected-mode no {server_args}"
    )
    for _ in range(30):
        time.sleep(1)
        p = sh(f"redis-cli -p {REDIS_PORT} -t 3 ping")
        if "PONG" in p.stdout:
            return True
    print(f"  redis did not come up: {r.stdout}{r.stderr}")
    return False


def parse_nicstat(logpath, since_offset):
    """Sum the `[NICSTAT]` windows written after `since_offset`.

    Deltas per 5 s window (src/nic_profile.rs). Summing them over the benchmark
    gives a time budget attributable to the workload rather than to boot noise.
    """
    txt = Path(logpath).read_text(errors="replace")[since_offset:]
    tot = {}
    n = 0
    for line in txt.splitlines():
        if "[NICSTAT]" not in line:
            continue
        n += 1
        for k, v in re.findall(r"(\w+)=([0-9]+(?:\.[0-9]+)?)", line):
            tot[k] = tot.get(k, 0.0) + float(v)
        # tx_wait=61.2ms(5.0us/pkt max=137us) — the headline, in its own shape
        m = re.search(r"tx_wait=([0-9.]+)ms\(([0-9.]+)us/pkt", line)
        if m:
            tot["tx_wait_ms"] = tot.get("tx_wait_ms", 0.0) + float(m.group(1))
        m = re.search(r"poll=([0-9]+)c/([0-9]+)prog ([0-9.]+)ms\(([0-9.]+)us/c", line)
        if m:
            tot["poll_calls"] = tot.get("poll_calls", 0.0) + float(m.group(1))
            tot["poll_ms"] = tot.get("poll_ms", 0.0) + float(m.group(3))
        m = re.search(r"rx=([0-9]+)p", line)
        if m:
            tot["rx_p"] = tot.get("rx_p", 0.0) + float(m.group(1))
        m = re.search(r"tx=([0-9]+)p", line)
        if m:
            tot["tx_p"] = tot.get("tx_p", 0.0) + float(m.group(1))
    tot["_windows"] = n
    return tot


def sweep(label, port, clients, requests, repeats, outdir, cooldown, test="ping"):
    out = Path(outdir) / f"sweep_{label}.json"
    cmd = [
        sys.executable, str(REPO / "scripts/benchmarks/redis_conc_sweep.py"),
        "--port", str(port), "--label", label, "--test", test,
        "--clients", clients, "--requests", str(requests),
        "--repeats", str(repeats), "--cooldown", str(cooldown),
        "--out", str(out),
    ]
    p = subprocess.run(cmd, capture_output=True, text=True)
    print(p.stdout)
    if p.returncode:
        print(p.stderr)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--smp", default="1,2,4")
    ap.add_argument("--clients", default="1,2,4,8,16,32")
    ap.add_argument("--requests", type=int, default=10000)
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--cooldown", type=float, default=8.0)
    ap.add_argument("--out", default="logs/redis_why")
    ap.add_argument("--keep-up", action="store_true",
                    help="leave the last arm booted instead of shutting down")
    ap.add_argument("--tree", default=None,
                    help="build+run from another checkout (branch A/B). "
                         "Built with cwd=tree, never --manifest-path.")
    ap.add_argument("--features", default=FEATURES)
    ap.add_argument("--tag", default="",
                    help="suffix for arm labels, so an A/B does not overwrite")
    ap.add_argument("--test", default="ping")
    ap.add_argument("--redis-args", default="--save ''",
                    help="args passed to redis-server, e.g. \"--save '900 1'\"")
    args = ap.parse_args()

    outdir = REPO / args.out
    outdir.mkdir(parents=True, exist_ok=True)

    for smp in [int(s) for s in args.smp.split(",")]:
        label = f"akuma-smp{smp}{args.tag}"
        print(f"\n{'='*66}\n== {label}"
              f"{'  tree=' + args.tree if args.tree else ''}"
              f"  features={args.features}  redis={args.redis_args}\n{'='*66}")
        kill_existing_qemu()
        logpath = outdir / f"boot_smp{smp}{args.tag}.log"
        boot(smp, logpath, tree=args.tree, features=args.features)
        if not wait_for_sshd(logpath):
            print(f"  {label}: sshd never came up, skipping")
            continue
        n = gssh("nproc").stdout.strip()
        print(f"  booted, guest nproc={n} (expected {smp})")
        if n != str(smp):
            print(f"  !! core count mismatch, arm is not what it claims")
        if not start_redis(args.redis_args):
            continue

        # Mark where the log is now: NICSTAT windows after this point belong to
        # the benchmark, not to boot.
        mark = logpath.stat().st_size
        sweep(label, REDIS_PORT, args.clients, args.requests,
              args.repeats, outdir, args.cooldown, args.test)
        st = parse_nicstat(logpath, mark)
        print(f"  [NICSTAT] {st.get('_windows', 0)} windows: "
              f"rx={st.get('rx_p', 0):,.0f}p tx={st.get('tx_p', 0):,.0f}p "
              f"tx_wait={st.get('tx_wait_ms', 0):,.0f}ms "
              f"poll={st.get('poll_calls', 0):,.0f}c/{st.get('poll_ms', 0):,.0f}ms")

    if not args.keep_up:
        kill_existing_qemu()
    print("\ndone")


if __name__ == "__main__":
    main()
