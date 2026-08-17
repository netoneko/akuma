#!/usr/bin/env python3
"""Host-side timing server for the delayed-first-byte investigation.

Run this on the macOS host; the guest reaches it at 10.0.2.2:<port> over SLIRP
with no `hostfwd` rule (guest -> host is always allowed). It is the target for
`nettest-std` and `nettest-reqwest` (`userspace/nettest/rust/`), and it is the
"host side" the minimal repro in `docs/archive/SOCKET_DELAYED_FIRST_BYTE_HANG.md`
asks for -- generalised, so one server covers the whole sweep instead of a
per-delay one-liner.

    scripts/net_delay_server.py                 # listen on 0.0.0.0:18080
    scripts/net_delay_server.py --port 18081
    scripts/net_delay_server.py --verbose       # log every request + write

Routes
------
  GET /health                 200 immediately. Liveness, and the "instant reply"
                              control that every ruled-out test in the archive
                              doc used.
  GET /delay/<secs>           Read the request, sleep <secs>, THEN send the
                              whole response. Nothing at all crosses the wire
                              during the sleep -- this is a delayed FIRST byte.
  GET /gap/<pre>/<gap>        Send headers + a first chunk after <pre> seconds,
                              sleep <gap>, send a second chunk, close. The
                              connection is established and has already carried
                              data before the long idle window, so this
                              separates "delayed first byte" from "any long idle
                              window", which the archive doc explicitly could
                              not distinguish.
  GET /sse/<gap>/<n>          <n> SSE events, <gap> seconds apart, then
                              `data: [DONE]`. Shaped like the Ollama/OpenAI
                              streaming response nca actually consumes.
  GET /drip/<total>/<n>       <n> equal chunks spread over <total> seconds.
                              Steady streaming -- the control that the archive
                              doc reports as always healthy.
  GET /big/<mb>[/<secs>]      <mb> MiB of body after an optional <secs> delay.
                              For the RX-throughput question: Akuma posts ONE
                              2 KB virtio RX buffer at a time, so a big body
                              measures how fast poll-driven RX actually drains.
  POST <any of the above>     Same behaviour; the request body is read and
                              discarded first. Lets a probe test a large
                              REQUEST (nca's failing case had a ~2900-token
                              system prompt) against a delayed response.

Every response carries `Connection: close` so a client can read to EOF without
implementing chunked decoding -- except /gap, /sse and /drip, which must be
chunked to deliver anything before the body is complete.

Threaded, so a sweep that leaves a socket parked for 35 s does not block the
next request.
"""

import argparse
import socket
import socketserver
import sys
import threading
import time

VERBOSE = False


def log(msg: str) -> None:
    if VERBOSE:
        print(f"[delay-server] {time.strftime('%H:%M:%S')} {msg}", flush=True)


def clamp(value: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, value))


class Handler(socketserver.StreamRequestHandler):
    # A guest that hangs must hang against a server that is still there. Long
    # enough to outlast the biggest delay in the probes' default ladder (35 s)
    # plus the kernel's own 30 s blocking-recv cap.
    timeout = 300

    # ---- wire helpers ------------------------------------------------------

    def send_raw(self, data: bytes, what: str) -> None:
        self.wfile.write(data)
        self.wfile.flush()
        log(f"{self.client_address[0]} <- {what} ({len(data)}B)")

    def send_head(self, status: str, headers: dict) -> None:
        head = f"HTTP/1.1 {status}\r\n"
        for k, v in headers.items():
            head += f"{k}: {v}\r\n"
        head += "\r\n"
        self.send_raw(head.encode(), f"head {status}")

    def send_full(self, body: bytes, ctype: str = "text/plain") -> None:
        self.send_head(
            "200 OK",
            {
                "Content-Type": ctype,
                "Content-Length": str(len(body)),
                "Connection": "close",
            },
        )
        self.send_raw(body, "body")

    def send_chunk(self, data: bytes) -> None:
        self.send_raw(b"%x\r\n%s\r\n" % (len(data), data), "chunk")

    def end_chunks(self) -> None:
        self.send_raw(b"0\r\n\r\n", "last-chunk")

    def start_chunked(self, ctype: str = "text/plain") -> None:
        self.send_head(
            "200 OK",
            {
                "Content-Type": ctype,
                "Transfer-Encoding": "chunked",
                "Cache-Control": "no-cache",
                "Connection": "close",
            },
        )

    def fail(self, status: str, msg: str) -> None:
        body = msg.encode()
        self.send_head(
            status,
            {
                "Content-Type": "text/plain",
                "Content-Length": str(len(body)),
                "Connection": "close",
            },
        )
        self.send_raw(body, "error")

    # ---- request parsing ---------------------------------------------------

    def read_request(self):
        """Return (method, path) or None. Drains any request body."""
        line = self.rfile.readline(65536)
        if not line:
            return None
        try:
            method, path, _ = line.decode("latin-1").split(" ", 2)
        except ValueError:
            return None
        # Logged HERE, before the headers and body are consumed. Logging only
        # after the drain made a client that sent its headers but stalled
        # mid-body indistinguishable from one that never connected at all —
        # which is exactly the state a write-side hang leaves behind.
        log(f"{self.client_address[0]} -> REQUEST-LINE {method} {path}")

        headers = {}
        while True:
            h = self.rfile.readline(65536)
            if h in (b"\r\n", b"\n", b""):
                break
            try:
                k, v = h.decode("latin-1").split(":", 1)
                headers[k.strip().lower()] = v.strip()
            except ValueError:
                pass

        # Drain the body so a probe that POSTs a large prompt is measuring the
        # response delay and not a server that stopped reading mid-request.
        n = int(headers.get("content-length", "0") or 0)
        if n:
            log(f"{self.client_address[0]} -> expecting {n}B body")
        got = 0
        last_report = 0
        while got < n:
            block = self.rfile.read(min(16384, n - got))
            if not block:
                log(f"{self.client_address[0]} !! body TRUNCATED at {got}/{n}B (peer stopped)")
                break
            got += len(block)
            # Progress every 16 KiB: a stall partway through the body is the
            # signature of a lost EPOLLOUT edge, and the byte count says exactly
            # how much made it through before the writer went to sleep.
            if got - last_report >= 16384 or got == n:
                log(f"{self.client_address[0]} -> body {got}/{n}B")
                last_report = got
        if n:
            log(f"{self.client_address[0]} -> drained {got}/{n}B request body")
        return method, path

    # ---- routes ------------------------------------------------------------

    def handle(self):
        try:
            self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        except OSError:
            pass
        req = self.read_request()
        if req is None:
            return
        method, path = req
        log(f"{self.client_address[0]} -> {method} {path}")
        try:
            self.route(path)
        except (BrokenPipeError, ConnectionResetError) as e:
            log(f"{self.client_address[0]} !! client went away: {e}")

    def route(self, path: str) -> None:
        parts = [p for p in path.split("?")[0].split("/") if p]

        if not parts or parts[0] == "health":
            self.send_full(b"ok\n")
            return

        kind = parts[0]
        args = parts[1:]

        def num(i, default=0.0, lo=0.0, hi=600.0):
            try:
                return clamp(float(args[i]), lo, hi)
            except (IndexError, ValueError):
                return default

        if kind == "delay":
            secs = num(0, 0.0)
            log(f"sleeping {secs}s before the first byte")
            time.sleep(secs)
            self.send_full(f"delayed {secs}s\n".encode())

        elif kind == "gap":
            pre = num(0, 0.0)
            gap = num(1, 10.0)
            time.sleep(pre)
            self.start_chunked()
            self.send_chunk(f"first after {pre}s\n".encode())
            log(f"idling {gap}s mid-stream")
            time.sleep(gap)
            self.send_chunk(f"second after {gap}s gap\n".encode())
            self.end_chunks()

        elif kind == "sse":
            gap = num(0, 1.0)
            count = int(num(1, 5.0, 1, 1000))
            self.start_chunked("text/event-stream")
            for i in range(count):
                time.sleep(gap)
                self.send_chunk(
                    f'data: {{"i":{i},"gap":{gap}}}\n\n'.encode()
                )
            self.send_chunk(b"data: [DONE]\n\n")
            self.end_chunks()

        elif kind == "drip":
            total = num(0, 5.0)
            count = int(num(1, 10.0, 1, 10000))
            per = total / count
            self.start_chunked()
            for i in range(count):
                time.sleep(per)
                self.send_chunk(f"chunk {i}/{count}\n".encode())
            self.end_chunks()

        elif kind == "big":
            mb = int(num(0, 1.0, 1, 512))
            secs = num(1, 0.0)
            time.sleep(secs)
            # One allocation, sent as one write: the guest sees a single burst,
            # which is the regime where a one-buffer-at-a-time RX path shows up
            # as throughput collapse rather than as an error.
            self.send_full(b"A" * (mb * 1024 * 1024), "application/octet-stream")

        else:
            self.fail("404 Not Found", f"unknown route {path}\n")


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main() -> int:
    global VERBOSE
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--port", type=int, default=18080)
    ap.add_argument("--host", default="0.0.0.0")
    ap.add_argument("--verbose", "-v", action="store_true")
    args = ap.parse_args()
    VERBOSE = args.verbose

    srv = Server((args.host, args.port), Handler)
    print(
        f"[delay-server] listening on {args.host}:{args.port} "
        f"— guest reaches it at http://10.0.2.2:{args.port}",
        flush=True,
    )
    print(
        "[delay-server] routes: /health /delay/<s> /gap/<pre>/<gap> "
        "/sse/<gap>/<n> /drip/<total>/<n> /big/<mb>[/<s>]",
        flush=True,
    )
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        print("\n[delay-server] bye", flush=True)
    finally:
        srv.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
