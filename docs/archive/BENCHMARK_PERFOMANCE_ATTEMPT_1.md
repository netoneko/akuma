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

## 2. Redis — measured 2026-08-24/25, `more-fixes` + trace gating

All figures `c=1` PING unless stated, median of 3, `SMP=4`, on the kernel with
the ungated `epoll_ctl` trace fixed
([`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md) §9 — pre-fix
numbers are not comparable to anything).

### 2a. Apples-to-apples: client and server both inside the box

This is the comparison that isolates the OS. Same benchmark, same transport,
client in the same container/guest as the server.

| arm | µs/rt | **rps** | **% of Docker** | Docker faster by |
|---|---:|---:|---:|---:|
| Akuma, UNIX socket | 41 | **24,390** | — | — |
| Docker, TCP loopback | 38 | **26,316** | 100 % | 1.0x |
| Akuma, TCP loopback | 114 | **8,772** | **33 %** | **3.0x** |

Two readings:

- **Akuma's IPC is at parity.** Its UNIX-socket round trip (24,390 rps) is
  within 8 % of Docker's *TCP loopback* (26,316 rps). Scheduling and process
  wakeup are not the deficit. (No Docker UNIX-socket control was taken, so this
  is indicative, not a like-for-like win.)
- **The gap is Akuma's TCP path**: 41 → 114 µs, i.e. ~70 µs, which is
  24,390 → 8,772 rps. **That ~70 µs is the whole in-kernel target.**

Stability: 41/114 µs reproduced on three separate boots and did not move after
2,400 connections (41→44, 114→110). Use it as a **boot health check** — a boot
reading far above it is degraded and its other numbers must be discarded.

### 2b. Host-driven — what `redis_smp_sweep.py` reports

Client on the macOS host through QEMU `hostfwd`. Docker column from
[`REDIS_ROUND_TRIP_STAGE_TRACE.md`](REDIS_ROUND_TRIP_STAGE_TRACE.md) §4.

| clients | Akuma rps | Docker rps | **Akuma % of Docker** | Docker faster by |
|---:|---:|---:|---:|---:|
| 1 | 2,373 | 8,734 | **27 %** | 3.7x |
| 2 | 3,213 | 15,699 | **20 %** | 4.9x |
| 4 | 16,807 | 22,422 | **75 %** | 1.3x |
| 32 | 20,284 | 64,516 | **31 %** | 3.2x |

**~70 % of the host-driven deficit is QEMU, not Akuma.** Host-driven `c=1` is
476 µs against 114 µs in-guest, so **329 µs is SLIRP plus the host stack**.
Docker's equivalent hop costs 76 µs (114 → 38). Quote §2a for OS comparisons
and §2b only as "what our benchmark harness reports".

## 3-5. Redis — other core counts / `main`

*(not run this session)*

## 6. nginx and httpd — NOT ESTABLISHED, and why that is itself the finding

**No trustworthy HTTP number came out of this session.** The same two servers,
same kernel, same docroot, measured across boots:

| server | measurements across boots |
|---|---|
| Akuma `httpd` | 890, 1,100, 1,880 µs (status-validated) |
| Akuma nginx | 114, 308, 513, **17,800** µs |
| Docker nginx (in-container control) | ~79 µs (**12,658 rps**) |

nginx spans **150x**. Within any one boot it is tight — 17.2 / 17.0 / 17.5 ms,
0 failures — so this is a **bimodal system state, not measurement noise**.
Redis does not do it (41/114 µs every healthy boot), which is what makes the
asymmetry interesting.

### 6a. The 17 ms is the epoll backstop — a lost wakeup

Characterised while a guest was in the slow regime (`ab -n 100`):

| arm | rps | ms/req |
|---|---:|---:|
| nginx `-c 1` | 56 | 17.8 |
| nginx `-c 4` | 134 | 29.7 |
| nginx `-c 1 -k` (keep-alive) | 116 | **8.6** |

**Keep-alive exactly halves it**, and 8.6 ms sits right under
`BLOCKING_POLL_INTERVAL_US` = **10 ms**, the epoll family's `backstop_us`
(`src/syscall/poll.rs:55`). The shape is ~2 backstop waits per non-keep-alive
request, ~1 with. So nginx is not computing slowly — **its `epoll_wait` is
missing the readiness wake and being rescued by the backstop timer.**

This reframes Part B ([`LONG_ROAD_TO_REDIS_PART_2.md`](LONG_ROAD_TO_REDIS_PART_2.md)
§8): lowering the backstop 10 ms → 1 ms improved sweep `c=1` by ~10 %, which
now looks like it was **shortening the rescue rather than fixing the miss**.
**Find the lost wakeup; do not tune the backstop.** Hypothesis, not proof — it
rests on the 8.6/17.8 ms shape and the constant matching, not on a traced miss.

### 6b. Harness requirements, learned the hard way

Four defects produced the 150x spread before any of it was believable:

1. **Validate status.** `curl -w '%{time_total}'` reports a time for a
   *connection-refused* request, so a dead server measures as very fast. This
   produced a phantom "6.8x faster with logging off" that a code change was
   built on and then reverted. Record `%{http_code}`; discard runs not all-200.
2. **`--no-keepalive` is TCP keepalive probes**, not HTTP connection reuse. Use
   `-H 'Connection: close'`, or `ab -k` for the reuse arm.
3. **One server per boot.** Four servers up made nginx read 6,149 µs where it
   otherwise read 308.
4. **The harness degrades the guest.** Repeated `pkill`/start cycles plus
   thousands of connections drove a guest to where servers stopped starting at
   all. Take the §2a redis reading each boot to detect it.

`ab` is not in the devbox image; `apk add apache2-utils` installs it and is
worth doing before any HTTP work — a `/bin/curl` shell loop puts client cost
inside the measurement.

## 7. llama.cpp — all arms

*(not run this session)*

## 8. Comparison to attempt 0

*(pending — filled in once the more-fixes numbers are in)*

## Background

- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) —
  the run this one re-does and extends; method, fairness discussion, and the
  three kernel fixes that superseded parts of it are there in full.
- [`../runbooks/run-docker-image.md`](../runbooks/run-docker-image.md) — the
  `sendfile` / `box run` config-injection issues hit setting up the Akuma nginx
  arm here.
