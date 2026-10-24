"""Send one prompt to an EXISTING session of a secured local `goose serve` and wait for the turn to end.

Usage: GOOSE_ACP_KEY_FILE=~/.akuma/goose/acp_secret_key python3 -I scripts/goose_acp_say.py SESSION_ID TEXT_FILE CWD
Prints the load result and the turn's stopReason/usage. Unlike goose_acp_prompt.py it does not open a
new session, so the agent keeps its context. The server is 127.0.0.1:3285 (scripts/kvm/goose/goose-kimi-mac.sh).
"""
import json, os, sys, threading, time, urllib.request
H = "http://127.0.0.1:3285/acp"
K = open(os.path.expanduser(os.environ.get("GOOSE_ACP_KEY_FILE", "~/.akuma/goose/acp_secret_key"))).read().strip()
HDR = {"content-type": "application/json", "accept": "application/json, text/event-stream", "x-secret-key": K}
SID = sys.argv[1]; TEXT = open(sys.argv[2]).read(); CWD = sys.argv[3]
msgs = []
def post(body, conn=None, sess=None):
    h = dict(HDR)
    if conn: h["acp-connection-id"] = conn
    if sess: h["acp-session-id"] = sess
    r = urllib.request.urlopen(urllib.request.Request(H, json.dumps(body).encode(), h), timeout=30)
    return r.headers.get("acp-connection-id"), r.status
def stream(conn, sess=None):
    h = {"x-secret-key": K, "accept": "text/event-stream", "acp-connection-id": conn}
    if sess: h["acp-session-id"] = sess
    for line in urllib.request.urlopen(urllib.request.Request(H, headers=h), timeout=14500):
        line = line.decode().strip()
        if line.startswith("data:"): msgs.append(json.loads(line[5:]))
def wait(i, t=60):
    end = time.time() + t
    while time.time() < end:
        for m in msgs:
            if m.get("id") == i: return m
        time.sleep(0.3)
conn, _ = post({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})
threading.Thread(target=stream, args=(conn,), daemon=True).start(); time.sleep(1)
post({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":SID,"cwd":CWD,"mcpServers":[]}}, conn)
print("load:", json.dumps(wait(2))[:300], flush=True)
threading.Thread(target=stream, args=(conn, SID), daemon=True).start(); time.sleep(1)
post({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":SID,"prompt":[{"type":"text","text":TEXT}]}}, conn, SID)
print("prompt ack:", json.dumps(wait(3, 14400))[:400], flush=True)
