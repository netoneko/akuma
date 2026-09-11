#!/usr/bin/env python3
"""Run the memory-syscall probes on local QEMU **and** the trashcan's
Firecracker at the same time, over the **console** instead of ssh.

# Why this exists next to `scripts/mem_suite.py`

`mem_suite.py` is the family's gate and stays the reference: it pushes each
probe over ssh, runs it, and applies the four-layer verdict (silent is not a
pass). That works against any guest with a network — the aarch64 devbox, and the
amd64 QEMU guest booted with `INIT=/bin/sshd`.

It does not work against the box's Firecracker, and not for a fixable reason:
`hpbox.firecracker` boots with `"network-interfaces": []`, so the guest has no
NIC at all and there is nothing to ssh to. That arm is the interesting one —
real KVM on real silicon against a laptop's TCG emulation — so the probes go in
on the **disk image** and report on the serial console instead.

The verdict logic is imported from `mem_suite`, not re-implemented. One
definition of "did this probe pass" across both transports is the whole point:
the rule that a silent probe is a failure was learned the hard way, and a second
copy of it is a second place to get it wrong.

# Usage

    python3 scripts/utils/amd64_mem_trials.py              # both, in parallel
    python3 scripts/utils/amd64_mem_trials.py --local-only
    python3 scripts/utils/amd64_mem_trials.py --smp 4
    python3 scripts/utils/amd64_mem_trials.py --only mmap_stress,cowstale

Exit status is 0 only if every probe that is expected to pass did, on every
trial that ran. See `EXPECTED_FAIL` for what "expected" means here and why each
entry is on the list.
"""

import argparse
import concurrent.futures as futures
import os
import pathlib
import re
import shutil
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "scripts"))

import mem_suite  # noqa: E402  (path set up above)

SRC = pathlib.Path(REPO) / "userspace/forktest/c_stress"
ARCH = "x86_64"
CC = mem_suite.ARCHES[ARCH]

# Probes that fail on this target for a reason that is **not** a memory-mapping
# defect, with the reason. Each was diagnosed rather than assumed — see
# `docs/archive/AKUMA_AMD64_MMAP_REGIONS.md` for the working.
#
# This list is a claim, not a mute button: a probe here that starts *passing* is
# reported as a surprise, because that means the gap it names has been closed
# and the entry should go.
# Four entries left on 2026-09-07 when `pread64`, `madvise`, `/proc/<pid>/` and
# `mremap` landed, and file-backed `MAP_PRIVATE` with them
# (`docs/archive/AKUMA_AMD64_MEMORY_CLOSEOUT.md`). Both survivors need **signal
# delivery**, which is trunk A2 — neither is a memory-mapping defect, and
# `mprotect` itself is verified working.
# **Empty since 2026-09-11, and the emptiness is the point.**
#
# It held the last two probes, both signals rather than memory:
#
#   mprotectlb            "needs a SIGSEGV handler; this target has no signal
#                          delivery" — it installs one and `siglongjmp`s out of
#                          it, so nothing about `mprotect` could be measured
#                          here until a fault could reach ring 3.
#   eager_mprotect_probe  "a killed child exits 128+SIGSEGV rather than
#                          reporting a signalled status, so WIFSIGNALED is never
#                          true" — `user_fault` passed a *positive* 139, which
#                          `encode_wait_status` reads as a clean exit.
#
# Both closed with `AKUMA_AMD64_FAULT_SIGNALS.md`: the exception stub saves the
# whole register file, `signal::deliver_fault_signal` builds an `rt_sigframe`
# from it, and a fault-killed process now leaves with `-SIGSEGV`. **amd64 passes
# all ten**, and `mprotectlb` reports "0 divergence(s) from Linux".
#
# Leave it as an empty dict rather than deleting it: the reporter's `PASS*`
# arm — "was expected to fail ... update EXPECTED_FAIL" — is what catches a
# future gap being quietly normalised, and it needs somewhere to write.
EXPECTED_FAIL = {}

# The banner the in-guest runner prints around each probe. Chosen to survive a
# torn console line at SMP>1: the marker is one short token on its own line, so
# an interleaved write from another core garbles at most the probe's own output
# and not the framing that finds it.
BEGIN = "=== PROBE %s ==="
END = "=== END %s"
RC = "=== RC "
DONE = "=== PROBES DONE ==="

# The in-guest runner: `amd64/probes/run_all.c`, compiled alongside the probes.
RUNNER = pathlib.Path(REPO) / "amd64/probes/run_all.c"


def build_probes(names):
    """Compile the selected probes for x86_64, into their own subdirectory."""
    if not shutil.which(CC):
        raise SystemExit(f"{CC} not found — install it (brew install FiloSottile/musl-cross/musl-cross "
                         f"or your distro's musl cross toolchain)")
    outdir = SRC / ARCH
    outdir.mkdir(parents=True, exist_ok=True)
    for name in names:
        subprocess.run([CC, "-O2", "-static", "-o", str(outdir / name),
                        str(SRC / f"{name}.c")], check=True)
    subprocess.run([CC, "-O2", "-static", "-o", str(outdir / "run_all"), str(RUNNER)],
                   check=True)
    return outdir


def runner_args(names):
    """`initargs=` for `run_all`: one comma-separated `name[:arg]` per probe.

    No spaces anywhere, and that is a hard requirement rather than a style: the
    kernel command line is split on whitespace, so `initargs=` is a single token
    and everything after the first space is silently dropped. The symptom is a
    program running with no arguments and printing its usage text, which reads
    like the program is broken.
    """
    parts = []
    for name in names:
        args, _timeout = mem_suite.PROBES[name]
        args = args.strip().replace("/tmp/mem_suite_data", "/probes/mem_suite_data")
        parts.append(f"{name}:{args}" if args else name)
    return ",".join(parts)


def inject_local(img, outdir, names):
    """Write the probes into an ext2 image with `debugfs`, on this host."""
    debugfs = shutil.which("debugfs") or "/opt/homebrew/opt/e2fsprogs/sbin/debugfs"
    if not os.path.exists(debugfs):
        raise SystemExit("debugfs not found (it ships with e2fsprogs)")

    def dfs(cmd):
        subprocess.run([debugfs, "-w", "-R", cmd, img], capture_output=True)

    dfs("mkdir /probes")
    tmp = pathlib.Path(REPO) / "target/x86_64-unknown-none/release"
    tmp.mkdir(parents=True, exist_ok=True)
    # The deterministic input the two file probes read, as in `mem_suite.stage`.
    (tmp / "mem_suite_data").write_bytes(b"A" * (4096 * 64))
    for name in list(names) + ["run_all"]:
        # `debugfs write` will not overwrite, so an image that already carries a
        # previous run's binaries would keep them — the stale-artifact trap in
        # `docs/archive/AB_STALE_BAKED_ARTIFACTS.md`, with the arm silently
        # measuring the *previous* kernel's probe.
        dfs(f"rm probes/{name}")
        dfs(f"write {outdir / name} probes/{name}")
        # `write` leaves 0644; a probe has to be executable to be a probe.
        dfs(f"sif probes/{name} mode 0100755")
    dfs("rm probes/mem_suite_data")
    dfs(f"write {tmp / 'mem_suite_data'} probes/mem_suite_data")


def inject_remote(outdir, names):
    """Same, into the box's image, over ssh. Requires e2fsprogs on the box."""
    import hpbox

    rc, out, _err = hpbox.ubuntu("command -v debugfs")
    if rc != 0 or not out.strip():
        raise SystemExit("no debugfs on the box: apt install e2fsprogs")
    img = hpbox.BOX_DISK
    staging = "/tmp/akuma-mem-probes"
    hpbox.ubuntu(f"rm -rf {staging}; mkdir -p {staging}")
    for name in list(names) + ["run_all"]:
        subprocess.run(hpbox.UB + [f"cat > {staging}/{name}"],
                       input=(outdir / name).read_bytes(),
                       capture_output=True, timeout=600)
    subprocess.run(hpbox.UB + [f"cat > {staging}/mem_suite_data"],
                   input=b"A" * (4096 * 64), capture_output=True, timeout=300)

    cmds = ["debugfs -w -R 'mkdir /probes' " + img]
    for name in list(names) + ["run_all"]:
        # `rm` first: `debugfs write` refuses an existing name, so without this
        # the box would keep running whatever was staged by a previous session.
        cmds.append(f"debugfs -w -R 'rm probes/{name}' {img}")
        cmds.append(f"debugfs -w -R 'write {staging}/{name} probes/{name}' {img}")
        cmds.append(f"debugfs -w -R 'sif probes/{name} mode 0100755' {img}")
    cmds.append(f"debugfs -w -R 'rm probes/mem_suite_data' {img}")
    cmds.append(f"debugfs -w -R 'write {staging}/mem_suite_data probes/mem_suite_data' {img}")
    hpbox.ubuntu(" ; ".join(cmds), timeout=600)


def score(log, names):
    """Split a console log on the banners and apply `mem_suite.verdict`.

    A probe whose `BEGIN` marker never appeared is reported as `NOT REACHED`
    rather than skipped: the run died partway, and calling the rest "not run" is
    the honest answer where calling them absent-therefore-fine is the trap this
    whole family of harnesses exists to avoid.
    """
    results = {}
    for name in names:
        begin = BEGIN % name
        if begin not in log:
            results[name] = (False, "NOT REACHED — the run stopped before this probe", 0, "")
            continue
        body = log.split(begin, 1)[1]
        m = re.search(re.escape(END % name) + r"\s*\n.*?" + re.escape(RC) + r"(-?\d+) ===",
                      body, re.S)
        out = body[: m.start()] if m else body
        rc = int(m.group(1)) if m else 0
        if not m:
            results[name] = (False, "NO END MARKER — the probe did not return", 0, out.strip())
            continue
        ok, why, div = mem_suite.verdict(name, out, rc)
        results[name] = (ok, why, div, out.strip())
    return results


def local_qemu(smp, names, timeout_s, ssh_port, http_port):
    """Boot the probes as `init` under QEMU and return the console log.

    Reads the console as it streams and stops on the `DONE` banner, for the
    reason `amd64_trials.py` documents at length: the guest halts with `cli; hlt`
    and never exits, so waiting for the process costs the whole timeout however
    fast the run was.
    """
    img = os.path.join(REPO, "target/x86_64-unknown-none/release/amd64-root.img")
    env = dict(os.environ, SMP=str(smp), SSH_PORT=str(ssh_port), HTTP_PORT=str(http_port),
               DISK=img, INIT="/probes/run_all", INITARGS=runner_args(names))
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
        # Kill only our own instance, matched on its own forward. Never
        # `pkill -f qemu-system-x86_64`: other VMs are someone else's work.
        subprocess.run(["pkill", "-f", f"hostfwd=tcp::{ssh_port}-"], capture_output=True)
    return "".join(lines)


def remote_firecracker(smp, names, timeout_s, do_deploy):
    import hpbox

    if do_deploy:
        rc, msg = hpbox.deploy()
        if rc not in (0, None):
            return f"deploy failed: {msg}"
        print(f"[mem-trials] {msg}", flush=True)
    rc, out = hpbox.build()
    if rc not in (0, None):
        return f"build failed: {out[-800:]}"
    inject_remote(SRC / ARCH, names)
    return hpbox.firecracker(vcpus=smp, init="/probes/run_all",
                             initargs=runner_args(names), timeout_s=timeout_s)


def report(title, log, names):
    print(f"\n===== {title} =====")
    if not log.strip():
        print("  NO LOG — the trial produced nothing")
        return False
    results = score(log, names)
    unexpected = []
    for name in names:
        ok, why, _div, out = results[name]
        expected = EXPECTED_FAIL.get(name)
        if ok:
            tag = "PASS"
            if expected:
                tag = "PASS*"
                why += f"  (was expected to fail: {expected} — update EXPECTED_FAIL)"
                unexpected.append(name)
        elif why.startswith(("NOT REACHED", "NO END MARKER")):
            # Deliberately **not** excused by `EXPECTED_FAIL`. A probe that never
            # ran has told us nothing, and calling it a known failure is the
            # silent-pass trap wearing a different hat: the first version of this
            # reporter did exactly that and printed six reassuring `known` lines
            # for a boot that had not run a single probe.
            tag = "FAIL"
            unexpected.append(name)
        elif expected:
            tag = "known"
            why = expected
        else:
            tag = "FAIL"
            unexpected.append(name)
        print(f"  {tag:<6} {name:<22} {why}")
        if tag == "FAIL":
            print("\n".join("         | " + l for l in out.splitlines()[-10:]))
    good = sum(1 for n in names if results[n][0])
    print(f"  -> {good}/{len(names)} passed, "
          f"{len(names) - good - len([n for n in names if not results[n][0] and n in EXPECTED_FAIL])} "
          f"unexpected failure(s)")
    return not unexpected


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("--only", help="comma-separated subset of probe names")
    ap.add_argument("--local-only", action="store_true")
    ap.add_argument("--remote-only", action="store_true")
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--no-deploy", action="store_true")
    ap.add_argument("--ssh-port", type=int, default=2245)
    ap.add_argument("--http-port", type=int, default=8045)
    ap.add_argument("--timeout", type=int, default=900)
    a = ap.parse_args(argv)

    names = list(mem_suite.PROBES)
    if a.only:
        names = [n.strip() for n in a.only.split(",")]
        unknown = [n for n in names if n not in mem_suite.PROBES]
        if unknown:
            print(f"unknown probe(s): {', '.join(unknown)}", file=sys.stderr)
            return 2

    outdir = SRC / ARCH
    if not a.no_build:
        outdir = build_probes(names)
        print(f"built {len(names)} probe(s) for {ARCH} with {CC}", flush=True)

    if not a.remote_only:
        # The image has to exist before anything can be written into it, and
        # `run.sh` only builds one when `DISK` is unset — which it will not be,
        # because the probes have to go in first.
        img = os.path.join(REPO, "target/x86_64-unknown-none/release/amd64-root.img")
        if not os.path.exists(img):
            subprocess.run(["sh", os.path.join(REPO, "amd64", "mkdisk.sh"), img, "128"],
                           cwd=REPO, check=True, capture_output=True)
        inject_local(img, outdir, names)

    jobs = {}
    with futures.ThreadPoolExecutor(max_workers=2) as pool:
        if not a.remote_only:
            jobs["qemu/tcg smp=%d" % a.smp] = pool.submit(
                local_qemu, a.smp, names, a.timeout, a.ssh_port, a.http_port)
        if not a.local_only:
            jobs["firecracker smp=%d" % a.smp] = pool.submit(
                remote_firecracker, a.smp, names, min(a.timeout, 300), not a.no_deploy)
        logs = {k: j.result() for k, j in jobs.items()}

    ok = True
    for title, log in logs.items():
        ok = report(title, log, names) and ok
    print()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
