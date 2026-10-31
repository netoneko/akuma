#!/usr/bin/env python3 -I
import os
"""gate2.py KERNEL SMP BOOTS [TRIALS]: boot the kernel BOOTS times on ryzen KVM; per boot run the fork/wait probe TRIALS times."""
import subprocess, sys, time
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "utils"))
import hpbox
K, SMP, BOOTS = sys.argv[1], sys.argv[2], int(sys.argv[3]); TRIALS = int(sys.argv[4]) if len(sys.argv) > 4 else 3
P = os.path.dirname(os.path.abspath(__file__))
def rz(c, t=120):
    rc, o, e = hpbox.ryzen_root(c, timeout=t); return o
data = open(K, "rb").read()
subprocess.run(hpbox.RZ_ROOT + ["cat > /root/tx-target/x86_64-unknown-none/release/taoxichuan.new && chmod +x /root/tx-target/x86_64-unknown-none/release/taoxichuan.new && mv /root/tx-target/x86_64-unknown-none/release/taoxichuan.new /root/tx-target/x86_64-unknown-none/release/taoxichuan"], input=data, check=True, timeout=300)
CMD = 'sh -c "uname -a; echo hi | cat; for i in 1 2 3 4 5 6 7 8; do (echo f | cat | cat); done; ls / | wc -l; echo ALLDONE"'
tot = ok = 0
for b in range(BOOTS):
    rz("TX_RUN=/root/tx-run2 bash /root/tx-pop.sh stop; sleep 1")
    rz(f"TX_SMP={SMP} TX_APPEND=smp={SMP} TX_DISK=/root/tx-run/goose.orig TX_RUN=/root/tx-run2 bash /root/tx-pop.sh boot fresh >/dev/null; sleep 20")
    res = []
    for t in range(TRIALS):
        try:
            r = subprocess.run(["ssh", "-o", "StrictHostKeyChecking=no", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-p", "2224", "root@192.168.1.126", CMD], capture_output=True, text=True, timeout=30)
            good = "ALLDONE" in r.stdout
        except subprocess.TimeoutExpired:
            good = False
        res.append(good); tot += 1; ok += good
        if not good: break
    bad = rz("grep -acE 'Fault|PANIC|sig!' /root/tx-run2/console.log").strip()
    print(f"boot {b}: {res} faults={bad}", flush=True)
    if not all(res) or bad != "0":
        rz(f"cp /root/tx-run2/console.log /root/tx-fail-{b}.log")
print(f"{K.split('/')[-1]} smp={SMP}: {ok}/{tot}")
