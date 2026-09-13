#!/usr/bin/env python3
"""Run the in-guest kernel build on the amd64 Firecracker guest, and notice a stall.

`proposals/NEXT_AGENT_AMD64_SELFHOST_CARGO.md` § 6 asked for exactly this: every
wrong conclusion in the sessions before it came from getting one of four things
wrong by hand. This does them once, in code:

1. **Detach the build.** A long, silent ssh exec channel is killed, and the build
   dies with it. It runs under `setsid`, writing to files in the guest; nothing
   here holds a channel open for longer than one poll.
2. **Never merge streams in the guest shell.** This image's `/bin/sh` answers
   `Bad file descriptor` to `2>&1` (the trap `scripts/mem_suite.py` records), so
   stdout and stderr go to two files and are read separately.
3. **Count artifacts, not log lines.** `Compiling X` announces a start. Progress
   is `.rlib`/`.rmeta` files under the target directory; a build can print for
   ten minutes and produce nothing.
4. **Never match the bare word `error`.** `thiserror` contains it. Only
   `error[`, `error:` and `warning:`-free failure lines count.

And the thing the doc could not ask for because it had not been needed yet:
**catch the stall while it is happening**. Progress is sampled every `--poll`
seconds; when nothing has moved for `--stall` seconds the harness does not just
report a number, it captures the evidence that decays — the guest's process
list, its memory, the kernel console on the host side, and the tail of both
build streams — and keeps watching. A stall that resolves is reported as a
stall that resolved, with how long it lasted.

Usage:
    scripts/amd64_selfhost_build.py                 # build akuma-amd64, watch
    scripts/amd64_selfhost_build.py --package akuma-mmap --stall 120
    scripts/amd64_selfhost_build.py --attach        # watch a build already running
    scripts/amd64_selfhost_build.py --status        # one snapshot, then exit
"""
import argparse
import pathlib
import re
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent / "utils"))
import hpbox  # noqa: E402

KEY = "/root/akuma/target/x86_64-unknown-none/release/amd64-ssh-test-key"
SSH = ("ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "
       f"-o LogLevel=ERROR -i {KEY} -p 2222 root@10.0.2.15")
CONSOLE = "/root/akuma-fc.log"

SRC = "/src/akuma"
OUT = "/tmp/kbuild.out"
ERR = "/tmp/kbuild.err"
MARK = "/tmp/kbuild.done"
TARGET_DIR = "/tmp/ktarget"

GUEST_ENV = (
    "export HOME=/root CARGO_HOME=/root/.cargo; "
    "export PATH=/usr/local/rust/bin:/usr/libexec/gcc/x86_64-alpine-linux-musl/15.2.0:/usr/bin:/bin; "
    "export LD_LIBRARY_PATH=/usr/local/rust/lib; "
    f"export CARGO_TARGET_DIR={TARGET_DIR}; "
)

# The kernel console lines that mean something went wrong in the guest rather
# than in the build. Each is a symptom this tree has a doc for.
CONSOLE_ALARMS = ("[BKL] stuck", "[PANIC]", "WILD-", "Process table full",
                  "brk #1", "killing the process", "FREED-L0", "[PMM-RESURRECT]")


def guest(cmd, timeout=120):
    """Run one command in the guest. Short, so no channel is ever held open."""
    return hpbox.ubuntu(f'{SSH} "{cmd}"', timeout=timeout)


def guest_script(body, timeout=300):
    """Run a multi-line script in the guest without quoting it twice."""
    hpbox.ubuntu("cat > /root/probes/_kb.sh <<'OUTER'\n" + body + "\nOUTER", timeout=120)
    return hpbox.ubuntu(f"cat /root/probes/_kb.sh | {SSH} 'cat > /_kb.sh; sh /_kb.sh'",
                        timeout=timeout)


def start(package, jobs):
    """Launch the build detached, and return once it is confirmed running."""
    body = f"""#!/bin/sh
{GUEST_ENV}
rm -f {OUT} {ERR} {MARK}
cd {SRC} || exit 1
# setsid: the build must outlive the ssh channel that starts it.
# Two files, never `2>&1` — this shell cannot merge them.
busybox setsid sh -c 'cd {SRC}; {GUEST_ENV} cargo build -p {package} \\
    --target x86_64-unknown-none --release --offline -j{jobs} > {OUT} 2> {ERR}; \\
    echo $? > {MARK}' &
sleep 3
echo started
"""
    guest_script(body)


def snapshot():
    """Everything that says whether the build is moving, in one round trip."""
    body = f"""#!/bin/sh
echo "PHASE=$(cat {MARK} 2>/dev/null || echo running)"
echo "OUT_BYTES=$(busybox wc -c < {OUT} 2>/dev/null || echo 0)"
echo "ERR_BYTES=$(busybox wc -c < {ERR} 2>/dev/null || echo 0)"
echo "ARTIFACTS=$(find {TARGET_DIR} -name '*.rlib' -o -name '*.rmeta' 2>/dev/null | busybox wc -l)"
# Every file, not just finished artifacts: one big crate can hold the artifact
# count still for minutes while `rustc` writes temporaries the whole time, and a
# watcher that cannot tell those apart calls a slow compile a stall.
echo "FILES=$(find {TARGET_DIR} -type f 2>/dev/null | busybox wc -l)"
# WHICH crate is being compiled, from the live process list rather than from the
# log — cargo block-buffers its stderr to a file, so the log is minutes stale.
echo "CRATE=$(ps 2>/dev/null | busybox grep '[r]ustc --crate-name' | busybox head -1 | busybox sed 's/.*--crate-name \\([^ ]*\\).*/\\1/')"
echo "CARGO_PROCS=$(ps 2>/dev/null | busybox grep -c '[c]argo')"
echo "RUSTC_PROCS=$(ps 2>/dev/null | busybox grep -c '[r]ustc')"
echo "MEMFREE=$(busybox grep MemFree /proc/meminfo 2>/dev/null | busybox awk '{{print $2}}')"
echo "LAST_COMPILING=$(busybox grep -a Compiling {ERR} 2>/dev/null | busybox tail -1)"
"""
    _rc, out, _err = guest_script(body, timeout=180)
    fields = {}
    for line in out.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            fields[k.strip()] = v.strip()
    return fields


def vcpu_ticks():
    """CPU ticks the guest's vCPU has burned, read on the **host** side.

    The one liveness signal that cannot be fooled by a slow crate. Inside the
    guest nothing says whether work is happening: `ps` prints `0:00` for every
    process and `/proc/<pid>/stat`'s utime is not tracked on this target, so a
    20-minute `zerocopy` compile and a wedged one look identical from in there —
    no new files, no new log bytes, the same `rustc` pids.

    From out here the difference is obvious: Firecracker's process accumulates a
    second of CPU per second of wall clock while the guest computes, and stops
    when it parks. Measured 2026-09-13 during exactly that confusion: 3057 ticks
    in 30 s — a pegged vCPU — while every in-guest signal said "no change".

    `None` when the process cannot be read; the caller then falls back to the
    in-guest signals rather than calling a missing reading a stall.
    """
    _rc, out, _err = hpbox.ubuntu("pgrep -x firecracker | head -1", timeout=60)
    pid = out.strip().splitlines()[0] if out.strip() else None
    if not pid:
        return None
    _rc, out, _err = hpbox.ubuntu(f"cut -d' ' -f14,15 /proc/{pid}/stat", timeout=60)
    parts = out.split()
    if len(parts) < 2:
        return None
    return int(parts[0]) + int(parts[1])


def build_failed(err_tail):
    """A real cargo failure — never the bare word `error` (`thiserror` has it)."""
    return bool(re.search(r"^error(\[|:)", err_tail, re.M)) or "panicked at" in err_tail


def capture_stall(where, elapsed):
    """The evidence that decays: what is running, what the kernel is saying."""
    print(f"\n{'=' * 72}\nSTALL: no progress for {elapsed:.0f}s ({where})\n{'=' * 72}")
    body = f"""#!/bin/sh
echo "--- guest ps ---"; ps 2>/dev/null | busybox tail -25
echo "--- guest meminfo ---"; busybox head -3 /proc/meminfo 2>/dev/null
echo "--- build stderr tail ---"; busybox tail -12 {ERR} 2>/dev/null
echo "--- build stdout tail ---"; busybox tail -5 {OUT} 2>/dev/null
"""
    _rc, out, _err = guest_script(body, timeout=240)
    print(out)
    # The kernel's own console lives on the Ubuntu side and is the primary witness.
    _rc, con, _err = hpbox.ubuntu(f"busybox tail -40 {CONSOLE} 2>/dev/null || tail -40 {CONSOLE}",
                                  timeout=120)
    print("--- kernel console (host side) ---")
    print(con)
    alarms = [a for a in CONSOLE_ALARMS if a in con]
    if alarms:
        print(f"!! console alarms present: {', '.join(alarms)}")
    return alarms


def watch(poll, stall_after, budget):
    started = time.time()
    last_change = started
    last_key = None
    stalls = 0
    prev_ticks, prev_ticks_at = vcpu_ticks(), time.time()
    while True:
        s = snapshot()
        key = (s.get("ARTIFACTS"), s.get("FILES"), s.get("CRATE"),
               s.get("ERR_BYTES"), s.get("OUT_BYTES"))
        now = time.time()
        moved = key != last_key
        if moved:
            if last_key is not None and now - last_change > stall_after:
                print(f"    …stall of {now - last_change:.0f}s resolved on its own")
            last_key = key
            last_change = now
        # Host-side CPU, as a fraction of one vCPU since the last sample.
        ticks = vcpu_ticks()
        busy = None
        if ticks is not None and prev_ticks is not None and now > prev_ticks_at:
            busy = (ticks - prev_ticks) / 100.0 / (now - prev_ticks_at)
        prev_ticks, prev_ticks_at = ticks, now
        el = now - started
        print(f"[{el:6.0f}s] artifacts={s.get('ARTIFACTS'):>4} files={s.get('FILES'):>5} "
              f"rustc={s.get('RUSTC_PROCS')} free={s.get('MEMFREE')}kB "
              f"crate={s.get('CRATE') or '-':<22}"
              f"vcpu={'?' if busy is None else f'{busy * 100:3.0f}%'}"
              f"{'' if moved else ' (no new files)'}", flush=True)

        phase = s.get("PHASE", "running")
        if phase != "running":
            _rc, err_tail, _e = guest(f"busybox tail -25 {ERR}", timeout=120)
            print(f"\nBUILD EXITED rc={phase} after {el:.0f}s")
            print(err_tail)
            return 0 if phase == "0" else 1

        if not moved and now - last_change > stall_after:
            # A pegged vCPU with no new files is a **slow crate**, not a stall —
            # `zerocopy` holds both still for tens of minutes. Only an idle vCPU
            # with no progress is the thing worth capturing evidence for.
            if busy is not None and busy > 0.5:
                print(f"    (no new files for {now - last_change:.0f}s, but the vCPU is "
                      f"{busy * 100:.0f}% busy — a long compile, not a stall)", flush=True)
                last_change = now
            else:
                stalls += 1
                capture_stall(f"stall #{stalls}", now - last_change)
                last_change = now  # report again only after another stall window

        if el > budget:
            print(f"\nBUDGET REACHED ({budget}s) — build still running, left alive")
            return 2
        time.sleep(poll)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--package", default="akuma-amd64")
    ap.add_argument("--jobs", type=int, default=1)
    ap.add_argument("--poll", type=int, default=30, help="seconds between samples")
    ap.add_argument("--stall", type=int, default=300,
                    help="seconds of no progress that counts as a stall")
    ap.add_argument("--budget", type=int, default=7200, help="give up watching after")
    ap.add_argument("--attach", action="store_true", help="watch a build already running")
    ap.add_argument("--status", action="store_true", help="one snapshot, then exit")
    args = ap.parse_args()

    if args.status:
        for k, v in snapshot().items():
            print(f"{k}={v}")
        return 0
    if not args.attach:
        print(f"starting: cargo build -p {args.package} -j{args.jobs} (detached, in-guest)")
        start(args.package, args.jobs)
    return watch(args.poll, args.stall, args.budget)


if __name__ == "__main__":
    sys.exit(main())
