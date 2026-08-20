# `blocking_relax`: removing the yield kernel-wide wedges SMP=4 (2026-08-20)

**Status:** Regression found, root-caused, fixed by splitting the primitive. Fix
verified on both axes (SMP=4 boot suite + HTTP A/B).
**Commit that introduced it:** `1a29c9c3` "drop extra yield for networking gains".
**Fix:** `threading::blocking_relax_net` — the yield-less variant, wired into
`NetRuntime::blocking_relax` only.

## Summary

`crates/akuma-exec/src/threading/mod.rs`'s `blocking_relax()` was

```rust
pub fn blocking_relax() {
    yield_now();
    #[cfg(kernel_smp_shared)]
    idle_halt();
}
```

Removing `yield_now()` under `cfg(kernel_smp_shared)` is worth **+27 % HTTP
throughput and half the p90** — and it **permanently wedges `SMP=4`** in the
spawn/exec/reap path. Both effects are real, large, and reproducible. They are
separable because only one caller benefits: the socket wait loop.

| kernel | `blocking_relax` | SMP=4 boot suite | req/s | p50 | p90 | p99 |
|---|---|---|---:|---:|---:|---:|
| `efcac763` (pre-commit) | yield + halt | **294 passed** | 1,028 | 601 us | 2,411 us | 4,703 us |
| `1a29c9c3` (committed) | halt only, ALL callers | **23 passed, wedged** | 1,307 | 502 us | 1,166 us | 3,874 us |
| fix (split) | halt only for sockets | **294 passed** | **1,339** | **482 us** | **967 us** | **3,808 us** |
| Linux control (2026-08-19) | — | — | 1,641 | 576 us | 643 us | 882 us |

HTTP figures are medians of 5 x 2000 requests, all three arms measured in one
session against the same `httpd`. Per-run ranges are disjoint between arms
(pre-commit 1,005-1,037; committed 1,249-1,358; split 1,236-1,471), i.e. the
effect is far outside the ~6 % session drift documented in
[`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §11.7.

## Correction: every absolute below was measured with a saturated host core

Found at the end of the session: an **orphaned `redis-benchmark` from a previous
session** (`-n 100000 -t set`, `redis_matrix.sh`'s defaults) had been running for
**22.7 hours at 100 % CPU**, aimed at guest port 4444 — which QEMU forwards into
the guest, so it was also firing connection attempts into the smoltcp stack under
test the whole time. `redis_matrix.sh`'s own header warns about exactly this
("orphaned load generators left by another session"); the session-start check
looked for stray QEMU but not for load generators. **Add load generators to that
check.**

Re-measuring the split arm on a clean host, same protocol, fresh boot:

| split arm | with orphan | clean host | delta |
|---|---:|---:|---|
| req/s | 1,339 | **1,494** | +11.6 % |
| p50 | 481.7 us | **415.4 us** | -14 % |
| p90 | 967.4 us | **725.8 us** | -25 % |
| p99 | 3,807.7 us | **3,794.0 us** | **-0.4 %** |

**Every A/B conclusion in this document stands** — all arms ran under the identical
handicap, so the deltas are unaffected. What is wrong is the *absolutes*, which are
~12 % low.

**The Linux ratio is not quotable at all right now, in either direction.** Git
dates the `1,641 req/s` control to `32f6f60a`, 2026-08-19 22:50 — about 18 h into
the orphan's run (its 22.7 h of CPU at ~100 %, killed ~03:35 on 2026-08-20, puts
its start near 05:07 on the 19th). So the control is contaminated too, and
**asymmetrically**: the orphan hit Akuma through host CPU *and* connection pressure
on forwarded guest port 4444, while the Docker arm saw only host CPU — its traffic
never entered the container. Akuma carried the heavier handicap, so the clean-host
ratio is probably better than the 1,494/1,641 = 91 % arithmetic suggests, but
**both sides must be re-measured on a clean host before any ratio is quoted.**
The 82 % figure quoted earlier in this session is superseded and was doubly wrong
(contaminated Akuma numerator, contaminated Linux denominator).

**p99 not moving is an accidental control worth keeping**: host contention costs
throughput, p50 and p90, and does not touch p99. That is independent confirmation
that the ~3 ms p99 floor is structural (one timer tick), not a load artifact.

A second trap from the same episode: a re-measure taken on the **already-running**
VM — after an `apk add` and a redis session — showed 814-988 req/s with p90 2,990-
4,934 us, i.e. *worse* than the contaminated run. That was the §11.2 socket-table
state, not the host: p50 was unchanged (448-480 us) while only the tail moved.
**A clean-host re-measure still needs a fresh boot**; changing two variables
measures neither.

## The regression

`MEMORY=2048 SMP=4 cargo run --release`, default features, nothing else differing:

| | pre-commit | committed |
|---|---:|---:|
| tests `PASSED` | 294 | **23** |
| tests `FAILED` | 0 | 0 |
| `smp_shared_cooperative_wait` | PASSED | PASSED |
| `smp_shared_blocking_wait_peer_progress` | **PASSED** | never reached |
| `[BKL] stuck` lines | 90 | 81 |
| outcome | ran to completion | serial log frozen, QEMU alive |

Run twice on the committed kernel: **23 / 81 both times, frozen at the same
point.** Deterministic, not flaky.

The freeze lands immediately after `smp_shared_cooperative_wait PASSED`, in a
spawn-churning test — the log's last lines are a `spawn` / `TERM` / `AS-FREE`
cycle (`pid=69..72`) that stops mid-sequence.

### Two things that are NOT the signal

- **`[BKL] stuck` lines are not the discriminator.** Both arms emit ~85 of them.
  The pre-commit kernel recovers and continues to 294 passes; the committed one
  never emits another line. Counting `stuck` lines would have called these two
  runs equivalent.
- **`owner=` is what to read, and `tag=511` is meaningless** (the profiler is off
  by default). All the stuck lines here are `owner=1`, i.e. core 0 holds the BKL
  — `owner` stores `core_id + 1`, so `owner=0` would mean *unowned*. See
  [`BKL_BARGE_TICKET_LEAK.md`](BKL_BARGE_TICKET_LEAK.md).

### Why single-core missed it entirely

`MEMORY=2048 cargo run --release` (no `SMP=`) gives **286 passed / 0 failed** on
the committed kernel — clean. The regression test for precisely this wedge class
prints

```
[Test] smp_shared_blocking_wait_peer_progress SKIPPED (single CPU; boot with SMP>1)
```

`blocking_relax` is a **shared-kernel-SMP primitive**; its `idle_halt` half
compiles out off `cfg(kernel_smp_shared)`. A single-core suite cannot exercise it.
**Any change to `blocking_relax` must be verified at `SMP=4`, and the acceptance
signal is `smp_shared_blocking_wait_peer_progress PASSED`, not the pass count.**

## Why the two call sites need different behaviour

The yield is not redundant with the BKL drop, and the two callers wait on
different things:

- **The socket waiter is woken by a device interrupt.** With the yield, the park
  is a scheduler pass + SGI *before* the WFI is entered, so a packet landing in
  that window ends no halt — the waiter arrives at WFI just after its own wake and
  sleeps to the next timer tick. Without the yield it is already in WFI when the
  packet lands, and the NIC IRQ ends the halt directly.
- **The spawn/exec/reap waiter is woken by another thread on its own core.** It
  genuinely depends on `yield_now` running a READY peer locally; halting instead
  leaves the peer unscheduled and the wait never resolves. That is the original
  socket-recv / `exec_with_io_cwd` cross-core wedge
  ([`../runbooks/debug-smp.md`](../runbooks/debug-smp.md)).

The socket path reaches `blocking_relax` through the `NetRuntime::blocking_relax`
**function pointer** (`src/main.rs`); every other caller
(`process/exec.rs`, `process/children.rs`, `process/mod.rs`, `src/smp_shared.rs`,
`process_tests.rs`) calls `threading::blocking_relax` directly. So the split needs
no call-site audit — one pointer changes, nothing else moves.

## Mechanism, measured

`[NICSTAT]` windows matched at ~12,170 rx packets (normalising by traffic is
mandatory here — §11.7):

| per window | with yield | without |
|---|---:|---:|
| `relax` parks | 11,263-11,753 | **54,137-69,406** |
| us per park | 356-370 | **59-77** |
| polls per packet | 6.40-7.21 | 9.48-10.71 |
| us per poll | 11.1-11.9 | **6.9-7.7** |
| laps per packet | 3.44-4.41 | 2.22-3.44 |
| NIC IRQs per packet | 1.26-1.29 | 1.28 |

**More parks, each ~5x shorter** is the "wakes are landing" signature from §11.7,
and the exact inverse of the swallowed-wake failure mode that killed every wake
experiment in §8 and §11.4. NIC IRQs per packet is unchanged, so this is not a
change in interrupt load — it is the same wakes arriving usefully.

## Where the loop's time actually goes — and what the fix did NOT change

`scripts/benchmarks/nicstat_breakdown.py` turns a `[NICSTAT]` window into a time
budget. Two rules keep it honest: a window is `dt` ms of *wall* clock but
`dt * SMP` ms of *core* time, and `relax`/`poll` are accumulated per thread so
they are shares of core time; `tx_wait`/`rx_post`/`rx_done` happen *inside*
`poll`, so they are shares of `poll` and are never added alongside it.

Per 5 s window at `SMP=4` (20,000 ms of core budget), matched at ~12,170 packets:

| | pre-commit | split |
|---|---:|---:|
| parked (`relax`) | 4,167-4,188 ms (20.8 %) | 4,120-4,164 ms (20.7 %) |
| — parks | 11,263-11,753 | 55,821-62,261 |
| — us per park | 356-370 | 67-74 |
| — **us parked per packet** | **342.3** | **338.3-342.9** |
| in `poll()` | 920-982 ms (4.7 %) | 851-891 ms (4.4 %) |
| — us poll per packet | 75.6-80.6 | 70.1-73.2 |
| `wake_all` pass | 82-87 ms | 57-68 ms |

**The total parked time is unchanged — 342 us per packet on both arms.** The fix
does not park less; it parks in **5x finer grain**, so a wake lands inside the
same total instead of waiting out the tick. That is the whole mechanism, and it
explains why p90 halved while every throughput-side cost moved by <10 %: this is a
wake-*latency* fix, not a work reduction. The +27 % came free rather than by
spending more CPU.

Inside `poll()` the split is stable across every arm and every window:

| component | share of `poll` | cost |
|---|---:|---|
| `tx_wait` | **30.6-32.3 %** | 22.5-25.1 us per TX packet |
| smoltcp stack / other | 61.6-62.7 % | — |
| `rx_post` | 6.1-7.1 % | 4.2-5.4 us per packet |
| `rx_done` | 0 % | — |

The largest *named* remaining cost in the loop is `tx_wait`, ~31 % of all poll
time — but **it is not kernel-side work.** `net-noalloc`'s static rings were
re-measured on top of this fix ([`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §12)
and `tx_wait` per packet is unchanged (22.5-22.7 -> 22.7-23.0 us), so it is
neither allocation nor copy cost; it is time waiting on virtio TX completion,
i.e. device/host side. It should not be cited as a kernel-side target.

Two context numbers worth keeping: `poll()` is only **4.4 % of the core budget**
and parked time ~21 % (about one core of four). The machine is close to idle — the
remaining gap to Linux is latency, not capacity, which is the same conclusion
§11.7 reached from the other direction ("85 % of contended BKL spin is idle +
irq/sched, i.e. cores that had nothing to do").

## p99 is now tick-quantised, and that is a floor

p90 moved 2,411 -> 967 us (-60 %); p99 barely moved, 4,703 -> 3,808 us. They are
different mechanisms and only p90 was park latency.

`TIMER_INTERVAL_US = 3_000` (`src/config.rs:884`; the 10 ms value is
`extreme-size` only). p99 3,808 us is **one tick plus ~800 us of real work** —
a single missed wake falling back to the timer backstop. `poll max` in the same
windows agrees independently: 3,504-3,698 us on the split arm, 4,222-4,383 us on
the pre-commit arm.

So **p99 cannot go below ~3 ms by further wake tuning.** The levers are (a) find
the remaining lost wake, or (b) shorten the tick — and (b) is already at its
measured floor: below ~2.5 ms HVF on darwin/arm64 declines to sleep the vCPU
thread, turning the idle WFI into a no-op and burning a saturated host core per
guest core (100 % at SMP=1, 330 % at SMP=4, vs ~5.6 % at 3 ms). See
[`AKUMA_TIME_EXTRACTION.md`](AKUMA_TIME_EXTRACTION.md).

## What this cost us to learn (method)

- **A single-core boot suite does not verify an SMP primitive.** 286/0 looked like
  a green light on a kernel that freezes at `SMP=4`.
- **The wedge evidence is a test, not a workload.** No real workload (self-host
  build, meow) was shown to freeze on the committed kernel — the devbox at SMP=4
  booted and served 10,000 HTTP requests happily. What makes the test credible is
  that it is a deterministic hard freeze in a test written for this exact wedge
  class, reproduced identically twice. Stated here so nobody re-derives the
  scope by surprise.
- **Back-to-back benchmark runs walk into the §11.2 socket-table cliff.** Runs 4-5
  of an arm scored **half** runs 1-3 with no code change:

  | window | polls/pkt | us/poll | parks | us/park | req/s |
  |---|---:|---:|---:|---:|---:|
  | runs 1-3 | 6.4-7.0 | 11.3-12.2 | 6.9k-16.1k | 236-349 | 1,030-1,077 |
  | runs 4-5 | 4.2 | 16.6-16.8 | 7.3k-7.6k | 531-585 | 525-537 |

  That is the swallowed-wake signature appearing from **run order alone** — it
  would have been read as a real regression on any arm unlucky enough to be
  measured second. A 25 s settle between runs (smoltcp holds `TimeWait` for
  `CLOSE_DELAY` = 10 s) removes it: 5 runs within 3 %. `--settle` in
  `scripts/benchmarks/run_nic_ab.py` exists for this and should not be lowered.
- **A stale QEMU from a previous session silently blocks the next boot.** It holds
  the host forwards and the new VM dies with `Could not set up host forwarding
  rule` — which looks nothing like a port conflict at first glance.
- **The readiness marker can be torn by console interleaving.** `[herd] Started
  sshd` arrived as `[herd] Started [syscall] socket(type=TCP) = fd 3`, so a
  contiguous-string poll waited forever on a perfectly healthy VM and cost a full
  arm. `run_nic_ab.py` now confirms readiness with a real ssh round trip and
  treats the log marker as a hint.

## Reproducing

```bash
# The regression (on 1a29c9c3): wedges after ~23 tests, deterministically.
MEMORY=2048 SMP=4 cargo run --release
grep -ac 'peer_progress' <log>     # 0 = never reached = wedged

# The fix: 294 passed, and the acceptance line must be present.
grep -a 'smp_shared_blocking_wait_peer_progress PASSED' <log>

# The throughput half, one arm at a time (5 x 2000, 25 s settle):
cargo build --release --features devbox-smoltcp,no-tests,net-profile
scripts/benchmarks/run_nic_ab.py --label <arm> --runs 5 --settle 25
```

## Background

- [`AKUMA_NET_ISSUES.md`](AKUMA_NET_ISSUES.md) §9 (the doorbell re-arm fix),
  §11.4 (why per-core wake targeting failed, and the role the yield played in
  making it unimplementable), §11.7 (measurement discipline), §12 (this work).
- [`../runbooks/debug-smp.md`](../runbooks/debug-smp.md) — the original
  socket-recv / `exec_with_io_cwd` wedge and the `idle_halt` + fair-ticket-BKL fix.
- [`../reference/subsystems/locking.md`](../reference/subsystems/locking.md) — the
  FIFO ticket invariant, and how to read `[BKL] stuck`.
