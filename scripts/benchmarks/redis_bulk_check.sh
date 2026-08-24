#!/bin/bash
# Bulk-transfer guard for the delayed-ACK change.
#
# Turning delayed ACK back on (`set_ack_delay(Some(10ms))`) halves the packets
# on a request/response workload, which is where the throughput ceiling is. The
# comment that originally justified `None` claimed delayed ACK would throttle
# receive-heavy traffic to ~65KB/10ms by making the sender wait for a window
# update. smoltcp 0.12 says otherwise — `immediate_ack_to_transmit()` forces an
# ACK once one full MSS of unacked data has arrived, and `window_to_update()`
# forces one when the receive window doubles — but that is a code reading, and a
# code reading is not a measurement.
#
# So measure it. Large values exercise both directions:
#   SET -d N  the guest RECEIVES N bytes per op   <- the case the comment feared
#   GET -d N  the guest SENDS N bytes per op
#
# Run once per kernel and diff. A real regression shows up as SET falling while
# PING rises; if both hold, the comment was wrong and the change is free.
set -u
PORT="${PORT:-4444}"
LABEL="${1:-unlabelled}"

echo "### bulk check: $LABEL (port $PORT)"
redis-cli -p "$PORT" -t 5 ping >/dev/null || { echo "redis not answering"; exit 1; }

for size in 4096 65536; do
  for t in set get; do
    # -c 8: enough to keep the pipe full without approaching the socket budget
    # (ATTEMPT_0 §7). -n scaled down for the big payload so each cell is seconds.
    n=$(( size >= 65536 ? 20000 : 50000 ))
    out=$(redis-benchmark -h 127.0.0.1 -p "$PORT" -n "$n" -c 8 -d "$size" \
                          -P 1 -t "$t" --csv 2>&1 | tail -1)
    # --csv row is "TEST","rps","avg","min","p50",... — take field 2 by
    # splitting on the quote-comma-quote separator. A greedy `.*,` sed grabs a
    # LATER field (it matches to the last comma) and silently reports a latency
    # percentile as throughput.
    ops=$(echo "$out" | awk -F'","' '{gsub(/"/,"",$2); print $2}')
    # MB/s is the number that matters for bulk; ops/s alone hides payload size.
    mbps=$(python3 -c "print(f'{$ops*$size/1e6:.1f}')" 2>/dev/null || echo "?")
    printf "  %-4s d=%-6s %10s ops/s  %8s MB/s\n" "$t" "$size" "$ops" "$mbps"
    sleep 8
  done
done
