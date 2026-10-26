"""Steer the turn that is running in an EXISTING session of the ryzen `goose serve` (unauthenticated).

Usage: python3 -I scripts/goose_acp_steer.py SESSION_ID TEXT_FILE [CWD]
Env:   GOOSE_ACP (default http://192.168.1.159:3284/acp)

`session/prompt` into a session that already has an active run is refused ("use _goose/unstable/session/steer"),
and steer wants `expectedRunId`. The server names the active run in the error to a steer with a wrong id, so this
script sends one bogus steer to learn the id, then the real one. Prints the ack (`messageId`) or the error.
SESSION_ID is the `session:` line near the top of the driver log of goose_acp_prompt.py. Used by the supervisor to
send corrections mid-session (docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md, stage 7).
"""
import json
import os
import sys
import threading
import time
import urllib.request

H = os.environ.get("GOOSE_ACP", "http://192.168.1.159:3284/acp")
HDR = {"content-type": "application/json", "accept": "application/json, text/event-stream"}
SID, TEXT, CWD = sys.argv[1], open(sys.argv[2]).read(), (sys.argv[3] if len(sys.argv) > 3 else "/root")
msgs = []


def post(body, conn=None, sess=None):
    h = dict(HDR)
    if conn:
        h["acp-connection-id"] = conn
    if sess:
        h["acp-session-id"] = sess
    r = urllib.request.urlopen(urllib.request.Request(H, json.dumps(body).encode(), h), timeout=30)
    return r.headers.get("acp-connection-id")


def stream(conn, sess=None):
    h = {"accept": "text/event-stream", "acp-connection-id": conn}
    if sess:
        h["acp-session-id"] = sess
    for line in urllib.request.urlopen(urllib.request.Request(H, headers=h), timeout=600):
        line = line.decode().strip()
        if line.startswith("data:"):
            msgs.append(json.loads(line[5:]))


def wait(i, t=90):
    end = time.time() + t
    while time.time() < end:
        for m in msgs:
            if m.get("id") == i:
                return m
        time.sleep(0.3)
    return None


def steer(i, conn, run_id):
    post({"jsonrpc": "2.0", "id": i, "method": "_goose/unstable/session/steer",
          "params": {"sessionId": SID, "expectedRunId": run_id, "prompt": [{"type": "text", "text": TEXT}]}}, conn, SID)
    return wait(i)


conn = post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
threading.Thread(target=stream, args=(conn,), daemon=True).start()
time.sleep(1)
post({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": SID, "cwd": CWD, "mcpServers": []}}, conn)
wait(2, 60)
threading.Thread(target=stream, args=(conn, SID), daemon=True).start()
time.sleep(1)
probe = steer(3, conn, "run_unknown") or {}
run_id = ((probe.get("error") or {}).get("data") or {}).get("actualRunId")
if not run_id:
    print("no active run found:", json.dumps(probe)[:300])
    sys.exit(1)
print("steer ack:", json.dumps(steer(4, conn, run_id))[:300])
