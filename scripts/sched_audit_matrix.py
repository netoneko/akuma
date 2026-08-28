#!/usr/bin/env python3
"""Scheduler A/B measurement driver (2026-08-18 scheduling audit).

Boots a *saved kernel binary* per arm — arm identity is provable by sha, not by
the state of the working tree — runs the `ncaprobe` battery plus a 128 MB
download over ssh, and saves medians to JSON. Round 1 of every probe is
discarded (cold caches / first-touch faults), per the investigation's ground
rules in `docs/archive/SCHEDULING_INVESTIGATION.md`.

Produced the matrix in that doc's "Resolution" section. Kept for re-running the
comparison after any scheduler change (tick length, wake preemption, run-queue
policy).

## Preparing arm binaries

Each arm is a *raw binary* at `/tmp/<name>.bin`, built from a known source
state:

    # base: 10 ms tick, WAKE_DEADLINE_PREEMPT = false
    # fixed: 1 ms tick + wake preemption (as committed 2026-08-18)
    cargo build --release
    rust-objcopy -O binary target/aarch64-unknown-none/release/akuma /tmp/schedaudit-ab.bin
    shasum /tmp/schedaudit-*.bin          # record; arms must differ

    # devbox-smoltcp
    scripts/build_devbox_smoltcp.sh
    rust-objcopy -O binary target/aarch64-unknown-none/release/akuma /tmp/schedaudit-devbox.bin

    # rump devbox at SMP=4 — build_devbox.sh is --no-default-features, so smp-shared
    # has to be added explicitly or the secondaries are never brought up:
    cargo build --release --no-default-features --features \
      "devbox,sound,no-tests,rump-tests,smp-shared,sc-aio,sc-sysv-ipc,sc-framebuffer,\
sc-containers,sc-timerfd,sc-eventfd,sc-pidfd,sc-epoll"
    rust-objcopy -O binary target/aarch64-unknown-none/release/akuma /tmp/schedaudit-rump.bin

To build a baseline arm from a tree that already has the fix, stash just the two
files (`git stash push -- crates/akuma-exec/src/threading/mod.rs src/config.rs`),
build, objcopy, then `git stash pop`.

`ncaprobe` must be built for the guest (`userspace/build.sh --ncaprobe-only`);
this script serves it over HTTP to the VM.

## Usage

    scripts/sched_audit_matrix.py run release-smp1-base release-smp1-fixed
    scripts/sched_audit_matrix.py run --only sleep,pipe,download release-smp4-base
    scripts/sched_audit_matrix.py run --repeat 2 --interleave \\
        --out /tmp/dl-recheck.json release-smp4-base release-smp4-fixed
    scripts/sched_audit_matrix.py report                 # markdown matrix
    scripts/sched_audit_matrix.py arms                   # list known arms

## Traps

- **Run on AC power.** A battery run showed a uniform ~40 % degradation across
  unrelated axes and nearly flipped the decision. The signature of host-side
  throttling is *every* axis moving together; a real scheduler effect moves
  axes in different directions.
- Arms measured an hour apart are not a controlled comparison. Use
  `--interleave` when a delta looks decision-critical.
- The `extreme-size` profile cannot run this probe: the 1.2 MB static binary
  plus its 2 MB heap does not fit a 4-8 MB box (fork failures / kernel-heap
  OOM). Those are memory-floor facts, not scheduler results.
"""
import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vm_ready

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TMP = "/tmp"
SSH_PORT = 2222          # rump arms listen on 2223 (their own SLIRP); set per arm
DEFAULT_RESULTS = "/tmp/schedaudit-results.json"
PROBE_PORT = 8899
BIG_PORT = 8898
BIG_DIR = "/tmp/bigserve"
BIG_MB = 128


def ssh_argv():
    return ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "ConnectTimeout=20",
            "-p", str(SSH_PORT), "root@localhost"]

# name -> kernel binary (in /tmp), smp, memory MB, disk image, tick, preemption
ARMS = {
    "release-smp1-base":   dict(bin="schedaudit-base.bin",   smp=1, mem=2048, disk="disk.img",   tick="10 ms", preempt=False),
    "release-smp1-b":      dict(bin="schedaudit-b.bin",      smp=1, mem=2048, disk="disk.img",   tick="10 ms", preempt=True),
    "release-smp1-fixed":  dict(bin="schedaudit-ab.bin",     smp=1, mem=2048, disk="disk.img",   tick="1 ms",  preempt=True),
    "release-smp4-base":   dict(bin="schedaudit-base.bin",   smp=4, mem=2048, disk="disk.img",   tick="10 ms", preempt=False),
    "release-smp4-fixed":  dict(bin="schedaudit-ab.bin",     smp=4, mem=2048, disk="disk.img",   tick="1 ms",  preempt=True),
    "devbox-smp4-base":    dict(bin="schedaudit-devbox-base.bin", smp=4, mem=4096, disk="devbox.img", tick="10 ms", preempt=False),
    "devbox-smp4-fixed":   dict(bin="schedaudit-devbox.bin", smp=4, mem=4096, disk="devbox.img", tick="1 ms",  preempt=True),
    # rump devbox: NetBSD stack is the only network stack (RUMP_NIC=1, ssh on 2223).
    # NOTE: scripts/build_devbox.sh is --no-default-features, so `smp-shared` must be
    # added to DEVBOX_FEATURES to get a real SMP=4 kernel — see the docstring.
    "rump-smp4-base":      dict(bin="schedaudit-rump-base.bin", smp=4, mem=4096, disk="devbox.img", tick="10 ms", preempt=False, rump=True),
    "rump-smp4-fixed":     dict(bin="schedaudit-rump.bin", smp=4, mem=4096, disk="devbox.img", tick="1 ms", preempt=True, rump=True),
    # extreme-size: functional check only, the probe does not fit (see Traps)
    "extreme-4m":          dict(bin="schedaudit-ext.bin",    smp=1, mem=4,    disk="disk.img",   tick="10 ms", preempt=True),
}
ALL_PROBES = ["sleep", "poll", "pipe", "term", "termnet", "download", "https", "idle"]
# HTTPS probe target. busybox wget cannot do TLS in this image — use the real
# curl from bootstrap/bin (see docs/archive/NATIVE_STACK_USERSPACE_INTERNET.md).
HTTPS_URL = "https://example.com/"
CURL = "/bin/curl"


def sh(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).stdout.strip()


def ssh(cmd, timeout=1200):
    """SSH into the guest. Binary-safe: sshd's banner can carry non-UTF8 bytes."""
    r = subprocess.run(ssh_argv() + [cmd], capture_output=True, timeout=timeout)
    return ((r.stdout or b"") + (r.stderr or b"")).decode("utf-8", "replace")


def log_count(path, needle):
    try:
        with open(path, "rb") as f:
            return f.read().count(needle)
    except FileNotFoundError:
        return 0


def parse(out, pat, cast=float):
    m = re.search(pat, out, re.M)
    return cast(m.group(1)) if m else None


def qemu_cmd(binary, disk, smp, mem, rump=False):
    cmd = ["qemu-system-aarch64", "-semihosting", "-machine", "virt,gic-version=3",
           "-accel", "hvf", "-cpu", "host", "-smp", str(smp), "-m", str(mem),
           "-serial", "mon:stdio", "-display", "none",
           "-netdev", "user,id=net0,hostfwd=tcp::2222-:22,hostfwd=tcp::8080-:8080",
           "-global", "virtio-mmio.force-legacy=false",
           "-device", "virtio-net-device,netdev=net0,bus=virtio-mmio-bus.0",
           "-drive", f"file={disk},if=none,format=raw,id=hd0",
           "-device", "virtio-blk-device,drive=hd0,bus=virtio-mmio-bus.1",
           "-device", "virtio-rng-device,bus=virtio-mmio-bus.2"]
    if rump:
        # NIC1 on virtio-mmio-bus.4 is the rump stack's raw L2 tap (/dev/net/tap0),
        # with its own SLIRP: DHCP + a 10.0.2.2 gateway, and host :2223 -> box :22.
        # Mirrors RUMP_NIC=1 in scripts/cargo_runner.sh.
        cmd += ["-netdev", "user,id=net1,hostfwd=tcp::2223-:22",
                "-device", "virtio-net-device,netdev=net1,bus=virtio-mmio-bus.4"]
    return cmd + ["-kernel", binary]


def start_servers():
    """Serve the probe binary and the download payload to the guest."""
    procs = []
    probe_dir = os.path.join(REPO, "userspace/ncaprobe/target/aarch64-unknown-linux-musl/release")
    if not os.path.exists(os.path.join(probe_dir, "ncaprobe")):
        sys.exit(f"ncaprobe not built: {probe_dir}/ncaprobe (userspace/build.sh --ncaprobe-only)")
    os.makedirs(BIG_DIR, exist_ok=True)
    big = os.path.join(BIG_DIR, "big.bin")
    if not os.path.exists(big) or os.path.getsize(big) != BIG_MB * 1024 * 1024:
        print(f"[setup] creating {BIG_MB} MB {big}", flush=True)
        subprocess.run(["dd", "if=/dev/urandom", f"of={big}", "bs=1048576",
                        f"count={BIG_MB}"], capture_output=True, check=True)
    for port, cwd in ((PROBE_PORT, probe_dir), (BIG_PORT, BIG_DIR)):
        procs.append(subprocess.Popen(
            ["python3", "-m", "http.server", str(port), "--bind", "0.0.0.0"],
            cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    time.sleep(1)
    return procs


def run_arm(name, cfg, probes, tag):
    binary = os.path.join(TMP, cfg["bin"])
    if not os.path.exists(binary):
        print(f"[{tag}] MISSING kernel binary {binary} — see module docstring", flush=True)
        return None
    src = cfg["disk"] if os.path.isabs(cfg["disk"]) else os.path.join(REPO, cfg["disk"])
    disk = f"{TMP}/{tag}.img"
    logp = f"{TMP}/{tag}.log"
    res = dict(arm=name, smp=cfg["smp"], memory=cfg["mem"], tick=cfg["tick"],
               preempt=cfg["preempt"], kernel_sha=sh(["shasum", binary]).split()[0],
               host_load=sh(["uptime"]))
    subprocess.run(["cp", "-c", src, disk], check=True)
    log = open(logp, "wb")
    print(f"[{tag}] booting -smp {cfg['smp']} -m {cfg['mem']} ({cfg['tick']}, "
          f"preempt={cfg['preempt']})", flush=True)
    global SSH_PORT
    SSH_PORT = 2223 if cfg.get("rump") else 2222
    qemu = subprocess.Popen(qemu_cmd(binary, disk, cfg["smp"], cfg["mem"], cfg.get("rump", False)),
                            stdout=log, stderr=subprocess.STDOUT, cwd=REPO)
    try:
        t0 = time.time()
        while time.time() - t0 < 420:
            if qemu.poll() is not None:
                print(f"[{tag}] QEMU exited rc={qemu.returncode}", flush=True)
                return None
            # Readiness = an ssh round-trip, not a log marker: at SMP>1 the
            # marker line arrives torn across cores and some builds never print
            # it at all, so grepping for it times out against healthy VMs
            # (scripts/vm_ready.py has the measurements).
            if vm_ready.ssh_probe(SSH_PORT):
                break
            time.sleep(2)
        else:
            print(f"[{tag}] guest never answered ssh", flush=True)
            return None
        time.sleep(8)  # let herd settle
        res["boot_s"] = round(time.time() - t0)
        print(f"[{tag}] up in {res['boot_s']}s", flush=True)

        needs_probe = any(p in probes for p in ("sleep", "poll", "pipe", "term", "termnet"))
        if needs_probe:
            out = ssh("curl -s -o /tmp/nb http://10.0.2.2:%d/ncaprobe && chmod +x /tmp/nb "
                      "&& echo FETCHED" % PROBE_PORT, timeout=300)
            if "FETCHED" not in out:
                print(f"[{tag}] probe fetch failed:\n{out}", flush=True)
                return None

        def battery(cmd, rounds):
            outs = []
            for i in range(rounds):
                outs.append(ssh(cmd))
                print(f"[{tag}] {cmd} round {i + 1}/{rounds}", flush=True)
            return outs

        def record(key, cmd, pat, rounds=5, cast=float):
            vals = [parse(o, pat, cast) for o in battery(cmd, rounds)]
            res[key + "_all"] = vals
            res[key] = [v for v in vals[1:] if v is not None]  # discard round 1

        if "sleep" in probes:
            record("sleep_1ms", "/tmp/nb sleepbench", r"^\s*1000 ->\s+(\d+)", cast=int)
        if "poll" in probes:
            record("poll_1ms", "/tmp/nb pollbench", r"^\s*1 ms ->\s+(\d+)", cast=int)
        if "pipe" in probes:
            record("pipe_us", "/tmp/nb pipebench", r"RESULT: ([\d.]+) us/iter")
        if "term" in probes:
            tv = [dict(p90=parse(o, r"p90=(\d+)"), p99=parse(o, r"p99=(\d+)"),
                       max=parse(o, r"max=(\d+)"),
                       stalls=parse(o, r"writes over 10ms \(visible stalls\): (\d+)", int),
                       wall_ms=parse(o, r"in (\d+) ms", int))
                  for o in battery("/tmp/nb termbench", 5)]
            res["term_all"] = tv
            res["term"] = tv[1:]
        if "termnet" in probes:
            res["term_net"] = [dict(p90=parse(o, r"p90=(\d+)"),
                                    stalls=parse(o, r"writes over 10ms \(visible stalls\): (\d+)", int),
                                    kib=parse(o, r"concurrent download moved (\d+) KiB", int))
                               for o in battery("/tmp/nb termbench --net", 3)]
        if "download" in probes:
            vals = []
            for i in range(5):
                o = ssh("/bin/busybox time /bin/busybox wget -q -O /dev/null "
                        "http://10.0.2.2:%d/big.bin 2>&1" % BIG_PORT)
                m = re.search(r"real\s+(\d+)m\s+([\d.]+)s", o)
                vals.append(60 * int(m.group(1)) + float(m.group(2)) if m else None)
                print(f"[{tag}] download {i + 1}/5: {vals[-1]}", flush=True)
            res["download_s_all"] = vals
            res["download_s"] = [v for v in vals[1:] if v is not None]
        if "https" in probes:
            # Latency breakdown, not throughput: DNS / TCP connect / TLS+first byte
            # / total. On rump every one of those phases is a proxied syscall chain,
            # so this is the axis the sysproxy tax shows up on most clearly.
            hv = []
            for i in range(5):
                o = ssh(f"{CURL} -sS -o /dev/null -w "
                        f"'%{{time_namelookup}} %{{time_connect}} "
                        f"%{{time_starttransfer}} %{{time_total}}\\n' {HTTPS_URL} 2>&1")
                m = re.search(r"^([\d.]+) ([\d.]+) ([\d.]+) ([\d.]+)\s*$", o, re.M)
                hv.append(dict(dns=float(m.group(1)), connect=float(m.group(2)),
                               first_byte=float(m.group(3)), total=float(m.group(4)))
                          if m else None)
                print(f"[{tag}] https {i + 1}/5: {hv[-1] or o.strip()[:120]}", flush=True)
            res["https_all"] = hv
            res["https"] = [h for h in hv[1:] if h]

        if "idle" in probes:
            c1 = log_count(logp, b"[Heartbeat]")
            time.sleep(35)
            res["idle_heartbeats_35s"] = log_count(logp, b"[Heartbeat]") - c1

        time.sleep(2)
        for needle, key in ((b"PASSED", "suite_passed"), (b"FAILED", "suite_failed"),
                            (b"POOL contended", "pool_skips"), (b"[BKL] stuck", "bkl_stuck"),
                            (b"Time jump", "time_jumps"), (b"[WATCHDOG]", "watchdog"),
                            (b"[OOM]", "oom"), (b"fork failed", "fork_fail")):
            res[key] = log_count(logp, needle)
        res["host_load_end"] = sh(["uptime"])
        return res
    finally:
        qemu.terminate()
        try:
            qemu.wait(timeout=15)
        except subprocess.TimeoutExpired:
            qemu.kill()
        log.close()
        if os.path.exists(disk):
            os.unlink(disk)
        print(f"[{tag}] VM down, disk removed (log {logp})", flush=True)


def med(xs):
    xs = [x for x in (xs or []) if x is not None]
    return statistics.median(xs) if xs else None


def cmd_run(args):
    unknown = [a for a in args.arms if a not in ARMS]
    if unknown:
        sys.exit(f"unknown arm(s): {unknown}; try `arms`")
    probes = ALL_PROBES if args.only == "all" else args.only.split(",")
    bad = [p for p in probes if p not in ALL_PROBES]
    if bad:
        sys.exit(f"unknown probe(s): {bad}; known: {ALL_PROBES}")
    if "AC Power" not in sh(["pmset", "-g", "batt"]):
        print("WARNING: not on AC power — see Traps in the module docstring", flush=True)
    if subprocess.run(["pgrep", "-f", "qemu-system-aarch64"], capture_output=True).returncode == 0:
        sys.exit("a QEMU instance is already running; port 2222 would collide")

    results = json.load(open(args.out)) if os.path.exists(args.out) else {}
    servers = start_servers()
    try:
        order = []
        for rep in range(args.repeat):
            arms = args.arms if not args.interleave else args.arms
            for a in arms:
                order.append((a, rep))
        if args.interleave:
            order.sort(key=lambda x: (x[1], args.arms.index(x[0])))
        for name, rep in order:
            tag = name if args.repeat == 1 else f"{name}-r{rep + 1}"
            r = run_arm(name, ARMS[name], probes, tag)
            if r:
                r["repeat"] = rep + 1
                results[tag] = r
                json.dump(results, open(args.out, "w"), indent=1)
                print(f"[{tag}] saved -> {args.out}", flush=True)
                print(f"  sleep={med(r.get('sleep_1ms'))}us poll={med(r.get('poll_1ms'))}us "
                      f"pipe={med(r.get('pipe_us'))}us dl={med(r.get('download_s'))}s", flush=True)
    finally:
        for p in servers:
            p.terminate()
    cmd_report(argparse.Namespace(out=args.out))


def cmd_report(args):
    if not os.path.exists(args.out):
        sys.exit(f"no results at {args.out}")
    results = json.load(open(args.out))
    hdr = ("| arm | SMP | RAM | tick | wake-preempt | sleep 1 ms | poll 1 ms | pipe µs/iter "
           "| term p90 | stalls >10 ms | term+net p90 | 128 MB dl | https total | suite P/F | BKL stuck |")
    print(hdr)
    print("|" + "---|" * 15)
    for tag, r in results.items():
        t = [x for x in (r.get("term") or []) if x.get("p90") is not None]
        tn = [x for x in (r.get("term_net") or []) if x.get("p90") is not None]

        def ms(v):
            return "—" if v is None else f"{v / 1000:.2f} ms"
        p90 = med([x["p90"] for x in t])
        stalls = med([x["stalls"] for x in t if x.get("stalls") is not None])
        np90 = med([x["p90"] for x in tn])
        dl = med(r.get("download_s"))
        https = med([h["total"] for h in (r.get("https") or [])])
        pipe = med(r.get("pipe_us"))
        suite = (f"{r.get('suite_passed')}/{r.get('suite_failed')}"
                 if r.get("suite_passed") is not None else "—")
        print(f"| {tag} | {r.get('smp', '?')} | {r.get('memory', '?')} | {r.get('tick', '?')} "
              f"| {'ON' if r.get('preempt') else 'off'} | {ms(med(r.get('sleep_1ms')))} "
              f"| {ms(med(r.get('poll_1ms')))} | {'—' if pipe is None else f'{pipe:.2f}'} "
              f"| {'—' if p90 is None else (f'{p90:.0f} µs' if p90 < 1000 else ms(p90))} "
              f"| {'—' if stalls is None else int(stalls)} | {ms(np90)} "
              f"| {'—' if dl is None else f'{dl:.2f} s'} "
              f"| {'—' if https is None else f'{https:.2f} s'} | {suite} "
              f"| {r.get('bkl_stuck', '—')} |")


def cmd_arms(_args):
    for name, cfg in ARMS.items():
        print(f"{name:22} /tmp/{cfg['bin']:28} smp={cfg['smp']} mem={cfg['mem']} "
              f"disk={cfg['disk']} tick={cfg['tick']} preempt={cfg['preempt']}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    run = sub.add_parser("run", help="boot arms and measure")
    run.add_argument("arms", nargs="+")
    run.add_argument("--only", default="all",
                     help=f"comma-separated probe subset ({','.join(ALL_PROBES)})")
    run.add_argument("--repeat", type=int, default=1)
    run.add_argument("--interleave", action="store_true",
                     help="round-robin the arms instead of finishing one at a time")
    run.add_argument("--out", default=DEFAULT_RESULTS)
    run.set_defaults(func=cmd_run)
    rep = sub.add_parser("report", help="print the markdown matrix")
    rep.add_argument("--out", default=DEFAULT_RESULTS)
    rep.set_defaults(func=cmd_report)
    arms = sub.add_parser("arms", help="list known arms")
    arms.set_defaults(func=cmd_arms)
    a = ap.parse_args()
    a.func(a)


if __name__ == "__main__":
    main()
