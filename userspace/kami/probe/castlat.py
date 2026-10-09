#!/usr/bin/env python3
"""Key-to-frame latency of kami's CDP sequence against a local Chrome, as the
Linux/macOS expectation for what `scroll_try.py` measures on Akuma.

    python3 castlat.py [URL] [--chrome PATH] [--size WxH] [--fmt png|jpeg] [--n 10]

Launches Chrome over --remote-debugging-pipe with kami's flags (headless, no
GPU), attaches to the first page target, sets the viewport, navigates, starts
the screencast (kami's parameters: everyNthFrame 1, maxWidth/Height = the
page), lets it settle, then N times: Input.dispatchMouseEvent mouseWheel
deltaY 120 at the page centre (kami's `j`), and the time until the next
Page.screencastFrame arrives (each frame is acked at once). Prints the
per-key latencies and their median, plus the frame sizes, so the PNG-encode
share can be judged by running it again with --fmt jpeg or a smaller --size.
"""
import json, os, statistics, subprocess, sys, time

url = "file:///tmp/render.html"
chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
w, h, fmt, n = 1920, 1176, "png", 10
a = sys.argv[1:]
while a:
    x = a.pop(0)
    if x == "--chrome": chrome = a.pop(0)
    elif x == "--size": w, h = map(int, a.pop(0).split("x"))
    elif x == "--fmt": fmt = a.pop(0)
    elif x == "--n": n = int(a.pop(0))
    else: url = x

r1, w1 = os.pipe()  # chrome reads fd 3
r2, w2 = os.pipe()  # chrome writes fd 4
def pre():
    os.dup2(r1, 3); os.dup2(w2, 4)
flags = ["--headless", "--no-sandbox", "--disable-gpu", "--disable-gpu-compositing",
         "--disable-software-rasterizer", "--remote-debugging-pipe", "--no-first-run",
         "--hide-scrollbars", "--mute-audio", f"--window-size={w},{h}",
         "--user-data-dir=/tmp/kami-castlat", "about:blank"]
p = subprocess.Popen([chrome, *flags], preexec_fn=pre, pass_fds=(3, 4), stderr=subprocess.DEVNULL)
os.close(r1); os.close(w2)
rd = os.fdopen(r2, "rb", buffering=0); wr = os.fdopen(w1, "wb", buffering=0)
buf = b""; nid = 0; session = None

def read_msg(timeout=20):
    global buf
    end = time.time() + timeout
    while b"\0" not in buf:
        if time.time() > end: raise TimeoutError("no message")
        chunk = rd.read(1 << 20)
        if not chunk: raise EOFError("chrome closed the pipe")
        buf += chunk
    m, buf = buf.split(b"\0", 1)
    return json.loads(m)

def send(method, params=None, sess=True):
    global nid
    nid += 1
    msg = {"id": nid, "method": method, "params": params or {}}
    if sess and session: msg["sessionId"] = session
    wr.write(json.dumps(msg).encode() + b"\0")
    return nid

def call(method, params=None, sess=True, timeout=20):
    i = send(method, params, sess)
    while True:
        m = read_msg(timeout)
        if m.get("id") == i: return m.get("result", m)

def wait_event(name, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        m = read_msg(timeout)
        if m.get("method") == name: return m
    raise TimeoutError(name)

targets = call("Target.getTargets", sess=False)["targetInfos"]
page = next(t for t in targets if t["type"] == "page")
session = call("Target.attachToTarget", {"targetId": page["targetId"], "flatten": True}, sess=False)["sessionId"]
call("Emulation.setDeviceMetricsOverride", {"width": w, "height": h, "deviceScaleFactor": 1, "mobile": False})
call("Page.enable")
call("Page.navigate", {"url": url})
wait_event("Page.loadEventFired")
call("Page.startScreencast", {"format": fmt, "everyNthFrame": 1, "maxWidth": w, "maxHeight": h}
     | ({"quality": 80} if fmt == "jpeg" else {}))

def pump(seconds):
    """Drain events for `seconds`, acking frames; returns (frames, last size)."""
    end = time.time() + seconds; frames = 0; size = 0
    while time.time() < end:
        try: m = read_msg(max(0.01, end - time.time()))
        except TimeoutError: break
        if m.get("method") == "Page.screencastFrame":
            send("Page.screencastFrameAck", {"sessionId": m["params"]["sessionId"]})
            frames += 1; size = len(m["params"]["data"]) * 3 // 4
    return frames, size

settle_frames, size = pump(2.0)
print(f"settled: {settle_frames} frames in 2 s, last frame ~{size // 1024} KB, {fmt} {w}x{h}")
lat = []
for i in range(n):
    t0 = time.time()
    send("Input.dispatchMouseEvent", {"type": "mouseWheel", "x": w / 2, "y": h / 2,
                                      "deltaX": 0, "deltaY": 120})
    first = None; extra = 0
    while time.time() - t0 < 2.0:
        try: m = read_msg(2.0 - (time.time() - t0))
        except TimeoutError: break
        if m.get("method") == "Page.screencastFrame":
            send("Page.screencastFrameAck", {"sessionId": m["params"]["sessionId"]})
            size = len(m["params"]["data"]) * 3 // 4
            if first is None:
                first = time.time() - t0
                # keep draining briefly for trailing frames of the same scroll
                t_end = time.time() + 0.3
                while time.time() < t_end:
                    try: m2 = read_msg(t_end - time.time())
                    except TimeoutError: break
                    if m2.get("method") == "Page.screencastFrame":
                        send("Page.screencastFrameAck", {"sessionId": m2["params"]["sessionId"]}); extra += 1
                break
    lat.append(first)
    print(f"key {i + 1}: {'%.0f ms' % (first * 1000) if first else 'NO FRAME'}  (+{extra} trailing), frame ~{size // 1024} KB")
    time.sleep(0.7)
ok = [x for x in lat if x]
print(f"castlat: {fmt} {w}x{h}: n={len(ok)} key->frame median {statistics.median(ok) * 1000:.0f} ms, "
      f"min {min(ok) * 1000:.0f}, max {max(ok) * 1000:.0f}; no-frame {len(lat) - len(ok)}")
try: call("Page.stopScreencast", timeout=3)
except Exception: pass
p.kill()
