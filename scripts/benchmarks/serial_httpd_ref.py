#!/usr/bin/env python3
"""A shape-matched Linux reference for `userspace/httpd`.

nginx is the wrong control for Akuma's httpd. nginx is event-driven, multi-worker
and serves from a warm cache; `userspace/httpd` accepts ONE connection at a time,
serves it to completion, closes, and loops. Comparing the two charges Akuma's
kernel for a difference in server architecture.

This server has httpd's exact shape:

  * one thread, one connection at a time, no concurrency at all
  * `accept` -> `recv` -> read the file off disk -> `send` -> `close`
  * HTTP/1.0, `Connection: close`, no keep-alive
  * the file is re-read from the filesystem on every request (no cache)

It is deliberately a *conservative* reference: CPython's per-syscall overhead
makes it slower than a C server of the same shape, so any remaining gap to Akuma
is a floor on the gap, not a ceiling.

Run it in a container with the same CPU and memory budget as the devbox:

    docker run -d --name akuma-httpd-serial --cpuset-cpus=0-3 -m 4g \
        -p 8082:8080 -v "$PWD/scripts/benchmarks":/s:ro -v /tmp/akwww:/www:ro \
        python:alpine python3 /s/serial_httpd_ref.py 8080 /www

then measure it with the same driver as Akuma:

    scripts/benchmarks/bench_nic_rtt.py --mode http --target localhost:8082
"""

import os
import socket
import sys


def main() -> int:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    root = sys.argv[2] if len(sys.argv) > 2 else "/www"

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    # Backlog 8: Akuma's default MAX_BACKLOG, so a burst queues the same way.
    srv.listen(8)
    print(f"serial_httpd_ref: serving {root} on :{port}", flush=True)

    while True:
        conn, _ = srv.accept()
        try:
            conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            req = conn.recv(8192)
            if not req:
                continue
            line = req.split(b"\r\n", 1)[0].decode("latin1")
            parts = line.split()
            path = parts[1] if len(parts) > 1 else "/"
            if ".." in path:
                conn.sendall(b"HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\n"
                             b"Connection: close\r\n\r\n")
                continue
            fs = os.path.join(root, "index.html" if path == "/" else path.lstrip("/"))
            try:
                # Re-read per request, like httpd: no caching in either server.
                with open(fs, "rb") as fh:
                    body = fh.read()
            except OSError:
                conn.sendall(b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n"
                             b"Connection: close\r\n\r\n")
                continue
            hdr = (f"HTTP/1.0 200 OK\r\nContent-Type: text/html\r\n"
                   f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n")
            conn.sendall(hdr.encode() + body)
        except OSError:
            pass
        finally:
            conn.close()


if __name__ == "__main__":
    sys.exit(main())
