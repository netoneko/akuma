#!/usr/bin/env python3
"""Multi-process / multi-thread SQLite WAL stress, run INSIDE the guest with any python3
(on the trashcan: /root/.local/share/uv/python/cpython-3.14.8-linux-x86_64-musl/bin/python3).

    python3 sqlite_wal_stress.py threads 6 40     # one process, 6 connections  -> integrity ok
    python3 sqlite_wal_stress.py procs   4 40     # 4 processes, one WAL db     -> "malformed" until
                                                  # MAP_SHARED file mappings are coherent across
                                                  # processes (userspace/forktest/c_stress/shmcoh.c)

    python3 sqlite_wal_stress.py mixed   4 40     # 4 processes x 3 threads, NO pre-created schema
                                                  # (every connection races `create ... if not
                                                  # exists`), an indexed table with updates,
                                                  # deletes and index lookups: goose's shape

Measured 2026-10-03: threads ok, procs corrupt, WITH real fcntl record locks in place — so the
locks (crates/akuma-reclock) are necessary and not sufficient.
docs/archive/AKUMA_AMD64_AGENT_STAGING_AND_ACCOUNTING.md section 15.
"""
import sqlite3, threading, os, sys, time, random, subprocess
mode = sys.argv[1] if len(sys.argv) > 1 else "threads"   # threads | procs | mixed | spawn
# `spawn` is `mixed` plus a child process (`sh -c true`, fork+exec through
# posix_spawn) started from inside an open write transaction every few
# iterations — goose runs its shell tool calls while its sqlx pool is live.
spawn = mode == "spawn"
if spawn:
    mode = "mixed"
nw = int(sys.argv[2]) if len(sys.argv) > 2 else 6
secs = int(sys.argv[3]) if len(sys.argv) > 3 else 40
path = f"/tmp/walstress-{mode}.db"
for s in ("", "-wal", "-shm"):
    try: os.unlink(path+s)
    except FileNotFoundError: pass
SCHEMA = ("create table if not exists t(id integer primary key, w int, n int, tag text, b blob)",
          "create index if not exists t_wn on t(w, n)", "create index if not exists t_tag on t(tag)")
# WAL is set once, here: racing `pragma journal_mode=wal` on a fresh file fails
# with SQLITE_BUSY immediately on Linux and macOS too (it takes no busy wait).
# `mixed` leaves the schema to the workers, who race `create ... if not exists`.
c = sqlite3.connect(path); c.execute("pragma journal_mode=wal")
if mode != "mixed":
    for q in SCHEMA: c.execute(q)
c.commit(); c.close()
stop = time.time() + secs
errs = []; counts = [0]*nw
T0 = time.time()
def work(i):
    last = ["connect"]
    def ex(c, q, a=()):
        last[0] = " ".join(q.split()[:2])
        t = time.time()
        try:
            return c.execute(q, a)
        except sqlite3.OperationalError as e:
            raise sqlite3.OperationalError(f"{e} [{last[0]} after {time.time() - t:.1f}s, t+{time.time() - T0:.0f}s]")
    try:
        c = sqlite3.connect(path, timeout=30, isolation_level=None)
        ex(c, "pragma synchronous=normal")
        if mode == "mixed":
            ex(c, "begin immediate")
            for q in SCHEMA: ex(c, q)
            ex(c, "commit")
        n = 0
        while time.time() < stop:
            ex(c, "begin immediate")
            for _ in range(5):
                ex(c, "insert into t(w,n,tag,b) values(?,?,?,?)",
                          (i, n, f"tag{random.randrange(50)}", os.urandom(random.choice((100, 3000))))); n += 1
            if spawn and n % 20 < 5:
                subprocess.run(["sh", "-c", "true"])
            if mode == "mixed":
                ex(c, "update t set tag=?, b=? where w=? and n=?", (f"tag{random.randrange(50)}", os.urandom(200), i, random.randrange(n)))
                ex(c, "delete from t where w=? and n=?", (i, random.randrange(n)))
            ex(c, "commit")
            if mode == "mixed":
                ex(c, "select count(*) from t where tag=?", (f"tag{random.randrange(50)}",)).fetchone()
                ex(c, "select max(n) from t where w=?", (i,)).fetchone()
            if n % 50 == 0: ex(c, "select count(*), sum(length(b)) from t").fetchone()
        counts[i] = n
    except Exception as e:
        errs.append(f"w{i}: {type(e).__name__}: {e}")
    finally:
        # Close even on error: a worker that dies inside `begin immediate`
        # otherwise holds the write lock until process exit, and every other
        # worker reports `database is locked` 30 s later — a cascade that
        # hides the first error rather than adding new ones.
        try: c.close()
        except Exception: pass
def proc_body(i):
    if mode == "mixed":
        ts = [threading.Thread(target=work, args=(i * 3 + k,)) for k in range(3)]
        [t.start() for t in ts]; [t.join() for t in ts]
    else:
        work(i)
if mode == "mixed":
    counts = [0] * (nw * 3)
if mode == "threads":
    ts = [threading.Thread(target=work, args=(i,)) for i in range(nw)]
    [t.start() for t in ts]; [t.join() for t in ts]
else:
    pids = []
    for i in range(nw):
        p = os.fork()
        if p == 0:
            proc_body(i)
            if errs: print(f"pid {os.getpid()}: {errs[:3]}", flush=True)
            os._exit(1 if errs else 0)
        pids.append(p)
    bad = sum(1 for p in pids if os.waitpid(p, 0)[1] != 0)
    if bad: errs.append(f"{bad} worker process(es) failed")
c = sqlite3.connect(path)
res = c.execute("pragma integrity_check").fetchall()
n = c.execute("select count(*) from t").fetchone()[0]
print(f"mode={mode} workers={nw} errors={errs[:3]} rows={n} integrity={res[:3]} wal={os.path.getsize(path+'-wal') if os.path.exists(path+'-wal') else 0}")
