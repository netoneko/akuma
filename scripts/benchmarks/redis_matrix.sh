#!/bin/bash
# Four-arm Redis matrix: Akuma vs Docker/Linux, forwarded and in-guest.
#
# Results and full analysis: docs/archive/BENCHMARK_PERFORMANCE_ATTEMPT_0.md
#
# Why four arms and not two
# -------------------------
# Where the CLIENT runs matters more than anything else, and it does not matter
# the same way on both kernels:
#
#   Docker: crossing the host port-forward COSTS ~5.6x
#   Akuma:  crossing the host port-forward GAINS ~6x
#
# because Akuma's in-guest arm puts both endpoints on the kernel under test and
# routes them through Akuma's own smoltcp loopback — charging the kernel twice.
# So the arms are not symmetric handicaps, and only arm-to-arm comparisons mean
# anything. Collect all four or state which you skipped.
#
# Why these flags
# ---------------
# --per-test / --cooldown are mandatory on Akuma, not stylistic: the socket pool
# cannot survive nine tests back to back (DEVBOX_ISSUES Issue 16), and
# redis-benchmark EXITS 0 after printing "No file descriptors available", so the
# missing cells look like they were never requested. -c 20 sits inside the
# budget with a cooldown. Whatever you pick, pick it for every arm.
#
# The runs are serialized on purpose. Two arms at once measure each other.
#
# Usage
# -----
#   scripts/benchmarks/redis_matrix.sh                 # 4-core: SMP=4 vs cpuset 0-3
#   CORES=1 scripts/benchmarks/redis_matrix.sh         # 1-core: SMP=1 vs cpuset 0
#
# Prerequisites, both of which this script checks:
#   - a devbox-smoltcp VM booted at the matching SMP, with redis in a box on
#     guest port 4444 (docs/runbooks/run-redis.md §3, or the docker-export
#     workaround in DEVBOX_ISSUES Issue 18 while `box pull` is broken)
#   - a redis:alpine container pinned to the matching cpuset
set -u
cd "$(dirname "${BASH_SOURCE[0]}")/../.." || exit 1

CORES="${CORES:-4}"
if [ "$CORES" = "1" ]; then
    CPUSET="0"; TAG="smp1"; CONTAINER="akuma-redis-1cpu"
else
    CPUSET="0-$((CORES - 1))"; TAG="smp${CORES}"; CONTAINER="akuma-redis-bench"
fi
OUT="logs/redis_bench_${TAG}"
SSH_PORT="${SSH_PORT:-2222}"
GUEST_REDIS_PORT="${GUEST_REDIS_PORT:-4444}"
mkdir -p "$OUT"

COMMON=(--requests "${REQUESTS:-100000}" --clients "${CLIENTS:-20}" --size 64
        --pipelines 1,16 --repeats "${REPEATS:-3}" --per-test)

# --- preflight: the shapes must match, or the numbers compare two machines ----
GUEST_CORES=$(ssh -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
                  -o ConnectTimeout=10 -p "$SSH_PORT" root@localhost 'nproc' 2>/dev/null)
if [ "$GUEST_CORES" != "$CORES" ]; then
    echo "ABORT: guest reports nproc=$GUEST_CORES, expected $CORES." >&2
    echo "       Boot the devbox with SMP=$CORES first." >&2
    exit 1
fi
DOCKER_CORES=$(docker exec "$CONTAINER" nproc 2>/dev/null)
if [ "$DOCKER_CORES" != "$CORES" ]; then
    echo "ABORT: container $CONTAINER reports nproc=$DOCKER_CORES, expected $CORES." >&2
    echo "       docker run -d --name $CONTAINER --cpuset-cpus=$CPUSET -m 4g \\" >&2
    echo "                  -p 6379:6379 redis:alpine" >&2
    exit 1
fi
echo "preflight ok: guest nproc=$GUEST_CORES, container nproc=$DOCKER_CORES"

# A busy host invalidates every arm. This has bitten this benchmark twice —
# once from orphaned `while :; do :; done` load generators left by another
# session, once from a llama-bench that outlived its ssh channel.
echo "top CPU consumers right now (anything unexpected here invalidates the run):"
ps -Ao pid,pcpu,comm -r | head -6

echo "############ 1/4 akuma-fwd-$TAG ############"
scripts/benchmarks/bench_redis.py --label "akuma-fwd-$TAG" --port "$GUEST_REDIS_PORT" \
    "${COMMON[@]}" --cooldown 15 --out "$OUT/akuma_fwd.json"

echo "############ 2/4 akuma-box-$TAG ############"
scripts/benchmarks/bench_redis.py --label "akuma-box-$TAG" \
    --via "box:$SSH_PORT:redisbox" --port "$GUEST_REDIS_PORT" "${COMMON[@]}" --cooldown 15 \
    --bench-bin /usr/local/bin/redis-benchmark --cli-bin /usr/local/bin/redis-cli \
    --out "$OUT/akuma_box.json"

echo "############ 3/4 docker-fwd-$TAG ############"
scripts/benchmarks/bench_redis.py --label "docker-fwd-$TAG" --port 6379 \
    "${COMMON[@]}" --cooldown 3 --out "$OUT/docker_fwd.json"

echo "############ 4/4 docker-local-$TAG ############"
scripts/benchmarks/bench_redis.py --label "docker-local-$TAG" --via "docker:$CONTAINER" \
    --port 6379 "${COMMON[@]}" --cooldown 3 --out "$OUT/docker_local.json"

echo "############ DONE ############"
scripts/benchmarks/bench_redis.py --compare "$OUT/docker_fwd.json"   "$OUT/akuma_fwd.json"
scripts/benchmarks/bench_redis.py --compare "$OUT/docker_local.json" "$OUT/akuma_box.json"
date
