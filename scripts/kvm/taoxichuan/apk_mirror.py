#!/usr/bin/env python3 -I
"""apk_mirror.py [PORT] - the Alpine mirror the TCG battery's images name (`http://10.0.2.2:8130/main`).

The battery images' `/etc/apk/repositories` is `http://10.0.2.2:8130/main`, and the battery's `apk` and `git`
lines expect Alpine v3.24 (`git 2.54.0`). This serves `/main/...` and `/community/...` by fetching
`https://dl-cdn.alpinelinux.org/alpine/v3.24/...` once and caching each file under TX_RUN/apk-cache (the index
included, never refreshed), so a rerun is offline, gives the same `apk` output, and does not depend on a mirror
someone left running.

`tcg_rig.py battery` starts it in the background when nothing listens on the port; run it by hand to keep one up.
Environment: APK_MIRROR_UPSTREAM (default https://dl-cdn.alpinelinux.org/alpine/v3.24), TX_RUN (cache parent).
"""
import http.server
import os
import sys
import urllib.request

UPSTREAM = os.environ.get("APK_MIRROR_UPSTREAM", "https://dl-cdn.alpinelinux.org/alpine/v3.24")
RUN = os.environ.get("TX_RUN", os.path.join(os.environ.get("TMPDIR", "/tmp"), "tx-tcg"))
CACHE = os.path.join(RUN, "apk-cache")


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        path = self.path.split("?", 1)[0]
        parts = [p for p in path.split("/") if p]
        # Only repository files: no `..`, no absolute escapes.
        if not parts or parts[0] not in ("main", "community") or any(p in (".", "..") for p in parts):
            self.send_error(404)
            return
        local = os.path.join(CACHE, *parts)
        # Fetched once, then served from the cache - the index included, so the battery's `apk` line stays
        # the same until someone deletes the cache on purpose (a fresh index can change the package count).
        if not os.path.exists(local):
            try:
                with urllib.request.urlopen(UPSTREAM + "/" + "/".join(parts), timeout=120) as r:
                    data = r.read()
            except Exception as e:  # noqa: BLE001 - any upstream failure is a 502 to the guest
                self.send_error(502, str(e)[:200])
                return
            else:
                os.makedirs(os.path.dirname(local), exist_ok=True)
                tmp = local + ".part"
                with open(tmp, "wb") as f:
                    f.write(data)
                os.replace(tmp, local)
        with open(local, "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        pass


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8130
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
