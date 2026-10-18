#!/usr/bin/env python3
"""One reboot-loop cycle on ryzen, driven from the laptop.

    python3 overlays/ryzen/cycle.py <entry> [--grep PATTERN] [--log early|dmesg] [--transcript NAME]
                                    [--kernel PATH] [--no-rehearse] [--stay]

1. ship the kernel (default: this tree's `no-tests` build,
   `target/x86_64-unknown-none/release/akuma-amd64`) to `$W/out/akuma-amd64`
   and this tree's `grub.cfg` to `$W/akuma/overlays/ryzen/`;
2. rehearse menu entry <entry> in QEMU on ryzen (`qemu.sh`, `DISK=nvme`) and
   stop unless the guest reset itself and p3 checks clean;
3. `install.sh`, `arm.sh <entry>` — ryzen boots Akuma once;
4. wait for Pop to answer again;
5. print the lines of the boot's `boot-N.early` (`--log dmesg`: the full
   `boot-N.dmesg`, which services that reboot by themselves save) matching
   PATTERN (default `\\[rtw\\]`) from p3, mounted read-only, and with
   `--transcript NAME` the service's own `NAME-N.txt`.

`--stay` is for an entry that stays up (no autoreboot, e.g. 12): ship,
install and arm only — no rehearsal (the rehearsal needs a guest that resets
itself) and no wait for Pop.

Everything goes through `scripts/utils/hpbox.py`: kernel and config as the
`netoneko` user, everything else as root. Build the kernel first:
    cargo build -p akuma-amd64 --target x86_64-unknown-none --release --features no-tests
(`no-tests` matters: the plain build runs the self-test suite and changes boot.)
"""
import argparse
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(ROOT, "scripts", "utils"))
import hpbox  # noqa: E402

W = "/home/netoneko/akuma-metal"


def ship(local, remote):
    """Copy `local` to `remote` on ryzen, **verified**.

    Skips the transfer when the remote file already has the right md5 (the link
    is wifi: a 10 MB kernel is ~100 s). Uploads to `remote.new` and renames only
    after the md5 matches. The original `cat > x.new && mv x.new x` renamed
    whatever had arrived when the local timeout killed the ssh client — remote
    `cat` sees EOF and exits 0 — so a timeout installed a **truncated kernel**
    (2026-10-18: 8.8 of 10.5 MB, caught before `install.sh` ran).
    """
    import hashlib
    data = open(local, "rb").read()
    want = hashlib.md5(data).hexdigest()
    rc, o, _ = hpbox.ryzen(f"md5sum {remote} 2>/dev/null", timeout=60)
    if rc == 0 and o.split()[:1] == [want]:
        return
    r = subprocess.run(hpbox.RZ + [f"cat > {remote}.new"], input=data, capture_output=True, timeout=1800)
    if r.returncode:
        sys.exit(f"ship {local}: {r.stderr.decode(errors='replace')}")
    rc, o, e = hpbox.ryzen(f"echo '{want}  {remote}.new' | md5sum -c - && mv {remote}.new {remote}", timeout=120)
    if rc:
        sys.exit(f"ship {local}: md5 mismatch after upload: {o} {e}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("entry", type=int)
    ap.add_argument("--grep", default=r"\[rtw\]")
    ap.add_argument("--log", choices=("early", "dmesg"), default="early")
    ap.add_argument("--transcript", default="")
    ap.add_argument("--kernel", default=os.path.join(ROOT, "target/x86_64-unknown-none/release/akuma-amd64"))
    ap.add_argument("--no-rehearse", action="store_true")
    ap.add_argument("--stay", action="store_true", help="entry stays up: no rehearsal, no wait for Pop")
    a = ap.parse_args()

    ship(a.kernel, f"{W}/out/akuma-amd64")
    ship(os.path.join(ROOT, "overlays/ryzen/grub.cfg"), f"{W}/akuma/overlays/ryzen/grub.cfg")
    print("shipped", flush=True)

    if not (a.no_rehearse or a.stay):
        rc, o, e = hpbox.ryzen_root(
            f"cd {W}/akuma && DISK=nvme sh overlays/ryzen/qemu.sh {a.entry} 240 std 2>&1 "
            f"| grep -aE 'qemu rc|e2fsck rc|next_entry|panic|PANIC'; "
            f"grep -aE '{a.grep}' {W}/qemu/serial-{a.entry}.log", timeout=900)
        print(o.strip(), flush=True)
        if "e2fsck rc=0" not in o or "exited: guest reset" not in o:
            sys.exit("rehearsal failed — not booting the metal")

    rc, o, e = hpbox.ryzen_root(f"sh {W}/install.sh 2>&1 | grep DONE; sh {W}/akuma/overlays/ryzen/arm.sh {a.entry}",
                                timeout=300)
    print(o.strip(), flush=True)
    if a.stay:
        print("armed; ryzen boots entry", a.entry, "on its next reboot and stays there", flush=True)
        return
    t0 = time.time()
    down = False
    while time.time() - t0 < 900:
        try:
            rc, _, _ = hpbox.ryzen_root("true", timeout=20)
            if rc == 0 and down:
                break
            if rc != 0:
                down = True
        except subprocess.TimeoutExpired:
            down = True
        time.sleep(10)
    else:
        sys.exit("ryzen did not come back in 15 minutes — someone may need to press the power button")
    print(f"back in Pop after {int(time.time() - t0)} s", flush=True)
    extra = f"cat /mnt/akp3/var/log/ryzen/{a.transcript}-$N.txt; " if a.transcript else ""
    rc, o, e = hpbox.ryzen_root(
        "mkdir -p /mnt/akp3 && mount -o ro /dev/nvme0n1p3 /mnt/akp3 && { N=$(cat /mnt/akp3/var/log/ryzen/count); "
        f"echo boot-$N; grep -aE '{a.grep}' /mnt/akp3/var/log/ryzen/boot-$N.{a.log}; {extra}umount /mnt/akp3; }}")
    print(o, e)


if __name__ == "__main__":
    main()
