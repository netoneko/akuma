#!/usr/bin/env python3
"""In-guest: which platform font does Chromium pick for system-font text, and
how wide does that text measure? Separates "the font lookup failed" (width 0 /
no platform font) from "glyphs were chosen but never painted".
Written 2026-10-08, ryzen: system-font text is blank on Akuma with Chromium 152.
"""
import base64, os, sys, time
os.environ.setdefault("OUT", "/tmp")
sys.path.insert(0, "/")
import cdp  # noqa: E402

def say(*a):
    print("== fontcdp:", *a, flush=True)

c = cdp.Chrome(900, 300)
try:
    c.attach()
    c.viewport(900, 300, 1)
    load = c.goto("file:///tmp/page.html", settle=1.0)
    say("loaded after %.1fs" % load)
    c.call("DOM.enable", {})
    c.call("CSS.enable", {})
    doc = c.call("DOM.getDocument", {"depth": -1})["root"]["nodeId"]
    nodes = c.call("DOM.querySelectorAll", {"nodeId": doc, "selector": "div"})["nodeIds"]
    for n in nodes:
        try:
            r = c.call("CSS.getPlatformFontsForNode", {"nodeId": n})
            say("node", n, [(f.get("familyName"), f.get("glyphCount"), f.get("isCustomFont")) for f in r.get("fonts", [])])
        except Exception as e:  # noqa: BLE001
            say("node", n, "error", e)
    js = """(() => { const out = [];
      for (const f of ['serif','sans-serif','monospace','Helvetica','Noto Sans','DejaVu Sans','Liberation Sans']) {
        const ctx = document.createElement('canvas').getContext('2d');
        ctx.font = '26px ' + f; out.push(f + '=' + ctx.measureText('The quick brown fox').width.toFixed(1)); }
      for (const d of document.querySelectorAll('div')) out.push('div ' + d.getBoundingClientRect().width.toFixed(0));
      return out.join(' | '); })()"""
    r = c.call("Runtime.evaluate", {"expression": js, "returnByValue": True})
    say("widths:", r.get("result", {}).get("value"))
    shot = base64.b64decode(c.call("Page.captureScreenshot", {"format": "png"})["data"])
    open("/tmp/shot.png", "wb").write(shot)
    say("screenshot", len(shot), "bytes")
finally:
    try:
        c.close()
    except Exception:  # noqa: BLE001
        pass
