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
  scripts/mem_suite.py --port 2244 --arch x86_64   # the amd64 guest
  scripts/mem_suite.py --docker --arch x86_64      # real Linux, the calibration arm

The probes are neutral C, so `--arch` picks only the cross compiler and the
output subdirectory. For the amd64 kernel, boot a guest with sshd on the
forwarded port first:

  SMP=1 SSH_PORT=2244 INIT=/bin/sshd sh amd64/run.sh

Exit status is 0 only if every selected probe passed, so it can gate an A/B arm.
"""
import argparse
import base64
import json
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

SRC = pathlib.Path(__file__).resolve().parent.parent / "userspace/forktest/c_stress"

# Which cross compiler builds the probes, and where the binary for that
# architecture is written.
#
# The probes are **architecture-neutral C** — they call `mmap`, `mprotect`,
# `madvise` and read `/proc/self/smaps`, and not one of them contains an `asm`
# block, a page-size constant or a syscall number. So the only thing that was
# aarch64-specific here was the compiler name, and the amd64 kernel gained an
# `mmap` region table on 2026-09-07 that wants exactly this gate
# (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md` item B1/B2).
#
# The binaries land in per-architecture subdirectories rather than beside the
# sources: two arches writing `c_stress/mmapsum` in turn is a stale-artifact
# trap, and the shape of it — an arm silently running the *other* arch's binary,
# or a build that "succeeded" because the previous one is still there — is one
# this tree has paid for before (`docs/archive/AB_STALE_BAKED_ARTIFACTS.md`).
ARCHES = {
    "aarch64": "aarch64-linux-musl-gcc",
    "x86_64": "x86_64-linux-musl-gcc",
}

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

# Extra `ssh` arguments, filled in from `--identity`. The devbox accepts any key
# the agent offers; the amd64 image stages exactly one public key into
# `/etc/sshd/authorized_keys` (`amd64/mkdisk.sh`), so that guest needs to be told
# which private key to present.
SSH_EXTRA = []


def ssh(port, cmd, timeout=900):
    """Run `cmd` in the guest; returns its **merged** stdout and stderr.

    The merge happens here rather than as a `2>&1` in the guest command, which
    is what it used to be. That redirect was redundant — both streams are
    concatenated below either way — and it excluded a guest whose shell cannot
    do it: the amd64 image answers `/bin/sh: 1: Bad file descriptor` to any
    `2>&1` and returns 1, so every probe failed before it started.
    """
    try:
        p = subprocess.run(SSH_BASE + SSH_EXTRA + ["-p", str(port), "root@localhost", cmd],
                           capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "", 124
    return p.stdout.decode(errors="replace") + p.stderr.decode(errors="replace"), p.returncode


# Does the guest have a `base64` applet? Probed once per run by `push`.
_HAVE_BASE64 = {}


def push(port, path, dest):
    """Copy one probe binary into the guest and make it executable.

    Base64 where the guest can decode it, raw bytes where it cannot. The amd64
    image's busybox is built without the `base64` applet, and the failure was
    silent in the worst way: `base64 -d > /tmp/x` left a **zero-byte** file
    behind — the shell created the redirect target before discovering the
    command did not exist — so every probe then "ran" and reported nothing.
    A guest with no `base64` is a guest the suite should still work on.

    Raw is safe on both: ssh with no `-t` allocates no pty, so the channel is
    8-bit clean. Verified by md5 across the transfer on the amd64 guest.
    """
    if port not in _HAVE_BASE64:
        out, _rc = ssh(port, "base64 --help >/dev/null 2>/dev/null && echo yes")
        _HAVE_BASE64[port] = "yes" in out
    if _HAVE_BASE64[port]:
        payload, cmd = base64.b64encode(path.read_bytes()), f"base64 -d > {dest}"
    else:
        payload, cmd = path.read_bytes(), f"cat > {dest}"
    subprocess.run(SSH_BASE + SSH_EXTRA + ["-p", str(port), "root@localhost",
                               f"{cmd} && chmod +x {dest}"],
                   input=payload, capture_output=True, timeout=600)


def stage(port):
    """A deterministic file for the two probes that read one.

    Its content must be stable across arms or `mmapsum`'s digests are not
    comparable, so it is a fixed pattern rather than whatever the host happens
    to have lying around.

    **Written from here, not generated in the guest.** It used to be
    `dd if=/dev/zero … | tr '\\0' 'A'`, which needs a `/dev/zero` — and the
    amd64 image has no `/dev` at all
    (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, open issue 2). The file was
    therefore never created there and both probes failed with
    `open/fstat(/tmp/mem_suite_data) failed`, which reads like an mmap defect and
    is not one. Sending the bytes over the same channel the probes arrive on
    depends on nothing in the guest but its shell's `>` redirect.
    """
    subprocess.run(SSH_BASE + SSH_EXTRA + ["-p", str(port), "root@localhost",
                                           "cat > /tmp/mem_suite_data"],
                   input=b"A" * (4096 * 64), capture_output=True, timeout=300)
    ssh(port, "ls -l /tmp/mem_suite_data")


# Docker platform per probe architecture, for the `--docker` calibration arm.
DOCKER_PLATFORM = {"aarch64": "linux/arm64", "x86_64": "linux/amd64"}


def docker_run(arch, outdir, name, args, timeout):
    """Run one probe on **real Linux** in a container, same binary, same args.

    This is the calibration arm, and it is the whole reason these probes are
    worth more than something written fresh for a bug: nearly every one carries a
    `docker run --platform … alpine /<probe>` line in its own header saying what
    a correct kernel prints. Running it from here rather than by hand means the
    Linux answer and the Akuma answer go through **one** `verdict`, so a
    difference in the table is a difference in the kernel and not in how the two
    were scored.

    `alpine` because the probes are static musl binaries: nothing in the image is
    linked against, it is only a filesystem to exec them from.
    """
    plat = DOCKER_PLATFORM[arch]
    binary = probe_binary(outdir, arch, name)
    cmd = ["docker", "run", "--rm", "--platform", plat,
           "-v", f"{binary.resolve()}:/probe:ro"]
    argv = ["/probe"]
    if args.strip():
        # The two file probes read a path; give the container its own copy at a
        # fixed mount point rather than whatever the guest path happened to be.
        data = pathlib.Path(tempfile.gettempdir()) / "mem_suite_data"
        if not data.exists() or data.stat().st_size != 4096 * 64:
            data.write_bytes(b"A" * (4096 * 64))
        cmd += ["-v", f"{data}:/data:ro"]
        argv.append("/data")
    cmd += ["alpine", *argv]
    try:
        p = subprocess.run(cmd, capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "", 124
    return p.stdout.decode(errors="replace") + p.stderr.decode(errors="replace"), p.returncode


def probe_binary(outdir, arch, name):
    """Where this arch's build of `name` is.

    Normally `c_stress/<arch>/<name>`. The fallback exists for `--no-build` on a
    tree from before the per-arch split: aarch64 binaries used to be written
    **beside their sources**, and several are committed there. Falling back for
    aarch64 keeps those usable; not falling back for anything else is the point,
    since running the flat file on another arch would silently push an aarch64
    binary to an x86 guest and score the `Exec format error` as a probe failure.
    """
    per_arch = outdir / name
    if per_arch.exists():
        return per_arch
    flat = SRC / name
    if arch == "aarch64" and flat.exists():
        return flat
    return per_arch


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
    ap.add_argument("--arch", default="aarch64", choices=sorted(ARCHES),
                    help="which guest to build for (default aarch64). The probe "
                         "sources are neutral; this only picks the compiler and "
                         "the output subdirectory.")
    ap.add_argument("--docker", action="store_true",
                    help="run the probes on real Linux in a container instead of "
                         "against a guest — the calibration arm. Needs no --port.")
    ap.add_argument("-i", "--identity",
                    help="ssh private key to present. Needed for the amd64 image, "
                         "which authorises exactly one key "
                         "(target/x86_64-unknown-none/release/amd64-ssh-test-key).")
    a = ap.parse_args()

    if a.identity:
        SSH_EXTRA.extend(["-i", a.identity, "-o", "IdentitiesOnly=yes"])

    selected = list(PROBES)
    if a.only:
        selected = [p.strip() for p in a.only.split(",")]
        unknown = [p for p in selected if p not in PROBES]
        if unknown:
            print(f"unknown probe(s): {', '.join(unknown)}", file=sys.stderr)
            return 2

    cc = ARCHES[a.arch]
    outdir = SRC / a.arch
    if not a.no_build:
        if not shutil.which(cc):
            print(f"{cc} not found — install it, or pass --no-build to reuse "
                  f"what is already in {outdir}", file=sys.stderr)
            return 2
        outdir.mkdir(parents=True, exist_ok=True)
        for name in selected:
            subprocess.run([cc, "-O2", "-static",
                            "-o", str(outdir / name), str(SRC / f"{name}.c")], check=True)
        print(f"built {len(selected)} probe(s) for {a.arch} with {cc}")

    if not a.docker:
        stage(a.port)

    results, failed, total_div = {}, [], 0
    for name in selected:
        args, timeout = PROBES[name]
        if a.docker:
            out, rc = docker_run(a.arch, outdir, name, args, timeout)
        else:
            push(a.port, probe_binary(outdir, a.arch, name), f"/tmp/{name}")
            out, rc = ssh(a.port, f"/tmp/{name} {args}", timeout=timeout)
        ok, why, div = verdict(name, out, rc)
        # Retry ONCE, and only on SILENT. A probe that printed nothing is either
        # dead or the ssh round-trip dropped its output, and those need opposite
        # verdicts — a second attempt is what separates them. Observed 2026-08-29:
        # `smapsdirty` reported SILENT once and then ran clean 3/3 by hand.
        #
        # This does not weaken the no-silent-pass rule: silent twice still fails,
        # and a FAIL or a bad exit code is never retried, so a probe cannot pass by
        # being run until it gets lucky.
        if not ok and why.startswith("SILENT") and not a.docker:
            out, rc = ssh(a.port, f"/tmp/{name} {args}", timeout=timeout)
            ok, why, div = verdict(name, out, rc)
            if ok:
                why += " (first attempt returned nothing — transport, not the probe)"
        total_div += div
        results[name] = {"ok": ok, "why": why, "rc": rc,
                         "diverge": div, "digests": digests(out),
                         "tail": out.strip()[-2000:]}
        if not ok:
            failed.append(name)
        print(f"{'PASS' if ok else 'FAIL'}  {name:<22} {why}")
        if not ok:
            print("\n".join("      | " + l for l in out.strip().splitlines()[-12:]))

    where = "docker/linux" if a.docker else f"guest :{a.port}"
    print(f"\n===== mem_suite ({a.arch}) on {where}: "
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
