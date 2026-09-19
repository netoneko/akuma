#!/bin/sh
# Run N one-shot `nca` prompts and report, per run, whether an answer arrived
# and how long it took. Runs INSIDE Akuma (busybox ash, POSIX sh only).
#
# Why this exists rather than a `sleep` in an ssh command. The failure being
# measured is intermittent and slow, and `nca` waits up to
# `provider::STREAM_READ_TIMEOUT_SECS` (300 s) before giving up
# (`docs/archive/AMD64_TRASHCAN_ISSUES.md` §4). A harness that samples for 75 s
# and reports "no answer" cannot tell a stalled stream from a slow one, and on
# 2026-09-19 it scored a run as a failure on exactly that basis. So: wait for
# the process to EXIT, and print the elapsed time next to the verdict.
#
# Timing comes from /proc/uptime, not `date`: this box has no working wall clock
# and the SNTP self-heal can step the clock mid-run, which would make a duration
# negative or enormous.
#
#   nca_stream_trials.sh [runs] [prompt]
#
# Output is one line per run plus a tally, and the exit status is the number of
# runs that produced no answer — so it works as a gate.

RUNS="${1:-5}"
PROMPT="${2:-reply with the single word ok}"
LOGDIR=/root/.nca-trials
mkdir -p "$LOGDIR"

now_ms() { set -- $(cat /proc/uptime); echo "${1}" | awk '{printf "%d", $1*1000}'; }

no_answer=0
i=1
while [ "$i" -le "$RUNS" ]; do
    log="$LOGDIR/run$i.log"
    rm -f "$log"
    t0=$(now_ms)
    # `--no-resume` so each run is a fresh session: a resumed one can answer
    # from history and score a pass without touching the network at all.
    nca -p "$PROMPT" --no-resume > "$log" 2>&1
    rc=$?
    t1=$(now_ms)
    ms=$((t1 - t0))

    # The answer is whatever nca printed after echoing the prompt back. Matching
    # the prompt's own text would always succeed (it is echoed), so this strips
    # the escape sequences and looks for a line that is neither the echo, nor
    # tracing, nor the session banner.
    ans=$(tr -d '\033' < "$log" \
          | grep -av "$PROMPT" \
          | grep -avE '^\[|nca starting|WARN|DEBUG|INFO|TRACE|Connected to|\[session\]|YOU|^\[2K|^$' \
          | head -1)

    if [ -n "$ans" ]; then
        echo "[nca-trial] run $i  ANSWER   ms=$ms rc=$rc  \"$ans\""
    else
        no_answer=$((no_answer + 1))
        echo "[nca-trial] run $i  NO-ANSWER ms=$ms rc=$rc  (log: $log)"
    fi
    i=$((i + 1))
done

echo "[nca-trial] $RUNS run(s), $no_answer with no answer"
exit "$no_answer"
