#!/usr/bin/env python3
"""Drive a local Chrome over --remote-debugging-pipe with kami's exact input
messages, to check what kami's keys and hints do to a real page.

    python3 keyprobe.py [URL] [CHROME]

Prints the URL after Tab+Enter (kami's `send_key`), then runs src/hints.js's
collect/draw/click and clicks the first hint with mouse events.
"""
import json, os, subprocess, sys, time

URL = sys.argv[1] if len(sys.argv) > 1 else "https://example.com/"
CHROME = sys.argv[2] if len(sys.argv) > 2 else "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
HINTS = open(os.path.join(os.path.dirname(__file__), "..", "src", "hints.js")).read()

r1, w1 = os.pipe()  # chrome reads fd 3
r2, w2 = os.pipe()  # chrome writes fd 4
def pre():
    os.dup2(r1, 3); os.dup2(w2, 4)
p = subprocess.Popen([CHROME, "--headless=new", "--disable-gpu", "--remote-debugging-pipe",
                      "--user-data-dir=/tmp/kami-keyprobe", "--no-first-run", "about:blank"],
                     preexec_fn=pre, pass_fds=(3, 4), stderr=subprocess.DEVNULL)
os.close(r1); os.close(w2)
rd = os.fdopen(r2, "rb", buffering=0); wr = os.fdopen(w1, "wb", buffering=0)
buf = b""; nid = 0; session = None

def read_msg(timeout=20):
    global buf
    end = time.time() + timeout
    while b"\0" not in buf:
        if time.time() > end: return None
        buf += rd.read(65536)
    m, buf = buf.split(b"\0", 1)
    return json.loads(m)

def call(method, params=None, sess=True):
    global nid
    nid += 1
    msg = {"id": nid, "method": method, "params": params or {}}
    if sess and session: msg["sessionId"] = session
    wr.write(json.dumps(msg).encode() + b"\0")
    while True:
        m = read_msg()
        if m is None: raise SystemExit(f"timeout on {method}")
        if m.get("id") == nid:
            if "error" in m: raise SystemExit(f"{method}: {m['error']}")
            return m["result"]

def ev(expr):
    return call("Runtime.evaluate", {"expression": expr, "returnByValue": True})["result"].get("value")

def key(name, code, text=""):
    kind = "keyDown" if text else "rawKeyDown"
    t = f',"text":"{text}"' if text else ""
    for k in (kind, "keyUp"):
        call("Input.dispatchKeyEvent", json.loads(
            f'{{"type":"{k}","key":"{name}","code":"{name}","windowsVirtualKeyCode":{code}' + (t if k == kind else "") + "}"))

try:
    tid = call("Target.createTarget", {"url": URL}, False)["targetId"]
    session = call("Target.attachToTarget", {"targetId": tid, "flatten": True}, False)["sessionId"]
    call("Page.enable")
    call("Emulation.setDeviceMetricsOverride", {"width": 960, "height": 540, "deviceScaleFactor": 1, "mobile": False})
    time.sleep(3)
    print("loaded:", ev("location.href"))

    # 1. kami's Tab then Enter.
    key("Tab", 9)
    print("active after Tab:", ev("document.activeElement.tagName + ' ' + (document.activeElement.href||'')"))
    key("Enter", 13, "\\r")
    time.sleep(3)
    print("url after Tab+Enter:", ev("location.href"))

    # 2. hints.js on a fresh load.
    call("Page.navigate", {"url": URL}); time.sleep(3)
    n = ev(HINTS + "\n;__kami.collect()")
    print("hints found:", n)
    labels = json.dumps([f"a{chr(97+i)}"[:1] for i in range(n)])
    print("draw:", ev(HINTS + f"\n;__kami.draw({labels})"))
    pos = ev("__kami.click(0)")
    print("click point:", pos)
    x, y, _ = pos.split(",")
    for kind, extra in (("mouseMoved", {}), ("mousePressed", {"button": "left", "buttons": 1, "clickCount": 1}),
                        ("mouseReleased", {"button": "left", "buttons": 0, "clickCount": 1})):
        call("Input.dispatchMouseEvent", {"type": kind, "x": float(x), "y": float(y), **extra})
    time.sleep(3)
    print("url after hint click:", ev("location.href"))
    # 3. wheel
    call("Page.navigate", {"url": "https://en.wikipedia.org/wiki/Paper"}); time.sleep(4)
    call("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": 480, "y": 270, "deltaX": 0, "deltaY": 600})
    time.sleep(1)
    print("scrollY after wheel 600:", ev("scrollY"))
finally:
    p.kill()
