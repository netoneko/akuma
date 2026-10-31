"""build_run.py KERNEL SMP [INIT] - the SMP gate: the in-guest -j4 build of the Taoxichuan kernel, timed, with the md5 of what it makes.

Boots KERNEL at SMP cores on the p3 toolchain root, checks the tree (fp.sh's checkout lines) out in the guest, runs cargo with its output in /tmp/b.log, and polls
that log from short sessions (a long silent ssh session gets dropped; the build survives it). Prints start/end epochs, wall seconds and the artifact md5."""
import os, subprocess, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "utils"))
import hpbox
K, SMP = sys.argv[1], sys.argv[2]
INIT = sys.argv[3] if len(sys.argv) > 3 else "/root/tx/sshd-fork-linuxabi"
P = os.path.dirname(os.path.abspath(__file__))
data = open(K, "rb").read()
d = "/root/tx-target-sig/x86_64-unknown-none/release/taoxichuan"
subprocess.run(hpbox.RZ_ROOT + [f"mkdir -p {os.path.dirname(d)} && cat > {d}.new && chmod +x {d}.new && mv {d}.new {d}"], input=data, check=True, timeout=300)
hpbox.ryzen_root("for r in 3 4 5 6; do TX_RUN=/root/tx-run$r bash /root/tx-pop.sh stop; done; sleep 1", timeout=60)
print(hpbox.ryzen_root(f"TX_TARGET=/root/tx-target-sig TX_PORT=2225 TX_RUN=/root/tx-run5 TX_SMP={SMP} TX_APPEND=smp={SMP} TX_MEM=3072 TX_INIT={INIT} bash /root/tx-pop.sh boot fresh; sleep 25", timeout=120)[1].strip(), flush=True)
SSH = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", "-p", "2225", "root@192.168.1.126"]

def run(c, t=120):
    for _ in range(3):
        try:
            r = subprocess.run(SSH + [c], capture_output=True, text=True, timeout=t)
            if r.stdout.strip():
                return r.stdout.strip()
        except Exception:
            pass
        time.sleep(3)
    return ""

script = open(P + "/fp.sh").read().splitlines()
pre = "; ".join(l for l in script if l.strip() and not l.startswith(("cargo", "date", "md5sum")))
t0 = time.time()
print("checkout:", "ok" if "CHECKOUT-DONE" in run("sh -c '" + pre.replace("'", "'\\''") + "; echo CHECKOUT-DONE'", 600) else "FAILED", int(time.time() - t0), "s", flush=True)
cmd = ('sh -c "export HOME=/root PATH=/usr/local/rust/bin:\\$PATH; cd /tmp/gh; date +%s > /tmp/b.log; '
       'cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt >> /tmp/b.log 2>&1; '
       'echo EXIT=\\$? >> /tmp/b.log; date +%s >> /tmp/b.log"')
subprocess.Popen(SSH + [cmd], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
for i in range(120):
    time.sleep(30)
    tail = run("sh -c 'tail -c 400 /tmp/b.log'", 60)
    if "EXIT=" in tail:
        print("log tail:", tail.replace("\n", " | ")[-300:])
        print("artifact:", run("sh -c 'md5sum /tmp/tgt/x86_64-unknown-none/release/taoxichuan; ls -l /tmp/tgt/x86_64-unknown-none/release/taoxichuan'", 60).replace("\n", " | "))
        break
    if i % 4 == 0:
        print(int(time.time() - t0), "s:", tail.replace("\n", " | ")[-120:], flush=True)
print("faults on console:", hpbox.ryzen_root("grep -acE 'Fault|PANIC|BADFRAME|stuck|never ack' /root/tx-run5/console.log", timeout=60)[1].strip())
