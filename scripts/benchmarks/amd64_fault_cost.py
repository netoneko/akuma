#!/usr/bin/env python3
"""Run `mem_fault_cost` inside the **amd64** guest, over the console.

# Why this exists next to `scripts/benchmarks/mem_ab_run.sh`

`mem_ab_run.sh` is the family's A/B driver and stays the reference for AArch64:
it pushes the probe over ssh and runs it in a live devbox. Neither half of that
works out of the box on this target —

  * `userspace/memprobe/c/build.sh` compiles with `aarch64-linux-musl-gcc` only;
  * the box's Firecracker boots with no NIC at all, so there is nothing to ssh
    to, and even locally an ssh round trip needs `INIT=/bin/sshd` and a booted
    userland before the probe can start.

So this does what `scripts/utils/amd64_mem_trials.py` does for the correctness
probes: build for `x86_64-linux-musl`, write the binary into the ext2 image with
`debugfs`, boot it **as init**, and read the answer off the serial console. The
injection helper is imported from that file rather than re-implemented — one
definition of "get a probe into an amd64 guest".

# What it measures, and how to read it

`mem_fault_cost` is the fault-path instrument: every arm faults or allocates.
Its own header states the method — each per-unit number is a *bracket*
(`(many - one) / (MANY - ONE)`), so the mmap, the munmap, the fork and the exit
cancel, and the control arms (`mmap_lazy`, `fork_exit`) are what say whether two
runs are comparable at all. **Read the ratios, not the nanoseconds**: TCG
boot-to-boot drift here is multiplicative.

`SMP=1` by default, and that is not a convenience: this target's `fork` demotes
the parent's live PTEs with no TLB shootdown, so the CoW arms are only
meaningful on one core (`docs/archive/AKUMA_AMD64_COW.md`).

# Usage

    scripts/benchmarks/amd64_fault_cost.py                    # one run
    scripts/benchmarks/amd64_fault_cost.py --runs 3           # three, A/B/A style
    scripts/benchmarks/amd64_fault_cost.py --passes 10        # cheaper, noisier
    scripts/benchmarks/amd64_fault_cost.py --label before     # tag the output

Build the kernel yourself first. This script deliberately does **not** chain a
`cargo build` into the boot: the harness that did raced a partially-linked
kernel and reported a run that never happened.
"""

import argparse
import os
import pathlib
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(REPO, "scripts", "utils"))

from amd64_mem_trials import inject_local  # noqa: E402

PROBE_SRC = pathlib.Path(REPO) / "userspace/memprobe/c"
OUTDIR = PROBE_SRC / "x86_64"
CC = "x86_64-linux-musl-gcc"
IMG = os.path.join(REPO, "target/x86_64-unknown-none/release/amd64-root.img")
# The probe's last line. Reading for it rather than waiting on the process is
# mandatory: the guest halts with `cli; hlt` and never exits, so a `wait` costs
# the whole timeout however fast the run was.
DONE = "mem_fault_cost: END"


def build_probe():
    OUTDIR.mkdir(parents=True, exist_ok=True)
    out = OUTDIR / "mem_fault_cost"
    subprocess.run(
        [CC, "-static", "-O2", "-Wall", "-Wextra", "-o", str(out),
         str(PROBE_SRC / "mem_fault_cost.c")],
        cwd=PROBE_SRC, check=True)
    return out


def boot(smp, passes, timeout_s, ssh_port, http_port):
    env = dict(os.environ, SMP=str(smp), SSH_PORT=str(ssh_port),
               HTTP_PORT=str(http_port), DISK=IMG,
               INIT="/probes/mem_fault_cost", INITARGS=str(passes))
    lines, proc = [], None
    state = {"done": False}
    try:
        proc = subprocess.Popen(["sh", os.path.join(REPO, "amd64", "run.sh")],
                                env=env, cwd=REPO, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, bufsize=1)

        def reader():
            for line in proc.stdout:
                lines.append(line)
                if DONE in line:
                    state["done"] = True

        threading.Thread(target=reader, daemon=True).start()
        deadline = time.time() + timeout_s
        while time.time() < deadline and not state["done"]:
            if proc.poll() is not None:
                break
            time.sleep(0.5)
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
        # Kill only our own instance, matched on its own forward.
        subprocess.run(["pkill", "-f", f"hostfwd=tcp::{ssh_port}-"],
                       capture_output=True)
    return "".join(lines)


def extract(log):
    """The probe's own report, from its banner to its last bracket line."""
    out, on = [], False
    for line in log.splitlines():
        # From the moment the kernel hands it the console, so a `FAIL: arm ...`
        # line — which the probe prints BEFORE its banner — is in the report.
        if "running /probes/mem_fault_cost" in line:
            on = True
            continue
        if on:
            out.append(line.rstrip())
        if on and DONE in line:
            break
    return out


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("--passes", type=int, default=20)
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--label", default="")
    ap.add_argument("--ssh-port", type=int, default=2246)
    ap.add_argument("--http-port", type=int, default=8046)
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--no-build-probe", action="store_true")
    a = ap.parse_args(argv)

    if not a.no_build_probe:
        p = build_probe()
        print(f"built {p} ({p.stat().st_size} bytes)", flush=True)

    if not os.path.exists(IMG):
        subprocess.run(["sh", os.path.join(REPO, "amd64", "mkdisk.sh"), IMG, "128"],
                       cwd=REPO, check=True, capture_output=True)
    inject_local(IMG, OUTDIR, ["mem_fault_cost"])

    tag = f" [{a.label}]" if a.label else ""
    rc = 0
    for i in range(a.runs):
        t0 = time.time()
        log = boot(a.smp, a.passes, a.timeout, a.ssh_port, a.http_port)
        report = extract(log)
        print(f"\n===== mem_fault_cost smp={a.smp} passes={a.passes} "
              f"run {i + 1}/{a.runs}{tag} ({time.time() - t0:.0f}s) =====")
        if not report:
            print("  NO REPORT — the probe produced nothing")
            print("\n".join("  | " + l for l in log.splitlines()[-25:]))
            rc = 1
            continue
        print("\n".join("  " + l for l in report))
    return rc


if __name__ == "__main__":
    sys.exit(main())
