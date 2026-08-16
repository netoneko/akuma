#!/usr/bin/env python3
"""Hunt for cross-stream corruption on Akuma's TCP listener, using Redis as the probe.

Every connection sends one `PING` and must read back exactly `+PONG\\r\\n`.
Anything else is reported with a hexdump — the point is to catch a reply that
belongs to *another* connection, which is what a recycled listener-pool socket or
a socket-table slot reused under a live fd would produce. Redis is only the
responder: the failure mode being hunted is in `crates/akuma-net/src/socket.rs`,
not in Redis.

The symptom this was written for is a client reporting

    Error: Protocol error, got "t" as reply type byte

i.e. it read a byte that is not a RESP type marker (`+-:$*`) where a reply
should start. Seen once, 2026-08-16, on a server that was simultaneously being
hammered by a loop of short-lived raw connections; never reproduced
deliberately. See docs/archive/REDIS_END_TO_END.md §7.

Usage:
    scripts/redis_stream_integrity.py [--port 4544] [--conns 200] [--parallel 16]

Exit status is 0 only if every reply was byte-exact.
"""
import argparse
import socket
import sys
from concurrent.futures import ThreadPoolExecutor

PING = b"*1\r\n$4\r\nPING\r\n"
EXPECT = b"+PONG\r\n"
RESP_TYPES = b"+-:$*"


def one_connection(port, index, timeout):
    """Return None on success, or a description of what came back instead."""
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    except OSError as e:
        return f"conn {index}: connect failed: {e}"
    try:
        s.sendall(PING)
        got = b""
        # Read until we have the full expected reply or the peer stops talking.
        while len(got) < len(EXPECT):
            chunk = s.recv(len(EXPECT) - len(got))
            if not chunk:
                break
            got += chunk
    except OSError as e:
        return f"conn {index}: io failed: {e}"
    finally:
        s.close()

    if got == EXPECT:
        return None
    if not got:
        return f"conn {index}: EMPTY reply (accepted, then silence)"
    kind = "not a RESP type byte" if got[0:1] not in (
        bytes([c]) for c in RESP_TYPES
    ) else "wrong reply"
    return f"conn {index}: {kind}: {got!r}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=4544,
                    help="host port forwarded to the guest's redis (default 4544 = INSTANCE=1)")
    ap.add_argument("--conns", type=int, default=200, help="total connections")
    ap.add_argument("--parallel", type=int, default=16, help="concurrent connections")
    ap.add_argument("--timeout", type=float, default=10.0)
    args = ap.parse_args()

    with ThreadPoolExecutor(max_workers=args.parallel) as pool:
        results = list(pool.map(
            lambda i: one_connection(args.port, i, args.timeout),
            range(args.conns),
        ))

    failures = [r for r in results if r is not None]
    ok = len(results) - len(failures)
    print(f"{ok}/{len(results)} connections returned exactly {EXPECT!r}")
    for f in failures[:20]:
        print(f"  {f}")
    if len(failures) > 20:
        print(f"  ... and {len(failures) - 20} more")

    # An empty reply is a liveness/backlog problem; a non-RESP first byte is the
    # corruption this script exists to catch. Separate them in the exit message
    # so a run that only saw empties is not mistaken for a corruption hit.
    corrupt = [f for f in failures if "type byte" in f or "wrong reply" in f]
    if corrupt:
        print(f"\nCORRUPTION: {len(corrupt)} replies were not valid RESP for this connection")
        return 1
    if failures:
        print(f"\nNo corruption, but {len(failures)} connections did not complete "
              f"(backlog/socket budget — see DEVBOX_ISSUES.md Issue 16)")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
