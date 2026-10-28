"""steer_seen.py SESSION_ID MARKER - run ON THE BOX (python3 /tmp/steer_seen.py ID TAG < /dev/null).

Finds the steer "[steer TAG] ..." in goose's session database and reports whether the agent has had a turn
since: "SEEN id=N consumed=yes|no (K assistant messages after it)". Exit 0 = the message is in the session,
1 = it is not there. consumed=no right after a steer is normal while the agent is inside a long tool call
or model call: it reads steers at its next turn.
"""
import sqlite3
import sys

sid, tag = sys.argv[1], sys.argv[2]
db = sqlite3.connect("/root/.local/share/goose/sessions/sessions.db")
row = db.execute("select id from messages where session_id=? and role='user' and content_json like ? order by id limit 1",
                 (sid, f"%[steer {tag}]%")).fetchone()
if not row:
    print(f"NOT SEEN: no '[steer {tag}]' message in session {sid}")
    sys.exit(1)
after = db.execute("select count(*) from messages where session_id=? and role='assistant' and id>?", (sid, row[0])).fetchone()[0]
print(f"SEEN id={row[0]} consumed={'yes' if after else 'no'} ({after} assistant messages after it)")
