import os
import subprocess,sys,time
sys.path.insert(0,os.path.join(os.path.dirname(os.path.abspath(__file__)),"..","..","utils")); import hpbox
K,SMP=sys.argv[1],sys.argv[2]; INIT=sys.argv[4] if len(sys.argv)>4 else '/root/tx/sshd-fork-linuxabi'; WAIT=int(sys.argv[3]) if len(sys.argv)>3 else 8
P = os.path.dirname(os.path.abspath(__file__))
data=open(K,"rb").read()
d="/root/tx-target-sig/x86_64-unknown-none/release/taoxichuan"
subprocess.run(hpbox.RZ_ROOT+[f"cat > {d}.new && chmod +x {d}.new && mv {d}.new {d}"],input=data,check=True,timeout=300)
hpbox.ryzen_root("for r in 3 4 5; do TX_RUN=/root/tx-run$r bash /root/tx-pop.sh stop; done; sleep 1",timeout=60)
print(hpbox.ryzen_root(f"TX_TARGET=/root/tx-target-sig TX_PORT=2225 TX_RUN=/root/tx-run5 TX_SMP={SMP} TX_APPEND=smp={SMP} TX_MEM=3072 TX_INIT={INIT} bash /root/tx-pop.sh boot fresh; sleep 25",timeout=120)[1].strip())
SSH=["ssh","-o","StrictHostKeyChecking=no","-o","BatchMode=yes","-o","ConnectTimeout=20","-p","2225","root@192.168.1.126"]
script=open(P+"/fp.sh").read().splitlines()
pre="; ".join(l for l in script if l.strip() and not l.startswith("cargo") and not l.startswith("date") and not l.startswith("md5sum"))
t0=time.time()
r=subprocess.run(SSH+["sh -c '"+pre.replace("'","'\\''")+"; echo CHECKOUT-DONE'"],capture_output=True,text=True,timeout=600)
print("checkout",int(time.time()-t0),"s rc",r.returncode,flush=True)
CMD='sh -c "export HOME=/root PATH=/usr/local/rust/bin:\\$PATH; cd /tmp/gh; date +%s > /tmp/b.log; cargo build --release --target x86_64-unknown-none -p taoxichuan --features no-tests,trace-err -j4 --target-dir /tmp/tgt >> /tmp/b.log 2>&1; echo EXIT=\\$? >> /tmp/b.log; date +%s >> /tmp/b.log"'
p=subprocess.Popen(SSH+[CMD],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
for i in range(WAIT):
    time.sleep(20)
    try:
        q=subprocess.run(SSH+["sh -c 'ls /tmp/tgt 2>&1 | head -2; tail -c 250 /tmp/b.log'"],capture_output=True,text=True,timeout=60)
        print(int(time.time()-t0),"|",q.stdout.strip().replace("\n"," / ")[:260],flush=True)
    except Exception as e: print(int(time.time()-t0),"poll failed",flush=True)
