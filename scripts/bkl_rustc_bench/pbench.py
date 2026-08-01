"""Host-driven parallel rustc benchmark. Concurrency via N parallel ssh execs, so the
guest shell's `wait` (which hangs on Akuma) is never used."""
import subprocess, sys, time, statistics
from concurrent.futures import ThreadPoolExecutor

SMP = sys.argv[1]
SSH = ['ssh','-o','StrictHostKeyChecking=no','-o','UserKnownHostsFile=/dev/null',
       '-o','ConnectTimeout=15','-o','ServerAliveInterval=30','-p','2222','root@localhost']
CMD = {
 'nostd': '/usr/bin/rustc --crate-type=lib --emit=metadata /tmp/hello_nostd.rs -o /tmp/m{i}.rmeta',
 'std':   '/usr/bin/rustc -O /tmp/hello_std.rs -o /tmp/h{i}',
 'big':   '/usr/bin/rustc -O /tmp/big.rs -o /tmp/b{i}',
}
def one(cmd, t=1800):
    return subprocess.run(SSH + [cmd], capture_output=True, text=True, timeout=t).stdout

ART = {'nostd': '/tmp/m{i}.rmeta', 'std': '/tmp/h{i}', 'big': '/tmp/b{i}'}
MIN = {'nostd': 500, 'std': 100000, 'big': 100000}

def compile_and_verify(mode, i):
    """Run one rustc and PROVE it produced a real artifact. A failed compile is fast and
    would otherwise masquerade as a good measurement (locking.md playbook rule 6)."""
    one(f'/bin/busybox rm -f ' + ART[mode].format(i=i))
    err = one(CMD[mode].format(i=i) + ' 2>&1')
    sz = one('/bin/busybox wc -c < ' + ART[mode].format(i=i) + ' 2>&1').strip()
    try: n = int(sz)
    except ValueError: n = -1
    if n < MIN[mode]:
        raise RuntimeError(f'{mode}[{i}] artifact {n}B (<{MIN[mode]}) rustc_out={err.strip()[:200]!r}')
    return n

for f in ('hello_std.rs','hello_nostd.rs','big.rs'):
    one(f'/bin/busybox wget -q -O /tmp/{f} http://10.0.2.2:8899/{f}')

# ssh round-trip overhead, to report alongside (not subtracted)
ov = []
for _ in range(5):
    t = time.time(); one('/bin/busybox true'); ov.append(time.time()-t)
print(f'RESULT smp={SMP} mode=sshoverhead conc=1 median={statistics.median(ov):.2f}', flush=True)

for mode in ('nostd','std','big'):
    for conc in (1, 4):
        reps = []
        for rep in range(2):
          try:
            t = time.time()
            with ThreadPoolExecutor(max_workers=conc) as ex:
                sizes = list(ex.map(lambda i: compile_and_verify(mode, i), range(conc)))
            reps.append(time.time()-t)
            assert len(set(sizes)) == 1, f'artifact sizes differ: {sizes}'
            print(f'  smp={SMP} {mode} conc={conc} rep={rep} {reps[-1]:.2f}s artifact={sizes[0]}B', flush=True)
          except Exception as e:
            print(f'  FAIL smp={SMP} {mode} conc={conc} rep={rep}: {e}', flush=True)
        if reps:
            print(f'RESULT smp={SMP} mode={mode} conc={conc} median={statistics.median(reps):.2f} n={len(reps)}', flush=True)
        else:
            print(f'RESULT smp={SMP} mode={mode} conc={conc} ALL_REPS_FAILED', flush=True)
print(f'SMP{SMP}_DONE', flush=True)
