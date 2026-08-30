#!/usr/bin/env bash
# A/B the recoverable reader/writer lock against the `spinning_top::RwSpinlock`
# it replaced in `akuma-ext2` (docs/archive/AKUMA_EXT2_CLEANUP.md §5 step 4).
#
# Why a microbenchmark and not the end-to-end probe: `ext2probe-host`'s whole
# run is ~0.21 s, most of it reading the image into RAM, so a 10 ms host clock
# cannot resolve a per-acquire change of tens of nanoseconds. `akuma-ext2`
# acquires on 25 sites, at least once per filesystem operation, so the fast path
# is what a swap can regress silently. The contended path is deliberately NOT
# measured — its cost is dominated by how long the holder holds (device I/O),
# and the two locks' waiting behaviour differs by design.
#
# The two arms are in ONE binary, so there is no stale-artifact hazard and no
# rebuild between them. `write_holding` / `read_holding` are the rows that
# matter: those are the entry points `akuma-ext2` actually calls.
#
# Usage: scripts/benchmarks/locks_rw_ab.sh [repeats]   (default 3)
set -euo pipefail
cd "$(dirname "$0")/../.."
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
REPEATS=${1:-3}

cargo build --release -p akuma-locks-rw-cell --bin lock-ab --features cli --target "$HOST"
BIN="target/$HOST/release/lock-ab"

for i in $(seq 1 "$REPEATS"); do
    echo "--- repeat $i/$REPEATS ---"
    "$BIN"
done

cat <<'NOTE'

Reading it: the binary already reports the minimum of 7 internal passes, so the
repeats above are a stability check, not a sample to average. On an idle host
they agree to ~0.02 ns/op; if they do not, something else is using the CPU and
the numbers cannot carry a claim (see docs/archive/ for this host's variance).
NOTE
