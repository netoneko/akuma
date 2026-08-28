#!/usr/bin/env python3
"""Fast A/B sampler for the cowstale/bssfork fork-CoW probe flake.

## Which sampling method this is

`docs/runbooks/verify-trim-fat-change.md` § "Two ways of sampling a flaky
probe, and why they disagree" (added 2026-08-28) documents that the SAME
commit gives DIFFERENT pass rates depending on how you sample it:

    method                                          cowstale   bssfork
    one boot, probe re-invoked 5x over ssh           0/5        2/5
    fresh boot per run, via verify_trim.py --tier 2  2/5        5/5

Neither is wrong, but the numbers are not interchangeable, and comparing a rate
from one method against a rate from the other is exactly what makes an
unrelated harness change look like a regression.

This script implements a THIRD, distinct method:

    fresh boot per run, ORDERED EXERCISE SLICE (madvshared, mremapmove,
    cowstale, bssfork, bssfork 20 8 1) instead of the full ~14-exercise gate

Call this the "sliced-fresh-boot" method. It reboots for every sample, like
`verify_trim.py --tier 2` does, so it should track that method's rate rather
than the harsher in-boot-repetition one -- but it has not been calibrated
against a full `--tier 2` run often enough to promise the numbers are
identical, only that the reboot discipline is the same. **Numbers out of this
script must only ever be compared against other numbers out of this script**
(or, cautiously, against `--tier 2` numbers, never against in-boot-repeat
numbers). Always report the method name next to a number, per the runbook's
rule 1: "a bare '2/5' is not a result."

## Why the exercise order matters, and why this isn't just "run cowstale fast"

`madvshared` and `mremapmove` are not padding. `verify_trim.py`'s `EXERCISES`
list runs them immediately before `cowstale`/`bssfork` on every boot, and an
isolated boot that skips straight to `cowstale` (nothing run first) was
measured 5/5 clean on commits that scored only 2/5 through the real ordered
gate. That gap is the same state-accumulation effect the runbook describes for
in-boot repetition, just at a coarser grain: whatever `madvshared` and
`mremapmove` leave behind (retired CoW/mmap state, allocator residue) changes
`cowstale`'s odds of tripping its stale-write-fault. So this script runs the
EXACT prefix of `EXERCISES` up to and including `bssfork_spread1` in the EXACT
same order, not just `cowstale` alone -- the value-add over the full gate is
skipping the other ~9 exercises *after* that point, which don't feed back into
cowstale/bssfork's own result.

## What this script does NOT duplicate

It imports `EXERCISES`, `exercise_suite`, `ssh`, `sh`, `read_log`, `MAIN_REPO`
from `scripts/verify_trim.py` and calls `exercise_suite` (temporarily swapping
in the 5-item slice) rather than reimplementing the ssh/nohup/poll machinery,
the healthy-output predicates, or the `mmapsum` digest comparison. If you need
the *whole* gate (clippy, host tests, all 14 exercises, tier 4), use
`verify_trim.py` directly -- this script is only for fast, repeated sampling
of the fork/CoW flake specifically.

## Operational rules (same as verify_trim.py / CLAUDE.md)

  * `--instance` defaults to 9 (not 0, not 3 -- both are commonly in use by
    other sessions/scripts in this repo; pass your own to avoid a port clash).
  * `--memory` must be >= 2048 (MB). Below that, a boot-test build hits a
    QEMU/HVF `Assertion failed: (isv)` and QEMU exits 134 before booting --
    `docs/archive/QEMU_HVF_ISV_BUG.md` root cause 5. This is a host-hypervisor
    limit, not a kernel bug; refusing under it here avoids reading a QEMU exit
    as a probe failure.
  * QEMU is killed ONLY by matching this run's own forwarded ssh port
    (`hostfwd=tcp::<port>-:22`) -- never a broad `pkill -f qemu-system-aarch64`,
    which kills every other VM the user or another session has running.
  * Readiness is an ssh round-trip via `scripts/vm_ready.py`
    (`vm_ready.wait_ready`), never a boot-log marker grep -- see that script's
    docstring and CLAUDE.md § "Waiting for a VM" for why both directions of
    that check are wrong.
  * ssh is driven from Python (the `ssh` CLI is blocked by policy for the
    agent); this script reuses `verify_trim.ssh`.
  * Any boot-log grep in this script uses `grep -a` -- QEMU emits a control
    byte that makes plain `grep` treat the log as binary and print nothing.
  * When the commit under test is built in a linked worktree, `DISK` is
    pointed at the MAIN repo's `disk.img` (a linked worktree has no 3 GB,
    gitignored `disk.img` of its own) -- resolved the same way
    `verify_trim.py` resolves `MAIN_REPO`.

## Host contention moves these numbers

Tier 3 is timing-sensitive. A second QEMU pinning several cores on the same
host is easily enough to change a verdict (this is also why the runbook warns
against it). **Do not run two arms of an A/B concurrently on one host** --
sample arm A to completion, then arm B, never interleaved.

## Interface

    scripts/probe_ab.py <commit-ish|elf-path> [--runs N] [--instance N]
                         [--memory 2048] [--smp 4] [--label NAME] [--out FILE]
                         [--keep-worktree]

Prints a `=== SUMMARY (sliced-fresh-boot, N=<runs>) ===` block with one
`<exercise>: <pass>/<runs> ok` line per probe in the slice, plus the raw
per-run verdict list -- meant to be diffed against a run of this same script
on another commit-ish, the same way `verify_trim.py --out` is.
"""

import argparse
import os
import subprocess
import sys
import re
import tempfile
import time

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
if SCRIPT_DIR not in sys.path:
    sys.path.insert(0, SCRIPT_DIR)

# MAIN_REPO is this script's own physical location's repo root -- it is NOT
# computed from cwd, unlike verify_trim.py's MAIN_REPO, because this script
# manages its own worktrees explicitly rather than assuming it is invoked
# from within one. This script always lives in the main checkout (per its
# own instructions), so this is stable regardless of where you run it from.
MAIN_REPO = os.path.dirname(SCRIPT_DIR)

MIN_MEMORY_MB = 2048
DEFAULT_INSTANCE = 9


def parse_memory(s):
    """Accepts '2048' or '2048M'; returns (mb_int, cargo_runner_string)."""
    digits = "".join(c for c in s if c.isdigit())
    if not digits:
        raise ValueError(f"unparseable --memory value: {s!r}")
    mb = int(digits)
    if mb < MIN_MEMORY_MB:
        raise SystemExit(
            f"--memory {s} is below {MIN_MEMORY_MB} MB: a boot-test build hits "
            f"the QEMU/HVF 'Assertion failed: (isv)' bug and exits 134 before "
            f"booting (docs/archive/QEMU_HVF_ISV_BUG.md root cause 5). That "
            f"reads as a boot failure but is a host-hypervisor limit, not the "
            f"commit under test.")
    runner_str = s if s.upper().endswith("M") else f"{mb}M"
    return mb, runner_str


def resolve_target(spec, keep_worktree):
    """Returns (elf_path, cleanup_fn). `spec` is either an existing file (used
    directly as the ELF) or a commit-ish (built fresh in a throwaway worktree)."""
    if os.path.isfile(spec):
        return os.path.abspath(spec), (lambda: None)

    # Treat as a commit-ish. Resolve it first so a typo fails before we build
    # a worktree directory for it.
    rev = subprocess.run(["git", "-C", MAIN_REPO, "rev-parse", "--short", spec],
                          capture_output=True, text=True)
    if rev.returncode != 0:
        raise SystemExit(f"{spec!r} is neither an existing file nor a resolvable "
                          f"git commit-ish in {MAIN_REPO}:\n{rev.stderr.strip()}")
    short = rev.stdout.strip()
    wt_dir = tempfile.mkdtemp(prefix=f"probe_ab_{short}_")
    os.rmdir(wt_dir)  # git worktree add wants to create this itself
    print(f"# building {spec} ({short}) in worktree {wt_dir}", file=sys.stderr)
    add = subprocess.run(["git", "-C", MAIN_REPO, "worktree", "add", wt_dir, spec],
                          capture_output=True, text=True)
    if add.returncode != 0:
        raise SystemExit(f"git worktree add failed:\n{add.stderr}")

    def cleanup():
        if keep_worktree:
            print(f"# leaving worktree at {wt_dir} (--keep-worktree)", file=sys.stderr)
            return
        subprocess.run(["git", "-C", MAIN_REPO, "worktree", "remove", "--force", wt_dir],
                        capture_output=True)

    build = subprocess.run(["cargo", "build", "--release"], cwd=wt_dir,
                            capture_output=True, text=True, timeout=1800)
    if build.returncode != 0:
        cleanup()
        raise SystemExit(f"cargo build --release failed for {spec} ({short}):\n"
                          f"{build.stderr[-4000:]}")
    elf = os.path.join(wt_dir, "target", "aarch64-unknown-none", "release", "akuma")
    if not os.path.isfile(elf):
        cleanup()
        raise SystemExit(f"build succeeded but ELF not found at {elf}")
    return elf, cleanup


def boot(elf_path, port, instance, smp, memory_str, boot_log_path):
    subprocess.run(["pkill", "-f", f"hostfwd=tcp::{port}-:22"], capture_output=True)
    time.sleep(2)
    env = os.environ.copy()
    env.update({
        "DISK": os.path.join(MAIN_REPO, "disk.img"),
        "INSTANCE": str(instance),
        "SMP": str(smp),
        "MEMORY": memory_str,
    })
    log = open(boot_log_path, "w")
    vm = subprocess.Popen([os.path.join(MAIN_REPO, "scripts", "cargo_runner.sh"), elf_path],
                          cwd=MAIN_REPO, env=env, stdout=log, stderr=subprocess.STDOUT)
    return vm, log


def shutdown(vm, log, port):
    vm.terminate()
    try:
        vm.wait(timeout=10)
    except subprocess.TimeoutExpired:
        vm.kill()
    log.close()
    subprocess.run(["pkill", "-f", f"hostfwd=tcp::{port}-:22"], capture_output=True)
    time.sleep(1)


def fault_tail(boot_log_path, max_lines=8):
    """grep -a for the [Fault]/[WPF] signature lines, per CLAUDE.md's -a rule
    (QEMU's control byte makes plain grep print nothing on these logs)."""
    r = subprocess.run(["grep", "-a", "-E", r"\[Fault\]|\[WPF\]", boot_log_path],
                        capture_output=True, text=True)
    lines = [l for l in r.stdout.splitlines() if l.strip()]
    return lines[-max_lines:]


def main():
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("target", help="commit-ish to build, or an existing ELF path")
    ap.add_argument("--runs", type=int, default=5,
                     help="fresh-boot samples to take (default 5, per the runbook's "
                          "'a single sample means nothing')")
    ap.add_argument("--instance", type=int, default=DEFAULT_INSTANCE,
                     help=f"QEMU INSTANCE, shifts ports (default {DEFAULT_INSTANCE})")
    ap.add_argument("--memory", default="2048M", help="minimum 2048 (MB)")
    ap.add_argument("--smp", type=int, default=4)
    ap.add_argument("--label", default=None, help="label for output (default: target)")
    ap.add_argument("--out", help="also write the summary block to this file")
    ap.add_argument("--keep-worktree", action="store_true",
                     help="don't remove the throwaway worktree afterwards (ignored "
                          "if target was already an ELF path)")
    args = ap.parse_args()

    _, memory_str = parse_memory(args.memory)
    label = args.label or args.target
    port = _ssh_port(args.instance)

    elf_path, cleanup = resolve_target(args.target, args.keep_worktree)

    # Deferred import: keeps --help instant and independent of being run
    # inside a git checkout. verify_trim.py computes REPO/MAIN_REPO from cwd
    # at import time, which is irrelevant here -- we only want its EXERCISES
    # data and its ssh/exercise_suite machinery, not its own repo resolution.
    import verify_trim as vt

    SLICE = vt.EXERCISES[:5]
    assert [name for name, _, _ in SLICE] == [
        "madvshared", "mremapmove", "cowstale", "bssfork", "bssfork_spread1"
    ], "verify_trim.py's EXERCISES order changed -- update the slice or this assert"

    scratch = tempfile.gettempdir()
    # `label` defaults to the target spec, which is often a PATH — interpolating
    # it straight into a filename produced a nested directory that does not
    # exist (`.../probe_ab_target/aarch64-unknown-none/release/akuma_1.log`) and
    # the first real run died on it. Keep the label readable in the summary, but
    # flatten it for filenames.
    safe_label = re.sub(r"[^A-Za-z0-9._-]+", "_", label).strip("_") or "run"
    per_run = []
    try:
        for run in range(1, args.runs + 1):
            boot_log_path = os.path.join(scratch, f"probe_ab_{safe_label}_{run}.log")
            print(f"# run {run}/{args.runs}: booting {label} "
                  f"(instance={args.instance} smp={args.smp} port={port})",
                  file=sys.stderr)
            vm, log = boot(elf_path, port, args.instance, args.smp, memory_str, boot_log_path)
            try:
                import vm_ready
                if not vm_ready.wait_ready(port=port, timeout=180, proc=vm):
                    print(f"# run {run}: VM never became ssh-ready, skipping", file=sys.stderr)
                    per_run.append({name: "BOOT-TIMEOUT" for name, _, _ in SLICE})
                    continue
                results = {}
                original_exercises = vt.EXERCISES
                vt.EXERCISES = SLICE
                try:
                    vt.exercise_suite(port, args.smp, results)
                finally:
                    vt.EXERCISES = original_exercises
                run_verdicts = {}
                for name, _, _ in SLICE:
                    key = f"smp{args.smp}.ex.{name}"
                    verdict = results.get(key, "MISSING")
                    run_verdicts[name] = verdict
                    print(f"  {name}: {verdict}", file=sys.stderr)
                    if verdict == "UNEXPECTED":
                        for l in fault_tail(boot_log_path):
                            print(f"    {l}", file=sys.stderr)
                per_run.append(run_verdicts)
            finally:
                shutdown(vm, log, port)
    finally:
        cleanup()

    lines = [f"=== SUMMARY (sliced-fresh-boot, N={args.runs}) ==="]
    lines.append(f"label: {label}")
    lines.append(f"smp: {args.smp}  memory: {memory_str}  instance: {args.instance}")
    for name, _, _ in SLICE:
        verdicts = [r[name] for r in per_run]
        n_ok = sum(1 for v in verdicts if v == "ok")
        lines.append(f"{name}: {n_ok}/{len(verdicts)} ok  {verdicts}")
    block = "\n".join(lines)
    print(block)
    if args.out:
        with open(args.out, "w") as f:
            f.write(block + "\n")
    return 0


def _ssh_port(instance):
    import vm_ready
    return vm_ready.ssh_port(instance)


if __name__ == "__main__":
    sys.exit(main())
