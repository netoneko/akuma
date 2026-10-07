#!/usr/bin/env python3
"""Pull PNG frames out of headless chromium over CDP (--remote-debugging-pipe).

Speaks CDP on fds 3/4 (NUL-terminated JSON), so no websocket library and no
TCP: the same transport would work on Akuma with nothing but pipes.

  cdp.py bench            screenshots + screencast, timings to /out/results.json
  cdp.py strace URL       one load + one screenshot under strace -f
"""
import base64, io, json, os, statistics, subprocess, sys, time

from PIL import Image

OUT = os.environ.get("OUT", "/out")

ANIM = ("data:text/html,<style>body{margin:0;background:%23123;color:%23eee;"
        "font:48px sans-serif}%23b{width:300px;height:300px;background:%23e64;"
        "animation:s 2s linear infinite;margin:200px}@keyframes s{to{transform:"
        "rotate(360deg)}}</style><div id=b></div><p id=c></p><script>let n=0;"
        "(function f(){document.getElementById('c').textContent='frame '+(n++);"
        "requestAnimationFrame(f)})()</script>")

PAGES = {
    "example": "https://example.com/",
    "wikipedia": "https://en.wikipedia.org/wiki/Linux",
}


class Chrome:
    def __init__(self, w, h, wrap=()):
        to_r, self.to_w = os.pipe()    # us -> chrome fd 3
        self.fr_r, fr_w = os.pipe()    # chrome fd 4 -> us
        args = [*wrap, "chromium", "--headless", "--no-sandbox", "--disable-gpu",
                "--remote-debugging-pipe", f"--window-size={w},{h}",
                "--hide-scrollbars", "--mute-audio", "--no-first-run",
                "--disable-dev-shm-usage", "--user-data-dir=/tmp/prof",
                *os.environ.get("CHROME_EXTRA", "").split(), "about:blank"]
        self.log = open(os.path.join(OUT, "chromium.stderr"), "ab")
        # stdin/stdout carry the pipe ends; the shell moves them to 3/4.
        self.p = subprocess.Popen(
            ["sh", "-c", 'exec "$@" 3<&0 4>&1 </dev/null >/dev/null', "sh", *args],
            stdin=to_r, stdout=fr_w, stderr=self.log)
        os.close(to_r)
        os.close(fr_w)
        self.buf = b""
        self.next_id = 0
        self.events = []
        self.session = None

    def send(self, method, params=None, session=True):
        self.next_id += 1
        msg = {"id": self.next_id, "method": method, "params": params or {}}
        if session and self.session:
            msg["sessionId"] = self.session
        os.write(self.to_w, json.dumps(msg).encode() + b"\0")
        return self.next_id

    def recv(self):
        while b"\0" not in self.buf:
            chunk = os.read(self.fr_r, 1 << 20)
            if not chunk:
                raise EOFError("chromium closed the pipe")
            self.buf += chunk
        msg, self.buf = self.buf.split(b"\0", 1)
        return json.loads(msg)

    def call(self, method, params=None, session=True):
        want = self.send(method, params, session)
        while True:
            m = self.recv()
            if m.get("id") == want:
                if "error" in m:
                    raise RuntimeError(f"{method}: {m['error']}")
                return m["result"]
            if "method" in m:
                self.events.append(m)

    def wait_event(self, name, timeout):
        end = time.monotonic() + timeout
        for i, m in enumerate(self.events):
            if m["method"] == name:
                return self.events.pop(i)
        while time.monotonic() < end:
            m = self.recv()
            if m.get("method") == name:
                return m
            if "method" in m:
                self.events.append(m)
        raise TimeoutError(name)

    def attach(self):
        pages = [t for t in self.call("Target.getTargets", session=False)["targetInfos"]
                 if t["type"] == "page"]
        tid = pages[0]["targetId"] if pages else self.call(
            "Target.createTarget", {"url": "about:blank"}, session=False)["targetId"]
        self.session = self.call("Target.attachToTarget",
                                 {"targetId": tid, "flatten": True},
                                 session=False)["sessionId"]
        self.call("Page.enable")

    def viewport(self, w, h, dsf):
        self.call("Emulation.setDeviceMetricsOverride",
                  {"width": w, "height": h, "deviceScaleFactor": dsf, "mobile": False})

    def goto(self, url, settle=1.5):
        self.events.clear()
        t0 = time.monotonic()
        self.call("Page.navigate", {"url": url})
        self.wait_event("Page.loadEventFired", 45)
        load = time.monotonic() - t0
        time.sleep(settle)
        return load

    def close(self):
        try:
            self.call("Browser.close", session=False)
        except Exception:
            pass
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def decode_cost(data):
    """What the fb side pays: PNG/JPEG -> raw BGRX bytes ready to copy."""
    t0 = time.perf_counter()
    im = Image.open(io.BytesIO(data))
    raw = im.convert("RGB").tobytes("raw", "BGRX")
    return time.perf_counter() - t0, im.size, len(raw)


def shot(c, name, fmt, fast=False, reps=3):
    params = {"format": fmt}
    if fmt == "jpeg":
        params["quality"] = 90
    if fast:
        params["optimizeForSpeed"] = True
    times, data = [], None
    for _ in range(reps):
        t0 = time.perf_counter()
        data = base64.b64decode(c.call("Page.captureScreenshot", params)["data"])
        times.append(time.perf_counter() - t0)
    ext = "jpg" if fmt == "jpeg" else "png"
    with open(os.path.join(OUT, f"{name}.{ext}"), "wb") as f:
        f.write(data)
    dec, size, rawlen = decode_cost(data)
    return {"name": name, "format": fmt, "optimizeForSpeed": fast,
            "capture_ms_median": round(statistics.median(times) * 1e3, 1),
            "bytes": len(data), "raw_bytes": rawlen, "pixels": size,
            "decode_ms": round(dec * 1e3, 1)}


def screencast(c, name, fmt, seconds=5.0, keep=3, max_wh=None):
    params = {"format": fmt, "everyNthFrame": 1}
    if max_wh:
        # without these the frame is scaled down to the window size
        params["maxWidth"], params["maxHeight"] = max_wh
    if fmt == "jpeg":
        params["quality"] = 90
    c.events.clear()
    c.call("Page.startScreencast", params)
    t0 = time.monotonic()
    frames, sizes, decs = 0, [], []
    while time.monotonic() - t0 < seconds:
        m = c.recv()
        if m.get("method") != "Page.screencastFrame":
            continue
        p = m["params"]
        c.send("Page.screencastFrameAck", {"sessionId": p["sessionId"]})
        data = base64.b64decode(p["data"])
        frames += 1
        sizes.append(len(data))
        if frames <= keep:
            ext = "jpg" if fmt == "jpeg" else "png"
            with open(os.path.join(OUT, f"{name}-{frames}.{ext}"), "wb") as f:
                f.write(data)
        if frames % 5 == 1:
            decs.append(decode_cost(data)[0])
    elapsed = time.monotonic() - t0
    c.call("Page.stopScreencast")
    return {"name": name, "format": fmt, "frames": frames,
            "fps": round(frames / elapsed, 2),
            "mean_bytes": int(statistics.mean(sizes)) if sizes else 0,
            "decode_ms_median": round(statistics.median(decs) * 1e3, 1) if decs else None}


def bench():
    results = {"chromium": subprocess.run(["chromium", "--version"], capture_output=True,
                                          text=True).stdout.strip(),
               "screenshots": [], "screencast": [], "loads": {}}
    for (w, h, dsf) in [(1920, 1080, 1), (1920, 1080, 2)]:
        tag = f"{w * dsf}x{h * dsf}"
        c = Chrome(w, h)
        c.attach()
        c.viewport(w, h, dsf)
        for pname, url in PAGES.items():
            results["loads"][f"{pname}@{tag}"] = round(c.goto(url), 2)
            results["screenshots"].append(shot(c, f"{pname}-{tag}-png", "png"))
            results["screenshots"].append(shot(c, f"{pname}-{tag}-pngfast", "png", fast=True))
            results["screenshots"].append(shot(c, f"{pname}-{tag}-jpeg", "jpeg"))
            print(json.dumps(results["screenshots"][-3:]), flush=True)
        c.goto(ANIM, settle=0.5)
        for fmt in ("png", "jpeg"):
            r = screencast(c, f"anim-{tag}-cast-{fmt}", fmt, max_wh=(w * dsf, h * dsf))
            results["screencast"].append(r)
            print(json.dumps(r), flush=True)
        c.close()
    with open(os.path.join(OUT, "results.json"), "w") as f:
        json.dump(results, f, indent=2)


def strace(url):
    wrap = ["strace", "-f", "-qq", "-s", "96", "-o", os.path.join(OUT, "strace.log")]
    c = Chrome(1920, 1080, wrap=wrap)
    c.attach()
    c.viewport(1920, 1080, 1)
    c.goto(url)
    data = shot(c, "strace-run", "png", reps=1)
    print(json.dumps(data))
    c.close()


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    if sys.argv[1] == "bench":
        bench()
    elif sys.argv[1] == "cast4k":
        c = Chrome(1920, 1080)
        c.attach()
        c.viewport(1920, 1080, 2)
        c.goto(ANIM, settle=0.5)
        for fmt in ("png", "jpeg"):
            print(json.dumps(screencast(c, f"anim-3840x2160-cast-{fmt}", fmt, max_wh=(3840, 2160))), flush=True)
        c.close()
    elif sys.argv[1] == "strace":
        strace(sys.argv[2] if len(sys.argv) > 2 else PAGES["example"])
