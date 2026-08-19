#!/usr/bin/env python3
"""Drive one arm of a networking A/B: boot the devbox, start httpd, benchmark it.

Companion to `bench_nic_rtt.py`, which measures a single run. This wraps the whole
arm so both sides of an A/B are produced by the same procedure in the same session
— the discipline `docs/archive/AKUMA_NET_ISSUES.md` §11.7 exists to enforce:

  * the SAME script boots both arms, so a difference cannot come from how they were
    launched;
  * `-n 2000` and 5 runs by default, because at `-n 400` p90 ranged 1,143-5,048 us
    across runs of one build — enough noise to invent or hide a 2x change;
  * every run gets its own `[NICSTAT]` window slice, so poll/lap counts can be
    normalised by packet count rather than compared raw.

Usage (one arm):

    scripts/benchmarks/run_nic_ab.py --label baseline --runs 5

It assumes the kernel is ALREADY BUILT with `--features devbox-smoltcp,no-tests,
net-profile`; it does not build, so the caller controls which binary is under test.
Results land in `logs/ab/<label>/` as one JSON per run plus a median summary.

SSH is blocked as a CLI in this environment, so the guest is driven through
`subprocess` with an explicit argv — never a shell string.
"""

import argparse
import json
import os
import re
import signal
import statistics
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(os.path.dirname(__file__))))
REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))

SSH_BASE = [
    "ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10", "-p", "2222", "root@localhost",
]

# Both startup paths: extreme-size has the kernel spawn sshd, every other profile
# lets herd do it. Match both so the poll does not silently hang if the profile
# changes — and match `Started` alone as well, because the console is shared and
# another thread's line lands MID-MARKER routinely:
#     [herd] Started [syscall] socket(type=TCP) = fd 3
# which cost one full arm before it was noticed. The log marker is only a hint
# here anyway; `wait_ready` confirms with a real ssh round trip.
READY_RE = re.compile(rb"sshd started|Started sshd|\[herd\] Started")


def ssh(cmd: str, timeout: float = 60.0) -> subprocess.CompletedProcess:
    return subprocess.run(SSH_BASE + [cmd], capture_output=True, timeout=timeout)


def boot(log_path: str, memory: int, smp: int, disk: str, features: str):
    """Launch QEMU via the cargo runner. Returns the Popen; caller must kill it."""
    env = dict(os.environ, DISK=disk, MEMORY=str(memory), SMP=str(smp))
    log = open(log_path, "wb")
    p = subprocess.Popen(
        ["cargo", "run", "--release", "--features", features],
        cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL, start_new_session=True,
    )
    return p, log


def wait_ready(log_path: str, proc, timeout: float = 420.0) -> bool:
    """Wait until the guest actually answers ssh.

    The serial-log marker is checked first because it is cheap, but it is NOT the
    acceptance test: console interleaving can split it (see READY_RE), and a marker
    can print before the listener is really usable. The acceptance test is a real
    ssh command returning 0. NEVER wait on the QEMU process itself — it runs forever.
    """
    deadline = time.time() + timeout
    saw_marker = False
    while time.time() < deadline:
        if proc.poll() is not None:
            print(f"  ! QEMU exited early with {proc.returncode}", file=sys.stderr)
            return False
        if not saw_marker:
            try:
                with open(log_path, "rb") as fh:
                    saw_marker = bool(READY_RE.search(fh.read()))
            except FileNotFoundError:
                pass
        try:
            if ssh("echo ready", timeout=15).returncode == 0:
                return True
        except subprocess.TimeoutExpired:
            pass
        time.sleep(3)
    return False


def log_len(path: str) -> int:
    try:
        return os.path.getsize(path)
    except OSError:
        return 0


def slice_log(path: str, start: int, end: int, out: str):
    """Write the bytes this run produced to its own file, so --nicstat sees only
    the windows that overlap the run instead of the whole boot."""
    with open(path, "rb") as fh:
        fh.seek(start)
        data = fh.read(max(0, end - start))
    with open(out, "wb") as fh:
        fh.write(data)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--label", required=True, help="arm name; results go to logs/ab/<label>/")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--count", type=int, default=2000, help="requests per run (§11.7: not 400)")
    ap.add_argument("--mode", default="http", choices=("connect", "echo", "http"))
    ap.add_argument("--memory", type=int, default=4096)
    ap.add_argument("--smp", type=int, default=4)
    ap.add_argument("--disk", default="devbox.img")
    ap.add_argument("--features", default="devbox-smoltcp,no-tests,net-profile")
    ap.add_argument("--settle", type=float, default=25.0,
                    help="seconds to idle between runs. NOT cosmetic: smoltcp holds "
                         "TimeWait for CLOSE_DELAY=10 s, so back-to-back 2000-request "
                         "runs walk the socket table into the MAX_SOCKETS cliff "
                         "(AKUMA_NET_ISSUES.md §11.2) and later runs of an arm score "
                         "half the earlier ones. Settling makes run N comparable to "
                         "run N of the other arm instead of to its own position.")
    ap.add_argument("--keep-running", action="store_true",
                    help="leave the VM up after the runs (for manual poking)")
    args = ap.parse_args()

    outdir = os.path.join(REPO, "logs", "ab", args.label)
    os.makedirs(outdir, exist_ok=True)
    boot_log = os.path.join(outdir, "boot.log")

    print(f"[ab:{args.label}] booting SMP={args.smp} MEMORY={args.memory} disk={args.disk}")
    proc, log_fh = boot(boot_log, args.memory, args.smp, args.disk, args.features)
    try:
        if not wait_ready(boot_log, proc):
            print(f"[ab:{args.label}] VM never reached sshd — see {boot_log}", file=sys.stderr)
            return 1
        print(f"[ab:{args.label}] sshd up")
        time.sleep(3)

        # Start httpd fresh. `pkill` first so a stale one from an earlier arm can
        # never be the thing under test.
        ssh("pkill httpd; true")
        time.sleep(1)
        r = ssh("cd / && HTTPD_QUIET=1 nohup /bin/httpd 8080 >/tmp/httpd.log 2>&1 & sleep 1; echo started")
        print(f"[ab:{args.label}] httpd: {r.stdout.decode(errors='replace').strip()}")
        time.sleep(2)

        results = []
        for i in range(args.runs):
            mark = log_len(boot_log)
            run_json = os.path.join(outdir, f"run{i}.json")
            cmd = [
                sys.executable, os.path.join(REPO, "scripts", "benchmarks", "bench_nic_rtt.py"),
                "--mode", args.mode, "--target", "localhost:8080",
                "-n", str(args.count), "--label", f"{args.label}-{i}",
                "--out", run_json,
            ]
            print(f"[ab:{args.label}] run {i + 1}/{args.runs}")
            rc = subprocess.run(cmd, cwd=REPO)
            if rc.returncode != 0:
                print(f"  ! run {i} failed rc={rc.returncode}", file=sys.stderr)
                continue
            # Slice the NICSTAT windows this run produced and re-parse them in.
            piece = os.path.join(outdir, f"run{i}.nicstat.log")
            slice_log(boot_log, mark, log_len(boot_log), piece)
            with open(run_json) as fh:
                res = json.load(fh)
            sys.path.insert(0, os.path.join(REPO, "scripts", "benchmarks"))
            import bench_nic_rtt as bnr
            windows = bnr.parse_nicstat(piece)
            if windows:
                res["nicstat"] = bnr.busiest_window(windows)
                res["nicstat_windows"] = len(windows)
                res["nicstat_all"] = windows
                with open(run_json, "w") as fh:
                    json.dump(res, fh, indent=2)
            results.append(res)
            sk = (res.get("nicstat") or {}).get("sockets")
            print(f"  run {i}: {res['rate_per_s']}/s p50={res['p50_us']:.0f} "
                  f"p90={res['p90_us']:.0f} p99={res['p99_us']:.0f} sockets={sk}")
            if i + 1 < args.runs:
                time.sleep(args.settle)

        if not results:
            return 1

        def med(key):
            vals = [r[key] for r in results if key in r and r[key] is not None]
            return statistics.median(vals) if vals else None

        summary = {
            "label": args.label,
            "runs": len(results),
            "count": args.count,
            "median": {k: med(k) for k in ("rate_per_s", "p50_us", "p90_us", "p99_us", "min_us", "max_us")},
            "per_run": [{k: r.get(k) for k in ("rate_per_s", "p50_us", "p90_us", "p99_us")} for r in results],
        }
        with open(os.path.join(outdir, "summary.json"), "w") as fh:
            json.dump(summary, fh, indent=2)
        print(json.dumps(summary, indent=2))
        return 0
    finally:
        if not args.keep_running:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        log_fh.close()


if __name__ == "__main__":
    sys.exit(main())
