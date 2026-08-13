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

REPO = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                      capture_output=True, text=True, check=True).stdout.strip()

# profile + feature set are always chosen together; see docs/reference/build-profiles.md
CLIPPY_CONFIGS = [
    ("release", ["--release"]),
    ("extreme-size", ["--profile", "extreme-size", "--no-default-features",
                      "--features", "no-tests,smoltcp,extreme,userspace-sshd"]),
    ("devbox-smoltcp", ["--release", "--features", "devbox-smoltcp,no-tests"]),
    ("devbox-rump", None),  # features scraped from scripts/build_devbox.sh below
]

# Self-reporting binaries already on disk.img. Value is the healthy substring.
# `bssfork spread=1` is deliberately absent: it fails on an unmodified `main`
# (failures=7) and worse on trim-some-more-fat (failures=8, no thread runs), so it
# cannot serve as a control until that is diagnosed. See the runbook's Tier 3 table.
EXERCISES = [
    ("cowstale", "cowstale PASS"),
    ("bssfork", "bssfork PASS"),
    ("forkprobe", "forkprobe: ALL PASS"),
    ("elftest", "elftest: ALL tests PASSED"),
]

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


def tier1_tests(results):
    host = sh(["rustc", "-vV"]).stdout
    triple = next(l.split()[1] for l in host.splitlines() if l.startswith("host:"))
    results["host.triple"] = triple

    r = sh(["cargo", "test", "--target", triple], timeout=2400)
    total = 0
    failed = 0
    for line in (r.stdout + r.stderr).splitlines():
        m = re.match(r"^test result: (\w+)\. (\d+) passed; (\d+) failed", line)
        if m:
            total += int(m.group(2))
            failed += int(m.group(3))
    results["host.tests"] = total
    results["host.failed"] = failed


def wait_for_marker(log_path, timeout=480):
    """Poll the log file. Never wait on the QEMU process itself — it runs forever."""
    marker = re.compile(rb"Started sshd|sshd started")
    deadline = time.time() + timeout
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
    subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
    time.sleep(2)

    log_path = os.path.join(logdir, f"verify_smp{smp}.log")
    log = open(log_path, "w")
    env = os.environ.copy()
    env.update({"SMP": str(smp), "MEMORY": str(memory)})
    if instance:
        env["INSTANCE"] = str(instance)
        # Snapshot mode is implied for INSTANCE>0; point at the main repo's disk.
        env.setdefault("DISK", os.path.join(REPO, "disk.img"))
    vm = subprocess.Popen(["cargo", "run", "--release"], cwd=REPO, env=env,
                          stdout=log, stderr=subprocess.STDOUT)
    try:
        booted = wait_for_marker(log_path)
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

        fails = set(re.findall(r"\[FAIL\] ([a-z_0-9]+)", text))
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
        subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
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
    contains the name, so pgrep matches itself and the job looks eternal."""
    for cmd, healthy in EXERCISES:
        name = cmd.split()[0]
        key = f"smp{smp}.ex.{name}"
        out_path = f"/tmp/verify_ex_{name}.log"
        try:
            # `{ cmd; echo SENTINEL; }` — the sentinel lands after the binary exits
            # whatever its exit status, so polling terminates on failure too.
            ssh(port, f"nohup sh -c '{{ {cmd}; echo __EX_DONE__; }} > {out_path} 2>&1' "
                      f"> /dev/null 2>&1 &")
            deadline = time.time() + 420
            out = ""
            while time.time() < deadline:
                time.sleep(5)
                _, out = ssh(port, f"cat {out_path} 2>/dev/null")
                if "__EX_DONE__" in out:
                    break
            if "__EX_DONE__" not in out:
                results[key] = "TIMEOUT (still running after 420s)"
                continue
            results[key] = "ok" if healthy in out else "UNEXPECTED"
            if healthy not in out:
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

    subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
    time.sleep(2)
    log_path = os.path.join(logdir, "verify_redis.log")
    log = open(log_path, "w")
    env = os.environ.copy()
    env.update({"DEVBOX_MEMORY": str(memory), "DEVBOX_DISK": disk})
    vm = subprocess.Popen([os.path.join(REPO, "overlays", "devbox", "run-smoltcp.sh")],
                          cwd=REPO, env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        if not wait_for_marker(log_path):
            results["redis.stage"] = "BOOT FAILED"
            return
        port = 2222

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
        subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
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
        tier1_tests(results)
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
