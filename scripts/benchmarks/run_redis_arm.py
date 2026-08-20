#!/usr/bin/env python3
"""One arm of a Redis A/B on Akuma: boot the devbox, start redis, benchmark it.

Sibling of `run_nic_ab.py` (HTTP round trips); this one drives the **pipelined**
workload, which is the shape `AKUMA_NET_ISSUES.md` §7 says the static rings
(`net-noalloc`) should suit and which was never measured.

Redis comes from apk (`apk add redis`), not `box pull` — that is Issue 18 in
DEVBOX_ISSUES and is still open. The server runs on guest port 4444, which the
cargo runner already forwards, and the client runs on the **host** (arm A,
"forwarded"). Arm A measures the forwarder as much as the kernel, so it is only
meaningful Akuma-vs-Akuma — which is exactly what an A/B of two kernel builds is.

`--per-test` + `--cooldown` are mandatory here and not stylistic: a single
redis-benchmark invocation covering several tests back to back exhausts Akuma's
socket pool, and redis-benchmark **exits 0** after printing "No file descriptors
available", so the missing cells look unrequested rather than failed.
"""

import argparse, json, os, signal, subprocess, sys, time

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SSH = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
       "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=10", "-p", "2222", "root@localhost"]


def ssh(cmd, timeout=120):
    return subprocess.run(SSH + [cmd], capture_output=True, timeout=timeout)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--features", default="devbox-smoltcp,no-tests,net-profile")
    ap.add_argument("--requests", type=int, default=20000)
    ap.add_argument("--clients", type=int, default=20)
    ap.add_argument("--pipelines", default="1,16")
    ap.add_argument("--tests", default="ping,set,get,lpush,mset")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--cooldown", type=float, default=12.0)
    args = ap.parse_args()

    outdir = os.path.join(REPO, "logs", "redis_ab", args.label)
    os.makedirs(outdir, exist_ok=True)
    boot_log = os.path.join(outdir, "boot.log")

    # A busy host invalidates the arm. This bit us 2026-08-20: an orphaned
    # redis-benchmark from a previous session ran 22 h at 100 % CPU aimed at the
    # forwarded guest port, costing ~12 % throughput and 25 % of p90.
    busy = subprocess.run(["ps", "-Ao", "pid,pcpu,comm", "-r"], capture_output=True)
    print("[preflight] top CPU:\n" + "\n".join(busy.stdout.decode().splitlines()[:5]))
    for line in busy.stdout.decode().splitlines()[1:6]:
        if any(g in line for g in ("redis-benchmark", "bench_", "stress")):
            sys.exit(f"ABORT: load generator still running: {line.strip()}")

    env = dict(os.environ, DISK="devbox.img", MEMORY="4096", SMP="4")
    log = open(boot_log, "wb")
    proc = subprocess.Popen(["cargo", "run", "--release", "--features", args.features],
                            cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT,
                            stdin=subprocess.DEVNULL, start_new_session=True)
    try:
        deadline = time.time() + 420
        while time.time() < deadline:
            if proc.poll() is not None:
                sys.exit(f"QEMU exited early rc={proc.returncode}")
            try:
                if ssh("echo ready", timeout=15).returncode == 0:
                    break
            except subprocess.TimeoutExpired:
                pass
            time.sleep(3)
        else:
            sys.exit("guest never answered ssh")
        print(f"[{args.label}] guest up")

        ssh("pkill redis-server; pkill httpd; true")
        time.sleep(2)
        ssh('nohup redis-server --port 4444 --protected-mode no --save "" '
            '>/tmp/redis.log 2>&1 & sleep 3; echo started', timeout=90)
        time.sleep(3)
        r = ssh("redis-cli -p 4444 ping")
        if b"PONG" not in r.stdout:
            sys.exit(f"redis not answering in guest: {r.stdout!r} {r.stderr!r}")
        print(f"[{args.label}] redis up")

        out_json = os.path.join(outdir, "result.json")
        rc = subprocess.run([sys.executable,
                             os.path.join(REPO, "scripts", "benchmarks", "bench_redis.py"),
                             "--label", args.label, "--port", "4444",
                             "--requests", str(args.requests), "--clients", str(args.clients),
                             "--size", "64", "--pipelines", args.pipelines,
                             "--repeats", str(args.repeats), "--tests", args.tests,
                             "--per-test", "--cooldown", str(args.cooldown),
                             "--out", out_json], cwd=REPO)
        print(f"[{args.label}] bench rc={rc.returncode} -> {out_json}")
        return rc.returncode
    finally:
        try:
            os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        log.close()


if __name__ == "__main__":
    sys.exit(main())
