"""Per-session goose cost, queried ON the box (scp does not work against Akuma's sshd).

Run it there:  python3 goose_box_stats.py [SINCE]      # SINCE = 'YYYY-MM-DD', default today (UTC)
Push it with:  B=$(base64 < scripts/kvm/goose_box_stats.py | tr -d '\n'); wifi-ssh.py ... "echo $B | base64 -d > /tmp/gbs.py; python3 /tmp/gbs.py"

Columns: session, name, wall (created -> updated), LLM calls, input/output/cache-read tokens, largest context.
The lowest-tier GLM plan is metered by tokens and time worked, so these are the numbers the experiment doc records
after each session. Copies sessions.db (+ wal/shm) to /tmp/gbs first: the live files are being written.
"""
import datetime as dt
import os
import shutil
import sqlite3
import sys

since = sys.argv[1] if len(sys.argv) > 1 else dt.datetime.utcnow().strftime("%Y-%m-%d")
d = "/tmp/gbs"
os.makedirs(d, exist_ok=True)
for f in ("sessions.db", "sessions.db-wal", "sessions.db-shm"):
    shutil.copy("/root/.local/share/goose/sessions/" + f, d + "/" + f)
c = sqlite3.connect(d + "/sessions.db")
fmt = "%Y-%m-%d %H:%M:%S"
q = ("select s.id, s.name, s.created_at, s.updated_at, count(u.session_id), sum(u.input_tokens), sum(u.output_tokens),"
     " sum(u.cache_read_tokens), max(u.input_tokens) from sessions s join usage_ledger u on u.session_id=s.id"
     " where s.created_at>=? group by s.id order by s.created_at")
tot = [0, 0, 0, 0, 0]
for sid, name, cr, up, n, i, o, cache, mx in c.execute(q, (since,)):
    wall = (dt.datetime.strptime(up, fmt) - dt.datetime.strptime(cr, fmt)).total_seconds()
    print(f"{sid:12} {name[:28]:28} wall={wall/60:6.1f}m calls={n:4} in={i:9} out={o:7} cache={cache:9} max_ctx={mx}")
    for k, v in enumerate((wall, n, i, o, cache)):
        tot[k] += v
print(f"TOTAL wall={tot[0]/60:.1f}m calls={tot[1]} in={tot[2]} out={tot[3]} cache_read={tot[4]}")
