#!/usr/bin/env python3
"""Build, push and run the memory-syscall correctness probes against a guest.

The memory family's regression gate. Unlike epoll — whose gate is one probe
(`epollops`) printing PASS/FAIL lines — this family already had ten probes in
`userspace/forktest/c_stress/` and no runner, so each one was invoked by hand and
its verdict read by eye. That is the gap this closes: one command, one exit
status, and a refusal to score a silent probe as a pass.

The ten are heterogeneous by design, because they were written at different
times for different incidents, and normalising them would mean rewriting probes
that currently work:

  mmap_stress           mmap/munmap churn; verdict is the exit code
  mmapsum               digest printer — `madv:` is the MADV_WILLNEED
                        file-corruption regression check (see --json)
  mmap_file             file-backed mmap read-back; verdict is the exit code
  mprotectlb            mprotect + TLB behaviour; prints FAIL lines
  mremapmove            mremap move/grow; prints `ALL PASS` or `N FAILURES`
  madvshared            MADV_DONTNEED on a CoW-shared frame; prints PASS/FAIL
  shmanon               MAP_SHARED|MAP_ANONYMOUS across fork; prints PASS/FAIL
  cowstale              CoW stale-write fault; prints PASS/FAIL
  eager_mprotect_probe  eager-region protection record; prints RESULT/PASS
  smapsdirty            /proc/self/smaps dirty accounting; prints PASS/FAIL

So the verdict is layered rather than one regex:

  1. The probe must have produced output. A probe that died before printing, or
     whose output never came back, is a FAIL — never a pass. This is the property
     `futex_suite.py` has and the reason this file is modelled on it.
  2. A non-zero exit is a FAIL.
  3. Any `FAIL` line in the output is a FAIL, whatever the exit code said.
  4. A `DIVERGE` line is a known, documented difference from Linux (see "Known
     divergences" in docs/reference/subsystems/syscalls/mem.md). It is reported
     and does NOT fail the run — the distinction `epoll_suite.py` introduced, so
     a documented divergence stays green without hiding.

`mmapsum`'s digests are not compared against a hardcoded value on purpose: a
baked-in hash rots the first time a probe or an allocator changes, and then gets
deleted rather than investigated. `--json` writes them out so two arms of an A/B
can be diffed against each other, which is the comparison that actually means
something.

Usage:
  scripts/mem_suite.py --port 2322                 # build, push, run all ten
  scripts/mem_suite.py --port 2322 --no-build      # reuse what is in the tree
  scripts/mem_suite.py --port 2322 --only mmapsum,cowstale
  scripts/mem_suite.py --port 2322 --json out.json # save digests for an A/B diff

Exit status is 0 only if every selected probe passed, so it can gate an A/B arm.
"""
import argparse
import base64
import json
import pathlib
import re
import subprocess
import sys

SRC = pathlib.Path(__file__).resolve().parent.parent / "userspace/forktest/c_stress"

# name -> (args in the guest, seconds). The arg-taking probes get a file staged
# by `stage()`; the rest default their own parameters.
PROBES = {
    "mmap_stress":          ("", 300),
    "mmapsum":              ("/tmp/mem_suite_data", 300),
    "mmap_file":            ("/tmp/mem_suite_data", 300),
    "mprotectlb":           ("", 300),
    "mremapmove":           ("", 300),
    "madvshared":           ("", 300),
    "shmanon":              ("", 300),
    "cowstale":             ("", 420),
    "eager_mprotect_probe": ("", 300),
    "smapsdirty":           ("", 300),
}

SSH_BASE = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
            "-o", "LogLevel=ERROR"]


def ssh(port, cmd, timeout=900):
    try:
        p = subprocess.run(SSH_BASE + ["-p", str(port), "root@localhost", cmd],
                           capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "", 124
    return p.stdout.decode(errors="replace") + p.stderr.decode(errors="replace"), p.returncode


def push(port, path, dest):
    subprocess.run(SSH_BASE + ["-p", str(port), "root@localhost",
                               f"base64 -d > {dest} && chmod +x {dest}"],
                   input=base64.b64encode(path.read_bytes()),
                   capture_output=True, timeout=600)


def stage(port):
    """A deterministic file for the two probes that read one.

    Its content must be stable across arms or `mmapsum`'s digests are not
    comparable, so it is generated in the guest from a fixed pattern rather than
    copied from whatever the host happens to have.
    """
    ssh(port, "dd if=/dev/zero bs=4096 count=64 2>/dev/null | tr '\\0' 'A' "
              "> /tmp/mem_suite_data; ls -l /tmp/mem_suite_data")


def verdict(name, out, rc):
    """See the four layers in the module docstring."""
    # Word-boundary, anywhere on the line: these ten probes disagree about where
    # the verdict sits. `epollops` prints `FAIL <name>`, but `smapsdirty` prints
    # `<name>  FAIL  <detail>` and `mremapmove` prints `N FAILURES` — an anchored
    # pattern silently missed the last two and left the exit code as the only
    # signal, which is exactly the single-point-of-failure this file exists to
    # avoid.
    diverges = len(re.findall(r"\bDIVERGE\b", out))
    fails = len(re.findall(r"\bFAIL(?:URE)?S?\b", out))
    if not out.strip():
        return False, f"SILENT (rc={rc}) — probe printed nothing, not scored as a pass", diverges
    if rc == 124:
        return False, "TIMEOUT — probe did not return", diverges
    if rc != 0:
        return False, f"rc={rc}", diverges
    if fails:
        return False, f"{fails} FAIL line(s)", diverges
    return True, f"ok{f', {diverges} DIVERGE' if diverges else ''}", diverges


def digests(out):
    """`mmapsum`'s `label: hex` lines, for cross-arm comparison."""
    return dict(re.findall(r"^(\w+):\s+([0-9a-f]{8,})\s*$", out, re.M))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=2222)
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--only", help="comma-separated subset of probe names")
    ap.add_argument("--json", help="write per-probe results and digests here")
    a = ap.parse_args()

    selected = list(PROBES)
    if a.only:
        selected = [p.strip() for p in a.only.split(",")]
        unknown = [p for p in selected if p not in PROBES]
        if unknown:
            print(f"unknown probe(s): {', '.join(unknown)}", file=sys.stderr)
            return 2

    if not a.no_build:
        for name in selected:
            subprocess.run(["aarch64-linux-musl-gcc", "-O2", "-static",
                            "-o", str(SRC / name), str(SRC / f"{name}.c")], check=True)
        print(f"built {len(selected)} probe(s)")

    stage(a.port)

    results, failed, total_div = {}, [], 0
    for name in selected:
        args, timeout = PROBES[name]
        push(a.port, SRC / name, f"/tmp/{name}")
        out, rc = ssh(a.port, f"/tmp/{name} {args} 2>&1", timeout=timeout)
        ok, why, div = verdict(name, out, rc)
        total_div += div
        results[name] = {"ok": ok, "why": why, "rc": rc,
                         "diverge": div, "digests": digests(out),
                         "tail": out.strip()[-2000:]}
        if not ok:
            failed.append(name)
        print(f"{'PASS' if ok else 'FAIL'}  {name:<22} {why}")
        if not ok:
            print("\n".join("      | " + l for l in out.strip().splitlines()[-12:]))

    print(f"\n===== mem_suite on guest :{a.port}: "
          f"{'PASS' if not failed else 'FAIL'} "
          f"({len(selected) - len(failed)}/{len(selected)} probes, {total_div} DIVERGE) =====")
    if failed:
        print("failed: " + ", ".join(failed))

    if a.json:
        pathlib.Path(a.json).write_text(json.dumps(results, indent=2))
        print(f"wrote {a.json}")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
