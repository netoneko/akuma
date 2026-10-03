#!/usr/bin/env python3
"""Does a writable-MAP_SHARED workload corrupt ext2 directories? Local QEMU + host e2fsck.

The gate for the 2026-10-03 trashcan report (`git commit` -> "unable to write
file .git/objects/7d/...: No such file or directory", then `.git` gone;
`docs/archive/AMD64_GIT_REPO_VANISH.md`). Runs `userspace/forktest/c_stress/
fsintegrity.c` in the guest on a COPY of the root image, then shuts the guest
down and runs `e2fsck -fn` on the copy from the host — the guest's own view
(the canary workers) and the on-disk view (fsck) are both reported.

    scripts/utils/amd64_fsintegrity_check.py                    # 60 s, 2 gitsim + 4 mapsim
    scripts/utils/amd64_fsintegrity_check.py --control          # no mapsim: the control arm
    scripts/utils/amd64_fsintegrity_check.py --smp 4 -t 120
"""

import argparse
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import amd64_ring3_check as r3  # noqa: E402
from amd64_mem_trials import inject_local  # noqa: E402

E2FSCK = shutil.which("e2fsck") or "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck"


def fsck(img):
    r = subprocess.run([E2FSCK, "-fn", img], capture_output=True, text=True)
    return r.returncode, r.stdout + r.stderr


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--smp", type=int, default=1)
    ap.add_argument("-t", "--seconds", type=int, default=60)
    ap.add_argument("--git", type=int, default=2)
    ap.add_argument("--map", type=int, default=4)
    ap.add_argument("--hog", type=int, default=0, help="MiB of anonymous memory a hog process holds")
    ap.add_argument("--mem", type=int, default=0, help="guest RAM in MiB (default run.sh's 2048)")
    ap.add_argument("--control", action="store_true", help="no mapsim workers")
    ap.add_argument("--img", default=os.path.join(r3.REPO, "target/x86_64-unknown-none/release/amd64-fsi.img"))
    ap.add_argument("--ssh-port", type=int, default=2267)
    ap.add_argument("--http-port", type=int, default=8067)
    ap.add_argument("--boot-timeout", type=int, default=300)
    a = ap.parse_args(argv)

    r3.OUTDIR.mkdir(parents=True, exist_ok=True)
    subprocess.run([r3.CC, "-static", "-O1", "-pthread", "-o", str(r3.OUTDIR / "fsintegrity"),
                    str(r3.PROBE_SRC / "fsintegrity.c")], check=True)
    # A fresh image every run: a previous run's damage must not be this run's.
    if os.path.exists(a.img):
        os.unlink(a.img)
    subprocess.run(["sh", os.path.join(r3.REPO, "amd64", "mkdisk.sh"), a.img, "128"],
                   cwd=r3.REPO, check=True, capture_output=True)
    rc, base = fsck(a.img)
    print(f"baseline fsck: rc={rc} (mkdisk's own noise; only NEW lines below count)")
    r3.IMG = a.img
    inject_local(a.img, r3.OUTDIR, ["fsintegrity"])

    if a.mem:
        os.environ["MEMORY"] = str(a.mem)
    proc, lines = r3.boot(a.smp, a.ssh_port, a.http_port)
    result = 1
    try:
        if not r3.wait_ready(a.ssh_port, a.boot_timeout, proc):
            print("NOT READY\n" + "".join(lines[-40:]))
            return 1
        print(f"guest up (smp={a.smp})", flush=True)
        env = ("FSI_NOMAP=1 " if a.control else "") + (f"FSI_HOG_MB={a.hog} " if a.hog else "")
        rc, out = r3.ssh(a.ssh_port, f"{env}/probes/fsintegrity /fsi {a.seconds} {a.git} {a.map}",
                         timeout=a.seconds + 240)
        print(out.rstrip())
        print(f"workload rc={rc}")
        rc2, d = r3.ssh(a.ssh_port, "touch /fsi/reclaim_kick; rm /fsi/reclaim_kick; sync; dmesg | grep -a -i 'fpcache-rw\\|MM-WB\\|PMM-\\|PANIC\\|kill\\|oom\\|SEGV\\|fault' | cut -c1-200 | tail -25", timeout=60)
        print(d.rstrip())
        result = rc
    finally:
        time.sleep(2)
        subprocess.run(["pkill", "-f", f"hostfwd=tcp::{a.ssh_port}-"], capture_output=True)
        time.sleep(1)
    rc, out = fsck(a.img)
    known = set(base.splitlines())
    new = [l for l in out.splitlines() if l.strip() and l not in known and "files (" not in l]
    print(f"post-run e2fsck -fn: rc={rc}; {len(new)} new line(s) vs baseline")
    print("\n".join(new[:80]))
    import re
    dbg = shutil.which("debugfs") or "/opt/homebrew/opt/e2fsprogs/sbin/debugfs"
    for ino in sorted({m.group(1) for l in new for m in [re.search(r"[Ii]node (\d+)", l)] if m}):
        r = subprocess.run([dbg, "-R", f"ncheck {ino}", a.img], capture_output=True, text=True)
        print(f"  inode {ino} is: " + " | ".join(r.stdout.strip().splitlines()[1:]))
    ok = result == 0 and not new
    print("RESULT:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
