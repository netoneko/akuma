"""Drive a `goose serve` (ACP over HTTP) end to end: initialize, session/new, one prompt.

Replies arrive on SSE streams, not in the POST response (202): one connection-scoped
and one session-scoped. Prints the agent text. docs/archive/GOOSE_GLM_SERVE_ON_RYZEN.md
"""
import json, sys, threading, urllib.request, time
import os
H = os.environ.get("GOOSE_ACP", "http://192.168.1.159:3284/acp")
HDR = {"content-type": "application/json", "accept": "application/json, text/event-stream"}
def post(body, conn=None, sess=None):
    h = dict(HDR)
    if conn: h["acp-connection-id"] = conn
    if sess: h["acp-session-id"] = sess
    r = urllib.request.urlopen(urllib.request.Request(H, json.dumps(body).encode(), h), timeout=30)
    return r.headers.get("acp-connection-id"), r.read()
conn, b = post({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})
msgs = []
def stream():
    r = urllib.request.urlopen(urllib.request.Request(H, headers={"accept":"text/event-stream","acp-connection-id":conn}), timeout=120)
    for line in r:
        line = line.decode().strip()
        if line.startswith("data:"):
            msgs.append(json.loads(line[5:]))
threading.Thread(target=stream, daemon=True).start()
time.sleep(1)
def wait(id_, t=90):
    end = time.time() + t
    while time.time() < end:
        for m in msgs:
            if m.get("id") == id_: return m
        time.sleep(0.3)
post({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/root","mcpServers":[]}}, conn)
r = wait(2); print("session/new:", json.dumps(r)[:300])
sid = r["result"]["sessionId"]
def stream2():
    rr = urllib.request.urlopen(urllib.request.Request(H, headers={"accept":"text/event-stream","acp-connection-id":conn,"acp-session-id":sid}), timeout=150)
    for line in rr:
        line = line.decode().strip()
        if line.startswith("data:"):
            msgs.append(json.loads(line[5:]))
threading.Thread(target=stream2, daemon=True).start()
time.sleep(1)
post({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":"Reply with the single word: pong"}]}}, conn, sid)
r = wait(3, 150); print("prompt done:", json.dumps(r)[:300])
text = "".join(m["params"]["update"].get("content",{}).get("text","") for m in msgs if m.get("method")=="session/update" and m["params"]["update"].get("sessionUpdate")=="agent_message_chunk")
print(len(msgs),"msgs")
print("agent said:", text)
