import os
import subprocess,time,sys
PORT=sys.argv[1] if len(sys.argv)>1 else "2225"
SSH=["ssh","-o","StrictHostKeyChecking=no","-o","BatchMode=yes","-o","ConnectTimeout=20","-p",PORT,"root@192.168.1.126"]
def run(c,t=90):
    for _ in range(3):
        try:
            r=subprocess.run(SSH+[c],capture_output=True,text=True,timeout=t)
            if r.stdout.strip(): return r.stdout.strip().replace("\n"," | ")[:900]
        except Exception: pass
        time.sleep(2)
    return "(no output)"
print("fetch:",run('cd /tmp; for n in groupexit_thread pipewake spawnprobe spawnalias; do wget -q -O $n http://10.0.2.2:8099/$n; chmod +x $n; done; ls -la groupexit_thread pipewake spawnprobe spawnalias | wc -l'))
for m in (0,1,2):
    print("groupexit",m,":",run(f'cd /tmp; timeout 10 ./groupexit_thread {m}; echo rc=$?',40),flush=True)
for n in ("pipewake","spawnprobe","spawnalias"):
    print(n,":",run(f'cd /tmp; timeout 40 ./{n} 2>&1 | tail -6; echo rc=$?',90),flush=True)
