#!/usr/bin/env python3
"""NIC round-trip latency benchmark — Akuma vs Linux, measured through the NIC.

Companion to `scripts/benchmarks/bench_redis.py`, which measures *throughput*.
This one measures the thing `docs/archive/BENCHMARK_PERFORMANCE_ATTEMPT_0.md` §4
identified as the actual ceiling: **how long one network round trip takes**.

    akuma-fwd   ~50 us/round trip      docker-fwd  ~17 us/round trip

That section derived those numbers by dividing redis-benchmark ops/s by pipeline
depth, which is indirect. Here the round trip is measured directly, one sample at
a time, so the distribution (not just the mean) is visible — and p99 vs p50 is
what tells you whether you are looking at a fixed per-packet cost or a wake-up
that sometimes waits for a timer tick.

# The two modes, and why `connect` is the headline

    connect   time from connect() to the SYN-ACK landing.  ONE round trip.
              Answered entirely inside the guest kernel's TCP stack — smoltcp
              completes the handshake with no userspace involvement at all.
              This isolates the NIC + stack path and nothing else, which makes
              it the number to optimise and the number that is honestly
              comparable between two different kernels.

    echo      request/response on an ALREADY ESTABLISHED connection. Also one
              round trip, but a *data* one: the handshake, the accept, and the
              forwarder's own connection setup are all excluded, and a userspace
              server is in the loop. This is the direct measurement of the
              quantity ATTEMPT_0 §4 derived from ops/s, and it is the fairest
              cross-kernel number as long as both sides run the same server
              (redis `PING` is the intended payload -- `redis:alpine` on Docker
              against the same image in an Akuma box).

    http      connect + GET + response + close against `userspace/httpd`.
              Several round trips plus a userspace accept/read/write cycle, so
              it is the end-to-end figure a user would feel. Use it to confirm
              a `connect` win survives contact with a real server; do not use it
              to compare kernels, because the two servers are different programs.

Note on `connect` across a *forwarder*: Docker Desktop's proxy opens a fresh
backend connection per inbound connection, so `--mode connect` charges it a full
userspace connect that QEMU's SLIRP does differently. Measured 2026-08-19:
docker-fwd connect p50 was 137 us against redis, an order above its ~17 us data
round trip. So `connect` is the right metric for *Akuma against itself* (A/B of
two kernel builds) and a poor one for Akuma against Docker; use `echo` for that.

# Arms

Follow `bench_redis.py`'s rule: **compare arm to arm.** Both arms below put the
client on the macOS host and the server in a guest, reached over that guest's
host port forward:

    akuma-fwd    host -> QEMU SLIRP        -> smoltcp -> httpd
    docker-fwd   host -> Docker's proxy    -> Linux   -> any listener

The forwarders are different software (SLIRP is not Docker's purpose-built
proxy), and that is a real caveat on any absolute ratio. It is bounded, though:
§5 of ATTEMPT_0 shows SLIRP carrying 247,525 ops/s on the same path, so SLIRP is
not what caps Akuma at ~20k round trips/s.

`--mode connect` deliberately needs nothing but a listening socket on the far
side, so the *server* program drops out of the comparison entirely.

# Usage

    # Baseline the Linux reference (any container with a published port)
    bench_nic_rtt.py --label docker-fwd --target localhost:6379 --out docker.json

    # Akuma, with the kernel's own NIC counters folded in
    bench_nic_rtt.py --label akuma-base --target localhost:8080 \
        --nicstat logs/akuma.log --out akuma_base.json

    # A/B two Akuma builds
    bench_nic_rtt.py --compare akuma_base.json akuma_noalloc.json

`--nicstat LOG` parses the `[NICSTAT]` windows the kernel prints under the
`net-profile` feature (see `src/nic_profile.rs`) and reports only the windows
that overlap the measurement, so device-level cost per packet can be read
alongside the host-observed latency.

# Reading the result

Latency here is dominated by fixed per-packet costs, not by queueing, so the
median is stable and the useful comparison is p50. A p99 far above p50 (say 20x)
means some round trips waited for a scheduler tick rather than for the wire —
on Akuma that is the `blocking_relax` WFI park, since there is no virtio-net
interrupt (only IRQ 27, the timer, is registered).
"""

from __future__ import annotations

import argparse
import json
import re
import socket
import statistics
import sys
import time

# ---------------------------------------------------------------------------
# Sampling
# ---------------------------------------------------------------------------


def _pct(sorted_us: list[float], q: float) -> float:
    """Nearest-rank percentile. Small sample counts make interpolation dishonest."""
    if not sorted_us:
        return float("nan")
    k = max(0, min(len(sorted_us) - 1, int(round(q * (len(sorted_us) - 1)))))
    return sorted_us[k]


def sample_connect(host: str, port: int, timeout: float) -> float:
    """One TCP handshake, in microseconds.

    The socket is closed with SO_LINGER 0 so the teardown is a single RST rather
    than a FIN exchange: a graceful close would leave the server in TIME_WAIT and
    (on Akuma) occupy a socket slot from a budget of 128, which a few thousand
    samples would exhaust. The RST also keeps the teardown off the measured path.
    """
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
    try:
        t0 = time.perf_counter()
        s.connect((host, port))
        t1 = time.perf_counter()
        return (t1 - t0) * 1e6
    finally:
        s.close()


class EchoConn:
    """A held-open connection for `--mode echo`.

    Opened once and reused for every sample, which is the whole point: it takes
    the handshake and the forwarder's connection setup out of the measurement so
    what is left is one request/response exchange on an established socket.
    `TCP_NODELAY` is mandatory here — without it Nagle would batch the request
    and the number measured would be the delayed-ACK timer, not the stack.
    """

    def __init__(self, host: str, port: int, timeout: float,
                 payload: bytes, expect: bytes):
        self.payload = payload
        self.expect = expect
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.sock.connect((host, port))

    def sample(self) -> float:
        """One exchange, in microseconds.

        Reads until `expect` is seen rather than until a fixed byte count: a
        short read mid-response would otherwise stop the clock early and leave
        the rest of the reply to be mis-attributed to the *next* sample, which
        silently halves the reported latency.
        """
        t0 = time.perf_counter()
        self.sock.sendall(self.payload)
        buf = b""
        while self.expect not in buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise OSError("peer closed mid-exchange")
            buf += chunk
        t1 = time.perf_counter()
        return (t1 - t0) * 1e6

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def sample_http(host: str, port: int, timeout: float, path: str) -> float:
    """One connect + GET + full response read + close, in microseconds.

    HTTP/1.0 with no keep-alive, because that is what `userspace/httpd` speaks:
    the response ends at EOF, so "read until the peer closes" is the correct and
    unambiguous completion signal.
    """
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    try:
        t0 = time.perf_counter()
        s.connect((host, port))
        s.sendall(f"GET {path} HTTP/1.0\r\nHost: {host}\r\n\r\n".encode())
        n = 0
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            n += len(chunk)
        t1 = time.perf_counter()
        if n == 0:
            raise OSError("empty HTTP response")
        return (t1 - t0) * 1e6
    finally:
        s.close()


def run(mode: str, host: str, port: int, count: int, warmup: int,
        timeout: float, path: str, gap_ms: float,
        payload: bytes = b"PING\r\n", expect: bytes = b"PONG") -> dict:
    conn: EchoConn | None = None
    if mode == "echo":
        conn = EchoConn(host, port, timeout, payload, expect)
        fn = lambda h, p, t: conn.sample()  # noqa: E731 - uniform sample signature
    elif mode == "connect":
        fn = sample_connect
    else:
        fn = lambda h, p, t: sample_http(h, p, t, path)  # noqa: E731
    try:
        return _run_loop(fn, mode, host, port, count, warmup, timeout, gap_ms)
    finally:
        if conn is not None:
            conn.close()


def _run_loop(fn, mode: str, host: str, port: int, count: int, warmup: int,
              timeout: float, gap_ms: float) -> dict:

    for _ in range(warmup):
        try:
            fn(host, port, timeout)
        except OSError:
            pass

    samples: list[float] = []
    errors = 0
    t_start = time.time()
    for i in range(count):
        try:
            samples.append(fn(host, port, timeout))
        except OSError as e:
            errors += 1
            if errors <= 3:
                print(f"  ! sample {i}: {e}", file=sys.stderr)
            if errors > count // 4 and errors > 10:
                print("  ! aborting: over 25% of samples failed", file=sys.stderr)
                break
        if gap_ms:
            time.sleep(gap_ms / 1000.0)
    t_end = time.time()

    if not samples:
        raise SystemExit("no successful samples — is anything listening there?")

    srt = sorted(samples)
    wall = t_end - t_start
    return {
        "mode": mode,
        "target": f"{host}:{port}",
        "count": len(samples),
        "errors": errors,
        "wall_s": round(wall, 3),
        "rate_per_s": round(len(samples) / wall, 1) if wall > 0 else 0.0,
        "min_us": round(srt[0], 1),
        "p50_us": round(_pct(srt, 0.50), 1),
        "p90_us": round(_pct(srt, 0.90), 1),
        "p99_us": round(_pct(srt, 0.99), 1),
        "max_us": round(srt[-1], 1),
        "mean_us": round(statistics.fmean(srt), 1),
        "stdev_us": round(statistics.pstdev(srt), 1),
        "t_start": t_start,
        "t_end": t_end,
    }


# ---------------------------------------------------------------------------
# [NICSTAT] parsing
# ---------------------------------------------------------------------------

# The kernel prints three lines per window (src/nic_profile.rs). They are keyed
# by window number so a torn/interleaved serial log still reassembles: console
# output from other threads lands between them routinely.
_L1 = re.compile(
    r"\[NICSTAT\] w=(\d+) dt=(\d+)ms rx=(\d+)p/(\d+)kB tx=(\d+)p/(\d+)kB "
    r"lo=(\d+)p/(\d+)kB drop=(\d+)")
_L2 = re.compile(
    r"\[NICSTAT\] w=(\d+) tx_wait=(\d+)ms\(([\d.]+)us/pkt max=(\d+)us\) "
    r"rx_post=(\d+)ms\(([\d.]+)us\) rx_done=(\d+)ms")
_IRQ = re.compile(
    r"\[NICSTAT\] w=(\d+) nic_irq=(\d+)(?: orphan=(\d+) tx_stall=(\d+))?")
_L3 = re.compile(
    r"\[NICSTAT\] w=(\d+) poll=(\d+)c/(\d+)prog (\d+)ms\(([\d.]+)us/c max=(\d+)us\) "
    r"empty=(\d+) relax=(\d+)/(\d+)ms\(([\d.]+)us\)")


def parse_nicstat(log_path: str) -> list[dict]:
    """Reassemble `[NICSTAT]` windows from a QEMU serial log.

    Opened in binary and decoded with `errors="replace"`: QEMU emits a control
    byte that makes the log non-UTF-8, which is the same reason greps against
    these logs need `-a` (see the repo memory on ugrep binary detection).
    """
    windows: dict[int, dict] = {}
    with open(log_path, "rb") as fh:
        for raw in fh:
            line = raw.decode("utf-8", errors="replace")
            if "[NICSTAT]" not in line:
                continue
            if (m := _L1.search(line)):
                w = windows.setdefault(int(m.group(1)), {"w": int(m.group(1))})
                w.update(dt_ms=int(m.group(2)), rx_pkts=int(m.group(3)),
                         rx_kb=int(m.group(4)), tx_pkts=int(m.group(5)),
                         tx_kb=int(m.group(6)), lo_pkts=int(m.group(7)),
                         lo_kb=int(m.group(8)), tx_drops=int(m.group(9)))
            elif (m := _L2.search(line)):
                w = windows.setdefault(int(m.group(1)), {"w": int(m.group(1))})
                w.update(tx_wait_ms=int(m.group(2)), tx_us_per_pkt=float(m.group(3)),
                         tx_max_us=int(m.group(4)), rx_post_ms=int(m.group(5)),
                         rx_post_us=float(m.group(6)), rx_done_ms=int(m.group(7)))
            elif (m := _IRQ.search(line)):
                w = windows.setdefault(int(m.group(1)), {"w": int(m.group(1))})
                w["nic_irq"] = int(m.group(2))
                if m.group(3) is not None:
                    w["orphan"] = int(m.group(3))
                    w["tx_stall"] = int(m.group(4))
            elif (m := _L3.search(line)):
                w = windows.setdefault(int(m.group(1)), {"w": int(m.group(1))})
                w.update(poll_calls=int(m.group(2)), poll_prog=int(m.group(3)),
                         poll_ms=int(m.group(4)), poll_us_per_call=float(m.group(5)),
                         poll_max_us=int(m.group(6)), rx_empty=int(m.group(7)),
                         relax=int(m.group(8)), relax_ms=int(m.group(9)),
                         relax_us=float(m.group(10)))
    return [windows[k] for k in sorted(windows)]


def busiest_window(windows: list[dict]) -> dict | None:
    """The window that carried the most packets — the one the run happened in.

    Wall-clock correlation is not possible: `[NICSTAT]` timestamps are guest
    uptime, and nothing stamps host time into the serial log. Packet count is a
    sound proxy as long as the VM is otherwise idle, which is the documented way
    to run this (an ssh session left open is enough to pollute it).
    """
    scored = [w for w in windows if w.get("rx_pkts")]
    return max(scored, key=lambda w: w["rx_pkts"]) if scored else None


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------


def print_result(r: dict) -> None:
    print(f"\n  {r['label']}  [{r['mode']}]  {r['target']}")
    print(f"    samples {r['count']}  errors {r['errors']}  "
          f"rate {r['rate_per_s']}/s")
    print(f"    min {r['min_us']:>9.1f} us")
    print(f"    p50 {r['p50_us']:>9.1f} us   <- the comparison number")
    print(f"    p90 {r['p90_us']:>9.1f} us")
    print(f"    p99 {r['p99_us']:>9.1f} us")
    print(f"    max {r['max_us']:>9.1f} us")
    n = r.get("nicstat")
    if n:
        print(f"    NIC window w={n['w']} ({n.get('dt_ms', '?')} ms):")
        print(f"      rx {n.get('rx_pkts')}p/{n.get('rx_kb')}kB   "
              f"tx {n.get('tx_pkts')}p/{n.get('tx_kb')}kB   "
              f"drops {n.get('tx_drops')}")
        print(f"      tx blocking wait  {n.get('tx_us_per_pkt')} us/pkt "
              f"(max {n.get('tx_max_us')} us, {n.get('tx_wait_ms')} ms total)")
        print(f"      rx buffer post    {n.get('rx_post_us')} us/post "
              f"({n.get('rx_post_ms')} ms total)")
        print(f"      poll              {n.get('poll_us_per_call')} us/call, "
              f"{n.get('poll_calls')} calls, {n.get('rx_empty')} empty")
        print(f"      blocking_relax    {n.get('relax')} parks, "
              f"{n.get('relax_us')} us each")
        irq = n.get("nic_irq")
        if irq is not None:
            note = "  <- 0 means the NIC SPI never reached the CPU" if irq == 0 else ""
            print(f"      NIC interrupts    {irq}{note}")
        if n.get("tx_stall") is not None:
            stall = n["tx_stall"]
            note = "" if stall == 0 else "  <- TX ring too shallow; these frames blocked"
            print(f"      TX ring stalls    {stall}{note}   orphan tokens {n.get('orphan')}")


def compare(a: dict, b: dict) -> None:
    print(f"\n  {a['label']}  ->  {b['label']}   [{a['mode']}]")
    print(f"  {'metric':<10} {a['label'][:14]:>14} {b['label'][:14]:>14} "
          f"{'change':>10}")
    print(f"  {'-'*10} {'-'*14} {'-'*14} {'-'*10}")
    for k in ("min_us", "p50_us", "p90_us", "p99_us", "rate_per_s"):
        av, bv = a.get(k), b.get(k)
        if av is None or bv is None:
            continue
        if av == 0:
            chg = "  n/a"
        elif k == "rate_per_s":
            chg = f"{bv / av:+.2f}x"
        else:
            pct = (bv - av) / av * 100.0
            chg = f"{pct:+.1f}%"
        print(f"  {k:<10} {av:>14.1f} {bv:>14.1f} {chg:>10}")
    if a["mode"] != b["mode"]:
        print("\n  ! different modes — these are not comparable")


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--target", default="localhost:8080",
                    help="host:port to measure (default localhost:8080)")
    ap.add_argument("--mode", choices=("connect", "echo", "http"),
                    default="connect",
                    help="connect = one handshake (best for A/B of two Akuma "
                         "builds); echo = one data round trip on a held-open "
                         "connection (best for Akuma vs Linux); "
                         "http = full request against userspace/httpd")
    ap.add_argument("--payload", default="PING\r\n",
                    help="--mode echo: bytes to send each sample "
                         "(default: a redis PING)")
    ap.add_argument("--expect", default="PONG",
                    help="--mode echo: substring that marks the reply complete")
    ap.add_argument("--path", default="/", help="URL path for --mode http")
    ap.add_argument("-n", "--count", type=int, default=500)
    ap.add_argument("--warmup", type=int, default=20,
                    help="unmeasured samples first (ARP, route cache, page-ins)")
    ap.add_argument("--gap-ms", type=float, default=0.0,
                    help="idle time between samples; >0 measures the COLD path, "
                         "where the guest has gone back to WFI between round trips")
    ap.add_argument("--timeout", type=float, default=5.0)
    ap.add_argument("--label", default=None)
    ap.add_argument("--out", default=None, help="write JSON result here")
    ap.add_argument("--nicstat", default=None,
                    help="QEMU serial log to pull [NICSTAT] windows from")
    ap.add_argument("--compare", nargs=2, metavar=("BASE", "NEW"),
                    help="compare two saved JSON results and exit")
    args = ap.parse_args()

    if args.compare:
        with open(args.compare[0]) as fh:
            a = json.load(fh)
        with open(args.compare[1]) as fh:
            b = json.load(fh)
        compare(a, b)
        return 0

    host, _, port_s = args.target.rpartition(":")
    if not host:
        ap.error("--target must be host:port")
    port = int(port_s)
    label = args.label or f"{host}:{port}"

    print(f"[bench_nic_rtt] {label}: {args.count} x {args.mode} to {host}:{port}")
    r = run(args.mode, host, port, args.count, args.warmup,
            args.timeout, args.path, args.gap_ms,
            args.payload.encode().decode("unicode_escape").encode(),
            args.expect.encode().decode("unicode_escape").encode())
    r["label"] = label
    r["gap_ms"] = args.gap_ms

    if args.nicstat:
        windows = parse_nicstat(args.nicstat)
        if not windows:
            print(f"  ! no [NICSTAT] windows in {args.nicstat} — is the kernel "
                  f"built with --features net-profile?", file=sys.stderr)
        else:
            r["nicstat"] = busiest_window(windows)
            r["nicstat_windows"] = len(windows)

    print_result(r)

    if args.out:
        with open(args.out, "w") as fh:
            json.dump(r, fh, indent=2)
        print(f"\n  wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
