#!/usr/bin/env python3
"""Multi-process / multi-thread SQLite WAL stress, run INSIDE the guest with any python3
(on the trashcan: /root/.local/share/uv/python/cpython-3.14.8-linux-x86_64-musl/bin/python3).

    python3 sqlite_wal_stress.py threads 6 40     # one process, 6 connections  -> integrity ok
    python3 sqlite_wal_stress.py procs   4 40     # 4 processes, one WAL db     -> "malformed" until
                                                  # MAP_SHARED file mappings are coherent across
                                                  # processes (userspace/forktest/c_stress/shmcoh.c)

Measured 2026-10-03: threads ok, procs corrupt, WITH real fcntl record locks in place — so the
locks (crates/akuma-reclock) are necessary and not sufficient.
docs/archive/AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md section 15.
"""
import sqlite3, threading, os, sys, time, random
mode = sys.argv[1] if len(sys.argv) > 1 else "threads"   # threads | procs
nw = int(sys.argv[2]) if len(sys.argv) > 2 else 6
secs = int(sys.argv[3]) if len(sys.argv) > 3 else 40
path = f"/tmp/walstress-{mode}.db"
for s in ("", "-wal", "-shm"):
    try: os.unlink(path+s)
    except FileNotFoundError: pass
c = sqlite3.connect(path); c.execute("pragma journal_mode=wal"); c.execute("create table t(id integer primary key, w int, n int, b blob)"); c.commit(); c.close()
stop = time.time() + secs
errs = []; counts = [0]*nw
def work(i):
    try:
        c = sqlite3.connect(path, timeout=30); c.execute("pragma synchronous=normal")
        n = 0
        while time.time() < stop:
            c.execute("begin immediate")
            for _ in range(5):
                c.execute("insert into t(w,n,b) values(?,?,?)", (i, n, os.urandom(3000))); n += 1
            c.commit()
            if n % 50 == 0: c.execute("select count(*), sum(length(b)) from t").fetchone()
        counts[i] = n; c.close()
    except Exception as e:
        errs.append(f"w{i}: {type(e).__name__}: {e}")
if mode == "threads":
    ts = [threading.Thread(target=work, args=(i,)) for i in range(nw)]
    [t.start() for t in ts]; [t.join() for t in ts]
else:
    pids = []
    for i in range(nw):
        p = os.fork()
        if p == 0:
            work(i); os._exit(1 if errs else 0)
        pids.append(p)
    bad = sum(1 for p in pids if os.waitpid(p, 0)[1] != 0)
    if bad: errs.append(f"{bad} worker process(es) failed")
c = sqlite3.connect(path)
res = c.execute("pragma integrity_check").fetchall()
n = c.execute("select count(*) from t").fetchone()[0]
print(f"mode={mode} workers={nw} errors={errs[:3]} rows={n} integrity={res[:3]} wal={os.path.getsize(path+'-wal') if os.path.exists(path+'-wal') else 0}")
