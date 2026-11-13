import os
import subprocess, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "utils"))
import hpbox
K, SMP = sys.argv[1], sys.argv[2]
P = os.path.dirname(os.path.abspath(__file__))
def rz(c, t=120):
    return hpbox.ryzen_root(c, timeout=t)[1]
data = open(K, "rb").read()
dst = "/root/tx-target/x86_64-unknown-none/release/taoxichuan"
subprocess.run(hpbox.RZ_ROOT + [f"cat > {dst}.new && chmod +x {dst}.new && mv {dst}.new {dst}"], input=data, check=True, timeout=300)
rz("TX_RUN=/root/tx-run2 bash /root/tx-pop.sh stop; TX_RUN=/root/tx-run3 bash /root/tx-pop.sh stop; sleep 1")
print(rz(f"TX_RUN=/root/tx-run3 TX_SMP={SMP} TX_APPEND=smp={SMP} TX_MEM=3072 TX_INIT=/root/tx/sshd-fork-linuxabi bash /root/tx-pop.sh boot fresh; sleep 25").strip(), flush=True)
SSH = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-p", "2224", "root@192.168.1.126"]
SCRIPT = "; ".join(l for l in open(P + "/fp.sh").read().splitlines() if l.strip())
CMD = "sh -c '" + SCRIPT.replace("'", "'\\''") + "'"
t0 = time.time()
try:
    r = subprocess.run(SSH + [CMD], capture_output=True, text=True, timeout=3000)
    print("rc", r.returncode, "wall", int(time.time() - t0), "s")
    print(r.stdout[-900:])
except subprocess.TimeoutExpired:
    print("TIMEOUT after", int(time.time() - t0))
print(rz("grep -acE 'Fault|PANIC|BADFRAME|stuck|never ack' /root/tx-run3/console.log").strip(), "fault-ish console lines")
