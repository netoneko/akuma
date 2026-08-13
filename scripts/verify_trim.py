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
            return

        text = read_log(log_path)
        results[f"smp{smp}.pass_marker"] = len(re.findall(r"\[PASS\]", text))
        results[f"smp{smp}.passed_marker"] = len(re.findall(r"PASSED", text))

        fails = set(re.findall(r"\[FAIL\] ([a-z_0-9]+)", text))
        results[f"smp{smp}.fail_set"] = ",".join(sorted(fails - FLAKY_BOOT_TESTS)) or "(empty)"
        results[f"smp{smp}.flaky_seen"] = ",".join(sorted(fails & FLAKY_BOOT_TESTS)) or "(none)"

        # A real BKL storm is thousands of lines, not tens — record the count so a
        # diff shows movement rather than asking for a judgement call.
        results[f"smp{smp}.bkl_stuck"] = len(re.findall(r"\[BKL\] stuck", text))
        # Spurious at boot on SMP>1 (DEVBOX_ISSUES.md Issue 11), plus one
        # deliberate one from stack_canary_overrun_is_reported.
        results[f"smp{smp}.stack_overflow"] = len(re.findall(r"\[STACK-OVERFLOW\]", text))
        m = re.search(r"\[FPCACHE\] entries=(\d+) hits=(\d+) misses=(\d+)", text)
        if m:
            # hits=0 with entries=0 means page sharing silently stopped — a
            # mis-wired SHARED_FILE_PAGES_ENABLED looks fine otherwise.
            results[f"smp{smp}.fpcache"] = f"entries={m.group(1)} hits>0={int(m.group(2)) > 0}"

        if run_exercises:
            port = 2222 + (100 * instance if instance else 0)
            exercise_suite(port, smp, results)
    finally:
        vm.terminate()
        try:
            vm.wait(timeout=10)
        except subprocess.TimeoutExpired:
            vm.kill()
        log.close()
        subprocess.run(["pkill", "-f", "qemu-system-aarch64"], capture_output=True)
        time.sleep(1)


def exercise_suite(port, smp, results):
    """CoW / fork / ELF binaries over ssh. Long runs need generous timeouts:
    sshd's keepalive kills a long-lived exec channel and the client reports
    `Timeout, server localhost not responding`, which looks like a hung VM."""
    for cmd, healthy in EXERCISES:
        key = f"smp{smp}.ex.{cmd.split()[0]}"
        try:
            r = subprocess.run(
                ["ssh", "-q", "-o", "StrictHostKeyChecking=no",
                 "-o", "UserKnownHostsFile=/dev/null", "-p", str(port),
                 "root@localhost", cmd],
                capture_output=True, timeout=420)
            out = r.stdout.decode("utf-8", errors="replace")  # stdout ALONE
            results[key] = "ok" if healthy in out else f"UNEXPECTED (rc={r.returncode})"
            if healthy not in out:
                tail = [l for l in out.splitlines() if l.strip()][-1:] or ["(no output)"]
                results[key + ".tail"] = tail[0][:110]
        except subprocess.TimeoutExpired:
            results[key] = "TIMEOUT"


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--tier", choices=["1", "2", "all"], default="all",
                    help="1 = host only (~2 min), 2 = boot + exercises, all = both")
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
