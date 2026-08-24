# Benchmark performance, attempt 1 (2026-08-24)

Re-run of `BENCHMARK_PERFORMANCE_ATTEMPT_0.md`'s Redis/llama.cpp matrix, extended
with nginx and a third arm: the same devbox-smoltcp kernel built from `main`
instead of the working branch (`more-fixes`, HEAD `3b38cc2a` at the time of this
run — the redis kernel-crossing fixes summarized in that document's own header
had already landed). Four questions, not one:

1. Did anything change on `more-fixes` since attempt 0 (redis, llama.cpp)?
2. How does `more-fixes` compare to `main` at the same core counts?
3. How do both compare to Docker/Linux on the same hardware?
4. Does either kernel branch build slower/faster (`-j4 --offline`, host cross-compile)?

Status: **SUPERSEDED for Redis by
[`REDIS_ROUND_TRIP_CEILING.md`](REDIS_ROUND_TRIP_CEILING.md) (2026-08-24).**
The Redis matrix here (§2-§5) never completed — the first arm hit the
`redis-benchmark` livelock, which is now root-caused (client-side, see
[`REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md`](REDIS_BENCHMARK_HOST_CONTENTION_LIVELOCK.md)).
It was re-run as a **concurrency sweep** instead of at a single client count,
which is what exposed the mechanism. Carry forward from that document:

> - Question 1 (did anything change on `more-fixes` since attempt 0?) and
>   question 2 (`more-fixes` vs `main`) are **both answered: no**. The two
>   branches are indistinguishable — 14,085 vs 13,850 rps at c=32, same 1.945
>   tx/rx ratio.
> - Question 3 (vs Docker) is answered, but not as a single ratio: Akuma
>   plateaus from 4 clients on while Docker keeps scaling, so the gap is 1.8x
>   at c=4 and 4.6x at c=32. Quoting one number requires naming the client
>   count.
> - The `~8,000` rps this run was seeing, against attempt 0's `~20,000`, was
>   **VM uptime, not code** — a fresh boot of the same commit does 14,085.
>   §1 (build time) stands as written; nginx and llama.cpp were not reached.

Original status: **IN PROGRESS — being written incrementally as each arm completes.**

## 0. Environment

- Host: same Apple Silicon Mac as attempt 0, Hypervisor.framework (HVF) for both
  Akuma (QEMU) and Docker Desktop.
- `more-fixes` HEAD: `3b38cc2a` ("use constants for syscalls").
- `main` HEAD: `351a8722` ("docs"), checked out in a separate git worktree
  (`/private/tmp/akuma_main_wt`) so the primary tree's uncommitted state was
  never touched.
- Docker containers created fresh for this run: `akuma-redis-bench`,
  `akuma-nginx-bench`, `akuma-llama-bench` (alpine:latest + `apk add llama.cpp`),
  all `--cpuset-cpus=0-3 -m 4g` for the SMP=4 arm.
- `redis:alpine` → Redis 8.10.0, same version as attempt 0.
- `nginx:alpine` → stock config, `sendfile on` (real Linux, no workaround needed).
- `llama.cpp` on Docker: package `llama.cpp-0.0.9564-r0`, matching the guest's
  binary build_commit `3b3da01dc21dc68e958efb898ab739c65ed08ca2` recorded in its
  own CSV output. Model `qwen3.5-0.8b-q4.gguf`, sha256
  `bd258782e35f7f458f8aced1adc053e6e92e89bc735ba3be89d38a06121dc517`, verified
  identical on the Akuma guest and in the Docker container.

### Environment caveat

`mdworker_shared` (Spotlight indexing) was consuming ~20% of a host core when
the redis matrix started — not an orphaned load generator (attempt 0's Trap 1),
but noted per that same trap's rule: check what else is running before trusting
a number.

---

## 1. Kernel build time — `-j4 --offline`, host cross-compile

**Not** the in-guest self-hosted build (`docs/archive/AKUMA_SELF_HOSTING.md`) —
that needs a `disk_selfhost.img` with a toolchain staged inside the guest, which
does not exist on this machine and was out of scope for this run. This is the
ordinary host-side build: `cargo build --release --features devbox-smoltcp,no-tests
--offline -j4`, timed after `cargo clean --release --target aarch64-unknown-none`,
three repeats per branch, same host, sequential (not concurrent, so the two
branches' builds never compete for the same cores).

| branch | rev | run 1 | run 2 | run 3 | median |
|---|---|---:|---:|---:|---:|
| more-fixes | 3b38cc2a | 10.48s | 10.69s | 10.71s | **10.69s** |
| main | 351a8722 | 11.20s | 10.89s | 10.99s | **10.99s** |

**No meaningful difference** — 2.8% apart, inside run-to-run noise (0.23s
spread within `more-fixes`'s own three runs). Whatever changed between `main`
and `more-fixes` did not move the compile-time needle.

---

## 2. Redis — SMP=4, `more-fixes`

*(running — `scripts/benchmarks/redis_matrix.sh`, results in
`logs/redis_bench_smp4/`)*

## 3. Redis — SMP=1, `more-fixes`

*(pending)*

## 4. Redis — SMP=4, `main`

*(pending)*

## 5. Redis — SMP=1, `main`

*(pending)*

## 6. nginx — all arms

*(pending — `scripts/benchmarks/bench_nic_rtt.py --mode http`, akuma box on
port 8080 with `sendfile off` set via a custom `conf.d/default.conf`, Docker on
port 8082 with stock config)*

## 7. llama.cpp — all arms

*(pending — `scripts/benchmarks/bench_llama.py`)*

## 8. Comparison to attempt 0

*(pending — filled in once the more-fixes numbers are in)*

## Background

- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) —
  the run this one re-does and extends; method, fairness discussion, and the
  three kernel fixes that superseded parts of it are there in full.
- [`../runbooks/run-docker-image.md`](../runbooks/run-docker-image.md) — the
  `sendfile` / `box run` config-injection issues hit setting up the Akuma nginx
  arm here.
