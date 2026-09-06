#!/usr/bin/env python3
"""Run the amd64 boot suite on local QEMU **and** the trashcan's Firecracker at
the same time, and report both.

# Why parallel

The two are independent machines that share nothing but the source tree, and
each takes minutes: locally a `cargo build --release` plus a TCG boot, remotely
a deploy plus a build on the box plus a KVM boot. Run back to back that is the
sum; run together it is the max, and the max is almost always the local TCG boot
(the box builds and boots under KVM, which is faster than emulating x86 on an
Apple Silicon laptop).

They also fail *differently*, which is the real argument: QEMU/TCG is the
microvm PVH path with virtio-MMIO and slirp; Firecracker on the box is KVM with
a real vCPU and the box's own disk. A change that breaks one and not the other
is the interesting case, and finding that out in one pass instead of two is the
difference between one iteration and two.

# Usage

    python3 scripts/utils/amd64_trials.py                    # both, default init
    python3 scripts/utils/amd64_trials.py --smp 4            # both at SMP=4
    python3 scripts/utils/amd64_trials.py --local-only       # skip the box
    python3 scripts/utils/amd64_trials.py --remote-only      # skip QEMU
    python3 scripts/utils/amd64_trials.py --grep 'block:'    # show matching lines

The remote trial calls `hpbox.deploy()` first, so the box builds *this*
worktree — committed or not. `--no-deploy` builds whatever it already has.

Exit status is 0 only if every trial that ran reported `0 failed`.

# What it does NOT do

It does not reboot the trashcan into Akuma. Firecracker runs on the *Ubuntu*
personality, so this needs no reboot at all and does not disturb whatever is
running — that is the whole reason it is the fast lane. Rebooting to bare metal
is `hpbox.stage()` + `hpbox.reboot_to("akuma")`, and it is a separate, slower
step you take once the fast lane is green. See
`docs/runbooks/amd64-bare-metal-loop.md`.
"""

import argparse
import concurrent.futures as futures
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)

TALLY = re.compile(r"self-test:\s+(\d+)\s+passed,\s+(\d+)\s+(?:failed|FAILED)")


class Trial:
    """One machine's result: its log, and what the suite said about it."""

    def __init__(self, name):
        self.name = name
        self.log = ""
        self.error = None
        self.seconds = 0.0

    @property
    def tally(self):
        """`(passed, failed)` from the last suite line, or `None` if absent.

        The *last* one: a boot that runs the suite twice (some rigs do) should
        be judged on the run that finished.
        """
        found = TALLY.findall(self.log)
        return (int(found[-1][0]), int(found[-1][1])) if found else None

    @property
    def ok(self):
        t = self.tally
        return self.error is None and t is not None and t[1] == 0

    def summary(self):
        if self.error:
            return f"{self.name}: ERROR — {self.error}"
        t = self.tally
        if t is None:
            # Silence is the dangerous answer: a boot that produced no tally
            # hung or died, and saying "no tally" is not the same as passing.
            return f"{self.name}: NO TALLY — the boot produced no self-test line ({self.seconds:.0f}s)"
        return f"{self.name}: {t[0]} passed, {t[1]} failed ({self.seconds:.0f}s)"

    def failures(self):
        return [l for l in self.log.splitlines() if "[FAIL]" in l or l.startswith("  FAILED:")]


def local_qemu(smp, init, initargs, ssh_port, http_port, timeout_s):
    """`amd64/run.sh`, stopped as soon as the suite has reported.

    # Why this streams instead of waiting

    **The guest never exits.** It finishes the suite, runs `init`, and halts with
    `cli; hlt` — so a plain `subprocess.run(timeout=…)` always costs the full
    timeout, whatever the kernel did. The first version of this harness set that
    to 900 s and every local trial took fifteen minutes to report a boot that
    had finished in ninety seconds.

    So: read the console line by line, and stop once the suite's tally has been
    printed *and* the run has gone quiet. The quiet window matters — the tally
    is not the last interesting line (`init` still runs after it), and cutting
    at the tally would truncate exactly the output a `--grep` is usually for.

    Non-default ports on purpose: this laptop frequently has an aarch64 devbox on
    2222, and a `hostfwd` collision makes QEMU fail to start in a way that reads
    exactly like a kernel that produced no output.
    """
    import threading

    t = Trial(f"qemu/tcg smp={smp}")
    env = dict(os.environ, SMP=str(smp), SSH_PORT=str(ssh_port), HTTP_PORT=str(http_port))
    if initargs:
        env["INITARGS"] = initargs
    if init:
        env["INIT"] = init

    # Seconds of silence after the tally before calling it done. Generous
    # against a TCG guest, which can stall for a second between lines under
    # load, and cheap: it is paid once.
    QUIET_AFTER_TALLY = 12.0

    start = time.time()
    proc = None
    lines = []
    try:
        proc = subprocess.Popen(
            ["sh", os.path.join(REPO, "amd64", "run.sh")],
            env=env, cwd=REPO, stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, bufsize=1,
        )

        state = {"tally_at": None}

        def reader():
            for line in proc.stdout:
                lines.append(line)
                if state["tally_at"] is None and TALLY.search(line):
                    state["tally_at"] = time.time()

        th = threading.Thread(target=reader, daemon=True)
        th.start()

        deadline = start + timeout_s
        while time.time() < deadline:
            if proc.poll() is not None:
                break
            seen = state["tally_at"]
            if seen is not None and time.time() - seen > QUIET_AFTER_TALLY:
                break
            time.sleep(0.5)
    except Exception as exc:  # noqa: BLE001 - reported, not swallowed
        t.error = repr(exc)
    finally:
        if proc is not None and proc.poll() is None:
            proc.kill()
        t.log = "".join(lines)
        t.seconds = time.time() - start
        # Kill only our own instance, matched on its own forward. Never
        # `pkill -f qemu-system-x86_64`: other VMs on this laptop are someone
        # else's work.
        subprocess.run(["pkill", "-f", f"hostfwd=tcp::{ssh_port}-"], capture_output=True)
    return t


def remote_firecracker(smp, init, initargs, timeout_s, do_deploy):
    """Push changed files, build on the box, boot under Firecracker."""
    import hpbox

    t = Trial(f"firecracker smp={smp}")
    start = time.time()
    try:
        if do_deploy:
            # `deploy` handles the awkward case by itself: a fix that is neither
            # pushed nor committed still reaches the box, as a reset to the
            # newest pushed commit plus a patch for everything after it.
            rc, msg = hpbox.deploy()
            if rc not in (0, None):
                t.error = f"deploy failed: {msg}"
                return t
            print(f"[trials] {msg}", flush=True)
        _rc, head = hpbox.box_head()
        # Printed, not just recorded: "which system is running" is the first
        # question this loop teaches, and "which commit is it building" is the
        # second. Both have cost hours.
        print(f"[trials] box tree at: {head}", flush=True)
        rc, out = hpbox.build()
        if rc not in (0, None):
            t.error = f"build failed: {out[-800:]}"
            return t
        t.log = hpbox.firecracker(vcpus=smp, init=init or "/bin/busybox",
                                  initargs=initargs or "uname,-a",
                                  timeout_s=timeout_s)
    except Exception as exc:  # noqa: BLE001
        t.error = repr(exc)
    finally:
        t.seconds = time.time() - start
    return t


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("--init", default="")
    ap.add_argument("--initargs", default="uname,-a")
    ap.add_argument("--local-only", action="store_true")
    ap.add_argument("--remote-only", action="store_true")
    ap.add_argument("--ssh-port", type=int, default=2244)
    ap.add_argument("--http-port", type=int, default=8044)
    ap.add_argument("--timeout", type=int, default=420,
                    help="hard bound per trial. The local guest never exits on "
                         "its own, so this is a backstop, not the expected cost — "
                         "it stops when the suite has reported and gone quiet.")
    ap.add_argument("--grep", default="", help="also print log lines matching this")
    ap.add_argument("--no-deploy", action="store_true",
                    help="build whatever the box already has, instead of "
                         "deploying this worktree to it first")
    args = ap.parse_args(argv)

    jobs = []
    with futures.ThreadPoolExecutor(max_workers=2) as pool:
        if not args.remote_only:
            jobs.append(pool.submit(local_qemu, args.smp, args.init, args.initargs,
                                    args.ssh_port, args.http_port, args.timeout))
        if not args.local_only:
            jobs.append(pool.submit(remote_firecracker, args.smp, args.init,
                                    args.initargs, min(args.timeout, 180),
                                    not args.no_deploy))
        trials = [j.result() for j in jobs]

    print()
    for t in trials:
        print(t.summary())
        for line in t.failures():
            print("   ", line.strip())
        if args.grep:
            for line in t.log.splitlines():
                if re.search(args.grep, line):
                    print("   ", line.strip())

    return 0 if all(t.ok for t in trials) else 1


if __name__ == "__main__":
    sys.exit(main())
