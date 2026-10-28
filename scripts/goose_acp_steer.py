"""Steer the turn that is running in an EXISTING session of the ryzen `goose serve` (unauthenticated).

Usage: python3 -I scripts/goose_acp_steer.py SESSION_ID TEXT_FILE [CWD] [--marker TAG] [--wait SECS]
Env:   GOOSE_ACP (default http://192.168.1.159:3284/acp)

`session/prompt` into a session that already has an active run is refused ("use _goose/unstable/session/steer"),
and steer wants `expectedRunId`. The server names the active run in the error to a steer with a wrong id, so this
script sends one bogus steer to learn the id, then the real one. Prints the ack (`messageId`) or why not.
SESSION_ID is the `session:` line near the top of the driver log of goose_acp_prompt.py. Used by the supervisor to
send corrections mid-session (docs/archive/MINIMAL_SELF_HOSTING_EXPERIMENT_AGENT_LOG.md, stage 7).
Prefer scripts/kvm/goose/steer.py, which also confirms the message reached the session database: an ack means the
server accepted the steer, not that the agent has read it.

Exit status: 0 only when the server acknowledged the steer with a messageId. Anything else prints a
"NOT DELIVERED: <reason>" line and exits 1 (the server refused) or 2 (no answer: network trouble).
With --marker TAG the text is sent as "[steer TAG] TEXT" so scripts/kvm/goose/steer_seen.py can find it.

HTTP timeouts: a read timeout on the SSE stream is normal (the server is idle between events), so the stream
reconnects with a short back-off instead of dying; a POST that times out or drops is retried a few times; each
step is POLLED for up to --wait seconds (default 120) for its answer rather than blocked on, so this script has
no overall kill timeout and a caller must not wrap it in one (a killed run prints nothing, and a silent kill was
once read as "sent"). A refusal by the server (an HTTP error status or a JSON-RPC error) is never retried: it is
the answer.
"""
import json
import os
import socket
import sys
import threading
import time
import urllib.error
import urllib.request

H = os.environ.get("GOOSE_ACP", "http://192.168.1.159:3284/acp")
HDR = {"content-type": "application/json", "accept": "application/json, text/event-stream"}

_args = sys.argv[1:]


def _opt(name, default=None):
    if name in _args:
        i = _args.index(name)
        val = _args[i + 1]
        del _args[i:i + 2]
        return val
    return default


MARKER = _opt("--marker")
WAIT = int(_opt("--wait", "120"))
if len(_args) < 2:
    print(__doc__)
    sys.exit(3)
SID, TEXT, CWD = _args[0], open(_args[1]).read(), (_args[2] if len(_args) > 2 else "/root")
if MARKER:
    TEXT = f"[steer {MARKER}] " + TEXT

msgs = []
lock = threading.Lock()
done = threading.Event()
NETWORK_ERRORS = (socket.timeout, TimeoutError, ConnectionError, urllib.error.URLError, OSError)


class Refused(Exception):
    """The server answered, and the answer was no."""


def post(body, conn=None, sess=None, tries=3):
    """POST one JSON-RPC message. Network trouble is retried; an HTTP error status is a refusal."""
    h = dict(HDR)
    if conn:
        h["acp-connection-id"] = conn
    if sess:
        h["acp-session-id"] = sess
    last = None
    for attempt in range(tries):
        try:
            r = urllib.request.urlopen(urllib.request.Request(H, json.dumps(body).encode(), h), timeout=30)
            return r.headers.get("acp-connection-id")
        except urllib.error.HTTPError as e:
            raise Refused(f"HTTP {e.code} {e.reason}") from e
        except NETWORK_ERRORS as e:
            last = e
            time.sleep(2 * (attempt + 1))
    raise last


def stream(conn, sess=None):
    """Keep an SSE stream open, reconnecting after read timeouts and drops, until `done`."""
    h = {"accept": "text/event-stream", "acp-connection-id": conn}
    if sess:
        h["acp-session-id"] = sess
    while not done.is_set():
        try:
            for raw in urllib.request.urlopen(urllib.request.Request(H, headers=h), timeout=60):
                if done.is_set():
                    return
                line = raw.decode(errors="replace").strip()
                if line.startswith("data:"):
                    try:
                        m = json.loads(line[5:])
                    except ValueError:
                        continue
                    with lock:
                        msgs.append(m)
        except urllib.error.HTTPError:
            return  # the server refuses this stream; the POST side reports why
        except NETWORK_ERRORS:
            pass  # an idle read timeout or a drop: reconnect
        time.sleep(1)


def wait(i):
    """Poll for the response with JSON-RPC id i; None if it does not come within WAIT seconds."""
    end = time.time() + WAIT
    while time.time() < end:
        with lock:
            for m in msgs:
                if m.get("id") == i:
                    return m
        time.sleep(0.3)
    return None


def steer(i, conn, run_id):
    post({"jsonrpc": "2.0", "id": i, "method": "_goose/unstable/session/steer",
          "params": {"sessionId": SID, "expectedRunId": run_id, "prompt": [{"type": "text", "text": TEXT}]}}, conn, SID)
    return wait(i)


def fail(code, why):
    print(f"NOT DELIVERED: {why}")
    done.set()
    sys.exit(code)


try:
    conn = post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
    threading.Thread(target=stream, args=(conn,), daemon=True).start()
    time.sleep(1)
    post({"jsonrpc": "2.0", "id": 2, "method": "session/load", "params": {"sessionId": SID, "cwd": CWD, "mcpServers": []}}, conn)
    if not wait(2):
        fail(2, f"no answer to session/load within {WAIT} s (is {SID} a session of this server?)")
    threading.Thread(target=stream, args=(conn, SID), daemon=True).start()
    time.sleep(1)
    probe = steer(3, conn, "run_unknown")
    if not probe:
        fail(2, f"no answer to the probe steer within {WAIT} s")
    run_id = ((probe.get("error") or {}).get("data") or {}).get("actualRunId")
    if not run_id:
        fail(1, "no active run in that session: " + json.dumps(probe)[:300])
    ack = steer(4, conn, run_id)
    if not ack:
        fail(2, f"no answer to the steer within {WAIT} s")
    if "error" in ack or not (ack.get("result") or {}).get("messageId"):
        fail(1, "the server refused it: " + json.dumps(ack)[:300])
    print("steer ack:", json.dumps(ack)[:300])
    done.set()
except Refused as e:
    fail(1, f"the server refused the request: {e}")
except NETWORK_ERRORS as e:
    fail(2, f"cannot reach {H}: {type(e).__name__}: {e}")
