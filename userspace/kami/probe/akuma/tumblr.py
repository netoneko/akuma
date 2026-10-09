#!/usr/bin/env python3
"""In-guest: load a URL in headless Chromium with kami's flag set and watch
what dies. Prints "== tumblr:" lines: navigation events, every screencast
frame (count, +t, bytes), target crashes, and how the browser ended (pipe
closed -> wait status). The latest frame is kept as /tmp/shot.png.
    tumblr.py [url] [seconds]
"""
import base64, collections, os, select, sys, time
os.environ.setdefault("OUT", "/tmp"); sys.path.insert(0, "/")
import cdp  # noqa: E402
url = sys.argv[1] if len(sys.argv) > 1 else "https://www.tumblr.com/"
dur = float(sys.argv[2]) if len(sys.argv) > 2 else 60
W, H = 1920, 1176
def say(*a): print("== tumblr: T%.2f" % time.clock_gettime(time.CLOCK_MONOTONIC), *a, flush=True)
c = cdp.Chrome(W, H)
t0 = time.monotonic()
names = collections.Counter(); frames = 0
try:
    c.attach(); c.viewport(W, H, 1)
    say("attached +%.1fs browser pid %d" % (time.monotonic() - t0, c.p.pid))
    c.send("Target.setDiscoverTargets", {"discover": True}, session=False)
    c.send("Page.navigate", {"url": url})
    c.send("Page.startScreencast", {"format": "png", "everyNthFrame": 1, "maxWidth": W, "maxHeight": H})
    end = time.monotonic() + dur
    while time.monotonic() < end:
        if b"\0" not in c.buf:
            r, _, _ = select.select([c.fr_r], [], [], 1.0)
            if not r:
                if c.p.poll() is not None:
                    say("browser exited rc=%r +%.1fs" % (c.p.returncode, time.monotonic() - t0)); break
                continue
        try:
            m = c.recv()
        except EOFError:
            rc = c.p.wait()
            say("pipe closed: browser rc=%r +%.1fs" % (rc, time.monotonic() - t0)); break
        name = m.get("method", "(reply)"); names[name] += 1
        if name == "Page.screencastFrame":
            p = m["params"]; c.send("Page.screencastFrameAck", {"sessionId": p["sessionId"]})
            frames += 1; data = base64.b64decode(p["data"])
            if frames <= 3 or frames % 10 == 0: say("frame %d +%.1fs %d bytes" % (frames, time.monotonic() - t0, len(data)))
            with open("/tmp/shot.png", "wb") as f: f.write(data)
        elif name in ("Page.frameNavigated", "Page.loadEventFired", "Page.frameStoppedLoading",
                      "Inspector.targetCrashed", "Target.targetCrashed", "Inspector.detached", "Target.detachedFromTarget"):
            extra = ""
            if name == "Page.frameNavigated": extra = m["params"]["frame"].get("url", "")[:60]
            if name == "Target.targetCrashed": extra = str(m["params"])[:120]
            say(name, "+%.1fs" % (time.monotonic() - t0), extra)
        elif name == "Target.targetCreated" or name == "Target.targetDestroyed":
            ti = m["params"].get("targetInfo", m["params"]); say(name, "+%.1fs" % (time.monotonic() - t0), str(ti)[:100])
    say("frames %d; events %s" % (frames, dict(names)))
finally:
    if c.p.poll() is None:
        say("browser still alive at the end; closing"); c.close()
