#!/bin/bash
# Two A/B arms for the "why is Akuma redis ~4x Docker" investigation, run
# sequentially so they never measure each other.
#
#   1. redis persistence on vs off — does `--save ''` (no RDB snapshots, what
#      every benchmark in docs/archive used) differ from a stock save policy?
#      Rules persistence in or out as a contributor to the ceiling.
#   2. `main` vs the working branch at the same core count — the branch A/B
#      BENCHMARK_PERFOMANCE_ATTEMPT_1.md §4 left pending.
#
# Both arms are full boots on the same disk with the same redis binary, so the
# only variable is the one named. Results land in logs/redis_why/.
set -u
cd "$(dirname "$0")/../.."

COMMON="--smp 4 --clients 1,4,16,32 --requests 10000 --repeats 3 --out logs/redis_why"

echo "########## ARM 1: redis --save '900 1' (persistence ON) ##########"
python3 -u scripts/benchmarks/redis_smp_sweep.py $COMMON \
    --tag=-save900 --redis-args="--save '900 1'"

echo
echo "########## ARM 2: main branch kernel (351a8722) ##########"
python3 -u scripts/benchmarks/redis_smp_sweep.py $COMMON \
    --tag=-main --tree /private/tmp/akuma_main_wt --keep-up

echo "ab chain done"
