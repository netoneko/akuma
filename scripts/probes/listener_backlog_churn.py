#!/usr/bin/env python3
"""Does connect+RST churn permanently break a listening socket?

Written for `docs/archive/NGINX_MISSING_SYSCALLS.md` Issue E1: after a
`bench_nic_rtt.py --mode connect` run (500 handshakes torn down with
`SO_LINGER 0`, i.e. a single RST rather than a FIN exchange), nginx on Akuma
stopped answering *permanently* — every later request got `Connection reset by
peer` until nginx was restarted. That was originally read as nginx's own
free-connection pool degrading. This probe exists to decide between that and
the competing kernel-side explanation.

# The two hypotheses, and what separates them

    nginx-side   nginx leaks an entry of its `worker_connections` pool (default
                 512) for each accepted-then-RST connection. Ceiling ~512.

    kernel-side  Akuma's listener is a *pool of pre-listening smoltcp sockets*
                 (`crates/akuma-net/src/socket.rs`, `MAX_BACKLOG` = 32). A
                 handle that reaches `Established` and is then RST before
                 `accept()` claims it goes to `Closed` and is never returned to
                 `Listen` — `socket_accept` only replaces handles it actually
                 hands out. Ceiling ~32 (or whatever `MAX_BACKLOG` is), and it
                 is independent of the server program.

So the discriminator is simply **how many churned connections it takes**, and
whether the number tracks `MAX_BACKLOG` or `worker_connections`. This probe
escalates the churn count and reports the first count that kills the listener.

**Answered 2026-08-20: kernel-side.** nginx died at 80 cumulative churned
connections and `userspace/httpd` — a blocking accept loop with no connection
pool at all — died at 24, both an order of magnitude below 512. Fixed in
`crates/akuma-net/src/socket.rs` (`listener_refresh`, plus `was_connected` so a
blocking `recv` on a reset connection returns `ECONNRESET` instead of parking
forever); both now survive 1088. Keep this probe as the regression: it is
cheap, it needs nothing inside the guest, and the failure it catches is
permanent and silent.

Because the mechanism under test is entirely in the kernel, the same run against
a Linux listener (Docker, `--target localhost:8080`) is the reference arm: Linux
removes an RST connection from the accept queue, so no count should ever kill it.

# Usage

    # Akuma guest, nginx (or anything) on the forwarded port
    scripts/probes/listener_backlog_churn.py --target localhost:8080

    # Linux reference
    docker run --rm -d -p 18080:80 nginx:alpine
    scripts/probes/listener_backlog_churn.py --target localhost:18080

    # One fixed count instead of the escalation
    scripts/probes/listener_backlog_churn.py --target localhost:8080 --count 40

`--no-accept-delay` churns as fast as the socket API allows, which is the
condition that makes the server *lose* the race to `accept()` before the RST
lands — that is the case that burns a backlog slot. `--pace MS` slows it down;
if a paced run survives a count that an unpaced run kills, the race is confirmed
as the trigger.
"""

from __future__ import annotations

import argparse
import socket
import sys
import time

LINGER_ZERO = b"\x01\x00\x00\x00\x00\x00\x00\x00"


def churn(host: str, port: int, count: int, pace_ms: float, timeout: float) -> tuple[int, int]:
    """`count` connect()s, each torn down with a RST. Returns (ok, failed)."""
    ok = failed = 0
    for _ in range(count):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(timeout)
        s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, LINGER_ZERO)
        try:
            s.connect((host, port))
            ok += 1
        except OSError:
            failed += 1
        finally:
            s.close()
        if pace_ms:
            time.sleep(pace_ms / 1000.0)
    return ok, failed


def alive(host: str, port: int, timeout: float, attempts: int = 3) -> tuple[bool, str]:
    """Can the listener still complete a handshake and answer a GET?

    Retried, and with a graceful close: a single failure right after churn could
    be a transient, and the question here is whether the listener is *permanently*
    dead. Any one success means alive.
    """
    last = "no attempt"
    for i in range(attempts):
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(timeout)
        try:
            s.connect((host, port))
            s.sendall(b"GET / HTTP/1.0\r\nHost: x\r\n\r\n")
            data = s.recv(256)
            if data:
                return True, data.split(b"\r\n", 1)[0].decode(errors="replace")
            last = "connected, empty reply"
        except OSError as exc:
            last = f"{type(exc).__name__}: {exc}"
        finally:
            s.close()
        if i + 1 < attempts:
            time.sleep(0.5)
    return False, last


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target", required=True, help="host:port of the listener under test")
    ap.add_argument("--count", type=int, default=None,
                    help="single churn count instead of the escalating sweep")
    ap.add_argument("--steps", default="8,16,24,32,48,64,128,256,512",
                    help="churn counts to try in order (cumulative, no restart between)")
    ap.add_argument("--pace", type=float, default=0.0, help="ms of sleep between churn connects")
    ap.add_argument("--timeout", type=float, default=5.0)
    args = ap.parse_args()

    host, _, port_s = args.target.rpartition(":")
    port = int(port_s)

    live, detail = alive(host, port, args.timeout)
    print(f"[pre ] listener alive={live} ({detail})")
    if not live:
        print("listener was already dead before churn — nothing to measure")
        return 2

    steps = [args.count] if args.count else [int(x) for x in args.steps.split(",")]
    total = 0
    for n in steps:
        ok, failed = churn(host, port, n, args.pace, args.timeout)
        total += n
        live, detail = alive(host, port, args.timeout)
        print(f"[churn] +{n:4d} (total {total:4d})  connect ok={ok} failed={failed}  "
              f"-> alive={live} ({detail})")
        if not live:
            print(f"\nLISTENER DIED after {total} cumulative churned connections.")
            return 1

    print(f"\nlistener survived {total} churned connections.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
