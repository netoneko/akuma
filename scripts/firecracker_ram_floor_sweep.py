#!/usr/bin/env python3
"""RAM/SMP floor finder for an amd64 kernel under Firecracker, driven through `herd`.

Boots a given kernel + ext2 disk under Firecracker with `init=/bin/herd`, stages a
`oneshot` herd service running the workload if the disk doesn't already have one, and
steps RAM down (at fixed SMP) until the workload stops completing cleanly. No stdin
interaction with the guest at any point — just boot, read the console for a fixed time
budget, and look for `[herd] Oneshot service <name> completed` plus the absence of a
`[FATAL]`/panic.

Why herd instead of driving an interactive shell (paws/busybox) over a pipe: three
stacked test-harness bugs, one after another, all looked like kernel regressions before
being root-caused as harness bugs — written up in full in
`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md` ("RAM floor: how low amd64 actually goes",
2026-11-11/12):

1. A marker-substring race — matching `"paws"` against the kernel's own
   `-- running /bin/paws --` line, which prints *before* the real `paws v0.3.0`
   shell-ready banner, so the typed command raced the shell actually being ready.
2. Stray/duplicate `firecracker` processes from earlier manual runs racing the sweep's
   own VM for the shared config/disk files.
3. The real one: writing a command to the guest's stdin, then a *blind* `time.sleep()`
   with zero reads of its stdout. The guest's console output backs up in the unread OS
   pipe; once it backs up enough, firecracker's write blocks, which stalls the guest.
   `select()` saying a fd is "ready" only promises *some* bytes, not a full line, so
   even continuous draining can still hang if the next read is a *blocking* `readline()`
   that waits forever for the rest of a line that never arrives.

herd sidesteps the whole class: there is no typed input to race, backpressure, or block
on — just a console message to wait for.

Usage:
    scripts/firecracker_ram_floor_sweep.py <label> <kernel> <disk.img> <ram_mib,...> \\
        [--command /bin/forktest_parent] [--args "-duration=4s"] [--smp 1,2,4]

Example (amd64 headless, no-tests build, forktest as the workload):
    cargo build -p akuma-amd64 --target x86_64-unknown-none --release \\
        --no-default-features --features no-tests
    scripts/firecracker_ram_floor_sweep.py amd64hl \\
        target/x86_64-unknown-none/release/akuma-amd64 /path/to/disk.img \\
        512,384,256,192,128,96,64,48,40,32,28,24,20,16,12,10,8
"""
import argparse
import fcntl
import json
import os
import select
import subprocess
import sys
import time

SERVICE_NAME = "ram_floor_sweep"


def stage_service(disk, command, args):
    """Write the oneshot herd service config onto `disk` if it's not there already."""
    conf_path = f"etc/herd/enabled/{SERVICE_NAME}.conf"
    check = subprocess.run(["debugfs", "-R", f"cat /{conf_path}", disk],
                            capture_output=True, text=True)
    if "File not found" not in check.stdout + check.stderr:
        return  # already staged (from a previous sweep run)
    conf = f"command = {command}\nargs = {args}\noneshot = true\nrestart = false\n"
    tmp = f"/tmp/{SERVICE_NAME}.conf"
    with open(tmp, "w") as f:
        f.write(conf)
    subprocess.run(["debugfs", "-w", "-R", f"write {tmp} {conf_path}", disk],
                    capture_output=True, text=True, check=True)


def run_cell(label, kernel, disk, smp, mem, vm_json, run_seconds=15.0):
    with open(vm_json, "w") as f:
        json.dump({
            "boot-source": {"kernel_image_path": kernel, "boot_args": "init=/bin/herd"},
            "drives": [{"drive_id": "rootfs", "path_on_host": disk,
                        "is_root_device": False, "is_read_only": False}],
            "network-interfaces": [],
            "machine-config": {"vcpu_count": smp, "mem_size_mib": mem},
        }, f)
    p = subprocess.Popen(
        ["firecracker", "--no-api", "--config-file", os.path.basename(vm_json)],
        cwd=os.path.dirname(vm_json) or ".",
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0,
    )
    fd = p.stdout.fileno()
    fl = fcntl.fcntl(fd, fcntl.F_GETFL)
    fcntl.fcntl(fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)
    buf, out = b"", []
    end = time.time() + run_seconds
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], min(end - time.time(), 0.5))
        if not ready:
            continue
        try:
            chunk = os.read(fd, 65536)
        except BlockingIOError:
            continue
        if not chunk:
            break
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            out.append(line.decode(errors="replace"))
    p.terminate()
    try:
        p.wait(timeout=5)
    except Exception:
        p.kill()
    log = "\n".join(out)
    fatal = any(m in log for m in ("[FATAL]", "[Fault]", "panicked", "PANIC"))
    completed = f"[herd] Oneshot service {SERVICE_NAME} completed" in log
    booted = "[herd] Userspace supervisor starting" in log
    ok = booted and not fatal and completed
    print(f"  {label} SMP={smp} MEM={mem:>5}MiB -> {'PASS' if ok else 'FAIL'}  "
          f"booted={booted} fatal={fatal} completed={completed}")
    log_dir = os.path.dirname(vm_json) or "."
    with open(os.path.join(log_dir, f"{label}_smp{smp}_mem{mem}.log"), "w") as f:
        f.write(log)
    return ok


def sweep(label, kernel, disk, ram_steps, smps, vm_json):
    results = {}
    for smp in smps:
        last_good = None
        for mem in ram_steps:
            if run_cell(label, kernel, disk, smp, mem, vm_json):
                last_good = mem
            else:
                break
        results[smp] = last_good
        sys.stdout.flush()
    return results


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                  formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("label")
    ap.add_argument("kernel")
    ap.add_argument("disk")
    ap.add_argument("ram_steps", help="comma-separated MiB values, highest first")
    ap.add_argument("--command", default="/bin/forktest_parent")
    ap.add_argument("--args", default="-duration=4s")
    ap.add_argument("--smp", default="1,2,4")
    ns = ap.parse_args()

    ram_steps = [int(x) for x in ns.ram_steps.split(",")]
    smps = [int(x) for x in ns.smp.split(",")]
    vm_json = os.path.join(os.path.dirname(os.path.abspath(ns.disk)) or ".", "akuma-vm.json")

    stage_service(ns.disk, ns.command, ns.args)
    results = sweep(ns.label, ns.kernel, ns.disk, ram_steps, smps, vm_json)
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
