"""Docker counterpart to /tmp/pbench.py — host-side timing + artifact verification, so the
two are measured identically (only the guest differs)."""
import subprocess, time, statistics
from concurrent.futures import ThreadPoolExecutor
CMD = {
 'nostd': 'rustc --crate-type=lib --emit=metadata /work/hello_nostd.rs -o /tmp/m{i}.rmeta',
 'std':   'rustc -O /work/hello_std.rs -o /tmp/h{i}',
 'big':   'rustc -O /work/big.rs -o /tmp/b{i}',
}
ART = {'nostd': '/tmp/m{i}.rmeta', 'std': '/tmp/h{i}', 'big': '/tmp/b{i}'}
MIN = {'nostd': 500, 'std': 100000, 'big': 100000}
def dex(c, t=1800):
    return subprocess.run(['docker','exec','dbench','sh','-c',c],
                          capture_output=True, text=True, timeout=t).stdout
ov=[]
for _ in range(5):
    t=time.time(); dex('true'); ov.append(time.time()-t)
print(f'RESULT docker mode=execoverhead conc=1 median={statistics.median(ov):.2f}', flush=True)
def cv(mode, i):
    dex('rm -f ' + ART[mode].format(i=i))
    err = dex(CMD[mode].format(i=i) + ' 2>&1')
    sz = dex('wc -c < ' + ART[mode].format(i=i) + ' 2>/dev/null').strip()
    n = int(sz) if sz.isdigit() else -1
    if n < MIN[mode]:
        raise RuntimeError(f'{mode}[{i}] artifact {n}B rustc_out={err.strip()[:200]!r}')
    return n
for mode in ('nostd','std','big'):
    for conc in (1,4):
        reps=[]
        for rep in range(2):
            try:
                t=time.time()
                with ThreadPoolExecutor(max_workers=conc) as ex:
                    sizes=list(ex.map(lambda i: cv(mode,i), range(conc)))
                reps.append(time.time()-t)
                print(f'  docker {mode} conc={conc} rep={rep} {reps[-1]:.2f}s artifact={sizes[0]}B', flush=True)
            except Exception as e:
                print(f'  FAIL docker {mode} conc={conc} rep={rep}: {e}', flush=True)
        if reps: print(f'RESULT docker mode={mode} conc={conc} median={statistics.median(reps):.2f} n={len(reps)}', flush=True)
        else: print(f'RESULT docker mode={mode} conc={conc} ALL_REPS_FAILED', flush=True)
print('DOCKER_DONE', flush=True)
