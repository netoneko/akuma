#!/usr/bin/env python3
"""Stage the ssh-able KVM guest on the ryzen and arm it (run from the Mac).

    python3 overlays/ryzen/kvm-serve-stage.py [--no-arm]

1. reads the public keys the ryzen's Akuma accepts (`/etc/sshd/authorized_keys` on its p3);
2. builds the guest's root here (`scripts/kvm/akuma_guest_rootfs.sh`, SERVE=1): busybox,
   sshd, and those keys plus the repo's test key as the guest's authorized keys;
3. uploads it to the ryzen's `~/akuma-metal/out/`;
4. runs `kvm-akuma-stage.sh` there with GUEST=serve (kernel already built on the ryzen:
   `~/akuma-metal/kvm-build.sh`) and, unless --no-arm, `arm.sh 28`.

Afterwards, once Akuma is up (about a minute): `ssh -p 2223 root@<ryzen-wifi-ip>` from the
LAN, or from the ryzen's own Akuma. Only public keys leave the ryzen.
"""
import argparse, os, subprocess, sys, tempfile
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "scripts", "utils"))
import hpbox

ap = argparse.ArgumentParser()
ap.add_argument("--no-arm", action="store_true")
a = ap.parse_args()
root = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
os.chdir(root)

rc, out, err = hpbox.ryzen_root("mountpoint -q /mnt/p3 || mount /dev/nvme0n1p3 /mnt/p3; cat /mnt/p3/etc/sshd/authorized_keys; umount /mnt/p3 2>/dev/null; true")
if rc != 0 or "ssh-" not in out:
    sys.exit(f"could not read the ryzen's authorized keys: rc={rc} {err}")
test = open("target/x86_64-unknown-none/release/amd64-ssh-test-key.pub").read()
keys = []
for line in (test + "\n" + out).splitlines():
    line = line.strip()
    if line.startswith("ssh-") and line not in keys:
        keys.append(line)
print(f"{len(keys)} distinct public keys for the guest")
w = tempfile.mkdtemp()
kf = os.path.join(w, "authorized_keys")
open(kf, "w").write("\n".join(keys) + "\n")
img = os.path.join(w, "akuma-serve-root.img")
env = dict(os.environ, SERVE="1", NET="1", AUTHORIZED_KEYS=kf)
subprocess.run(["sh", "scripts/kvm/akuma_guest_rootfs.sh", img, "target/x86_64-unknown-none/release/busybox-x86_64"], check=True, env=env)
print("uploading", os.path.getsize(img), "bytes")
with open(img, "rb") as f:
    r = subprocess.run(hpbox.RZ + ["cat > /home/netoneko/akuma-metal/out/akuma-serve-root.img"], stdin=f, capture_output=True)
if r.returncode:
    sys.exit("upload failed: " + r.stderr.decode(errors="replace"))
rc, out, err = hpbox.ryzen_root(
    "cd /home/netoneko/akuma-metal/kvm-akuma && SRC=/home/netoneko/akuma-metal/kvm-akuma GUEST=serve "
    "sh overlays/ryzen/kvm-akuma-stage.sh /home/netoneko/akuma-metal/out/akuma-amd64.kvm-akuma 2>&1 | tail -4", timeout=300)
print(out)
if rc:
    sys.exit(err)
if not a.no_arm:
    rc, out, err = hpbox.ryzen_root("cd /home/netoneko/akuma-metal/kvm-akuma && sh overlays/ryzen/arm.sh 28 2>&1 | tail -2")
    print(out, "armed; the ryzen reboots into Akuma")
