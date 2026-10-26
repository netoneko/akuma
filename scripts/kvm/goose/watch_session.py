import sqlite3, json, sys
db = sqlite3.connect('/root/.local/share/goose/sessions/sessions.db')
sid = sys.argv[1]
rows = db.execute("select role, content_json from messages where session_id=? order by id", (sid,)).fetchall()
calls = 0; last_text = ''; last_cmd = ''
for role, cj in rows:
    try: parts = json.loads(cj)
    except Exception: continue
    for p in parts:
        if p.get('type') == 'toolRequest':
            calls += 1; v = (p['toolCall'].get('value') or {}); a = v.get('arguments') or {}
            last_cmd = (a.get('command') or a.get('path') or '')[:140]
        elif p.get('type') == 'text' and role == 'assistant':
            last_text = p.get('text', '')[-260:]
print('msgs', len(rows), 'calls', calls)
print('last cmd:', last_cmd.replace('\n', ' '))
print('last text:', last_text.replace('\n', ' '))
