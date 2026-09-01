#!/usr/bin/env python3
"""
No-regression gate for trim-the-fat / refactor changes, in one invocation.

This is `docs/runbooks/verify-trim-fat-change.md` executed rather than
transcribed. Those changes are meant to be behaviour-preserving, so the gate is a
**comparison against a baseline**, not a green checkmark — the output is a single
`=== SUMMARY ===` block designed to be diffed against a run on another commit:

    scripts/verify_trim.py --out mine.txt
    git worktree add /tmp/base HEAD~1 && (cd /tmp/base && \
        scripts/verify_trim.py --instance 1 --out /tmp/base.txt)
    diff /tmp/base.txt mine.txt

Every measurement here exists because assembling it by hand produced a wrong
answer at least once:

  * `grep -a` on every boot-log read — QEMU emits a control byte that makes plain
    grep treat the log as binary and print nothing.
  * Test totals via sed+bc, never `awk -F'[ ;]' '{s+=$4}'` — consecutive
    separators shift fields and it silently under-counts.
  * Clippy is judged on its own exit status and diagnostic count, not on a
    `grep -c` of the human output (which matched a cached "Finished" line once and
    reported a phantom warning).
  * A forced touch before clippy, because a cached 0.03s run proves nothing.
  * Stale QEMU is killed first: it holds the forwarded ports and the new VM dies
    with `Could not set up host forwarding rule`, which reads exactly like a boot
    failure.
  * Boot tests are counted in BOTH marker formats. `[PASS]` and
    `[Test] <name> PASSED` are different sets; deleting a boot test may move only
    the second (the 2026-08-13 memory-math move took PASSED 273 -> 268 while
    [PASS] stayed 94).
  * `retired_reclaim_ab` is reported separately from the failure set, because it
    flips run to run on an unmodified tree and would otherwise mask a real
    failure or fake a regression.
  * ssh is driven from Python (the `ssh` CLI is blocked by policy for the agent)
    with stdout captured apart from stderr — an ssh banner folded into a stdout
    parse was one of the three false "findings" this runbook was written after.

Exit status is 0 when every tier ran; it is NOT a pass/fail verdict, because only
a diff against a baseline can say that. Read the summary.
"""

import argparse
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vm_ready

REPO = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                      capture_output=True, text=True, check=True).stdout.strip()

# The MAIN worktree, which is where `disk.img` lives. When this script runs from a
# baseline `git worktree` -- which is exactly what the runbook's A/B procedure tells
# you to do -- `REPO` is the linked worktree, and that has no `disk.img` (3 GB,
# gitignored, never copied). Pointing `DISK` at `REPO/disk.img` there makes QEMU exit
# with "Could not open ... No such file or directory", which this script reported as
# `booted: False` -- indistinguishable from "the baseline commit is broken", the most
# alarming possible misreading of a baseline arm. `--git-common-dir` resolves to the
# main repo's `.git` from either side, so its parent is the main worktree.
_COMMON_GIT_DIR = subprocess.run(
    ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
    capture_output=True, text=True, check=True).stdout.strip()
MAIN_REPO = os.path.dirname(_COMMON_GIT_DIR) or REPO

# profile + feature set are always chosen together; see docs/reference/build-profiles.md
CLIPPY_CONFIGS = [
    ("release", ["--release"]),
    ("extreme-size", ["--profile", "extreme-size", "--no-default-features",
                      "--features", "no-tests,smoltcp,extreme,userspace-sshd"]),
    ("devbox-smoltcp", ["--release", "--features", "devbox-smoltcp,no-tests"]),
    ("devbox-rump", None),  # features scraped from scripts/build_devbox.sh below
]

# Self-reporting binaries already on disk.img, as (result-key, command, healthy
# substring). `bssfork`'s CLI is positional (`bssfork [rounds] [threads]
# [spread]`), NOT `key=value` — the literal command `bssfork spread=1` feeds
# "spread=1" into `rounds`, which `strtoul` parses as 0, so the fork loop never
# runs and the liveness check flags every thread `[never ran]` before the
# scheduler gets to them. That mis-invocation is what produced the
# "BROKEN PRE-EXISTING" verdict recorded in this repo's history (failures=7/8,
# ticks=0, on both `main` and this branch) — corrected 2026-08-14, see
# docs/archive/PMM_EXTRACT.md §8. `bssfork 20 8 1` is the real spread=1 control
# and passes cleanly; keep both entries so a regression in either shape is caught.
#
# `madvshared` is the pass condition for the MADV_DONTNEED share-breaking fix
# (docs/archive/CARGO_HEAP_NULL_RC.md): it builds a CoW-shared frame by hand, with no
# allocator in the way, and reports whether a peer's page survived the advise. It
# runs in milliseconds and it is calibrated — the identical static binary PASSes
# all three phases on real Linux arm64, so a FAIL here is the kernel, not the
# probe. On a tree without the fix it reports two FAILs; `cowstale`/`bssfork` are
# the no-regression check beside it.
#
# `mremapmove` is the regression guard USER_COPY_FOLD.md §5 asked for: `sys_mremap`
# copies the payload into a brand-new (entirely lazy) destination mapping, and it
# used to truncate at the first page that faulted, silently — a `break` in the copy
# loop is indistinguishable from completion at the call site. Also calibrated ALL
# PASS on real Linux arm64, so a FAIL is the kernel.

def digests_agree(out):
    """`mmapsum` verdict: the four whole-file digests must be one value.

    `mmapsum <path>` prints six FNV-1a digests of the same file — `read:` (the
    known-good VFS path), `mmap1:`/`mmap2:` (one mapping, hashed twice),
    `madv:` (a MADV_WILLNEED pre-faulted mapping) and `mtA:`/`mtB:`. It prints
    no PASS line of its own, which is why this is a predicate and not a
    substring.

    Only the first four are compared, and that is deliberate: `mtA`/`mtB` hash
    ONE HALF of the file each, so they differ from the whole-file digest and
    from each other BY DESIGN (measured: mtA=4c20e7d2a619dc11,
    mtB=bc613dc1fc901ae3 against read=f73e24dbf056857f on /bin/busybox). A
    check that demanded all six agree would fail on a perfectly healthy kernel.
    Their value is cross-run stability, which a single run cannot judge.

    `madv:` is the one that matters most: on 2026-07-25 MADV_WILLNEED installed
    ZEROED frames over file-backed lazy pages, so this digest — and only this
    digest — diverged, which is what "llama.cpp produces garbage with mmap"
    looked like from the outside.
    """
    digests = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0].rstrip(":") in ("read", "mmap1", "mmap2", "madv"):
            digests[parts[0].rstrip(":")] = parts[1]
    # All four must be present AND equal. Requiring presence matters: a probe
    # that died after printing two lines would otherwise "agree" with itself.
    return len(digests) == 4 and len(set(digests.values())) == 1


# ---------------------------------------------------------------------------
# The second group was added 2026-08-15. Until then every exercise here was a
# fork / CoW / ELF-load probe, so whole families of `c_stress` binaries already
# sitting on `disk.img` — mprotect semantics, FP/NEON state across faults,
# file-backed mmap content, signal delivery, the allocator — were never run by
# the gate at all. Each entry below was run on a booted VM before being added,
# and its `healthy` string copied from that binary's ACTUAL output rather than
# guessed from its source. Selection rules, learned the hard way in that pass:
#
#   * It must terminate on its own, quickly. `spawnalias` (>155 s even at 300
#     rounds), `tidflags` and `clonearg` (both still printing nothing but their
#     banner after 4-5 minutes) are omitted for that reason, not because they
#     are uninteresting — a probe that never exits is 420 s of TIMEOUT per SMP
#     level. `termtest` is omitted because it blocks on terminal input.
#   * Its verdict must be a distinctive substring. `fpfault` / `neonfault` print
#     a `done, N/M` ratio, so the marker includes the `0/` — matching only
#     "done," would call a corrupting kernel healthy.
#   * A binary missing from `disk.img` reports UNEXPECTED with a `.tail` of
#     "sh: <name>: not found", which is how `futextest` (built in-tree, never
#     staged) presents. That is a legible result, not a false alarm.
#
# What each of the new ones guards:
#
#   `mprotectlb`    — mprotect downgrade/upgrade, PROT_NONE reads, PROT_READ
#                     writes, and a guard page's blast radius. Self-calibrating:
#                     it counts its own divergences FROM LINUX and prints the
#                     total, so the marker is the count, not a PASS.
#   `eager_mprotect_probe` — mprotect downgrade vs. the `[EAGER-UPGRADE]`
#                     fault-handler repair. Was a KNOWN FAIL from 2026-08-15;
#                     passes since 2026-09-01 (see KNOWN_FAIL_EXERCISES below).
#   `pthread_kill_eintr`   — a signal interrupting a blocked `read()`: EINTR
#                     without SA_RESTART, transparent restart with it. Prints a
#                     PHASE2 INFO line about deferred handler delivery that is a
#                     documented divergence, NOT a failure — do not "fix" the
#                     marker to match it.
#   `fpfault`       — all 32 Q registers canaried across every demand-paging
#                     fault. The llama.cpp "garbage with mmap" hypothesis probe.
#   `neonfault`     — NEON loads that straddle a page boundary into a not-yet
#                     -faulted page (the quantized-GEMM access shape).
#   `mmapsum`       — file content read four ways. See `digests_agree`.
#   `mmap_file`     — demand-paging a file-backed mapping end to end.
#   `allocstress`   — 2M allocations; the only exercise that leans on the heap
#                     rather than the fault path.
#   `stackstress`   — 100 rounds of deep recursion against the exception stack.
#
# `/bin/busybox` is the file argument for all three mmap probes: it is ~1.1 MB
# (272 faults, enough to be a real demand-paging workload), and it is the one
# file guaranteed present on any disk that can run this suite at all.
EXERCISES = [
    ("madvshared", "madvshared", "madvshared: ALL PASS"),
    ("mremapmove", "mremapmove", "mremapmove: ALL PASS"),
    ("cowstale", "cowstale", "cowstale PASS"),
    ("bssfork", "bssfork", "bssfork PASS"),
    ("bssfork_spread1", "bssfork 20 8 1", "bssfork PASS"),
    ("forkprobe", "forkprobe", "forkprobe: ALL PASS"),
    ("elftest", "elftest", "elftest: ALL tests PASSED"),
    # --- added 2026-08-15 (all measured at 3-10 s each on SMP=1) ---
    ("mprotectlb", "mprotectlb", "0 divergence(s) from Linux"),
    ("eager_mprotect", "eager_mprotect_probe", "RESULT: PASS"),
    ("pthread_kill_eintr", "pthread_kill_eintr", "RESULT: PASS"),
    ("fpfault", "fpfault /bin/busybox", "fpfault: done, 0/"),
    ("neonfault", "neonfault /bin/busybox", "neonfault: done, 0/"),
    ("mmapsum", "mmapsum /bin/busybox", digests_agree),
    ("mmap_file", "mmap_file /bin/busybox", "touched all pages"),
    ("allocstress", "allocstress", "allocations without failure!"),
    ("stackstress", "stackstress", "stackstress: PASSED"),
]

# Exercises that FAIL on an unmodified tree today. Reported as `KNOWN-FAIL` so a
# real regression elsewhere still reads as `UNEXPECTED`, and so a run where one
# starts passing says so loudly instead of looking identical to every other run.
#
# **Empty since 2026-09-01.** `eager_mprotect` was the only entry, and it flipped
# to passing — which the old comment here called out in advance as "a result",
# so this is that result being collected rather than the list being tidied.
#
# It failed from 2026-08-15 (24f7e1c1) with both phases reporting "write
# succeeded, no SIGSEGV — mprotect was defeated": the `[EAGER-UPGRADE]`
# fault-handler repair was firing on a region `mprotect` had downgraded
# (J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md §3, §6a). It now
# refuses the write, which is what `akuma-mmap`'s `prot_recorded` +
# `mprotect_eager_regions_in_range` were built to do
# (GRANT_RECORDS_VS_DENY_RECORDS.md).
#
# Verified as a real pass, not a silent one, before removing it — the boot log
# carries the positive evidence at both SMP levels:
#
#     [MPROTECT-DENY] pid=531 va=0x10422000 write refused by recorded protection
#     [WPF] ... ap_rw=false ...
#     [Fault] Process 531 (/bin/eager_mprotect_probe) SIGSEGV after 0.00s
#
# i.e. the write faulted and the process died, which is the probe's success
# condition. Both phases (pids 530 and 531) did so, on both arms of an A/B —
# so this is a landed fix, not a change under test.
#
# If an entry is ever added back, it must carry the same three things this one
# did: what fails, the measurement that established it, and what its flipping
# would mean.
KNOWN_FAIL_EXERCISES: set[str] = set()

# Known-flaky, threshold-driven: fails on an unmodified tree, and passes on one
# too. Compared separately so the failure SET stays meaningful.
FLAKY_BOOT_TESTS = {"retired_reclaim_ab"}

# Tier 4 (opt-in, `--tier 4`): the only exercise that puts sustained pressure on
# `alloc_page_zeroed_user` itself rather than on fork/CoW, and it verifies the bytes
# it writes — so a page the fault path filled from the wrong frame is a reported
# error, not a silent pass. Run it for PMM / fault-path / reclaim changes.
#
# It needs the devbox image (redis arrives via `apk add`, so a failure can be a
# NETWORKING failure) and it is NOT part of `--tier all`: it builds and boots a
# different profile, which would make the default summary un-diffable against a
# baseline taken without it. `--test-memory` exits before normal startup, which is why
# it works while `redis-server` proper still blocks on the empty /proc/<pid>/
# (LONG_ROAD_TO_REDIS.md) — a pass here says nothing about redis as a server.
REDIS_MEMTEST_MB = 512
REDIS_HEALTHY = "Your memory passed this test"


def sh(cmd, cwd=REPO, timeout=1800, env=None):
    e = os.environ.copy()
    if env:
        e.update(env)
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                          timeout=timeout, env=e)


def devbox_rump_features():
    """Scrape DEVBOX_FEATURES rather than duplicating the list here."""
    path = os.path.join(REPO, "scripts", "build_devbox.sh")
    with open(path) as f:
        m = re.search(r'DEVBOX_FEATURES="([^"]*)"', f.read())
    return m.group(1) if m else None


def tier1_clippy(results):
    """All four configurations. Three of them compile files the default does not."""
    # Defeat the cache so a clean result means something was actually checked.
    for p in ("src/main.rs", "src/exceptions.rs", "crates/akuma-exec/src/lib.rs"):
        fp = os.path.join(REPO, p)
        if os.path.exists(fp):
            os.utime(fp, None)

    for name, args in CLIPPY_CONFIGS:
        if args is None:
            feats = devbox_rump_features()
            if not feats:
                results[f"clippy.{name}"] = "SKIP (no DEVBOX_FEATURES found)"
                continue
            args = ["--release", "--no-default-features", "--features", feats]
        r = sh(["cargo", "clippy"] + args)
        # Judge on exit status + real diagnostic lines, not a grep of the prose.
        diags = [ln for ln in r.stderr.splitlines()
                 if ln.startswith("warning:") or ln.startswith("error")]
        ok = r.returncode == 0 and not diags
        results[f"clippy.{name}"] = "clean" if ok else f"{len(diags)} diag, rc={r.returncode}"
        if diags:
            results[f"clippy.{name}.first"] = diags[0][:120]


def tier1_tests(results, logdir="/tmp"):
    host = sh(["rustc", "-vV"]).stdout
    triple = next(l.split()[1] for l in host.splitlines() if l.startswith("host:"))
    results["host.triple"] = triple

    r = sh(["cargo", "test", "--target", triple], timeout=2400)
    out = r.stdout + r.stderr
    total = 0
    failed = 0
    for line in out.splitlines():
        m = re.match(r"^test result: (\w+)\. (\d+) passed; (\d+) failed", line)
        if m:
            total += int(m.group(2))
            failed += int(m.group(3))
    results["host.tests"] = total
    results["host.failed"] = failed

    # Name the failures, and keep the raw output.
    #
    # This block exists because the counts alone were useless the one time they
    # mattered: a 2026-08-14 run reported `host.failed: 1` with the total down
    # 533 -> 430, and the output was already gone — so which test failed, and
    # which binary aborted (an aborted binary prints no `test result:` line at
    # all, which is what a 100-test drop looks like), could not be answered. It
    # never reproduced in four re-runs. The runbook had listed saving this output
    # as the change that "would turn this from noise into a finding"; a
    # non-reproducing failure with no name attached is the worst of both.
    #
    # Written unconditionally, not only on failure: a *passing* run's file is the
    # baseline you diff the next failure against.
    names = re.findall(r"^(\S+) stdout ----$|^test (\S+) \.\.\. FAILED$", out, re.M)
    failed_names = sorted({a or b for a, b in names if (a or b)})
    if failed_names:
        results["host.failed_names"] = ",".join(failed_names)
    path = os.path.join(logdir, "verify_host_tests.log")
    try:
        with open(path, "w") as f:
            f.write(out)
        if failed:
            results["host.output"] = path
    except OSError:
        pass  # never let logging break the gate


def wait_for_marker(log_path, timeout=480, port=None, proc=None):
    """Readiness is an **ssh round-trip**, not a log grep. See `vm_ready.py`.

    The log-marker check this used to rely on is wrong in both directions: at
    SMP>1 the line arrives torn across cores (`[herd] Starting service: sshd` /
    `sshd (pid= 2)`, observed 2026-08-16), and some builds never print either
    spelling at all — measured 2026-08-28, a VM served ssh for 570 s of guest
    uptime with zero marker matches, so a 10-minute wait timed out against a
    healthy VM. The marker also never expires, so a stale log reads as ready.

    An ssh round-trip cannot be torn by another core's printf and tests the thing
    the gate needs: that the guest answers commands. The log marker is kept only
    as a fallback for the `booted: False` diagnostics path, which must still say
    something useful when ssh never comes up at all.
    """
    if port is not None:
        if vm_ready.wait_ready(port=port, timeout=timeout, proc=proc):
            return True
        # Fall through: ssh never answered. Say whether the guest got as far as
        # printing a marker, because "booted but unreachable" and "never booted"
        # are different bugs.
    marker = re.compile(rb"Started sshd|sshd started|sshd \(pid=")
    deadline = time.time() + (15 if port is not None else timeout)
    while time.time() < deadline:
        try:
            with open(log_path, "rb") as f:
                if marker.search(f.read()):
                    return True
        except FileNotFoundError:
            pass
        time.sleep(3)
    return False


def read_log(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", errors="replace")


def boot_once(smp, instance, memory, logdir, results, run_exercises):
    # Kill only THIS instance's stale QEMU, matched on its own forwarded ssh port.
    # A bare `pkill -f qemu-system-aarch64` — what this line used to be — also kills
    # every other VM the user has running, including a concurrent baseline run of
    # this very script. CLAUDE.md § "Waiting for a VM" bans the broad form for
    # exactly that reason.
    port = 2222 + 100 * (instance or 0)
    subprocess.run(["pkill", "-f", f"hostfwd=tcp::{port}-:22"], capture_output=True)
    time.sleep(2)

    log_path = os.path.join(logdir, f"verify_smp{smp}.log")
    log = open(log_path, "w")
    env = os.environ.copy()
    env.update({"SMP": str(smp), "MEMORY": str(memory)})
    if instance:
        env["INSTANCE"] = str(instance)
        # Snapshot mode is implied for INSTANCE>0; point at the MAIN worktree's disk
        # (see MAIN_REPO -- a linked baseline worktree has no disk.img of its own).
        env.setdefault("DISK", os.path.join(MAIN_REPO, "disk.img"))
    # Fail loudly instead of letting QEMU exit on a missing disk, which surfaces here
    # as `booted: False` and reads as a broken commit rather than a missing file.
    disk = env.get("DISK", os.path.join(REPO, "disk.img"))
    if not os.path.exists(disk):
        results[f"smp{smp}.booted"] = f"ERROR no disk image at {disk}"
        return
    vm = subprocess.Popen(["cargo", "run", "--release"], cwd=REPO, env=env,
                          stdout=log, stderr=subprocess.STDOUT)
    try:
        booted = wait_for_marker(log_path, port=port, proc=vm)
        results[f"smp{smp}.booted"] = booted
        if not booted:
            # A failed boot is the case that most needs triage, so report the three
            # things that distinguish its causes instead of just `booted: False`:
            # how far the self-test suite got, whether the kernel HALTED (a specific
            # fatal, not a timeout), and whether the run lost guest time.
            text = read_log(log_path)
            results[f"smp{smp}.pass_marker"] = len(re.findall(r"\[PASS\]", text))
            results[f"smp{smp}.host_timejumps"] = len(
                re.findall(r"Time jump detected", text))
            # `[SGI-S FATAL] new_sp=0` is a KNOWN, timing-dependent boot-suite race,
            # not necessarily your change: several tests fabricate a bare claimed
            # thread slot into READY/WAITING, and a slot with context sp=0 that the
            # scheduler dispatches halts the kernel (src/process_tests.rs:10489).
            # Observed 2026-08-13 at BOTH SMP levels in one loaded-host run and in
            # neither of two clean re-boots of the same tree. Re-run before debugging.
            halt = re.search(r"\[(SGI-S FATAL|PANIC|Kernel panic)\][^\n]{0,80}", text)
            if halt:
                results[f"smp{smp}.halt"] = halt.group(0)[:110]
            return

        text = read_log(log_path)
        results[f"smp{smp}.pass_marker"] = len(re.findall(r"\[PASS\]", text))
        results[f"smp{smp}.passed_marker"] = len(re.findall(r"PASSED", text))

        # Two failure formats exist and BOTH must land in fail_set: `[FAIL] name`
        # and `[Test] name FAILED: reason`. The second was invisible here until
        # 2026-08-14, when a genuinely failing [Test]-format test sailed through a
        # gate run as fail_set=(empty) — its only trace a passed_marker one lower,
        # which the runbook documents as tolerable ±1 flake. A failing test must
        # never be distinguishable from a flake by design.
        fails = set(re.findall(r"\[FAIL\] ([a-z_0-9]+)", text))
        fails |= set(re.findall(r"\[Test\] ([A-Za-z_0-9]+) FAILED", text))
        results[f"smp{smp}.fail_set"] = ",".join(sorted(fails - FLAKY_BOOT_TESTS)) or "(empty)"
        results[f"smp{smp}.flaky_seen"] = ",".join(sorted(fails & FLAKY_BOOT_TESTS)) or "(none)"

        # INFORMATIONAL, not an equality metric: this is load-driven and drifts
        # run to run on an unchanged tree (measured 93, 96, 109 at SMP=4 on the
        # same commit). A real storm is thousands of lines, not tens — compare
        # orders of magnitude, never exact counts.
        results[f"smp{smp}.bkl_stuck"] = len(re.findall(r"\[BKL\] stuck", text))
        # Spurious at boot on SMP>1 (DEVBOX_ISSUES.md Issue 11), plus one
        # deliberate one from stack_canary_overrun_is_reported.
        results[f"smp{smp}.stack_overflow"] = len(re.findall(r"\[STACK-OVERFLOW\]", text))
        m = re.search(r"\[FPCACHE\] entries=(\d+) hits=(\d+) misses=(\d+)", text)
        if m:
            # hits=0 with entries=0 means page sharing silently stopped — a
            # mis-wired SHARED_FILE_PAGES_ENABLED looks fine otherwise.
            # NOTE: `[FPCACHE]` is periodic and this snapshot is taken at the sshd
            # marker, so whether the line has been printed yet is a race — the key is
            # absent from some runs on an unchanged tree. Compare entries=/misses=;
            # `hits` is a monotonic counter read at an arbitrary instant.
            results[f"smp{smp}.fpcache"] = f"entries={m.group(1)} hits>0={int(m.group(2)) > 0}"

        if run_exercises:
            port = 2222 + (100 * instance if instance else 0)
            exercise_suite(port, smp, results)

        # Re-read AFTER the exercises: lost time accrues while they run, so counting it
        # from the boot snapshot above would report 0 for exactly the runs it must flag.
        #
        # The watchdog reports a ~100 ms jump for time the guest did not account for.
        # Its "(host sleep/wake)" text is the kernel's GUESS, not a measurement, and both
        # causes are real: the host descheduling QEMU (a background cargo/rust-analyzer
        # rebuild is enough — including this gate's own Tier 1 right before Tier 2), and
        # the guest losing time itself. A high count means this run's timing is
        # untrustworthy; it does not name a cause, and it does not explain a failure:
        # measured 2026-08-13, a clean tree scored 741 and passed every exercise, while
        # a run with 2866 saw `cowstale` TIMEOUT on a tree where it passes.
        #
        # It is also NOT the tell for the intermittent SMP=1 suite wedge
        # (`COW_PILE_AUDIT.md` §9 F8) — that has been observed with a count of ZERO on an
        # idle host. The tell for the wedge is the log simply stopping.
        results[f"smp{smp}.host_timejumps"] = len(
            re.findall(r"Time jump detected", read_log(log_path)))
    finally:
        vm.terminate()
        try:
            vm.wait(timeout=10)
        except subprocess.TimeoutExpired:
            vm.kill()
        log.close()
        # Narrow, for the same reason as the kill at the top of this function.
        subprocess.run(["pkill", "-f", f"hostfwd=tcp::{port}-:22"], capture_output=True)
        time.sleep(1)


def ssh(port, cmd, timeout=120):
    r = subprocess.run(
        ["ssh", "-q", "-o", "StrictHostKeyChecking=no",
         "-o", "UserKnownHostsFile=/dev/null", "-p", str(port),
         "root@localhost", cmd],
        capture_output=True, timeout=timeout)
    # stdout ALONE: an ssh banner folded into a stdout parse was one of the three
    # false "findings" the runbook was written after.
    return r.returncode, r.stdout.decode("utf-8", errors="replace")


def exercise_suite(port, smp, results):
    """CoW / fork / ELF binaries over ssh.

    Each one runs **detached under `nohup`** with output to a file, then is polled.
    Running them as a plain `ssh <cmd>` does not work for the long ones: sshd's
    keepalive kills a long-lived exec channel, the client returns rc=255, and the
    result looks exactly like a failing binary. `cowstale` (which does ~700M
    reader checks) hit this and was reported as UNEXPECTED on a tree where it
    passes — the measurement, not the code.

    Polling reads the output file and looks for a sentinel the shell appends after
    the binary exits. Do NOT poll with `pgrep <name>`: the ssh command line
    contains the name, so pgrep matches itself and the job looks eternal.

    The redirect is `rm -f` + `>>` (append), NOT `>`, and that is a kernel bug
    workaround rather than a style choice. In `{ probe; echo SENTINEL; } > file`
    the two child processes inherit ONE fd and must share ONE file offset; Akuma
    gives each its own, so `echo` writes its 12 bytes at offset 0 and destroys
    the first 12 bytes of the probe's output. Measured 2026-08-15:
    `{ /bin/echo AAAA…(24); /bin/echo BBB; } > f` yields `BBB\\nAAAA…`, while the
    same line with `>>` yields the correct `AAAA…\\nBBB`. It went unnoticed for as
    long as it did because every marker in the original EXERCISES list sits at the
    END of the output; `mmapsum`'s `read:` digest is on line 1 and would have been
    silently truncated to nonsense. O_APPEND writes always go to EOF, which is why
    `>>` sidesteps the shared-offset path entirely."""
    for name, cmd, healthy in EXERCISES:
        key = f"smp{smp}.ex.{name}"
        out_path = f"/tmp/verify_ex_{name}.log"
        try:
            # `{ cmd; echo SENTINEL; }` — the sentinel lands after the binary exits
            # whatever its exit status, so polling terminates on failure too.
            ssh(port, f"rm -f {out_path}; "
                      f"nohup sh -c '{{ {cmd}; echo __EX_DONE__; }} >> {out_path} 2>&1' "
                      f"> /dev/null 2>&1 &")
            deadline = time.time() + 420
            out = ""
            while time.time() < deadline:
                time.sleep(5)
                _, out = ssh(port, f"cat {out_path} 2>/dev/null")
                if "__EX_DONE__" in out:
                    break
            if "__EX_DONE__" not in out:
                # A bare TIMEOUT cannot be acted on: "the kernel failed this
                # probe" and "the kernel wedged" read identically, and the
                # difference decides whether you debug the change or the
                # scheduler. Report how far the probe got, so the summary says
                # which. Measured 2026-08-28: `pthread_kill_eintr` failed PHASE1
                # and then wedged in PHASE2's `pthread_join`, so the gate said
                # only TIMEOUT while the boot log had both facts
                # (PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md § "Re-verified").
                # Probes should also self-terminate — that probe now carries a
                # watchdog — but the gate must not depend on every probe being
                # well-behaved to produce a usable reading.
                results[key] = "TIMEOUT (still running after 420s)"
                lines = [l for l in out.splitlines()
                         if l.strip() and "__EX_DONE__" not in l]
                results[key + ".tail"] = (
                    f"{len(lines)} line(s) before the timeout; last: {lines[-1][:80]}"
                    if lines else "(no output at all — probe never printed)")
                continue
            # `healthy` is a substring for most probes and a predicate for the
            # ones whose verdict is a relation between output lines rather than a
            # line of its own (`mmapsum`).
            ok = healthy(out) if callable(healthy) else healthy in out
            if name in KNOWN_FAIL_EXERCISES:
                # Never just "UNEXPECTED": that word has to keep meaning
                # "regression". And never silently swallowed either — a
                # known-fail that starts passing is the whole reason to run it.
                results[key] = ("ok (KNOWN-FAIL now passes — drop it from "
                                "KNOWN_FAIL_EXERCISES)" if ok else "KNOWN-FAIL (expected)")
            else:
                results[key] = "ok" if ok else "UNEXPECTED"
            if not ok:
                # Kept for known-fails too: the tail is how you see that a
                # known failure still fails the SAME way.
                lines = [l for l in out.splitlines() if l.strip() and "__EX_DONE__" not in l]
                results[key + ".tail"] = (lines[-1] if lines else "(no output)")[:110]
        except subprocess.TimeoutExpired:
            results[key] = "TIMEOUT (ssh)"


def tier4_redis_memtest(results, memory, logdir, build):
    """`redis-server --test-memory` on devbox-smoltcp. See REDIS_MEMTEST_MB above.

    Deliberately reports WHICH stage failed rather than a bare pass/fail: an `apk add`
    that could not resolve DNS, a full devbox.img, an OOM kill and a genuine memory
    error all look like "the memtest didn't say the magic words" from the outside, and
    only one of them is a finding about this change."""
    if build:
        r = sh([os.path.join(REPO, "scripts", "build_devbox_smoltcp.sh")], timeout=3600)
        if r.returncode != 0:
            results["redis.stage"] = "BUILD FAILED"
            results["redis.detail"] = (r.stderr.strip().splitlines() or ["(no stderr)"])[-1][:110]
            return

    disk = os.path.join(REPO, "devbox.img")
    if not os.path.exists(disk):
        results["redis.stage"] = "SKIP (no devbox.img — overlays/devbox/bootstrap.sh)"
        return

    # Narrow: the devbox runner forwards 2222, so match that and leave any other
    # INSTANCE's VM — or a concurrent baseline run — alone.
    subprocess.run(["pkill", "-f", "hostfwd=tcp::2222-:22"], capture_output=True)
    time.sleep(2)
    log_path = os.path.join(logdir, "verify_redis.log")
    log = open(log_path, "w")
    env = os.environ.copy()
    env.update({"DEVBOX_MEMORY": str(memory), "DEVBOX_DISK": disk})
    vm = subprocess.Popen([os.path.join(REPO, "overlays", "devbox", "run-smoltcp.sh")],
                          cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        # `port` must be bound BEFORE the readiness wait, and the process handle is
        # `vm` — this line read `port=port, proc=qemu` until 2026-08-30, so tier 4
        # died with `UnboundLocalError` before ever booting anything. It is opt-in
        # (not part of `--tier all`), which is how it stayed broken.
        port = 2222
        if not wait_for_marker(log_path, port=port, proc=vm):
            results["redis.stage"] = "BOOT FAILED"
            return

        # devbox.img fills across sessions and ENOSPC surfaces as an `apk add` network
        # error, so record the free space rather than discovering it the hard way.
        # Pick the row carrying an actual NN% field, not `tail -1` and not merely a
        # line containing "%": busybox wraps long device names onto a second line so
        # the last line is not stably the data row, and the header's own "Use%" matches
        # a bare "%" test — which is how `tail -1` reported the HEADER once here.
        _, df = ssh(port, "busybox df -h /")
        row = next((l for l in reversed(df.splitlines())
                    if any(re.fullmatch(r"\d+%", t) for t in l.split())), "")
        results["redis.disk"] = " ".join(row.split()[-4:]) if row else "(unknown — df gave no data row)"

        if ssh(port, "command -v redis-server")[0] != 0:
            rc, out = ssh(port, "apk add redis", timeout=600)
            if ssh(port, "command -v redis-server")[0] != 0:
                results["redis.stage"] = "APK FAILED (networking or full disk, not the memory path)"
                results["redis.detail"] = (out.strip().splitlines() or ["(no output)"])[-1][:110]
                return

        out_path = "/tmp/verify_redis_memtest.log"
        ssh(port, f"nohup sh -c '{{ redis-server --test-memory {REDIS_MEMTEST_MB}; "
                  f"echo __EX_DONE__; }} > {out_path} 2>&1' > /dev/null 2>&1 &")
        deadline = time.time() + 900
        out = ""
        while time.time() < deadline:
            time.sleep(10)
            _, out = ssh(port, f"cat {out_path} 2>/dev/null")
            if "__EX_DONE__" in out:
                break

        if "__EX_DONE__" not in out:
            results["redis.stage"] = "TIMEOUT (still running after 900s)"
        elif REDIS_HEALTHY in out:
            results["redis.stage"] = "ok"
        elif "MEMORY ERROR DETECTED" in out:
            # The outcome this tier exists to catch: the fault path served a wrong or
            # stale frame. A data bug, not an OOM.
            results["redis.stage"] = "MEMORY ERROR DETECTED"
        else:
            # Most likely the escalation gave up and SIGSEGV'd it — check the boot log
            # for `[Fault] Process N (redis-server) SIGSEGV` and compare free pages
            # against the baseline before calling it a regression.
            results["redis.stage"] = "UNEXPECTED"
        if results["redis.stage"] != "ok":
            lines = [l for l in out.splitlines() if l.strip() and "__EX_DONE__" not in l]
            results["redis.detail"] = (lines[-1] if lines else "(no output)")[:110]

        boot = read_log(log_path)
        results["redis.vm_sigsegv"] = len(re.findall(r"\[Fault\] Process \d+ \(redis", boot))
        results["redis.timejumps"] = len(re.findall(r"Time jump detected", boot))
    finally:
        vm.terminate()
        try:
            vm.wait(timeout=10)
        except subprocess.TimeoutExpired:
            vm.kill()
        log.close()
        subprocess.run(["pkill", "-f", "hostfwd=tcp::2222-:22"], capture_output=True)
        time.sleep(1)


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tier", choices=["1", "2", "4", "all"], default="all",
                    help="1 = host only (~2 min), 2 = boot + exercises, all = both. "
                         "4 = the redis memtest on devbox-smoltcp (opt-in, for PMM / "
                         "fault-path / reclaim changes); NOT included in 'all' because "
                         "it builds and boots a different profile")
    ap.add_argument("--no-devbox-build", action="store_true",
                    help="tier 4 only: reuse the existing devbox-smoltcp kernel image "
                         "instead of rebuilding it")
    ap.add_argument("--smp", default="1,4",
                    help="comma-separated SMP levels to boot (default 1,4)")
    ap.add_argument("--memory", default="2048")
    ap.add_argument("--instance", type=int, default=0,
                    help="QEMU instance; >0 shifts ports and snapshots the disk, "
                         "so a baseline worktree can run without touching disk.img")
    ap.add_argument("--no-exercises", action="store_true",
                    help="skip the ssh binaries (boot markers only)")
    ap.add_argument("--out", help="also write the summary block to this file")
    args = ap.parse_args()

    logdir = os.environ.get("VERIFY_LOGDIR", "/tmp")
    os.makedirs(logdir, exist_ok=True)

    results = {}
    head = sh(["git", "rev-parse", "--short", "HEAD"]).stdout.strip()
    dirty = bool(sh(["git", "status", "--porcelain", "--", "src", "crates"]).stdout.strip())
    results["commit"] = head + ("+dirty" if dirty else "")

    t0 = time.time()
    if args.tier in ("1", "all"):
        tier1_clippy(results)
        tier1_tests(results, logdir)
    if args.tier in ("2", "all"):
        for smp in [int(s) for s in args.smp.split(",") if s.strip()]:
            boot_once(smp, args.instance, args.memory, logdir, results,
                      not args.no_exercises)
    if args.tier == "4":
        tier4_redis_memtest(results, args.memory, logdir, not args.no_devbox_build)
    results["elapsed_s"] = int(time.time() - t0)

    lines = ["=== SUMMARY ==="]
    # `commit` and `elapsed_s` last: they differ between any two runs by design,
    # so keeping them out of the sorted body keeps a diff to real changes.
    body = {k: v for k, v in results.items() if k not in ("commit", "elapsed_s")}
    for k in sorted(body):
        lines.append(f"{k}: {body[k]}")
    lines.append(f"# commit: {results['commit']}  elapsed: {results['elapsed_s']}s")
    block = "\n".join(lines)
    print(block)
    if args.out:
        with open(args.out, "w") as f:
            f.write(block + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
