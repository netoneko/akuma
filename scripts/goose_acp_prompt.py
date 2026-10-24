"""Send one long-running prompt to a `goose serve` ACP endpoint and log the turn.

Like goose_acp_probe.py, but for stage briefs that may run for an hour or more:
the SSE streams are reopened on read timeouts (a `kbuild` on the box produces no
stream traffic for minutes), and every event is appended to a log file so the
turn can be followed with `tail -f` while it runs.

Usage:
    python3 -I scripts/goose_acp_prompt.py BRIEF.md --out LOG.txt [--cwd /root]
        [--budget 10800] [--name STAGE]

Env: GOOSE_ACP (default http://192.168.1.159:3284/acp)

Exit 0 when the prompt's JSON-RPC response arrives (any stopReason); nonzero on
budget exhaustion or a protocol failure before the prompt was accepted.
docs/archive/GOOSE_GLM_SERVE_ON_RYZEN.md
"""
import argparse
import json
import os
import sys
import threading
import time
import urllib.request

H = os.environ.get("GOOSE_ACP", "http://192.168.1.159:3284/acp")
HDR = {"content-type": "application/json", "accept": "application/json, text/event-stream"}

ap = argparse.ArgumentParser()
ap.add_argument("brief", help="file with the prompt text")
ap.add_argument("--out", required=True, help="log file, appended")
ap.add_argument("--cwd", default="/root")
ap.add_argument("--budget", type=int, default=10800, help="seconds to wait for the turn to finish")
ap.add_argument("--name", default="prompt", help="label for log lines")
args = ap.parse_args()

with open(args.brief) as f:
    prompt_text = f.read()

lock = threading.Lock()
msgs = []          # every parsed SSE data: payload, both streams
done = threading.Event()


def log(line):
    line = f"[{time.strftime('%H:%M:%S')}] [{args.name}] {line}"
    with lock:
        with open(args.out, "a") as f:
            f.write(line + "\n")


def post(body, conn=None, sess=None, timeout=60):
    h = dict(HDR)
    if conn:
        h["acp-connection-id"] = conn
    if sess:
        h["acp-session-id"] = sess
    r = urllib.request.urlopen(urllib.request.Request(H, json.dumps(body).encode(), h), timeout=timeout)
    return r.headers.get("acp-connection-id"), r.read()


def record(m):
    with lock:
        msgs.append(m)
    try:
        if m.get("method") == "session/update":
            u = m["params"]["update"]
            kind = u.get("sessionUpdate")
            if kind == "agent_message_chunk":
                t = u.get("content", {}).get("text", "")
                if t.strip():
                    log("agent: " + t.strip()[:200])
            elif kind == "tool_call":
                log("tool_call: " + str(u.get("title"))[:200])
            elif kind == "tool_call_update":
                st = u.get("status")
                if st in ("completed", "failed"):
                    log(f"tool {st}: " + str(u.get("title") or u.get("toolCallId"))[:200])
    except Exception:
        pass


def stream_loop(headers, label):
    """Keep a SSE stream open; reopen on read timeouts until `done`."""
    while not done.is_set():
        try:
            r = urllib.request.urlopen(urllib.request.Request(H, headers=headers), timeout=180)
            log(f"{label} stream open")
            for raw in r:
                if done.is_set():
                    return
                line = raw.decode(errors="replace").strip()
                if line.startswith("data:"):
                    try:
                        record(json.loads(line[5:]))
                    except Exception:
                        pass
        except Exception as e:
            if done.is_set():
                return
            log(f"{label} stream dropped ({type(e).__name__}); reopening")
            time.sleep(3)


def wait_response(id_, budget):
    end = time.time() + budget
    while time.time() < end and not done.is_set():
        with lock:
            for m in msgs:
                if m.get("id") == id_:
                    return m
        time.sleep(1)
    return None


conn, _ = post({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}})
if not conn:
    print("no acp-connection-id from initialize", file=sys.stderr)
    sys.exit(2)
log(f"connected: {conn}")

threading.Thread(target=stream_loop, args=(
    {"accept": "text/event-stream", "acp-connection-id": conn}, "conn"), daemon=True).start()
time.sleep(1)

post({"jsonrpc": "2.0", "id": 2, "method": "session/new",
      "params": {"cwd": args.cwd, "mcpServers": []}}, conn)
r = wait_response(2, 120)
if not r or "result" not in r:
    print(f"session/new failed: {r}", file=sys.stderr)
    sys.exit(2)
sid = r["result"]["sessionId"]
log(f"session: {sid}")

threading.Thread(target=stream_loop, args=(
    {"accept": "text/event-stream", "acp-connection-id": conn,
     "acp-session-id": sid}, "session"), daemon=True).start()
time.sleep(1)

post({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
      "params": {"sessionId": sid,
                 "prompt": [{"type": "text", "text": prompt_text}]}}, conn, sid)
log(f"prompt sent ({len(prompt_text)} chars), waiting up to {args.budget}s")

r = wait_response(3, args.budget)
done.set()
if not r:
    log("BUDGET EXHAUSTED, no stopReason")
    print("budget exhausted", file=sys.stderr)
    sys.exit(1)
log("turn finished: " + json.dumps(r.get("result", r))[:400])
text = "".join(
    m["params"]["update"].get("content", {}).get("text", "")
    for m in msgs
    if m.get("method") == "session/update"
    and m["params"]["update"].get("sessionUpdate") == "agent_message_chunk")
with open(args.out, "a") as f:
    f.write(f"\n===== final agent text ({args.name}) =====\n{text}\n")
print("stopReason:", r.get("result", {}).get("stopReason"))
