#!/bin/sh
# In-guest runner for the kill-a-process-tree probe (run-fc.sh killtree.sh
# killtree): ROUNDS rounds over all four kill modes, then dmesg to /tmp so
# run-fc.sh can dump it. Linux expectation: every round `ok` in single-digit
# ms (mode 3 adds its 300 ms SIGTERM grace). On Akuma read the round table
# and the kernel's own lines: `[kill]` (a cross-core hard termination),
# `DRAIN INCOMPLETE` (leaked thread rows), `[BKL] stuck`, `thread table full`.
# See docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md.
ROUNDS=${1:-12}
echo "== killtree: $(uname -a)"
mkdir -p /tmp
/killtree $ROUNDS 0 2>&1
echo "== killtree exit status $?"
dmesg > /tmp/dmesg.txt 2>&1
sync
echo "== killtree done"
