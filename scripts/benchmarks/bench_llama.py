#!/usr/bin/env python3
"""llama.cpp throughput: Akuma vs Docker/Linux, same package, same weights.

Results and analysis: `docs/archive/BENCHMARK_PERFORMANCE_ATTEMPT_0.md`.

# Why this workload

Redis measures the kernel-crossing path and almost nothing else. llama.cpp at
`-t 1` measures the opposite: after the weights are loaded it is NEON arithmetic
in userspace, with the kernel out of the hot loop. The two bracket the same
question from opposite ends, which is why both live here.

`pp512` is prompt processing (prefill) — 512 tokens ingested as one batched
matmul, compute-bound and parallel. `tg128` is token generation (decode) — 128
tokens produced one at a time, each streaming the whole weight set, so it is
memory-bandwidth-bound and sequential. They scale differently; report both.

# The fairness control

Alpine ships `llama.cpp` and the Akuma guest is Alpine, so `apk add llama.cpp`
on both sides gives the same distro package, same version, same arch, built by
the same builders — differing only in which kernel runs it. This script refuses
to proceed if the two versions differ, or if the model's sha256 differs.

# Three traps this harness exists to avoid

1. **A long `ssh host cmd` dies at ~300 s with rc=255** (DEVBOX_ISSUES Issue 19)
   and the remote command is NOT killed with it. So the Akuma arm is always
   launched detached with a sentinel file and polled — never held open.
2. **A dropped channel leaves the previous run alive.** Two `llama-bench`
   processes competing for four cores produced a set of numbers that looked
   plausible and were garbage. `assert_quiet()` runs before every launch.
3. **`ps` on Akuma shows threads as processes** (Issue 21: `Tgid` is the
   thread's own pid, `Threads:` is always 1, `/proc/<pid>/task` is empty), so
   "how many llama-bench are running" cannot be answered by counting lines.
   Group by `PPid` instead — that is what `assert_quiet` does.

# Usage

    bench_llama.py --model bootstrap/models/qwen3.5-0.8b-q4.gguf --push
    bench_llama.py --arm akuma  --out logs/llama_bench/akuma.csv
    bench_llama.py --arm docker --out logs/llama_bench/docker.csv
    bench_llama.py --compare logs/llama_bench/docker.csv logs/llama_bench/akuma.csv

Run the arms one at a time. Two arms at once measure each other.
"""
import argparse, csv, hashlib, io, os, subprocess, sys, time

GUEST_MODEL = "/root/qwen3.5-0.8b-q4.gguf"
SENTINEL = "/root/lb.done"
CSV_OUT = "/root/lb.csv"
ERR_OUT = "/root/lb.err"
CONTAINER = "akuma-llama-bench"


def ssh(cmd, port, timeout=300, stdin=None):
    argv = ["ssh", "-q", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "ConnectTimeout=10", "-p", str(port), "root@localhost", cmd]
    r = subprocess.run(argv, capture_output=True, timeout=timeout, stdin=stdin)
    return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")


def dock(argv, timeout=7200):
    r = subprocess.run(["docker", "exec", CONTAINER] + argv, capture_output=True, timeout=timeout)
    return r.returncode, r.stdout.decode("utf-8", "replace"), r.stderr.decode("utf-8", "replace")


def assert_quiet(port):
    """Refuse to launch while another llama-bench is alive.

    Counting `ps` lines does not work — Issue 21 means every thread looks like a
    process. Group by PPid: a live run contributes one entry whose PPid is a
    shell plus N whose PPid is that entry.
    """
    rc, out, _ = ssh("ps ax 2>/dev/null | grep -a 'llama-bench -m' | grep -av grep | awk '{print $1}'",
                     port)
    pids = [p for p in out.split() if p.isdigit()]
    if not pids:
        return
    sys.exit(f"REFUSING TO RUN: {len(pids)} llama-bench ps entries already present "
             f"(pids {' '.join(pids)}).\nA previous run survived its ssh channel "
             f"(Issue 19). Kill it first:\n  ssh -p {port} root@localhost 'killall llama-bench'")


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def push(model, port):
    """Stream the weights into the guest and verify the hash on both sides.

    A short read on a 532 MB transfer yields a model that still loads and still
    generates text, so the hash check is not optional.
    """
    want = sha256(model)
    print(f"host sha256 {want}")
    t0 = time.time()
    with open(model, "rb") as f:
        rc, out, err = ssh(f"cat > {GUEST_MODEL} && sha256sum {GUEST_MODEL}", port,
                           timeout=7200, stdin=f)
    got = out.split()[0] if out.split() else "?"
    print(f"guest sha256 {got}   ({os.path.getsize(model)/1e6:.0f} MB in {time.time()-t0:.0f}s)")
    if got != want:
        sys.exit("HASH MISMATCH — transfer was short or corrupted; do not benchmark this")
    print("hashes match")

    # Same package on both sides, or the comparison is not one.
    _, a, _ = ssh("apk info -v llama.cpp 2>/dev/null | head -1", port)
    _, d, _ = dock(["apk", "info", "-v", "llama.cpp"])
    a, d = a.strip(), d.strip().splitlines()[0] if d.strip() else ""
    print(f"akuma package  {a}\ndocker package {d}")
    if a != d:
        sys.exit(f"PACKAGE MISMATCH {a!r} != {d!r} — install the same version on both sides")


def bench_args(a):
    args = ["-m", GUEST_MODEL, "-p", str(a.n_prompt), "-n", str(a.n_gen),
            "-t", a.threads, "-mmp", a.mmap, "-r", str(a.repeats), "-o", "csv"]
    if a.llama_poll is not None:
        # ggml's spin-before-park knob: 100 = worker threads never park, 0 = park
        # at every barrier. This is the discriminator for "is the -t N collapse
        # the futex/scheduler wake path, or contention?" — if --llama-poll 100
        # restores scaling, it is the wake path.
        args += ["--poll", str(a.llama_poll)]
    return args


def run_akuma(a):
    """Detached + sentinel, never a held-open channel. See trap 1."""
    assert_quiet(a.port)
    args = " ".join(bench_args(a))
    launch = (f"rm -f {CSV_OUT} {SENTINEL} {ERR_OUT}; "
              f"nohup sh -c 'cd /root && llama-bench {args} > {CSV_OUT} 2>{ERR_OUT}; "
              f"echo DONE > {SENTINEL}' >/dev/null 2>&1 & echo launched")
    rc, out, err = ssh(launch, a.port)
    print(f"launched: {out.strip()}")

    deadline = time.time() + a.timeout
    while time.time() < deadline:
        rc, out, _ = ssh(f"test -f {SENTINEL} && echo DONE; grep -ac qwen35 {CSV_OUT} 2>/dev/null",
                         a.port)
        if "DONE" in out:
            print("finished")
            break
        rows = next((l for l in out.split() if l.isdigit()), "?")
        print(f"  [{time.strftime('%H:%M:%S')}] rows={rows}", flush=True)
        time.sleep(a.poll)
    else:
        print("TIMED OUT — partial results follow; the run is still going in the guest",
              file=sys.stderr)

    rc, out, _ = ssh(f"cat {CSV_OUT}", a.port)
    rc, err, _ = ssh(f"tail -5 {ERR_OUT}", a.port)
    if err.strip():
        print(f"stderr tail:\n{err}", file=sys.stderr)
    return out


def run_docker(a):
    rc, out, err = dock(["llama-bench"] + bench_args(a))
    if rc != 0:
        print(f"docker arm failed rc={rc}: {err[-500:]}", file=sys.stderr)
    return out


def parse(text):
    """llama-bench prints load_backend lines to stdout before the CSV header."""
    lines = [l for l in text.splitlines() if l.startswith("build_commit") or l.startswith('"')]
    rows = {}
    for r in csv.DictReader(io.StringIO("\n".join(lines))):
        test = f"pp{r['n_prompt']}" if int(r["n_prompt"]) > 0 else f"tg{r['n_gen']}"
        rows[(test, r["n_threads"], r["use_mmap"])] = (float(r["avg_ts"]), float(r["stddev_ts"]))
    return rows


def show(label, rows):
    print(f"\n=== {label} ===")
    print(f"{'test':8s}{'-t':>4s}{'mmap':>6s}{'tok/s':>12s}{'stddev':>9s}")
    for (t, th, mm), (ts, sd) in sorted(rows.items()):
        print(f"{t:8s}{th:>4s}{mm:>6s}{ts:12,.2f}{sd:9.2f}")


def compare(base_path, mine_path):
    base, mine = parse(open(base_path).read()), parse(open(mine_path).read())
    print(f"{'test':8s}{'-t':>4s}{'mmap':>6s}{'base':>12s}{'mine':>12s}{'mine %':>10s}")
    for k in sorted(set(base) & set(mine)):
        b, m = base[k][0], mine[k][0]
        print(f"{k[0]:8s}{k[1]:>4s}{k[2]:>6s}{b:12,.2f}{m:12,.2f}{m/b*100:9.1f}%")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", choices=["akuma", "docker"])
    ap.add_argument("--compare", nargs=2, metavar=("BASE", "MINE"))
    ap.add_argument("--push", action="store_true", help="stream the model into the guest first")
    ap.add_argument("--model", default="bootstrap/models/qwen3.5-0.8b-q4.gguf")
    ap.add_argument("--port", type=int, default=2222, help="guest ssh port")
    ap.add_argument("--n-prompt", type=int, default=512)
    ap.add_argument("--n-gen", type=int, default=128)
    ap.add_argument("--threads", default="1,4", help="comma list; llama-bench sweeps it")
    ap.add_argument("--mmap", default="1,0", help="comma list of -mmp values")
    ap.add_argument("--repeats", type=int, default=3)
    ap.add_argument("--llama-poll", type=int, default=None,
                    help="ggml --poll (0-100): spin-before-park. 100 removes the "
                         "futex/scheduler wake path from the barrier inner loop")
    ap.add_argument("--timeout", type=int, default=5400, help="seconds to wait for the Akuma arm")
    ap.add_argument("--poll", type=int, default=30, help="seconds between sentinel checks")
    ap.add_argument("--out")
    a = ap.parse_args()

    if a.compare:
        return compare(*a.compare)
    if a.push:
        push(a.model, a.port)
        if not a.arm:
            return
    if not a.arm or not a.out:
        sys.exit("--arm and --out are required (or --compare, or --push alone)")

    text = run_akuma(a) if a.arm == "akuma" else run_docker(a)
    os.makedirs(os.path.dirname(a.out) or ".", exist_ok=True)
    open(a.out, "w").write(text)
    show(a.arm, parse(text))
    print(f"\nwrote {a.out}")


if __name__ == "__main__":
    main()
