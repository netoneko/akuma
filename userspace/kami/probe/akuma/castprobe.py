#!/usr/bin/env python3
"""In-guest: kami's CDP sequence (navigate, then Page.startScreencast) against
headless Chromium on Akuma, with timeouts so a silent browser cannot hang it.

  castprobe.py plain|bg [url]

Prints "== cast:" lines (run-fc.sh keeps those): every CDP event name with a
count, and for each screencast frame its size, alpha range and mean RGB. The
first frames are saved as /tmp/shot-cast-<mode>-N.png so run-fc.sh dumps them.
Written 2026-10-08: on the trashcan's metal kami's first frame was fully
transparent and no second frame arrived; the Firecracker smoke test had only
ever taken one-shot --screenshot, never this path.
"""
import base64, collections, io, os, select, sys, time

os.environ.setdefault("OUT", "/tmp")
sys.path.insert(0, "/")
import cdp  # noqa: E402
from PIL import Image  # noqa: E402

mode = sys.argv[1] if len(sys.argv) > 1 else "plain"
url = sys.argv[2] if len(sys.argv) > 2 else "file:///tmp/page.html"
W, H = 1920, 1080


def say(*a):
    print("== cast:", mode, os.environ.get("CHROME_EXTRA", ""), *a, flush=True)


c = cdp.Chrome(W, H)
try:
    c.attach()
    c.viewport(W, H, 1)
    if mode == "bg":
        c.call("Emulation.setDefaultBackgroundColorOverride",
               {"color": {"r": 255, "g": 255, "b": 255, "a": 1}})
    t0 = time.monotonic()
    load = c.goto(url, settle=0.5)
    say("loadEventFired after %.1fs" % load)
    # The same page through Page.captureScreenshot, for comparison: if this has
    # pixels and the screencast does not, the fault is the screencast path.
    shot = base64.b64decode(c.call("Page.captureScreenshot", {"format": "png"})["data"])
    sim = Image.open(io.BytesIO(shot)).convert("RGBA")
    slo, shi = sim.getchannel("A").getextrema()
    say("captureScreenshot %d bytes %dx%d alpha %d..%d meanRGB %s" %
        (len(shot), sim.width, sim.height, slo, shi,
         [round(sum(ch.getdata()) / (sim.width * sim.height)) for ch in sim.split()[:3]]))
    c.events.clear()
    c.call("Page.startScreencast", {"format": "png", "everyNthFrame": 1,
                                    "maxWidth": W, "maxHeight": H})
    say("screencast started at +%.1fs" % (time.monotonic() - t0))
    names = collections.Counter()
    frames = 0
    end = time.monotonic() + 8
    while time.monotonic() < end:
        # a full message may already be buffered
        if b"\0" not in c.buf:
            r, _, _ = select.select([c.fr_r], [], [], 1.0)
            if not r:
                continue
        m = c.recv()
        name = m.get("method", "(reply)")
        names[name] += 1
        if name != "Page.screencastFrame":
            continue
        p = m["params"]
        c.send("Page.screencastFrameAck", {"sessionId": p["sessionId"]})
        data = base64.b64decode(p["data"])
        frames += 1
        im = Image.open(io.BytesIO(data)).convert("RGBA")
        lo, hi = im.getchannel("A").getextrema()
        mean = [round(sum(ch.getdata()) / (W * H)) for ch in im.split()[:3]] if frames <= 3 else None
        say("frame %d +%.1fs %d bytes %dx%d alpha %d..%d meanRGB %s" %
            (frames, time.monotonic() - t0, len(data), im.width, im.height, lo, hi, mean))
        if frames <= 3:
            with open("/tmp/shot-cast-%s-%d.png" % (mode, frames), "wb") as f:
                f.write(data)
    say("frames:", frames, "events:", dict(names))
finally:
    c.close()
