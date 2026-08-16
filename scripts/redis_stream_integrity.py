#!/usr/bin/env python3
"""Hunt for stream corruption on Akuma's TCP listener, using Redis as the probe.

Redis is only the responder here; the failure modes being hunted live in the
kernel's socket and write paths, not in Redis.

Two modes, and **the default one is deliberately weak**:

- **default** — many short connections, each sending one `PING` and requiring
  exactly `+PONG\r\n` back. This catches a reply that belongs to a *different*
  connection: a recycled listener-pool socket, or a socket-table slot reused
  under a live fd. That class has bitten this tree before (the socket-fd
  refcount bug that surfaced as SSH MAC errors).
- **`--large`** — one connection per size, writing and reading back values from
  4 KiB to 1 MiB and verifying **every byte**. This catches corruption *within*
  a single reply.

The distinction is not academic. The `writev` short-write splice
(`docs/archive/WRITEV_SHORT_WRITE_SPLICE.md`) corrupted every reply larger than
smoltcp's ~16 KB TX window, and the default mode passed 700 connections against
the broken kernel without a murmur — because `+PONG\r\n` is 7 bytes. **A
negative result is only as strong as the size of the thing it exercised.** Run
`--large` before concluding a socket path is clean.

Client-visible symptom for both classes is the same:

    Error: Protocol error, got "\n" as reply type byte

i.e. the client read a byte that is not a RESP type marker (`+-:$*`) where a
reply should have started.

Usage:
    scripts/redis_stream_integrity.py [--port 4544] [--conns 200] [--parallel 16]
    scripts/redis_stream_integrity.py --port 4544 --large

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


def resp_cmd(*args):
    """Encode a RESP command; `bytes` args pass through untouched."""
    out = b"*%d\r\n" % len(args)
    for a in args:
        if isinstance(a, str):
            a = a.encode()
        out += b"$%d\r\n%s\r\n" % (len(a), a)
    return out


def _read_line(sock):
    line = b""
    while not line.endswith(b"\r\n"):
        c = sock.recv(1)
        if not c:
            raise EOFError("peer closed mid-line")
        line += c
    return line


def _read_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError(f"peer closed after {len(buf)} of {n} bytes")
        buf += chunk
    return buf


def large_reply_check(port, timeout):
    """SET then GET values of increasing size, verifying every byte returned.

    Sizes straddle the 16 KB smoltcp TX window, which is where a write path that
    mishandles short writes starts splicing. The payload is non-repeating so a
    missing run cannot hide behind identical neighbouring bytes.
    """
    failures = 0
    for size in (4096, 16384, 65536, 262144, 1048576):
        payload = bytes((i * 7 + (i >> 8) * 13) & 0xFF for i in range(size))
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        except OSError as e:
            print(f"  {size:>8}: connect failed: {e}")
            failures += 1
            continue
        try:
            s.sendall(resp_cmd("SET", "_integrity_probe", payload))
            reply = _read_line(s)
            if reply != b"+OK\r\n":
                print(f"  {size:>8}: SET rejected: {reply!r}")
                failures += 1
                continue

            s.sendall(resp_cmd("GET", "_integrity_probe"))
            hdr = _read_line(s)
            if not hdr.startswith(b"$"):
                print(f"  {size:>8}: reply is not a bulk string: {hdr!r}")
                failures += 1
                continue
            declared = int(hdr[1:-2])
            body = _read_exact(s, declared + 2)[:-2]

            if declared != size:
                print(f"  {size:>8}: server declared {declared} bytes")
                failures += 1
            elif body != payload:
                at = next(i for i in range(len(payload)) if body[i] != payload[i])
                print(f"  {size:>8}: CORRUPT at byte {at} "
                      f"(sent {payload[at]:#04x}, got {body[at]:#04x})")
                failures += 1
            else:
                print(f"  {size:>8}: OK — byte-exact")
        except (OSError, EOFError) as e:
            print(f"  {size:>8}: io failed: {e}")
            failures += 1
        finally:
            s.close()
    return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=4544,
                    help="host port forwarded to the guest's redis (default 4544 = INSTANCE=1)")
    ap.add_argument("--conns", type=int, default=200, help="total connections")
    ap.add_argument("--parallel", type=int, default=16, help="concurrent connections")
    ap.add_argument("--timeout", type=float, default=30.0)
    ap.add_argument("--large", action="store_true",
                    help="verify single replies of up to 1 MiB byte-for-byte "
                         "(catches within-reply corruption the PING mode cannot)")
    args = ap.parse_args()

    if args.large:
        print(f"large-reply integrity against 127.0.0.1:{args.port}")
        failures = large_reply_check(args.port, args.timeout)
        print("PASS" if not failures else f"FAIL — {failures} size(s) corrupt")
        return 1 if failures else 0

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
