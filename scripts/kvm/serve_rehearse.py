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

`--herd` boots the host the way the ryzen runs it instead of with the `kvmserve` flag: herd is
init (env HERD, default userspace/target/x86_64-unknown-none/release/herd), the kernel is armed
with `kvmctl kvmmem=...`, and a `vm = true` conf in /etc/herd/enabled brings the guest up through
/dev/kvm0. The guest's console is then herd's log, which the script reads back from the host
disk with debugfs once QEMU is stopped. `--herd-conf FILE` rehearses a real conf (a ryzen
`vm-serve.conf`) instead of the generated one; its paths must be ones the script stages
(/boot/akuma-guest).
"""
import os, subprocess, sys, tempfile, time, shutil, argparse

ap = argparse.ArgumentParser()
ap.add_argument("log")
ap.add_argument("--wait", type=int, default=900)
ap.add_argument("--hold", action="store_true")
ap.add_argument("--bulk", type=int, default=0, help="also upload and download this many KiB through ssh, with md5s")
ap.add_argument("--port", type=int, default=2223)
ap.add_argument("--herd", action="store_true", help="host init is herd; the guest is a vm = true service (see above)")
ap.add_argument("--herd-conf", help="with --herd: this conf instead of the generated one")
ap.add_argument("--restart", action="store_true", help="with --herd: a host oneshot kills the `herd vm-run` process (pid 2) after 90 s; herd must bring the guest back and ssh must work again")
ap.add_argument("--stress", type=int, default=0, help="rounds of overlapping + back-to-back sessions (the wedge shape from the ryzen, 2026-10-22)")
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
if a.herd:
    herd = os.environ.get("HERD", "userspace/target/x86_64-unknown-none/release/herd")
    shutil.copy(herd, f"{w}/host/bin/herd")
    os.chmod(f"{w}/host/bin/herd", 0o755)
    os.makedirs(f"{w}/host/etc/herd/enabled")
    os.makedirs(f"{w}/host/var/log/herd")
    os.makedirs(f"{w}/host/var/run/herd")
    conf = open(a.herd_conf).read() if a.herd_conf else (
        "vm = true\nvm_type = kvm\nvm_kernel = /boot/akuma-guest\nvm_ram_mib = 768\nvm_vcpus = 1\n"
        f"vm_forward = {a.port}:22\nvm_cmdline = init=/bin/busybox initargs=sh,/etc/guest.sh\n"
        "restart = true\nrestart_delay_ms = 5000\n")
    open(f"{w}/host/etc/herd/enabled/vm-serve.conf", "w").write(conf)
    if a.restart:
        # herd is pid 1, so the first `vm-run` is pid 2. Killing it is what a crash, `herd stop`
        # or `kill` looks like to the kernel: the owner is gone, the lease ends the guest.
        open(f"{w}/host/etc/herd/enabled/killvm.conf", "w").write(
            "command = /bin/busybox\nargs = kill 2\noneshot = true\nstart_delay_ms = 90000\n")
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
    "-append", f"virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 virtio_mmio.device=512@0xfeb00400:7 "
               + (f"kvmctl kvmmem=1536 init=/bin/herd initargs=daemon" if a.herd else
                  f"kvmtest kvmserve noladder kvmfwd={a.port} init=/bin/busybox initargs=sleep,1000000"),
    "-serial", f"file:{a.log}", "-display", "none", "-no-reboot"],
    stderr=subprocess.DEVNULL)

def log():
    try:
        return open(a.log, "rb").read().decode("latin1")
    except OSError:
        return ""

def herd_log():
    """The guest's console as herd logged it on the host disk (`--herd`): read after QEMU stops."""
    out = ""
    for f in ("vm-serve.log.old", "vm-serve.log"):
        r = subprocess.run([f"{e2}/debugfs", "-R", f"cat /var/log/herd/{f}", f"{w}/root.img"], capture_output=True)
        out += r.stdout.decode("latin1")
    return out

def finish(rc):
    if not a.hold:
        q.kill()
        q.wait()
        if a.herd:
            hl = herd_log()
            print(f"[REHEARSE] herd log vm-serve.log: {len(hl)} bytes; guest banner={'Akuma/amd64' in hl}; sshd marker={'AKUMA-GUEST-SSHD-STARTED' in hl}; vm-run created={hl.count('[vm-run] guest created')}x")
            if rc == 0 and a.restart and hl.count("[vm-run] guest created") < 2:
                print("[REHEARSE] restart: the second guest never says it was created")
                rc = 1
            if rc == 0 and "AKUMA-GUEST-SSHD-STARTED" not in hl:
                print("[REHEARSE] ssh worked but the guest console is not in herd's log")
                rc = 1
        shutil.rmtree(w, ignore_errors=True)
    else:
        print(f"QEMU pid {q.pid} left running; ssh -p {a.port} -i {key} root@127.0.0.1")
    sys.exit(rc)

t0 = time.time()
# Under `--herd` the guest's console is herd's log, not the serial line: herd's own "Started"
# line is the marker, and ssh succeeding is the proof the guest is up.
marker = "[herd] Started vm-serve" if a.herd else "AKUMA-GUEST-SSHD-STARTED"
while time.time() - t0 < a.wait and marker not in log():
    if q.poll() is not None:
        print("QEMU exited early"); finish(1)
    time.sleep(2)
if marker not in log():
    print("guest never reported started"); finish(1)
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
if ok and a.stress:
    # Overlapping sessions (the relay has two slots), then connection churn. A session that
    # never gets its banner, or a port that stops accepting, is the wedge.
    base = ["ssh", "-p", str(a.port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=30", "root@127.0.0.1"]
    for rnd in range(a.stress):
        t = time.time()
        ps = [subprocess.Popen(base + [f"sleep {4 + i}; echo s{i}-ok"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for i in range(3)]
        outs = []
        for p_ in ps:
            try:
                o, e = p_.communicate(timeout=120)
                outs.append(o.strip() or ("ERR " + e.strip()[-60:]))
            except subprocess.TimeoutExpired:
                p_.kill()
                outs.append("TIMEOUT")
        bad = [o for i, o in enumerate(outs) if o != f"s{i}-ok"]
        print(f"stress round {rnd}: overlap x3 {outs} in {time.time() - t:.0f}s")
        for k in range(3):
            try:
                r = subprocess.run(base + ["echo churn-ok"], capture_output=True, text=True, timeout=60)
                if "churn-ok" not in r.stdout:
                    bad.append("churn " + r.stderr.strip()[-60:])
            except subprocess.TimeoutExpired:
                bad.append("churn TIMEOUT")
        # Abusive clients, as a flaky link or a killed ssh produces: connect-and-drop, a
        # hold of more connections than there are slots, and garbage then close.
        import socket
        for k in range(8):
            c = socket.create_connection(("127.0.0.1", a.port), timeout=30)
            c.close()
        held = [socket.create_connection(("127.0.0.1", a.port), timeout=30) for _ in range(4)]
        time.sleep(3)
        for c in held:
            try:
                c.sendall(b"garbage\r\n")
            except OSError:
                pass
        for c in held:
            c.close()
        for k in range(3):
            c = socket.create_connection(("127.0.0.1", a.port), timeout=30)
            c.sendall(os.urandom(300))
            c.close()
        time.sleep(2)
        for k in range(2):
            try:
                r = subprocess.run(base + ["echo after-abuse-ok"], capture_output=True, text=True, timeout=90)
                if "after-abuse-ok" not in r.stdout:
                    bad.append("after-abuse " + r.stderr.strip()[-60:])
            except subprocess.TimeoutExpired:
                bad.append("after-abuse TIMEOUT")
        print(f"stress round {rnd}: abuse {'ok' if not bad else bad}")
        if bad:
            print("stress WEDGE:", bad)
            ok = False
            break
if ok and a.restart:
    # The oneshot fires 90 s after herd starts; wait for the replacement guest and talk to it.
    t1 = time.time()
    while time.time() - t1 < a.wait and log().count("[herd] Started vm-serve") < 2:
        time.sleep(2)
    again = False
    last = ""
    while log().count("[herd] Started vm-serve") >= 2 and time.time() - t1 < a.wait:
        try:
            r = subprocess.run(["ssh", "-p", str(a.port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
                                "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "ConnectTimeout=60", "-o", "LogLevel=ERROR",
                                "root@127.0.0.1", "echo hello-after-restart"], capture_output=True, text=True, timeout=240)
            last = r.stdout + r.stderr
            if "hello-after-restart" in r.stdout:
                again = True
                break
        except subprocess.TimeoutExpired:
            last = "ssh timed out"
        time.sleep(5)
    print(f"[REHEARSE] restart: herd started vm-serve {log().count('[herd] Started vm-serve')}x; ssh after restart: {'ok' if again else 'FAILED ' + last.strip()[-80:]}")
    ok = ok and again
print("[REHEARSE] akuma-serve" + (" (herd)" if a.herd else "") + ":", "PASS" if ok else "FAIL")
for l in log().splitlines():
    if "[KVM] relay" in l or "service guest" in l or "kvm0" in l or "kvmmem" in l or "[herd]" in l:
        print(l)
finish(0 if ok else 1)
