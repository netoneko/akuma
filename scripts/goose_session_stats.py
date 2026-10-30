"""Per-session metrics for a goose ACP session, read from goose's sessions.db.

Run it ON the box (python3 + sqlite3 exist there; the ssh link is too slow to copy the 16 MB db):

    ssh -p 2222 root@192.168.1.159 'python3 - "Taoxichuan s7"' < scripts/goose_session_stats.py

The argument is a substring of the session name (goose names a session after its first prompt).
One block per matching session, so two sessions of the same stage can be compared side by side.
Protocol and why these numbers: docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT.md § "Session protocol".
"""
import collections
import json
import re
import sqlite3
import sys

DB = "/root/.local/share/goose/sessions/sessions.db"
pat = sys.argv[1] if len(sys.argv) > 1 else "Taoxichuan"
db = sqlite3.connect(DB)

for sid, name, created, updated, tin, tout in db.execute(
        "select id,name,created_at,updated_at,accumulated_input_tokens,accumulated_output_tokens "
        "from sessions where name like ? order by created_at", (f"%{pat}%",)):
    calls, first_edit, doc_reads, src_reads = [], None, collections.Counter(), 0
    boots = commits = pushes = 0
    for (cj,) in db.execute("select content_json from messages where session_id=? and role='assistant' order by id", (sid,)):
        try:
            parts = json.loads(cj)
        except ValueError:
            continue
        for p in parts:
            if p.get("type") != "toolRequest":
                continue
            v = p["toolCall"].get("value") or {}
            n, a = v.get("name", "?"), v.get("arguments") or {}
            calls.append(n)
            if n in ("edit", "write") and first_edit is None:
                first_edit = len(calls)
            if n != "shell":
                continue
            c = a.get("command", "")
            for m in re.findall(r"docs/[\w./-]+\.md", c):
                if re.search(r"\b(sed|cat|head|grep|rg)\b", c):
                    doc_reads[m] += 1
            if re.search(r"\b(sed|cat|head|grep|rg)\b[^|;&]*\.rs\b", c):
                src_reads += 1
            boots += len(re.findall(r"herd (start|enable) vm-dev", c))
            commits += len(re.findall(r"git commit", c))
            pushes += len(re.findall(r"git push litter", c))
    print(f"== {name}  [{created} .. {updated}]")
    print(f"   tool calls {len(calls)} (shell {calls.count('shell')}, edit {calls.count('edit')}, write {calls.count('write')}); "
          f"calls before first edit: {first_edit}")
    print(f"   input tokens {(tin or 0)/1e6:.1f}M, output {(tout or 0)/1e3:.0f}k")
    print(f"   docs/*.md reads {sum(doc_reads.values())} over {len(doc_reads)} files; source reads {src_reads}")
    print(f"   vm-dev boots {boots}, git commits {commits}, pushes to litter {pushes}")
    print(f"   top docs: {doc_reads.most_common(5)}")
