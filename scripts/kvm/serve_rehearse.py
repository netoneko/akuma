#!/usr/bin/env python3
"""Rehearse the service guest (`kvmserve`) under QEMU TCG: ssh into the Akuma guest of an
Akuma host through the hypervisor's port forward.

    scripts/kvm/serve_rehearse.py LOG [--wait SECONDS] [--hold]

The host kernel boots under QEMU `microvm` with a user-mode NIC whose `hostfwd` sends
localhost:2223 to the host Akuma's own port 2223, where the relay listens; the relay
connects through the hypervisor's gateway to sshd on the guest's port 22. The check is a
real `ssh` from this machine running a command in the guest. Env: KERNEL, BUSYBOX, SSHD,
KEY (the private key whose .pub goes in the guest's authorized_keys), MEM, E2FSPROGS.
`--hold` leaves QEMU running (and prints how to reach it) instead of killing it.
"""
import os, subprocess, sys, tempfile, time, shutil, argparse

ap = argparse.ArgumentParser()
ap.add_argument("log")
ap.add_argument("--wait", type=int, default=900)
ap.add_argument("--hold", action="store_true")
ap.add_argument("--bulk", type=int, default=0, help="also upload and download this many KiB through ssh, with md5s")
ap.add_argument("--port", type=int, default=2223)
a = ap.parse_args()

here = os.path.dirname(os.path.abspath(__file__))
root = os.path.dirname(os.path.dirname(here))
os.chdir(root)
kernel = os.environ.get("KERNEL", "target/x86_64-unknown-none/release/akuma-amd64")
busybox = os.environ.get("BUSYBOX", "target/x86_64-unknown-none/release/busybox-x86_64")
key = os.environ.get("KEY", "target/x86_64-unknown-none/release/amd64-ssh-test-key")
e2 = os.environ.get("E2FSPROGS", "/opt/homebrew/opt/e2fsprogs/sbin")
w = tempfile.mkdtemp()
os.makedirs(f"{w}/host/boot")
os.makedirs(f"{w}/host/bin")
shutil.copy(kernel, f"{w}/host/boot/akuma-guest")
# The host needs an init that stays alive: without one the boot ends in `halt()` and the
# scheduler never runs the service task. (On the ryzen herd is init.)
shutil.copy(busybox, f"{w}/host/bin/busybox")
os.chmod(f"{w}/host/bin/busybox", 0o755)
subprocess.run([f"{e2}/mke2fs", "-q", "-t", "ext2", "-d", f"{w}/host", "-L", "kvmhost", f"{w}/root.img", "48M"], check=True)
env = dict(os.environ, SERVE="1", NET="1", AUTHORIZED_KEYS=key + ".pub")
subprocess.run(["sh", f"{here}/akuma_guest_rootfs.sh", f"{w}/guest.img", busybox], check=True, env=env, stdout=subprocess.DEVNULL)
# The service guest reads /boot/akuma-serve-root.img only without a second virtio-blk; give it one.
q = subprocess.Popen([
    "qemu-system-x86_64", "-M", "microvm", "-cpu", "EPYC-Milan,svm=on",
    "-global", "virtio-mmio.force-legacy=false", "-m", os.environ.get("MEM", "3072"), "-smp", "1",
    "-kernel", kernel,
    "-drive", f"id=d0,file={w}/root.img,format=raw,if=none", "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0",
    "-drive", f"id=d1,file={w}/guest.img,format=raw,if=none", "-device", "virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1",
    "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{a.port}-:{a.port}", "-device", "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.2",
    "-append", f"virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 virtio_mmio.device=512@0xfeb00400:7 kvmtest kvmserve noladder kvmfwd={a.port} init=/bin/busybox initargs=sleep,1000000",
    "-serial", f"file:{a.log}", "-display", "none", "-no-reboot"],
    stderr=subprocess.DEVNULL)

def log():
    try:
        return open(a.log, "rb").read().decode("latin1")
    except OSError:
        return ""

def finish(rc):
    if not a.hold:
        q.kill()
        q.wait()
        shutil.rmtree(w, ignore_errors=True)
    else:
        print(f"QEMU pid {q.pid} left running; ssh -p {a.port} -i {key} root@127.0.0.1")
    sys.exit(rc)

t0 = time.time()
while time.time() - t0 < a.wait and "AKUMA-GUEST-SSHD-STARTED" not in log():
    if q.poll() is not None:
        print("QEMU exited early"); finish(1)
    time.sleep(2)
if "AKUMA-GUEST-SSHD-STARTED" not in log():
    print("guest sshd never reported started"); finish(1)
print(f"guest up after {time.time() - t0:.0f}s; trying ssh")
last = ""
ok = False
while time.time() - t0 < a.wait:
    try:
        r = subprocess.run(["ssh", "-p", str(a.port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
                            "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "ConnectTimeout=60",
                            "root@127.0.0.1", "echo hello-from-the-guest; uname -a; cat /proc/cpuinfo | head -3"],
                           capture_output=True, text=True, timeout=240)
        last = r.stdout + r.stderr
        if "hello-from-the-guest" in r.stdout:
            ok = True
            break
    except subprocess.TimeoutExpired:
        last = "ssh timed out"
    time.sleep(5)
print(last.strip())
if ok and a.bulk:
    import hashlib
    base = ["ssh", "-p", str(a.port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "root@127.0.0.1"]
    data = os.urandom(a.bulk * 1024)
    h = hashlib.md5(data).hexdigest()
    t = time.time()
    try:
        r = subprocess.run(base + ["cat > /tmp/big && md5sum /tmp/big"], input=data, capture_output=True, timeout=900)
        print(f"bulk upload {a.bulk} KiB: {'ok' if h in r.stdout.decode() else 'MISMATCH ' + r.stdout.decode()} in {time.time() - t:.1f}s")
        t = time.time()
        r = subprocess.run(base + ["cat /tmp/big"], capture_output=True, timeout=900)
        print(f"bulk download {len(r.stdout)} bytes: {'ok' if hashlib.md5(r.stdout).hexdigest() == h else 'MISMATCH'} in {time.time() - t:.1f}s")
    except subprocess.TimeoutExpired:
        print("bulk transfer TIMED OUT")
        ok = False
print("[REHEARSE] akuma-serve:", "PASS" if ok else "FAIL")
for l in log().splitlines():
    if "[KVM] relay" in l or "service guest" in l:
        print(l)
finish(0 if ok else 1)
