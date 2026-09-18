#!/usr/bin/env python3
"""Build one crate in the box's Firecracker guest across an SMP x jobs matrix.

The question this answers is the one
`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md` §10 left open: `-j4`
wedges at SMP=4 and `-j1` does not, but nothing had separated the two
variables. Four cells do:

    vcpu=1 -j1   the anchor — §10 measured zerocopy here at 13.24 s
    vcpu=1 -j4   several processes, ONE core   -> does concurrency alone wedge?
    vcpu=4 -j1   one process, FOUR cores       -> do cores alone wedge?
    vcpu=4 -j4   both                          -> the known-bad cell

A wedge in 1x4 would mean the bug is not SMP at all; a wedge in 4x1 would mean
it is not job concurrency. §10's prior is that only 4x4 wedges, which is what
makes "multi-threaded user process on several cores" the suspect rather than
either half alone.

Run from the laptop; everything happens on the box (`scripts/utils/hpbox.py`).
Nothing here reboots the box — Firecracker runs on the Ubuntu personality.

    scripts/benchmarks/amd64_fc_build_matrix.py
    scripts/benchmarks/amd64_fc_build_matrix.py --crate zerocopy --cells 1x4,4x4
    scripts/benchmarks/amd64_fc_build_matrix.py --budget 1200 --repeat 3

# Why the build is timed from the host

The guest's `uptime_us` was a 10 ms tick until 2026-09-17 and is TSC-derived
now, but it is still the clock under test. The box runs the guest under KVM, so
host wall time is honest and independent — and the ssh round trip it includes
is ~10 ms against a 13 s build.

# What counts as a wedge, and why silence is not a pass

A cell that does not finish inside `--budget` is reported WEDGE, never as a
slow pass, and the evidence §10 used to characterise it is collected on the
spot from a *second* ssh connection: the guest's own `ps` (a process at 0:00
CPU that was created and never scheduled is the signature), the host-side vCPU
busy fraction (7 % means nothing is running, not that something is spinning),
and the console tail. A cell that fails to build for an ordinary reason —
a compile error, a missing vendor crate — is reported ERROR, which is a third
outcome and not a wedge.
"""

import argparse
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "utils"))
import hpbox  # noqa: E402

GUEST_IP = "10.0.2.15"
GUEST_PORT = 2222
GUEST_KEY = "/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key"
GUEST_SRC = "/src/akuma"
FC_JSON = "/root/akuma-fc.json"
FC_LOG = "/root/akuma-fc.log"

# A non-interactive shell in this guest inherits **nothing** — `env` prints
# `SHLVL` and `PWD` and that is all, so there is no `PATH` at all and `cargo`
# answers `/bin/sh: cargo: not found`. That line matches no `^error` grep, so a
# harness that does not set this measures four instant "failures" that look
# like a broken toolchain. Same shape as `hpbox.build`'s `BOX_CARGO`.
GUEST_ENV = ("export HOME=/root CARGO_HOME=/root/.cargo "
             "PATH=/usr/local/rust/bin:/usr/bin:/bin "
             "LD_LIBRARY_PATH=/usr/local/rust/lib;")

# The `akuma` alias is the *bare metal*, not this guest; these options reach the
# Firecracker guest from the Ubuntu side and must carry the key explicitly.
SSH = (f"ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "
       f"-o LogLevel=ERROR -o ConnectTimeout=8 -i {GUEST_KEY} "
       f"-p {GUEST_PORT} root@{GUEST_IP}")


def guest(cmd, timeout=120):
    """Run `cmd` in the Firecracker guest. Returns (rc, stdout, stderr).

    Single-quoted, and **no `2>&1`**: that redirect answers `Bad file
    descriptor` in this guest's shell and returns 1, so a harness that appends
    it measures nothing (the trap is in `amd64-bare-metal-loop.md`).
    """
    return hpbox.ubuntu(f"{SSH} '{cmd}'", timeout=timeout)


def set_vcpus(n):
    """Rewrite the FC config's vcpu count in place, leaving everything else."""
    py = (f"import json;d=json.load(open('{FC_JSON}'));"
          f"d['machine-config']['vcpu_count']={n};"
          f"json.dump(d,open('{FC_JSON}','w'));"
          f"print(d['machine-config'])")
    rc, out, err = hpbox.ubuntu(f'python3 -c "{py}"', timeout=60)
    return rc, (out + err).strip()


def boot_guest(wait_s=180):
    """Boot the guest and block until it answers ssh. Returns seconds, or None.

    Polls with a completed command, never by grepping the console for a marker
    — the reasons are in CLAUDE.md § "Waiting for a VM" and both directions of
    that mistake have cost time here.
    """
    hpbox.ubuntu("sh /root/akuma-fc-run.sh", timeout=120)
    t0 = time.time()
    while time.time() - t0 < wait_s:
        rc, out, _ = guest("echo READY", timeout=40)
        if "READY" in out:
            return time.time() - t0
        time.sleep(4)
    return None


def fc_cpu_fraction(sample_s=12):
    """Host-side busy fraction of the Firecracker process over `sample_s`.

    §10's decisive number: 7 % means the guest is running nothing, which tells
    a wedge apart from a livelock. Read from `/proc/<pid>/stat` utime+stime so
    it needs no `top` and no tty.
    """
    read = ("P=$(pgrep -x firecracker | head -1); "
            "[ -z \"$P\" ] && echo NOPROC && exit 0; "
            "awk '{print $14+$15}' /proc/$P/stat; "
            f"sleep {sample_s}; "
            "awk '{print $14+$15}' /proc/$P/stat; getconf CLK_TCK")
    rc, out, _ = hpbox.ubuntu(read, timeout=sample_s + 60)
    parts = out.split()
    if len(parts) < 3 or "NOPROC" in out:
        return None
    a, b, hz = float(parts[0]), float(parts[1]), float(parts[2])
    return (b - a) / hz / sample_s


def wedge_evidence(vcpus):
    """Everything §10 read off a wedged run, from a second connection."""
    ev = {}
    ev["cpu_fraction"] = fc_cpu_fraction()
    for name, cmd in (("ps", "ps"), ("free", "free"), ("uptime", "uptime")):
        rc, out, err = guest(cmd, timeout=60)
        ev[name] = (out or err).strip()[:2000]
    rc, out, _ = hpbox.ubuntu(f"tail -c 4000 {FC_LOG}", timeout=60)
    ev["console_tail"] = out.strip()[-2500:]
    # The counters this kernel already keeps for exactly this bug. `dmesg` is a
    # 64 KiB ring, so a long wedge can have overwritten the boot — grep, and
    # report absence as absence rather than as zero.
    rc, out, _ = guest("dmesg", timeout=90)
    marks = [ln for ln in out.splitlines()
             if re.search(r"SWITCH NO-BKL|SWITCH BADFRAME|SWITCH FRAME MOVED|"
                          r"SWITCH FREED-CR3|BKL\] stuck|TRAMP-MISMATCH|"
                          r"CANARY|PANIC", ln)]
    ev["tripwires"] = marks[-40:]
    ev["tripwire_count"] = len(marks)
    return ev


def run_cell(crate, vcpus, jobs, budget, target):
    """One (vcpus, jobs) cell. Returns a result dict."""
    res = {"vcpus": vcpus, "jobs": jobs, "outcome": "?", "seconds": None}

    rc, cfg = set_vcpus(vcpus)
    if rc not in (0, None):
        res.update(outcome="ERROR", detail=f"config rewrite failed: {cfg}")
        return res

    boot_s = boot_guest()
    if boot_s is None:
        rc, out, _ = hpbox.ubuntu(f"tail -c 2000 {FC_LOG}", timeout=60)
        res.update(outcome="NOBOOT", detail=out.strip()[-1200:])
        return res
    res["boot_seconds"] = round(boot_s, 1)

    # Clean only this crate, so the cell measures the crate and not the graph.
    guest(f"{GUEST_ENV} cd {GUEST_SRC} && "
          f"cargo clean -p {crate} --release --target {target}", timeout=180)

    build = (f"{GUEST_ENV} cd {GUEST_SRC} && cargo build -p {crate} "
             f"--target {target} --release --offline -j{jobs}")
    t0 = time.time()
    try:
        rc, out, err = guest(build, timeout=budget)
    except Exception as exc:                      # ssh itself timed out
        res.update(outcome="WEDGE", seconds=round(time.time() - t0, 1),
                   detail=f"ssh timed out after {budget}s ({type(exc).__name__})",
                   evidence=wedge_evidence(vcpus))
        return res
    elapsed = time.time() - t0
    blob = out + err

    if rc == 0 and "Finished" in blob:
        res.update(outcome="PASS", seconds=round(elapsed, 1))
        m = re.search(r"in ([\d.]+)s", blob)
        if m:
            res["cargo_seconds"] = float(m.group(1))
    elif elapsed >= budget * 0.95:
        res.update(outcome="WEDGE", seconds=round(elapsed, 1),
                   detail=blob.strip()[-800:], evidence=wedge_evidence(vcpus))
    else:
        res.update(outcome="ERROR", seconds=round(elapsed, 1),
                   detail=blob.strip()[-800:])
    return res


def parse_cells(s):
    out = []
    for cell in s.split(","):
        v, j = cell.lower().split("x")
        out.append((int(v), int(j)))
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--crate", default="zerocopy")
    ap.add_argument("--cells", default="1x1,1x4,4x1,4x4",
                    help="comma-separated <vcpus>x<jobs> (default 1x1,1x4,4x1,4x4)")
    ap.add_argument("--target", default="x86_64-unknown-none")
    ap.add_argument("--budget", type=int, default=600,
                    help="seconds before a cell is called WEDGE (default 600)")
    ap.add_argument("--repeat", type=int, default=1)
    a = ap.parse_args()

    if hpbox.which_system() != "ubuntu":
        print("the box is not on Ubuntu — Firecracker runs there. "
              "`python3 scripts/utils/hpbox.py reboot-to ubuntu` first.")
        return 2

    results = []
    for vcpus, jobs in parse_cells(a.cells):
        for rep in range(a.repeat):
            tag = f"vcpu={vcpus} -j{jobs}" + (f" rep{rep + 1}" if a.repeat > 1 else "")
            print(f"--- {a.crate}: {tag} ...", flush=True)
            r = run_cell(a.crate, vcpus, jobs, a.budget, a.target)
            r["rep"] = rep + 1
            results.append(r)
            print(f"    {r['outcome']}"
                  + (f"  {r['seconds']}s" if r.get("seconds") is not None else ""),
                  flush=True)
            if r["outcome"] == "WEDGE":
                ev = r.get("evidence", {})
                cf = ev.get("cpu_fraction")
                print(f"    vCPU busy: {cf:.1%}" if cf is not None
                      else "    vCPU busy: unreadable", flush=True)
                print(f"    tripwire lines in dmesg: {ev.get('tripwire_count')}",
                      flush=True)

    print("\n=== %s, in the box's Firecracker guest ===" % a.crate)
    print(f"{'vcpu':>5} {'jobs':>5} {'outcome':>8} {'wall':>8} {'cargo':>8}")
    for r in results:
        print(f"{r['vcpus']:>5} {r['jobs']:>5} {r['outcome']:>8} "
              f"{(str(r['seconds']) + 's') if r.get('seconds') is not None else '-':>8} "
              f"{(str(r.get('cargo_seconds')) + 's') if r.get('cargo_seconds') else '-':>8}")

    for r in results:
        if r["outcome"] in ("WEDGE", "ERROR", "NOBOOT"):
            print(f"\n--- {r['outcome']} detail: vcpu={r['vcpus']} -j{r['jobs']} ---")
            print((r.get("detail") or "")[:1500])
            ev = r.get("evidence")
            if ev:
                print("  vCPU busy fraction:", ev.get("cpu_fraction"))
                print("  guest ps:\n", (ev.get("ps") or "")[:1200])
                print("  tripwires (last 40):")
                for ln in ev.get("tripwires", []):
                    print("   ", ln[:160])
                print("  console tail:\n", (ev.get("console_tail") or "")[-1200:])

    return 0 if all(r["outcome"] == "PASS" for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
