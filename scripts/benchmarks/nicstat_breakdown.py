#!/usr/bin/env python3
"""Where does the time in a `[NICSTAT]` window actually go?

`bench_nic_rtt.py --nicstat` reports the raw counters; this turns them into a
time budget. Two rules make the output mean something:

  * **Wall vs core time.** A window is `dt` ms of wall clock but `dt * SMP` ms of
    core time. `relax` (threads parked in WFI) and `poll` (time inside
    `smoltcp_net::poll`) are accumulated PER THREAD, so they can and do sum past
    `dt` — they are shares of core time, not of the window.
  * **Nesting.** `tx_wait`, `rx_post` and `rx_done` happen INSIDE `poll`, so they
    are reported as shares of `poll`, never added alongside it. `poll_wait` and
    `wake` are the post-drop `wake_all()` pass and are outside it.

Usage:  scripts/benchmarks/nicstat_breakdown.py logs/ab/split/boot.log [--smp 4]
"""

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bench_nic_rtt as bnr  # noqa: E402


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("logs", nargs="+")
    ap.add_argument("--smp", type=int, default=4)
    ap.add_argument("--min-rx", type=int, default=10000,
                    help="skip windows below this packet count (idle/partial windows)")
    args = ap.parse_args()

    for path in args.logs:
        windows = [w for w in bnr.parse_nicstat(path)
                   if (w.get("rx_pkts") or 0) >= args.min_rx]
        if not windows:
            print(f"{path}: no loaded windows")
            continue
        print(f"\n=== {path} ({len(windows)} loaded windows, SMP={args.smp}) ===")
        for w in windows:
            dt = w.get("dt_ms") or 0
            core = dt * args.smp
            rx = w["rx_pkts"]
            relax = w.get("relax_ms", 0)
            poll = w.get("poll_ms", 0)
            tx_wait = w.get("tx_wait_ms", 0)
            rx_post = w.get("rx_post_ms", 0)
            pollwait = w.get("poll_wait_ms", 0)
            wake = w.get("wake_ms", 0)
            print(f"  w={w['w']} dt={dt}ms rx={rx}p  core budget {core}ms")
            print(f"    parked (relax)     {relax:>6}ms  {100*relax/core:>5.1f}% of core   "
                  f"{w.get('relax',0):>7} parks @ {w.get('relax_us',0):>6.1f}us")
            print(f"    in poll()          {poll:>6}ms  {100*poll/core:>5.1f}% of core   "
                  f"{w.get('poll_calls',0):>7} calls @ {w.get('poll_us_per_call',0):>6.1f}us "
                  f"(max {w.get('poll_max_us',0)}us)")
            if poll:
                print(f"      +- tx_wait       {tx_wait:>6}ms  {100*tx_wait/poll:>5.1f}% of poll   "
                      f"{w.get('tx_us_per_pkt',0):>6.1f}us/pkt (max {w.get('tx_max_us',0)}us)")
                print(f"      +- rx_post       {rx_post:>6}ms  {100*rx_post/poll:>5.1f}% of poll   "
                      f"{w.get('rx_post_us',0):>6.1f}us/pkt")
                other = poll - tx_wait - rx_post - (w.get("rx_done_ms", 0) or 0)
                print(f"      +- stack/other   {other:>6}ms  {100*other/poll:>5.1f}% of poll")
            print(f"    wake_all pass      {pollwait:>6}ms poll_wait + {wake}ms wake")
            print(f"    per packet: {1000*poll/rx:>6.1f}us poll, {1000*relax/rx:>7.1f}us parked, "
                  f"{w.get('poll_calls',0)/rx:>5.2f} polls, {(w.get('laps') or 0)/rx:>5.2f} laps")
    return 0


if __name__ == "__main__":
    sys.exit(main())
