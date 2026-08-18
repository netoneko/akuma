# Redis performance — Docker/Linux baseline for the Akuma comparison (2026-08-19)

**Status: baseline only.** These are Docker-on-macOS numbers, captured so that a
later `redis:alpine`-on-Akuma run has something to be measured against. **No
Akuma numbers here yet** — the "Akuma" columns are deliberately empty rather than
estimated.

Redis on Akuma already works: `docs/runbooks/run-redis.md` records the official
`redis:alpine` image pulled from Docker Hub and run in a box, reachable from the
host, verified 2026-08-16 on devbox-smoltcp. So this is a comparison that can
actually be completed, not an aspiration.

---

## Is it a fair comparison?

**At the platform level, yes — same machine, same hypervisor family.**

| | Docker Desktop | Akuma |
|---|---|---|
| Host | Apple M4 Pro, 12 cores, macOS (darwin 24.6.0) | same machine |
| Virtualization | `com.apple.Virtualization.VirtualMachine` — **Virtualization.framework** | QEMU `-accel hvf` — **Hypervisor.framework** |
| Guest | Linux 6.12.54-linuxkit, aarch64 | Akuma, aarch64 |
| Server | `redis:alpine` (Redis 8.10.0, `epoll`) | the *same image*, run in a box |
| Client | host-native `redis-benchmark`, Darwin arm64 | same binary, same flags |

Virtualization.framework is itself layered on Hypervisor.framework, so both
guests are scheduled by the same Apple hypervisor on the same silicon. The
client, the benchmark binary, the flags and the server image are identical.

**And yes, it favours Docker — but the size of that advantage is measurable, and
it is mostly not about the kernel.** The honest reading:

- Docker's guest is Linux 6.12 with a mature `epoll`/TCP stack and virtio-net
  paths that have had two decades of tuning. Akuma's is a research kernel on
  smoltcp. That part of the gap *is* the thing under study.
- QEMU's `-netdev user` (SLIRP) userspace NAT is a well-known throughput
  bottleneck, and Docker Desktop's port forwarder is a purpose-built proxy.
  **This part of the gap is plumbing, not kernel**, and on the host-forwarded
  path it is likely to dominate everything else.
- Docker Desktop's VM gets 12 vCPUs and 7.65 GiB by default; the Akuma devbox
  runs `SMP=4` with `MEMORY=4096`. Not matched, and worth matching before
  drawing conclusions.

So: fair as a *system-level* "what does Redis do on this Mac under each stack"
question. **Not** a clean kernel-vs-kernel measurement, and a raw ratio must not
be reported as "Akuma's kernel is N× slower than Linux".

### How much of the gap is the forwarding path? Measured: most of it.

Both arms below are the same Redis, in the same container, on the same host —
the *only* difference is whether the client crossed the host port-forward.

| test | P=1 forwarded | P=1 in-container | ratio | P=16 forwarded | P=16 in-container | ratio |
|---|---:|---:|---:|---:|---:|---:|
| GET | 62,814 | 257,069 | 4.09× | 892,857 | 3,703,704 | 4.15× |
| INCR | 62,657 | 252,525 | 4.03× | 917,431 | 3,125,000 | 3.41× |
| LPUSH | 56,850 | 317,460 | 5.58× | 578,035 | 704,225 | **1.22×** |
| MSET (10 keys) | 59,032 | 249,377 | 4.22× | 473,934 | 578,035 | **1.22×** |
| PING_INLINE | 63,371 | 246,305 | 3.89× | 909,091 | 3,571,428 | 3.93× |
| PING_MBULK | 61,652 | 257,732 | 4.18× | 884,956 | 3,571,428 | 4.04× |
| RPOP | 58,789 | 234,192 | 3.98× | 775,194 | 2,857,143 | 3.69× |
| SADD | 65,317 | 245,098 | 3.75× | 840,336 | 3,448,276 | 4.10× |
| SET | 63,171 | 253,807 | 4.02× | 775,194 | 2,564,102 | 3.31× |
| SPOP | 66,094 | 238,663 | 3.61× | 854,701 | 3,703,704 | 4.33× |

**Crossing the host port-forward costs ~4× on Docker's own stack.** The
forwarded arm is therefore measuring the forwarder far more than it measures
Redis or the kernel underneath it.

**The two exceptions are the useful tests.** At `P=16`, `LPUSH` and `MSET` show
only **1.22×** — the forward stops mattering because the *server* has become the
limit (bigger payloads, more work per operation). Those are the two rows where a
kernel difference would actually show through, and they are the ones to weight
most heavily when the Akuma numbers land.

---

## Baseline results

Redis 8.10.0 (`redis:alpine`), 100,000 requests, 50 clients, 64-byte payloads,
**median of 3 runs** per cell.

### Arm A — host client through the port-forward

`redis-benchmark` on macOS → `localhost:6379` → Docker Desktop forward → container.
This is the arm that matches how Akuma is reached (`localhost:4444` → QEMU
`hostfwd` → guest smoltcp → box).

| test | P=1 ops/s | P=16 ops/s | Akuma P=1 | Akuma P=16 |
|---|---:|---:|---:|---:|
| PING_INLINE | 63,371 | 909,091 | — | — |
| PING_MBULK | 61,652 | 884,956 | — | — |
| SET | 63,171 | 775,194 | — | — |
| GET | 62,814 | 892,857 | — | — |
| INCR | 62,657 | 917,431 | — | — |
| LPUSH | 56,850 | 578,035 | — | — |
| RPOP | 58,789 | 775,194 | — | — |
| SADD | 65,317 | 840,336 | — | — |
| SPOP | 66,094 | 854,701 | — | — |
| MSET (10 keys) | 59,032 | 473,934 | — | — |

### Arm B — client inside the container (no host forward)

| test | P=1 ops/s | P=16 ops/s | Akuma P=1 | Akuma P=16 |
|---|---:|---:|---:|---:|
| PING_INLINE | 246,305 | 3,571,428 | — | — |
| PING_MBULK | 257,732 | 3,571,428 | — | — |
| SET | 253,807 | 2,564,102 | — | — |
| GET | 257,069 | 3,703,704 | — | — |
| INCR | 252,525 | 3,125,000 | — | — |
| LPUSH | 317,460 | 704,225 | — | — |
| RPOP | 234,192 | 2,857,143 | — | — |
| SADD | 245,098 | 3,448,276 | — | — |
| SPOP | 238,663 | 3,703,704 | — | — |
| MSET (10 keys) | 249,377 | 578,035 | — | — |

### Noise floor — read this before calling any difference real

Spread across the 3 repeats, `(max − min) / median`:

- **Arm A**: 4.5 %–18 % on most cells, but **34.5 % on LPUSH P=16** and
  **33.5 % on MSET P=16**.
- **Arm B**: 5.5 %–16 %.

So on the forwarded arm a difference under roughly **35 %** on the pipelined
list/multi-key tests is **noise**, and under ~20 % elsewhere is not safe either.
Three repeats is the minimum; take more before reporting anything close.

---

## Method (reproduce exactly)

Use [`scripts/benchmarks/bench_redis.py`](../../scripts/benchmarks/bench_redis.py) —
it drives every arm, takes the median of N repeats, records the spread, and has a
`--compare` mode that gates each row against the measured noise floor instead of
letting you eyeball a ratio.

```bash
docker run -d --name akuma-redis-bench -p 6379:6379 redis:alpine

# Arm A — host client through the forward
scripts/benchmarks/bench_redis.py --label docker-fwd   --port 6379 --out docker_fwd.json

# Arm B — client inside the container
scripts/benchmarks/bench_redis.py --label docker-local --via docker:akuma-redis-bench \
                                  --out docker_local.json

scripts/benchmarks/bench_redis.py --compare docker_fwd.json docker_local.json
```

Defaults match the tables above: 100,000 requests, 50 clients, 64-byte payloads,
pipelines 1 and 16, 3 repeats. `--csv` from `redis-benchmark` is parsed rather
than the human table, which reflows between Redis versions.

## Completing the comparison — protocol for the Akuma arm

1. Boot devbox-smoltcp and start `redis:alpine` in a box on guest port **4444**
   (host 4444, or 4544 at `INSTANCE=1`) — `docs/runbooks/run-redis.md` §3.
2. **Collect both arms**, not just the host-side one. Comparing Akuma's Arm A
   against Docker's Arm B — or quoting only the forwarded number — attributes the
   QEMU SLIRP path to the kernel.

   ```bash
   scripts/benchmarks/bench_redis.py --label akuma-fwd   --port 4444 --out akuma_fwd.json
   scripts/benchmarks/bench_redis.py --label akuma-local --via ssh:2222 --port 4444 \
                                     --out akuma_local.json
   scripts/benchmarks/bench_redis.py --compare docker_fwd.json   akuma_fwd.json
   scripts/benchmarks/bench_redis.py --compare docker_local.json akuma_local.json
   ```

   The in-guest arm needs `redis-benchmark` present inside the guest — it ships
   in the `redis:alpine` image, so run it through the box (`--via docker:` has a
   `box`-flavoured equivalent) or install it; if neither is available, say so
   rather than substituting the forwarded number for it.
3. **Match the VM shape first**, or state that you did not: Docker's VM had 12
   vCPUs / 7.65 GiB here against the devbox default `SMP=4` / `MEMORY=4096`.
4. Weight `LPUSH`/`MSET` at `P=16` most heavily — per the table above, those are
   the cells where the server, not the forwarder, is the bottleneck.
5. Same `redis-benchmark` binary, same flags, median of ≥3, and check the spread
   before believing a gap.

## Background

- [`../runbooks/run-redis.md`](../runbooks/run-redis.md) — how Redis is actually
  run on Akuma, including the port-forwarding table and the `INSTANCE=N` offsets.
- [`../runbooks/verify-trim-fat-change.md`](../runbooks/verify-trim-fat-change.md)
  § Tier 4 — `redis-server --test-memory` on devbox, which exercises the memory
  path rather than throughput, and its scoping caveat that a passing memtest is
  not "redis works".
- [`LONG_ROAD_TO_REDIS.md`](LONG_ROAD_TO_REDIS.md) — why `redis-server` proper
  was blocked for a long time (`/proc/self/smaps`), background on what had to
  land before this comparison was possible at all.
