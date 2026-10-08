#!/usr/bin/env python3
"""In-guest: record a Chromium trace (all categories) around loading
/tmp/page.html and print the font-related events — which FontDataService
functions ran, how often, and their args. Linux is the control.
Written 2026-10-08: system-font text is blank on Akuma with Chromium 152.
"""
import collections, json, os, sys, time
os.environ.setdefault("OUT", "/tmp")
sys.path.insert(0, "/")
import cdp  # noqa: E402

def say(*a):
    print("== fonttrace:", *a, flush=True)

c = cdp.Chrome(900, 300)
events = []
try:
    c.attach()
    c.viewport(900, 300, 1)
    c.call("Tracing.start", {"traceConfig": {"recordMode": "recordAsMuchAsPossible",
                                              "includedCategories": ["*"]},
                             "transferMode": "ReportEvents"}, session=False)
    load = c.goto("file:///tmp/page.html", settle=1.0)
    say("loaded after %.1fs" % load)
    c.send("Tracing.end", {}, session=False)
    end = time.monotonic() + 60
    done = False
    while not done and time.monotonic() < end:
        m = c.recv()
        if m.get("method") == "Tracing.dataCollected":
            events.extend(m["params"]["value"])
        elif m.get("method") == "Tracing.tracingComplete":
            done = True
    say("trace events:", len(events), "complete" if done else "INCOMPLETE")
    names = collections.Counter()
    first = {}
    for e in events:
        n = e.get("name", "")
        if "ont" in n and ("Font" in n or "font" in n):
            names[(n, e.get("ph"))] += 1
            first.setdefault((n, e.get("ph")), e)
    for (n, ph), k in sorted(names.items()):
        say("%4d  %s [%s]" % (k, n, ph))
    for key, e in first.items():
        a = json.dumps(e.get("args", {}))[:300]
        if a != "{}":
            say("args of", key[0], a)
finally:
    try:
        c.close()
    except Exception:  # noqa: BLE001
        pass
