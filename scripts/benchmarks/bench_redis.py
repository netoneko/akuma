#!/usr/bin/env python3
"""Redis throughput benchmark — one harness for every arm of the Akuma/Docker comparison.

Baseline numbers and the full fairness analysis:
`docs/archive/REDIS_PERFORMANCE.md`.

# Why there are two arms and you must collect both

    Arm A (forwarded)  host redis-benchmark -> localhost:PORT -> VM port-forward -> redis
    Arm B (local)      redis-benchmark running INSIDE the guest/container -> 127.0.0.1

Measured 2026-08-19 on Docker's own stack, same Redis in both: **crossing the
host port-forward costs ~4x** (3.6x-5.6x at P=1). The forwarded arm therefore
measures the forwarder far more than it measures Redis or the kernel under it.

Comparing Akuma's Arm A against Docker's Arm B — or quoting only a forwarded
number — charges QEMU's SLIRP user-mode NAT to the Akuma kernel. Always compare
arm to arm.

The two cells where the *server* is the bottleneck rather than the forward are
`LPUSH` and `MSET` at `P=16` (only 1.22x between arms). Weight those most.

# Usage

    # Arm A — host client through a forwarded port
    bench_redis.py --label docker-fwd --port 6379 --out docker_fwd.json
    bench_redis.py --label akuma-fwd  --port 4444 --out akuma_fwd.json

    # Arm B — client inside the guest / container
    bench_redis.py --label akuma-local  --via box:2222:redisbox --out akuma_local.json
    bench_redis.py --label docker-local --via docker:NAME       --out docker_local.json

    # Compare two result files
    bench_redis.py --compare base.json mine.json

`--csv` output from redis-benchmark is parsed rather than the human table, which
reflows between Redis versions. Every cell is the median of `--repeats` runs
because these are noisy: measured spread reached 34.5% on LPUSH P=16.

# Akuma's socket budget forces --per-test

Each listener pre-allocates `MAX_BACKLOG` smoltcp sockets and closed sockets sit
in a deferred `pending_removal` queue, so the pool does not come back
immediately. A single redis-benchmark invocation covering all nine tests runs
them back to back and exhausts it: the run reports

    Could not connect to Redis at 127.0.0.1:4444: Can't create socket:
    No file descriptors available

and — crucially — **still exits 0**, so the missing tests look like they were
never asked for rather than like a failure. `--per-test` runs one invocation per
test with `--cooldown` seconds between them, and `bench()` treats that message
as a failed cell instead of trusting the exit status.

Whatever you pick, pick it for *both* arms: a Docker arm at `-c 50` all-tests
against an Akuma arm at `-c 16` per-test is not a comparison.
"""
import argparse, csv, io, json, platform, subprocess, sys, time

TESTS = "ping,set,get,incr,lpush,rpop,sadd,spop,mset"

# redis-benchmark exits 0 after printing this, so rc is not enough to detect it.
SOCKET_EXHAUSTED = "No file descriptors available"


def wrap(via, argv):
    """Build the real command line for a given transport.

    `ssh` is driven as a subprocess, never as an interactive shell — the repo's
    standard workaround for the ssh CLI being blocked by policy, and it also
    keeps stdout separate from any banner on stderr.
    """
    if via == "host":
        return argv
    kind, _, target = via.partition(":")
    if kind == "ssh":
        return ["ssh", "-q", "-o", "StrictHostKeyChecking=no",
                "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=10",
                "-p", target, "root@localhost", " ".join(argv)]
    if kind == "docker":
        return ["docker", "exec", target] + argv
    if kind == "box":
        # box:SSHPORT:BOXNAME — client runs *inside the box*, the Akuma analogue
        # of `docker exec`. `-i` is what makes `box use` relay stdout back.
        port, _, name = target.partition(":")
        return wrap(f"ssh:{port}", ["box", "use", name, "-i"] + argv)
    sys.exit(f"unknown --via {via!r} (want host, ssh:PORT, docker:NAME, or box:PORT:NAME)")


def run(via, argv, timeout=900):
    try:
        r = subprocess.run(wrap(via, argv), capture_output=True, timeout=timeout)
        return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")
    except subprocess.TimeoutExpired:
        return 255, "", "<timeout>"


def bench(via, host, port, requests, clients, size, pipeline, tests=TESTS,
          bench_bin="redis-benchmark"):
    rc, out, err = run(via, [bench_bin, "-h", host, "-p", str(port),
                             "-n", str(requests), "-c", str(clients), "-d", str(size),
                             "-P", str(pipeline), "-t", tests, "--csv"])
    if rc != 0:
        print(f"    FAILED rc={rc}: {err.strip()[:300]}", file=sys.stderr)
        return {}
    if SOCKET_EXHAUSTED in out:
        # Exit status is still 0 here. Without this check the affected tests
        # silently vanish from the result set and the run looks clean.
        print(f"    SOCKET BUDGET EXHAUSTED during '{tests}' P={pipeline}"
              f" — raise --cooldown or lower --clients", file=sys.stderr)
    res = {}
    for row in csv.reader(io.StringIO(out)):
        if len(row) >= 2 and row[0] != "test":
            try:
                res[row[0]] = float(row[1])
            except ValueError:
                pass
    return res


def compare(base_path, mine_path):
    base, mine = json.load(open(base_path)), json.load(open(mine_path))
    bi = {(r["pipeline"], r["test"]): r for r in base["rows"]}
    mi = {(r["pipeline"], r["test"]): r for r in mine["rows"]}
    print(f"base = {base['meta']['label']}   mine = {mine['meta']['label']}\n")
    print(f"{'test':16s} {'P':>3s} {'base ops/s':>13s} {'mine ops/s':>13s} {'ratio':>7s}  verdict")
    for k in sorted(set(bi) & set(mi), key=lambda x: (x[0], x[1])):
        b, m = bi[k], mi[k]
        ratio = m["median_ops"] / b["median_ops"] if b["median_ops"] else 0
        # Noise gate: a difference inside the larger of the two measured spreads
        # is not a result. Spread is (max-min)/median over the repeats.
        noise = max(b["spread_pct"], m["spread_pct"])
        delta = abs(ratio - 1) * 100
        verdict = "noise" if delta <= noise else ("faster" if ratio > 1 else "SLOWER")
        print(f"{k[1]:16s} {k[0]:3d} {b['median_ops']:13,.0f} {m['median_ops']:13,.0f} "
              f"{ratio:6.2f}x  {verdict} (noise floor {noise:.0f}%)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--compare", nargs=2, metavar=("BASE", "MINE"),
                    help="compare two result files instead of benchmarking")
    ap.add_argument("--label")
    ap.add_argument("--via", default="host",
                    help="host (default) | ssh:PORT | docker:NAME — where the CLIENT runs")
    ap.add_argument("--host", default="127.0.0.1", help="redis host AS SEEN BY THE CLIENT")
    ap.add_argument("--port", type=int, default=6379)
    ap.add_argument("--requests", type=int, default=100000)
    ap.add_argument("--clients", type=int, default=50)
    ap.add_argument("--size", type=int, default=64)
    ap.add_argument("--pipelines", default="1,16")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--tests", default=TESTS)
    ap.add_argument("--per-test", action="store_true",
                    help="one redis-benchmark invocation per test — required on Akuma, "
                         "whose socket pool cannot survive nine tests back to back")
    ap.add_argument("--cooldown", type=float, default=0.0,
                    help="seconds to wait between invocations, letting Akuma's deferred "
                         "pending_removal socket queue drain")
    ap.add_argument("--bench-bin", default="redis-benchmark",
                    help="path to redis-benchmark AS SEEN BY THE CLIENT")
    ap.add_argument("--cli-bin", default="redis-cli")
    ap.add_argument("--out")
    args = ap.parse_args()

    if args.compare:
        return compare(*args.compare)
    if not args.label or not args.out:
        sys.exit("--label and --out are required when benchmarking")

    rc, out, err = run(args.via, [args.cli_bin, "-h", args.host, "-p", str(args.port),
                                  "INFO", "server"], timeout=60)
    if rc != 0:
        sys.exit(f"cannot reach redis at {args.host}:{args.port} via {args.via}: {err.strip()[:200]}")
    info = {}
    for line in out.splitlines():
        if ":" in line and not line.startswith("#"):
            k, v = line.split(":", 1)
            info[k.strip()] = v.strip()

    meta = {"label": args.label, "via": args.via, "target": f"{args.host}:{args.port}",
            "redis_version": info.get("redis_version"), "os": info.get("os"),
            "multiplexing_api": info.get("multiplexing_api"),
            "client_platform": f"{platform.system()} {platform.machine()}",
            "requests": args.requests, "clients": args.clients, "size": args.size,
            "repeats": args.repeats, "tests": args.tests, "per_test": args.per_test,
            "cooldown": args.cooldown,
            "when": time.strftime("%Y-%m-%d %H:%M:%S")}
    print(json.dumps(meta, indent=2))

    groups = args.tests.split(",") if args.per_test else [args.tests]
    acc = {}
    for p in [int(x) for x in args.pipelines.split(",")]:
        for r in range(1, args.repeats + 1):
            print(f"  pipeline={p} repeat {r}/{args.repeats} ...", flush=True)
            for g in groups:
                for t, v in bench(args.via, args.host, args.port, args.requests,
                                  args.clients, args.size, p, g, args.bench_bin).items():
                    acc.setdefault((p, t), []).append(v)
                if args.cooldown:
                    time.sleep(args.cooldown)
    if not acc:
        sys.exit("no results — every benchmark invocation failed")

    rows = []
    for (p, t), vals in sorted(acc.items()):
        vals.sort()
        med = vals[len(vals) // 2]
        rows.append({"pipeline": p, "test": t, "median_ops": med,
                     "min_ops": vals[0], "max_ops": vals[-1], "samples": len(vals),
                     "spread_pct": round((vals[-1] - vals[0]) / med * 100, 1) if med else 0})

    json.dump({"meta": meta, "rows": rows}, open(args.out, "w"), indent=2)

    print(f"\n=== {args.label} ({args.via}) — median of {args.repeats}, "
          f"{args.requests} req, {args.clients} clients, {args.size}B ===")
    print(f"{'test':16s} {'P=1 ops/s':>14s} {'P=16 ops/s':>14s}   spread")
    for t in sorted({r["test"] for r in rows}):
        p1 = next((r for r in rows if r["test"] == t and r["pipeline"] == 1), None)
        p16 = next((r for r in rows if r["test"] == t and r["pipeline"] == 16), None)
        sp = max([r["spread_pct"] for r in (p1, p16) if r] or [0])
        flag = "  <-- noisy" if sp > 20 else ""
        print(f"{t:16s} {p1['median_ops'] if p1 else 0:14,.0f} "
              f"{p16['median_ops'] if p16 else 0:14,.0f}   {sp:4.1f}%{flag}")
    print(f"\nwrote {args.out}")


if __name__ == "__main__":
    main()
