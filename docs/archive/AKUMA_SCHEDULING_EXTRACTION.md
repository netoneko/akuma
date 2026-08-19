# Extracting the scheduling and self-tuning-policy layer

**Date: 2026-08-19.** Follow-up to
[`CROSS_CORE_THREAD_COLLAPSE.md`](CROSS_CORE_THREAD_COLLAPSE.md), which closed
the llama.cpp decode collapse for `-t 1/2/3` and left two open items that are
both *policy* questions: `-t 4` oversubscription (open item 2) and explicit-wake
latency (open item 4).

This session did four things, in this order, and each one changed what the next
one was:

1. Made the **file-page cache cap configurable and elastic** — the leading
   suspect for the remaining single-thread gap (§1).
2. **Simulated** the candidate `-t 4` policies instead of building them, in a
   new `crates/akuma-scheduler` (§2-§5). This killed two of the three
   candidates and re-diagnosed the `-t 4` collapse.
3. Noticed that three subsystems had independently hand-rolled the same
   observe/decide/hysteresis shape, and **extracted it into
   `crates/akuma-kacho`**, rewiring the existing users onto it (§6).
4. Marked the superseded parts of
   [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md)
   section by section (§8).

## The two headline results

> **1. The `-t 4` collapse is not oversubscription.** With a competent wake path
> the model puts `-t 4` at the *peak*; five runnable threads on four cores costs
> ~10-15%, not 14.6x. Reproducing the measured 14.6x needs a futex wake latency
> of **2-3 ms** — and `CROSS_CORE_THREAD_COLLAPSE.md` §2 independently measured
> 0.2-5 ms per futex call on hardware. **Open item 4, not open item 2.**
>
> **2. "Spread work evenly across cores" does not pay.** `4156` iters/s against
> today's `4154`; hard affinity scores `4150`; per-core run queues are not
> indicated at all. The entire available win at `-t 4` is the netpoll wake rate
> (`4975`, 99.8% of the work-conservation bound). Build the netpoll governor; do
> not build the scheduler governor.

## What is where

| path | what | built by |
|---|---|---|
| `crates/akuma-kacho` | **The Chief.** `Latch`, `hysteresis`, `ramp`, `rate_per_sec`. `no_std`, integer-only, pure. 10 tests | the kernel (real dependency) |
| `crates/akuma-scheduler` | Workload model, candidate policies, and the simulator that ranks them | host only — **not** in `default-members`, so `cargo build --release` never sees it |
| `src/file_page_cache.rs` | Elastic cache cap, on `akuma_kacho::hysteresis` | the kernel |
| `crates/akuma-timer` | Tick-demotion governor, now on `akuma_kacho::Latch` | the kernel |

```bash
HOST=$(rustc -vV | grep '^host:' | cut -d' ' -f2)
cargo test -p akuma-kacho     --target $HOST    # 10 tests
cargo test -p akuma-scheduler --target $HOST    #  7 tests, incl. the model's own guards
cargo run  -p akuma-scheduler --release --target $HOST   # the whole matrix, ~1 s
```

---

## 1. The file-page cache cap is now configurable and elastic

`CROSS_CORE_THREAD_COLLAPSE.md` §3 measured the cache pegged at
`entries=131072/131072 evict_mapped=20124` under a llama decode: the 532 MB
mmap'd model against a 512 MB cap (`RAM/8` at `MEMORY=4096`). That is the worst
case the cache has — an `evict_mapped` frees nothing (the frame survives on its
mappers' references) and still costs the next mapper a `read_at`, so the model's
own hot weight pages were being re-read from ext2 at ~2.6 K faults/s, charged to
every thread count.

Three knobs in `src/config.rs`, and an elastic cap in
`file_page_cache::reassess_cap`:

| knob | default | meaning |
|---|---|---|
| `FPCACHE_BASE_RAM_DIVISOR` | 8 | base cap = RAM/8, as before |
| `FPCACHE_INFLATE_PCT` | 20 | extra allowed **on top of the base cap**, when RAM can spare it. 0 restores the old fixed behaviour |
| `FPCACHE_INFLATE_HEADROOM_MULT` | 2 | free RAM demanded before granting it, as a multiple of the inflation |

At `MEMORY=4096`: 512 MB base, 614 MB inflated — enough for the 532 MB model
with ~82 MB left for binaries. Verified on a real boot at `MEMORY=2048`:

```
[fpcache] shared file-page cache enabled, base cap=65536 pages (+20% elastic)
[FPCACHE] entries=263/78643 hits=2635 misses=267 evict=0 evict_mapped=0 inval=3
```

`65536 x 1.2 = 78643`. The `[FPCACHE]` line now prints `entries=<n>/<cap>` so
the cap is visible in the heartbeat rather than only at boot.

Three properties worth stating, because each one is why this is safe:

- **The grant and the withdrawal use different thresholds.** Grow only with
  `2x` the inflation free; hold the growth until free RAM drops below `1x`. A
  workload parked on the line cannot toggle the cap on every check.
- **Withdrawing does not evict.** The over-cap trim in `insert` is lazy, so an
  over-cap cache drains one entry per subsequent insert, still preferring
  unmapped victims. Acute pressure remains the `shrink` hook's job.
- **Engaged-ness is derived, not stored** — the cap already knows whether the
  inflation is granted, so there is no second copy to disagree with it.

The reassessment runs every 512 *inserts*, and inserts only happen on a miss:
it samples fastest exactly when the cache is under pressure and goes silent when
it is serving hits. `pmm::free_count` is two relaxed atomic loads, no lock, so
it is safe to call from the IRQ-masked region and cannot join a lock cycle.

**Not yet measured against llama.** The mechanism is verified; the throughput
claim is not. That is the first thing to run.

---

## 2. Why the `-t 4` candidates were simulated instead of built

Three candidate policies were queued up — per-core run queues, hard affinity,
and an even-spreading placement governor. Each is a kernel build, a devbox boot
and a llama sweep, ~1 h apiece, measured on a machine whose build wall time has
+-4x variance (`FPCACHE_EVICTED_HOT_PAGES` / the +-4x note in
`CROSS_CORE_THREAD_COLLAPSE.md`) — wide enough to hide the
entire effect being measured.

So they were modelled first.

## 3. What the model is

4 cores, a 1 ms timer tick, a global run queue, and two kinds of tenant:

- a **barrier-synchronous compute group** of `-t N` threads. Work is split
  `total / N` across them (as `-t N` splits a matmul); each thread runs its
  slice, spins at the barrier for a budget, and futex-parks if the budget runs
  out. Nothing tells the model that a stalled thread stalls the group — that
  amplification *emerges* from the barrier.
- the **netpoll thread**, woken by packet arrivals and/or a periodic timer.

It models no caches, no TLBs, no memory bandwidth and no BKL, so it can rank
policies and size a wake period but **cannot predict tok/s**. Read ratios from
it, never absolutes.

## 4. Finding: fair-share arithmetic cannot produce the measured collapse

This is the result that reframed the open item. Five runnable threads on four
cores is 125% subscription, and the intuition was that this alone explains
`-t 4`. It does not, by an order of magnitude:

| wake path | model peak | model `-t 4` collapse |
|---|---:|---:|
| wake takes an idle core at SGI latency | `-t 4` | **1.0x** (no collapse at all) |
| wake waits out the rotation to the next tick | `-t 3` | 1.8x |
| **hardware** | **`-t 3`** | **14.6x** |

With a competent wake path the model puts `-t 4` at the *peak*. Oversubscription
costs ~10-15%, not 14.6x. Something else is doing the damage.

### And the model says what, and how much of it

Sweeping the futex wake latency — the one parameter the collapse is sensitive
to — locates it:

| futex wake latency | `-t 3` | `-t 4` | collapse |
|---:|---:|---:|---:|
| 60 us (bare SGI) | 3704 | 2111 | 1.8x |
| 250 us | 3704 | 2016 | 1.8x |
| 500 us | 3704 | 1000 | 3.7x |
| 1000 us | 3704 | 500 | 7.4x |
| **2000 us** | 3704 | 333 | **11.1x** |
| 5000 us | 3704 | 167 | 22.2x |

Reproducing the measured 14.6x needs a wake latency of **~2-3 ms**.

That number was not fitted to anything — it falls out of throughput. And it
lands on top of a completely independent hardware measurement:
`CROSS_CORE_THREAD_COLLAPSE.md` §2 read `/proc/<pid>/syscalls` during a live
decode and found the llama main thread "completing only `futex` calls, each
taking **0.2-5 ms**". Two unrelated measurements, same millisecond-scale wake
path. **Open item 4 (explicit-wake latency), not open item 2
(oversubscription), is the `-t 4` mechanism.**

## 5. Finding: even-spreading is worth ~nothing; the netpoll wake rate is worth 20%

Every placement policy, `-t 4`, netpoll unchanged:

| policy | `-t 1` | `-t 2` | `-t 3` | `-t 4` |
|---|---:|---:|---:|---:|
| round-robin (pre-fix) | 1250 | 2500 | 3704 | 4154 |
| immunity(5) — **today** | 1250 | 2500 | 3704 | 4154 |
| spread governor | 1250 | 2500 | 3704 | **4156** |
| pinned (hard affinity) | 1250 | 2500 | 3704 | 4150 |

Four policies, one number. Then the netpoll wake policy, placement held at
today's:

| netpoll policy | `-t 4` |
|---|---:|
| every tick — **today** | 4154 |
| traffic-adaptive (10 s window) | **4975** |
| spread + traffic-adaptive | 4975 |

The two are **not additive** — spread contributes nothing once netpoll is fixed,
because they were competing for the same scarcity. Work conservation confirms
the ceiling has been reached:

```
-t 4 immunity(5) — TODAY    compute 3.673 + netpoll 0.120 = 3.793 of 4 cores
-t 4 spread + adaptive      compute 3.990 + netpoll 0.004 = 3.994 of 4 cores
```

### The netpoll policy is free, and that is not obvious

The proposal: an RX interrupt still wakes netpoll **immediately**; only the
*periodic* wake backs off, toward 100 ms, as the trailing 10 s traffic window
goes quiet, tightening to the tick at 1000 pps.

`-t 3`, sweeping traffic:

| traffic | policy | core frac | wakes | mean pkt latency |
|---:|---|---:|---:|---:|
| 0 pps | every tick | 0.120 | 10000 | — |
| 0 pps | **adaptive** | **0.001** | 100 | — |
| 20 pps | every tick | 0.120 | 10000 | 110 us |
| 20 pps | **adaptive** | **0.004** | 300 | **110 us** |
| 1000 pps | every tick | 0.120 | 10000 | 110 us |
| 1000 pps | adaptive | 0.120 | 10000 | 110 us |
| 5000 pps | every tick | 0.600 | 50000 | 110 us |
| 5000 pps | adaptive | 0.600 | 50000 | 110 us |

Netpoll's core occupancy drops **30-120x** at low traffic with **identical**
packet latency, and at and above the busy threshold the policy is
bit-for-bit today's behaviour. The latency is untouched because the periodic
wake was never what serviced a packet promptly — the RX interrupt was. That is
the load-bearing observation, and it is why this knob is not a latency trade.

### Pinning does not answer the question, and hides a starvation bug

Hard affinity is the obvious alternative to per-core queues: no migration, no
displacement, no barrier stall. The model scores it at `4150` — indistinguishable
from today.

Worse, the first run *looked* better than that, because a pinned netpoll whose
home core is busy cannot migrate and simply does not run. The policy was
protecting the compute group by starving the network, and only measuring packet
latency alongside throughput exposed it. **Any policy compared on throughput
alone will pick the one that starves the latency-sensitive tenant.**

The same trap caught the spread governor. Its first version bought +14%
throughput by holding packets 10-20x longer (110 us -> 1105-2085 us), because a
single global starvation bound governs a barrier thread (for which 2 ms is one
barrier) and a packet (for which 2 ms is a stalled ACK). Fixing it needed two
things, both of which are the *general* lesson rather than anything about
spreading:

1. **A latency class with its own, much shorter starvation bound.**
2. **Placement that runs on the wake, not only on the next tick** — otherwise
   the class bound is capped by tick granularity however short it is set. This
   is open item 4 again, arriving from a second direction.

With both, spread matches today's latency (110 us) — and still delivers no
throughput.

## 6. The pattern, extracted: `crates/akuma-kacho`

Three separate subsystems have now arrived at the same shape — feed a
measurement in, get a policy decision out, with hysteresis so the decision
cannot flap:

- `akuma-timer`'s runtime governor: idle-loop iterations per tick -> demote the
  tick when the host stops honouring WFI (`crates/akuma-timer/src/lib.rs`).
- the file-page cache: free RAM -> grow the cap by up to
  `FPCACHE_INFLATE_PCT` (`src/file_page_cache.rs::reassess_cap`, 2026-08-19).
- netpoll, proposed here: packets per trailing 10 s -> wake period.

Each was a few dozen lines and each was written independently, so the pattern
was **extracted into `crates/akuma-kacho`** (2026-08-19) rather than
hand-rolled a fourth time:

| primitive | shape | user |
|---|---|---|
| `Latch` | N consecutive confirmations, then a one-way verdict that never reverses | the timer tick's WFI-spin demotion |
| `hysteresis` / `Hysteresis` | two thresholds with a hold band between them | the file-page cache's elastic cap |
| `ramp` | linear interpolation of a knob between two anchors, either direction | the proposed netpoll wake period |
| `rate_per_sec` | events/s over the **elapsed** span, not the nominal window | any windowed rate |

All four are pure, `no_std`, integer-only and host-tested (10 tests). The
existing users were rewired onto them, so nothing here is speculative
infrastructure: `akuma-timer`'s `Governor` is now a `Latch`,
`file_page_cache::reassess_cap` is now a `hysteresis` call, and the simulator's
netpoll policy is `rate_per_sec` + `ramp` — meaning the policy the simulation
scored is literally the decision function that would ship.

The value is not the code, which is trivial. It is that "what stops this
oscillating?" is now a question the type system asks you: `hysteresis` takes two
thresholds, and passing the same value twice — the bug every hand-rolled version
starts with — is a documented, pinned-by-test degenerate case.

### The name

*Kachou* (課長) is "section chief" — Aramaki's title in *Ghost in the Shell*,
and an accurate job description. A chief does not go into the field: he takes
reports and issues verdicts, and the operatives do the work. Nothing in the
crate touches hardware, owns state, allocates, or has a clock.

### Found while reading the timer governor: it is desensitised at SMP>1

**OPEN, not fixed.** `NETPOLL_ITERS` is incremented only by the BSP netpoll loop
(`src/main.rs:1445`), but `GOVERNOR_TICKS` is incremented by **every core** — the
timer handler is one shared dispatch table (`src/timer.rs:134`). So over a
`GOVERNOR_WINDOW_TICKS = 2000` window at SMP=4, only ~500 real ticks of BSP wall
time have passed, while the threshold is still computed as
`2000 x SPIN_ITER_PER_TICK`. The effective spin threshold is 40 iterations per
real tick instead of the intended 10: **the WFI-spin detector is 4x
desensitised, scaling with core count.**

Nothing observed has been attributed to this, and fixing it changes tick
behaviour on a demotion path, so it is recorded rather than changed. But the
detector's output cannot be trusted at SMP>1 until it is fixed, which matters
because "is netpoll's core-worth of CPU real work or unhonoured WFI?" is exactly
the question §5's netpoll finding wants answered on hardware.

## 7. What this changes in the open items

| item | before | after |
|---|---|---|
| 2. `-t 4` oversubscription | assumed to be the `-t 4` mechanism | **it is not** — worth ~10-15%, and the netpoll half of it is worth the other 20%. Do the netpoll governor. |
| 4. explicit-wake latency | "much less urgent post-§0" | **promoted: this is the `-t 4` mechanism.** The model needs 2-3 ms of wake latency to reproduce 14.6x, and §2 independently measured 0.2-5 ms |
| per-core run queues | the M5-class structural fix | **not indicated.** A single global queue reaches 99.8% of the work-conservation bound once netpoll is fixed; the queue was never the constraint |

## 8. What this supersedes in the benchmark doc

[`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) now
carries a per-section status banner, because every measurement in it predates
the three fixes of `CROSS_CORE_THREAD_COLLAPSE.md` — including the EL0
`CNTVCT_EL0`/`CNTFRQ_EL0` trap that was burning 30-80% of *every* core on *any*
workload:

| section | status |
|---|---|
| §8 llama `-t 1/2/3/4` = 36 / 1.6 / 0.28 / 0.18 | **superseded** — now 45.6 / 68.2 / 96.3 / 6.6 |
| §8 Result 1, decode at 86.6% of Linux | **superseded** — decode now exceeds the reference at `-t 1` |
| §9 "Akuma burns ~9x more CPU per kernel crossing" | **stale, direction known.** Every Redis cell paid the same trap tax and the same per-switch full TLB flush; the 9x can only shrink. **Not re-measured** — `scripts/benchmarks/redis_matrix.sh` |
| §4 fixed round-trip ceiling | **probably still real** — a property of the single netpoll drain loop, which nothing has changed. Re-confirm before relying on it |
| §9 "it is what happens when two cores touch the same user page" | **refuted** |
| §1-§3, §5-§7, §10 (method, arms, fairness, noise floor) | **still valid** — about how to measure, not what was measured |

The Redis re-run is the cheapest outstanding measurement in the tree: pure
measurement, no code risk, and it is the only number here that is stale in a
direction that flatters us.

## 9. Caveats — where this model must not be trusted

- **It cannot predict tok/s.** No caches, no TLBs, no memory bandwidth, no BKL.
  Ratios only.
- **`total_phase_work_us` and `barrier_spin_us` are fitted**, not measured, and
  the absolute iteration rates follow from them directly.
- **The 2-3 ms wake-latency conclusion is a prediction, not a measurement.**
  It is falsifiable on hardware: instrument the futex wake-to-run delay during a
  live `-t 4` decode. If it is not milliseconds, this write-up is wrong.
- Two model bugs were found and fixed while writing it (constant per-thread work
  instead of `total / N`, which flat-lined every policy; and wake placement that
  only ran on ticks, which made the tick period look like the bottleneck). A
  third may remain. `tests::fair_share_cannot_explain_the_measured_collapse`
  fails loudly if a future change makes the model reproduce 14.6x, because that
  would mean this document's central claim has changed.

## 10. What to do next, in order

1. **Re-run the llama sweep** against the elastic fpcache (§1). The mechanism is
   verified on a boot; the throughput claim is not measured at all.
2. **Instrument futex wake-to-run delay during a live `-t 4` decode.** This is
   the one experiment that falsifies §4's central claim. If the delay is not
   milliseconds, this document is wrong and the `-t 4` diagnosis reverts.
3. **Build the traffic-adaptive netpoll wake** (§5) — the only policy change the
   simulation endorses. The decision function already exists and is tested:
   `akuma_kacho::rate_per_sec` + `akuma_kacho::ramp`.
4. **Re-run the Redis matrix** (§8). Cheapest outstanding measurement in the
   tree, and the only stale number that flatters us.
5. Fix the timer governor's SMP skew (§6), so its verdict means something at
   SMP>1 before it is used to answer the netpoll question.
6. **Do not** build per-core run queues, hard affinity, or the spread governor
   (§5, §7).

## Background

- [`CROSS_CORE_THREAD_COLLAPSE.md`](CROSS_CORE_THREAD_COLLAPSE.md) — the
  investigation this continues; §2 has the hardware futex timings, §4 the
  results table, §5 the ranked open items.
- [`BENCHMARK_PERFORMANCE_ATTEMPT_0.md`](BENCHMARK_PERFORMANCE_ATTEMPT_0.md) —
  the measurements that opened it.
- [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) — the
  short-sleep floor and `WAKE_DEADLINE_PREEMPT`, the timer-wake half of the
  explicit-wake item.
