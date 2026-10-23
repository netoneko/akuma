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
ap.add_argument("--host-smp", type=int, default=1, help="QEMU -smp and the host kernel's smp=: host cores for vCPUs to spread over")
ap.add_argument("--host-cap", type=int, help="the host kernel's smp= when it should start fewer cores than QEMU has (the rest are for VMs, started on demand)")
ap.add_argument("--kvmcores", help="the host kernel's kvmcores= (the cores the hypervisor may use), e.g. 1-2")
ap.add_argument("--vcpus", type=int, default=1, help="with --herd: vm_vcpus of the generated conf")
ap.add_argument("--vm-cores", help="with --herd: vm_cores of the generated conf")
ap.add_argument("--probe", type=int, default=0, help="run this many parallel CPU-bound loops in the guest and print the wall time and /proc/cpuinfo's processor count (the vCPU speedup probe)")
ap.add_argument("--outbound", action="store_true",
                help="with --herd: `vm_outbound = true`; then fetch from a web server on this machine (by its LAN address, through the gateway's"
                     " outbound path and the host's slirp NIC) and resolve a name through the DNS proxy")
ap.add_argument("--host-ssh-bulk", type=int, default=0, metavar="MIB",
                help="with --herd: a herd oneshot on the HOST runs its own /bin/ssh against the relay and pulls this many MiB of /dev/zero through `wc -c`"
                     " (the ryzen's wedge shape, 2026-10-23, without a wifi link); the result is read back from the host disk")
ap.add_argument("--host-ssh-wait", type=int, default=300, help="with --host-ssh-bulk: seconds to let the transfer run before reading the result")
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
        f"vm = true\nvm_type = kvm\nvm_kernel = /boot/akuma-guest\nvm_ram_mib = 768\nvm_vcpus = {a.vcpus}\n" + (f"vm_cores = {a.vm_cores}\n" if a.vm_cores else "") +
        f"vm_forward = {a.port}:22\nvm_cmdline = init=/bin/busybox initargs=sh,/etc/guest.sh\n"
        + ("vm_outbound = true\n" if a.outbound else "") + "restart = true\nrestart_delay_ms = 5000\n")
    open(f"{w}/host/etc/herd/enabled/vm-serve.conf", "w").write(conf)
    if a.host_ssh_bulk:
        # The host's Akuma `ssh` client, with a raw ed25519 identity at /etc/sshd/id_ed25519 (its default
        # key) that the guest trusts: the 32-byte seed, and the public line appended to the guest's keys.
        import base64, struct
        seed = os.urandom(32)
        der = bytes.fromhex("302e020100300506032b657004220420") + seed
        pub = subprocess.run(["openssl", "pkey", "-inform", "DER", "-pubout", "-outform", "DER"], input=der, capture_output=True, check=True).stdout[-32:]
        blob = struct.pack(">I", 11) + b"ssh-ed25519" + struct.pack(">I", 32) + pub
        keys_file = f"{w}/authorized_keys"
        open(keys_file, "w").write(open(key + ".pub").read().rstrip("\n") + "\nssh-ed25519 " + base64.b64encode(blob).decode() + " hostcli\n")
        os.makedirs(f"{w}/host/etc/sshd")
        open(f"{w}/host/etc/sshd/id_ed25519", "wb").write(seed)
        os.makedirs(f"{w}/host/root/.ssh")
        shutil.copy(os.environ.get("HOST_SSH", "userspace/target/x86_64-unknown-none/release/ssh"), f"{w}/host/bin/ssh")
        os.chmod(f"{w}/host/bin/ssh", 0o755)
        open(f"{w}/host/root/hostbulk.sh", "w").write(
            "B=/bin/busybox\ni=0\nwhile [ $i -lt 40 ]; do\n"
            f"  echo yes | /bin/ssh -p {a.port} root@127.0.0.1 true >/var/log/herd/hostbulk.try 2>&1 && break\n"
            "  i=$((i+1)); $B sleep 5\ndone\necho started > /var/log/herd/hostbulk.out\n"
            f"n=$(/bin/ssh -p {a.port} root@127.0.0.1 'head -c {a.host_ssh_bulk << 20} /dev/zero' 2>/var/log/herd/hostbulk.err | $B wc -c)\n"
            "echo bytes=$n > /var/log/herd/hostbulk.out\n")
        open(f"{w}/host/etc/herd/enabled/hostbulk.conf", "w").write(
            "command = /bin/busybox\nargs = sh /root/hostbulk.sh\noneshot = true\nstart_delay_ms = 20000\n")
    if a.restart:
        # herd is pid 1, so the first `vm-run` is pid 2. Killing it is what a crash, `herd stop`
        # or `kill` looks like to the kernel: the owner is gone, the lease ends the guest.
        open(f"{w}/host/etc/herd/enabled/killvm.conf", "w").write(
            "command = /bin/busybox\nargs = kill 2\noneshot = true\nstart_delay_ms = 90000\n")
subprocess.run([f"{e2}/mke2fs", "-q", "-t", "ext2", "-d", f"{w}/host", "-L", "kvmhost", f"{w}/root.img", "48M"], check=True)
env = dict(os.environ, SERVE="1", NET="1", AUTHORIZED_KEYS=(f"{w}/authorized_keys" if a.host_ssh_bulk else key + ".pub"))
subprocess.run(["sh", f"{here}/akuma_guest_rootfs.sh", f"{w}/guest.img", busybox], check=True, env=env, stdout=subprocess.DEVNULL)
# The service guest reads /boot/akuma-serve-root.img only without a second virtio-blk; give it one.
q = subprocess.Popen([
    "qemu-system-x86_64", "-M", "microvm", "-cpu", "EPYC-Milan,svm=on",
    "-global", "virtio-mmio.force-legacy=false", "-m", os.environ.get("MEM", "3072"), "-smp", str(a.host_smp),
    "-kernel", kernel,
    "-drive", f"id=d0,file={w}/root.img,format=raw,if=none", "-device", "virtio-blk-device,drive=d0,bus=virtio-mmio-bus.0",
    "-drive", f"id=d1,file={w}/guest.img,format=raw,if=none", "-device", "virtio-blk-device,drive=d1,bus=virtio-mmio-bus.1",
    "-netdev", f"user,id=n0,hostfwd=tcp:127.0.0.1:{a.port}-:{a.port}", "-device", "virtio-net-device,netdev=n0,bus=virtio-mmio-bus.2",
    "-append", f"virtio_mmio.device=512@0xfeb00000:5 virtio_mmio.device=512@0xfeb00200:6 virtio_mmio.device=512@0xfeb00400:7 "
               + (f"smp={a.host_cap or a.host_smp} " if a.host_smp > 1 else "") + (f"kvmcores={a.kvmcores} " if a.kvmcores else "")
               + (f"kvmctl kvmmem=1536 init=/bin/herd initargs=daemon" if a.herd else
                  f"kvmtest kvmserve noladder kvmfwd={a.port} init=/bin/busybox initargs=sleep,1000000"),
    "-serial", f"file:{a.log}", "-display", "none", "-no-reboot"] + (["-accel", "tcg,thread=multi"] if os.environ.get("MTTCG") else []) + (["-monitor", f"unix:{os.environ['QMON']},server,nowait"] if os.environ.get("QMON") else []),
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
            for l in [l for l in hl.splitlines() if "relay: dns" in l or "relay: outbound" in l][:8]:
                print(f"[REHEARSE] {l.strip()[:200]}")
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
if ok and a.probe:
    # The vCPU speedup probe: N identical CPU-bound busybox loops at once. On one host core they
    # take N times as long as one; on N cores about as long as one. Wall time is this machine's
    # clock around the ssh, so the fixed cost of the connection is in both numbers: compare
    # --probe 1 against --probe N on the same boot.
    base = ["ssh", "-p", str(a.port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=60", "root@127.0.0.1"]
    loops = int(os.environ.get("PROBE_LOOPS", "200000"))
    body = f"i=0; while [ $i -lt {loops} ]; do i=$((i+1)); done"
    for n in sorted({1, a.probe}):
        cmd = "; ".join([f"({body}) &" for _ in range(n)] + ["wait", "echo probe-done"])
        t = time.time()
        r = subprocess.run(base + [f"grep -c ^processor /proc/cpuinfo; {cmd}"], capture_output=True, text=True, timeout=1800)
        cpus = r.stdout.split()[0] if r.stdout.split() else "?"
        print(f"[REHEARSE] probe x{n}: {time.time() - t:.1f}s (guest sees {cpus} processors) {'ok' if 'probe-done' in r.stdout else 'FAILED ' + r.stderr.strip()[-60:]}")
        ok = ok and "probe-done" in r.stdout
if ok and a.outbound:
    # The guest reaches a server on this machine (a LAN address: 10.0.2.2 is the guest's own gateway)
    # through the hypervisor's outbound path, and resolves a name through the DNS proxy.
    import hashlib, http.server, socket, threading, functools
    lan = ""
    for ifname in ("en0", "en1"):
        lan = subprocess.run(["ipconfig", "getifaddr", ifname], capture_output=True, text=True).stdout.strip()
        if lan:
            break
    if not lan:
        s_ = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s_.connect(("192.0.2.1", 9)); lan = s_.getsockname()[0]; s_.close()
    web = f"{w}/web"
    os.makedirs(web)
    big = os.urandom(4 << 20)
    open(f"{web}/probe.txt", "w").write("outbound-probe-ok\n")
    open(f"{web}/big.bin", "wb").write(big)
    handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=web)
    handler.log_message = lambda *a_, **k_: None
    srv = http.server.ThreadingHTTPServer(("0.0.0.0", 0), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    wp = srv.server_address[1]
    base = ["ssh", "-p", str(a.port), "-i", key, "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no",
            "-o", "UserKnownHostsFile=/dev/null", "-o", "BatchMode=yes", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=60", "root@127.0.0.1"]
    def guest(cmd, t=300):
        try:
            r = subprocess.run(base + [cmd], capture_output=True, timeout=t)
            return r.stdout.decode("latin1"), r.stderr.decode("latin1")
        except subprocess.TimeoutExpired:
            return "TIMEOUT", ""
    out, err = guest(f"wget -q -O - http://{lan}:{wp}/probe.txt")
    good = "outbound-probe-ok" in out
    print(f"[REHEARSE] outbound: wget http://{lan}:{wp}/probe.txt -> {out.strip()[:60]!r} {'ok' if good else 'FAILED ' + err.strip()[-80:]}")
    t = time.time()
    out, err = guest(f"wget -q -O - http://{lan}:{wp}/big.bin | md5sum")
    big_ok = hashlib.md5(big).hexdigest() in out
    print(f"[REHEARSE] outbound: 4 MiB download md5 {'ok' if big_ok else 'MISMATCH ' + out.strip()[:50] + err.strip()[-60:]} in {time.time() - t:.1f}s")
    # Names that resolve from here (example.com does not on every network).
    out, err = guest("nslookup neverssl.com 10.0.2.3 2>&1 | head -8")
    dns_ok = "Address" in out and "can't find" not in out.lower()
    print(f"[REHEARSE] outbound: nslookup neverssl.com -> {'ok' if dns_ok else 'FAILED'} {out.strip().replace(chr(10), ' | ')[:160]}")
    out, err = guest("wget -q -T 30 -O - http://neverssl.com/ 2>&1 | head -c 300")
    name_ok = "neverssl" in out.lower()
    print(f"[REHEARSE] outbound: wget http://neverssl.com/ -> {'ok' if name_ok else 'FAILED'} {out.strip().replace(chr(10), ' ')[:100]!r}")
    out, err = guest("wget -q -T 30 -O - http://dl-cdn.alpinelinux.org/alpine/latest-stable/releases/x86_64/latest-releases.yaml 2>&1 | head -c 200")
    apk_ok = "title" in out or "flavor" in out or "branch" in out
    print(f"[REHEARSE] outbound: wget the Alpine CDN's latest-releases.yaml -> {'ok' if apk_ok else 'FAILED'} {out.strip().replace(chr(10), ' ')[:100]!r}")
    srv.shutdown()
    ok = ok and good and big_ok and dns_ok and name_ok and apk_ok
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
if a.host_ssh_bulk:
    # Let the host's own ssh run (progress is the relay's `to_host=` counter), then read what it wrote.
    t2 = time.time()
    seen = ""
    while time.time() - t2 < a.host_ssh_wait:
        time.sleep(10)
        lines = [l for l in log().splitlines() if "relay: slot" in l and "to_host=" in l]
        cur = lines[-1].split("to_host=")[1].split()[0] if lines else "-"
        if cur != seen:
            seen = cur
            print(f"[REHEARSE] host ssh: relay to_host={cur} after {time.time() - t2:.0f}s", flush=True)
    q.kill()
    q.wait()
    for f in ("hostbulk.out", "hostbulk.err", "hostbulk.try"):
        r = subprocess.run([f"{e2}/debugfs", "-R", f"cat /var/log/herd/{f}", f"{w}/root.img"], capture_output=True)
        print(f"[REHEARSE] host {f}: {r.stdout.decode('latin1').strip()[-300:]!r}")
    # `wc -c` counts a few bytes of ssh's own chatter (the host-key notice) on top of the payload.
    got = subprocess.run([f"{e2}/debugfs", "-R", "cat /var/log/herd/hostbulk.out", f"{w}/root.img"], capture_output=True).stdout.decode("latin1").strip()
    ok = ok and got.startswith("bytes=") and got[6:].isdigit() and int(got[6:]) >= a.host_ssh_bulk << 20
print("[REHEARSE] akuma-serve" + (" (herd)" if a.herd else "") + ":", "PASS" if ok else "FAIL")
for l in log().splitlines():
    if "[KVM] relay" in l or "service guest" in l or "kvm0" in l or "kvmmem" in l or "[herd]" in l:
        print(l)
finish(0 if ok else 1)
