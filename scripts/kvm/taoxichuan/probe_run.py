import os
import subprocess,sys,time
sys.path.insert(0,os.path.join(os.path.dirname(os.path.abspath(__file__)),"..","..","utils")); import hpbox
K,SMP,INIT=sys.argv[1],sys.argv[2],sys.argv[3]
data=open(K,"rb").read()
d="/root/tx-target-sig/x86_64-unknown-none/release/taoxichuan"
subprocess.run(hpbox.RZ_ROOT+[f"cat > {d}.new && chmod +x {d}.new && mv {d}.new {d}"],input=data,check=True,timeout=300)
hpbox.ryzen_root("for r in 3 4 5 6; do TX_RUN=/root/tx-run$r bash /root/tx-pop.sh stop; done; sleep 1",timeout=60)
print(hpbox.ryzen_root(f"TX_TARGET=/root/tx-target-sig TX_PORT=2225 TX_RUN=/root/tx-run5 TX_SMP={SMP} TX_APPEND=smp={SMP} TX_MEM=3072 TX_INIT={INIT} bash /root/tx-pop.sh boot fresh; sleep 25",timeout=120)[1].strip())
SSH=["ssh","-o","StrictHostKeyChecking=no","-o","BatchMode=yes","-o","ConnectTimeout=20","-p","2225","root@192.168.1.126"]
def run(cmd,t=90):
    try:
        r=subprocess.run(SSH+[cmd],capture_output=True,text=True,timeout=t); return r.stdout.strip().replace("\n"," | ")
    except Exception: return "TIMEOUT"
print("fetch:",run('sh -c "cd /tmp; for n in groupexit_thread pipewake spawnprobe spawnalias; do wget -q -O $n http://10.0.2.2:8099/$n; chmod +x $n; done; ls -la groupexit_thread pipewake | cut -c1-60"'))
for m in (0,1,2):
    print("groupexit mode",m,":",run(f'sh -c "cd /tmp; timeout 10 ./groupexit_thread {m}; echo rc=\\$?"',40))
for n in ("pipewake","spawnprobe","spawnalias"):
    print(n,":",run(f'sh -c "cd /tmp; timeout 20 ./{n}; echo rc=\\$?"',60))
