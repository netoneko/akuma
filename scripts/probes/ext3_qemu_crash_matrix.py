#!/usr/bin/env python3
"""Kill a disposable QEMU guest after each actual ext3 journal I/O step."""
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

root, kernel, base, work = map(Path, sys.argv[1:5])
old = b"CRASH_TEST_OLD!!!\n"
new = b"CRASH_TEST_NEW!!!\n"
completed = False
for target in range(1, 65):
    image = work / f"crash-{target}.img"
    serial = work / f"crash-{target}.log"
    shutil.copyfile(base, image)
    cmd = ["qemu-system-x86_64", "-M", "microvm", "-accel", "tcg", "-cpu", "max",
           "-global", "virtio-mmio.force-legacy=false", "-kernel", str(kernel), "-m", "2048", "-smp", "1",
           "-drive", f"id=d0,file={root},format=raw,if=none", "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0",
           "-drive", f"id=d1,file={image},format=raw,if=none,cache=writeback", "-device", "virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1",
           "-append", f"virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 init=/bin/ext-crash-probe ext3crash={target}",
           "-serial", f"file:{serial}", "-display", "none", "-no-reboot"]
    proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    marker = f"EXT3_CRASH_STOP {target}".encode()
    deadline = time.monotonic() + 30
    outcome = None
    try:
        while time.monotonic() < deadline:
            log = serial.read_bytes() if serial.exists() else b""
            if marker in log:
                outcome = "crash"
                break
            if b"ext-crash-probe: PASS" in log:
                outcome = "complete"
                break
            if proc.poll() is not None:
                raise RuntimeError(f"QEMU exited before the probe: {proc.stderr.read().decode(errors='replace')}\n{log[-3000:].decode(errors='replace')}")
            time.sleep(0.02)
        if outcome is None:
            raise RuntimeError(f"step {target} timed out:\n{log[-3000:].decode(errors='replace')}")
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()
    if any(s in log for s in (b"STACK CANARY", b"SWITCH BADFRAME", b"[EXCEPTION]")):
        raise RuntimeError(f"guest fault at step {target}:\n{log[-3000:].decode(errors='replace')}")
    replay = subprocess.run(["e2fsck", "-fy", "-E", "journal_only", str(image)], capture_output=True)
    detail = replay.stdout + replay.stderr
    if replay.returncode not in (0, 1) or any(x in detail for x in (b"Fix?", b"Clear?", b"Recreate?", b"UNEXPECTED INCONSISTENCY")):
        raise RuntimeError(f"journal recovery failed at {target}:\n{detail.decode(errors='replace')}")
    check = subprocess.run(["e2fsck", "-fn", str(image)], capture_output=True)
    if check.returncode != 0:
        raise RuntimeError(f"filesystem inconsistent at {target}:\n{(check.stdout + check.stderr).decode(errors='replace')}")
    data = subprocess.run(["debugfs", "-R", "cat /payload", str(image)], capture_output=True, check=True).stdout
    durable_commit = f"EXT3_CRASH_STOP {target} 1".encode() in log
    if data not in (old, new) or ((outcome == "complete" or durable_commit) and data != new):
        raise RuntimeError(f"unexpected payload at {target}: {data!r}")
    image.unlink()
    print(f"step {target}: {outcome}, {'new' if data == new else 'old'}", flush=True)
    if outcome == "complete":
        if target < 9:
            raise RuntimeError("too few journal boundaries were exercised")
        completed = True
        break
if not completed:
    raise RuntimeError("journal step count exceeded the test bound")
