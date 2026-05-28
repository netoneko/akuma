#!/bin/bash
# Extract regression signals from a logs/daif/<ts>-<label>/boot.log.
# Writes signals.txt next to it; prints a one-line summary.
#
# Usage: ./scripts/daif_analyze.sh logs/daif/<run-dir>

set -euo pipefail

RUN_DIR="${1:?usage: daif_analyze.sh <run-dir>}"
LOG="$RUN_DIR/boot.log"
OUT="$RUN_DIR/signals.txt"

if [ ! -f "$LOG" ]; then
    echo "no boot.log in $RUN_DIR" >&2
    exit 1
fi

TMR=$(grep -c '^\[TMR\]' "$LOG" || true)
T0=$(grep -c '^\[Thread0\]' "$LOG" || true)
# SCHED warnings emitted *after* the DAIF test section are real regressions.
# The DAIF tests deliberately trigger one warning (test_yield_now_detects_masked_yield),
# so we exclude warnings before "DAIF tests complete" from the regression count.
SCHED_TOTAL=$(grep -c '\[SCHED\] WARNING' "$LOG" || true)
if grep -q 'DAIF tests complete' "$LOG"; then
    SCHED=$(awk '/DAIF tests complete/{found=1; next} found && /\[SCHED\] WARNING/{n++} END{print n+0}' "$LOG")
else
    SCHED="$SCHED_TOTAL"
fi
HEARTBEAT=$(grep -c '^\[Heartbeat\]' "$LOG" || true)
NEG=$(grep -cE 'PANIC:|FATAL|UNHANDLED EXCEPTION|kernel panic|BUG:|0xBAD[0-9A-Fa-f]*\b' "$LOG" || true)
DAIF_TEST_PASSES=$(grep -c '\[PASS\] test_irq_guard\|\[PASS\] test_nested_irq\|\[PASS\] test_with_irqs\|\[PASS\] test_yield_now' "$LOG" || true)
LAST_TMR=$(grep '^\[TMR\]' "$LOG" | tail -1 || true)
LAST_T0=$(grep '^\[Thread0\]' "$LOG" | tail -1 || true)

{
    echo "log: $LOG"
    echo "tmr_count: $TMR"
    echo "thread0_count: $T0"
    echo "heartbeat_count: $HEARTBEAT"
    echo "sched_warnings_total: $SCHED_TOTAL"
    echo "sched_warnings_post_tests: $SCHED"
    echo "negative_signals: $NEG"
    echo "daif_test_passes: $DAIF_TEST_PASSES"
    echo "last_tmr: $LAST_TMR"
    echo "last_thread0: $LAST_T0"
    echo ""
    echo "--- SCHED warnings (if any) ---"
    grep -n '\[SCHED\] WARNING' "$LOG" || echo "(none)"
    echo ""
    echo "--- negative signals (if any) ---"
    grep -nE 'PANIC:|FATAL|UNHANDLED EXCEPTION|kernel panic|BUG:|0xBAD[0-9A-Fa-f]*\b' "$LOG" | head -20 || echo "(none)"
} > "$OUT"

VERDICT="OK"
[ "$SCHED" -gt 0 ] && VERDICT="WARN"
[ "$NEG" -gt 0 ] && VERDICT="FAIL"
[ "$TMR" -lt 5 ] && VERDICT="FAIL"
[ "$T0" -lt 5 ] && VERDICT="FAIL"

echo "$VERDICT  tmr=$TMR t0=$T0 hb=$HEARTBEAT sched=$SCHED neg=$NEG daif_pass=$DAIF_TEST_PASSES  ($RUN_DIR)"
