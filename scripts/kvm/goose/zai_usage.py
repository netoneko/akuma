#!/usr/bin/env python3
"""zai_usage.py - z.ai quota and token use for the goose-glm runs (read-only, Mac side).

    python3 -I scripts/kvm/goose/zai_usage.py            # quota percent, resets, tokens in the two windows
    python3 -I scripts/kvm/goose/zai_usage.py --since "2026-10-26 17:00"   # tokens since a UTC time

Run it at the start and the end of every session and write both lines into the session's metrics entry in the agent log:
the delta is the session's real cost against the cap. Measured 2026-10-26 (plan "lite"): weekly cap about 205M tokens,
5-hour cap about 34M. The token is read from ~/.akuma/z.ai/token and only ever sent to api.z.ai; it is never printed.

Hourly buckets from the usage endpoint are UTC+8 and the current hour is partial, so a window total is exact only to the hour.
"""
import json
import os
import sys
import urllib.parse
import urllib.request
import datetime as dt

BASE = "https://api.z.ai/api/monitor/usage"
UTC = dt.timezone.utc
TZ8 = dt.timezone(dt.timedelta(hours=8))


def get(path):
    tok = open(os.path.expanduser("~/.akuma/z.ai/token")).read().strip()
    req = urllib.request.Request(BASE + path, headers={"Authorization": "Bearer " + tok, "Accept-Language": "en-US,en"})
    with urllib.request.urlopen(req, timeout=20) as r:
        return json.loads(r.read().decode())["data"]


def window_tokens(start_utc, now):
    """-> (total tokens, calls, {model: tokens}) inside [start_utc, now], per model: GLM-5.3 weighs more than -Flash."""
    a = (start_utc - dt.timedelta(hours=2)).astimezone(TZ8).strftime("%Y-%m-%d 00:00:00")
    b = (now + dt.timedelta(days=1)).astimezone(TZ8).strftime("%Y-%m-%d 23:59:59")
    q = urllib.parse.urlencode({"startTime": a, "endTime": b})
    d = get("/model-usage?" + q.replace("+", "%20"))
    xs = d["x_time"]
    calls = 0.0
    for x, c in zip(xs, d["modelCallCount"]):
        s = _bucket(x)
        if s:
            calls += (c or 0) * _frac(s, start_utc, now)
    per = {}
    for m in d.get("modelDataList", []):
        t = 0.0
        for x, v in zip(xs, m["tokensUsage"]):
            s = _bucket(x)
            if s and v:
                t += v * _frac(s, start_utc, now)
        per[m["modelName"]] = t
    return sum(per.values()), calls, per


def _bucket(x):
    try:
        return dt.datetime.strptime(x, "%Y-%m-%d %H:%M").replace(tzinfo=TZ8)
    except ValueError:
        return None  # a daily-granularity answer: keep ranges short


def _frac(s, lo_utc, hi_utc):
    lo, hi = max(s, lo_utc), min(s + dt.timedelta(hours=1), hi_utc)
    return max((hi - lo) / dt.timedelta(hours=1), 0.0)


def main():
    now = dt.datetime.now(UTC)
    lim = get("/quota/limit")
    out = []
    for row in lim["limits"]:
        if row["type"] != "TOKENS_LIMIT":
            continue
        reset = dt.datetime.fromtimestamp(row["nextResetTime"] / 1000, UTC)
        span = dt.timedelta(hours=5) if row["unit"] == 3 else dt.timedelta(days=7)
        label = "5h " if row["unit"] == 3 else "week"
        tok, calls, per = window_tokens(reset - span, now)
        split = "  [" + ", ".join(f"{k} {v/1e6:.1f}M" for k, v in per.items() if v) + "]"
        out.append(f"{label}: {row['percentage']:3d}%  used {tok/1e6:6.1f}M tokens ({calls:.0f} calls)  resets {reset:%m-%d %H:%M}Z{split}")
    stamp = f"{now:%Y-%m-%d %H:%M}Z"
    if "--since" in sys.argv:
        s = dt.datetime.strptime(sys.argv[sys.argv.index("--since") + 1], "%Y-%m-%d %H:%M").replace(tzinfo=UTC)
        tok, calls, per = window_tokens(s, now)
        split = "  [" + ", ".join(f"{k} {v/1e6:.1f}M" for k, v in per.items() if v) + "]"
        out.append(f"since {s:%m-%d %H:%M}Z: {tok/1e6:.1f}M tokens ({calls:.0f} calls){split}")
    print(stamp + "  plan=" + str(lim.get("level")))
    print("\n".join(out))


if __name__ == "__main__":
    main()
