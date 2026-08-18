# Terminal stutter: every short sleep costs a full round-robin pass

**Date:** 2026-08-17/18
**Status:** **REOPENED 2026-08-18** — real-world nca use shows this is
**not fixed**: freezes are still present after the landed changes, and
appear to get *worse*, not better, when network traffic and terminal
rendering happen concurrently. See
[Post-fix: nca still freezes](#post-fix-nca-still-freezes-2026-08-18) below.
The probe-driven results below were real, but did not transfer to actual nca
usage — the workload the probes model is apparently not the one that
matters. Original status text, kept for the record:
**FIXED 2026-08-18** — two changes landed:
`WAKE_DEADLINE_PREEMPT` in `crates/akuma-exec/src/threading/mod.rs` (the
deadline wake-pass arms the existing `PREEMPT_WAKE_TID` run-next hint) and
`TIMER_INTERVAL_US` 10 000 → 1 000, **profile-gated**: `extreme-size` keeps
10 ms. Validated on `release` SMP=1 and SMP=4, the 4 MB `extreme-size` floor,
devbox-smoltcp SMP=4, and (2026-08-18) the rump devbox at SMP=4, where the
10 ms baseline is the arm that fails — [`RUMP_ON_SMP.md`](RUMP_ON_SMP.md).
**One axis regressed:** bulk download throughput at SMP=4, ~2× on both
release and devbox-smoltcp; reproduced under an interleaved A/B and not
recovered ([What is still open](#what-is-still-open) item 6). Still open:
the full in-VM kernel-build A/B and the tick sweep.
**Symptom that started it:** "serious stutter in nca", then "looks like networking
stutters the terminal for unknown reason".

## Executive summary

`nanosleep` has a hard floor of **~35.5 ms regardless of the requested duration**.
sshd's session bridge calls `sleep_ms(1)` between polls
(`userspace/sshd/src/main.rs`), so **every byte of terminal output is forwarded at
~27 Hz, in bursts**. That is the stutter.

The floor is not a timer-resolution bug. The timer tick is 10 ms
(`TIMER_INTERVAL_US`, `src/config.rs`), and a thread whose sleep deadline has
expired has **no scheduling priority over threads that have been running all
along** — it waits for a full round-robin pass, one 10 ms timeslice per runnable
thread. Measured slope: **~13 ms of extra sleep per additional runnable thread.**

That is also why it looked like networking: network activity adds runnable
threads (tokio workers, sshd), which lengthens the round. Networking deepens the
stalls; it does not create them.

A one-line change — `TIMER_INTERVAL_US` 10 000 → 1 000 — removes the symptom and
improved **every** axis measured, including CPU-bound work. It is not committed.

> **Resolution addendum (2026-08-18):** both this and the principled fix landed
> — see [Resolution](#resolution-2026-08-18). The paragraph above, and
> everything in [Evidence](#evidence), describes the pre-fix kernel and is
> kept as the record of what was wrong. The mechanism it identifies (expired
> sleepers join the back of the round-robin queue) was confirmed in source by
> [`SCHEDULING_AUDIT.md`](SCHEDULING_AUDIT.md) before the fix was written.

## Evidence

All figures from an isolated VM: `-smp 1 -m 2048`, a COW clone of `disk.img`,
host = Apple Silicon under HVF. Probe: `userspace/ncaprobe` (`sleepbench`,
`termbench`, `pipebench`).

### 1. The sleep floor, against a Linux control

The same static musl binary, on Akuma and under `docker run --platform
linux/arm64`:

| requested | Akuma (10 ms tick) | Linux | Akuma (1 ms tick) |
|---|---|---|---|
| 500 µs | 36,277 (×72.6) | 1,002 (×2.0) | 2,005 (×4.0) |
| 1 ms | 35,702 (×35.7) | 1,991 (×2.0) | 2,004 (×2.0) |
| 2 ms | 35,738 (×17.9) | 2,988 (×1.5) | 2,004 (×1.0) |
| 5 ms | 35,367 (×7.1) | 6,910 (×1.4) | 6,017 (×1.2) |
| 10 ms | 35,380 (×3.5) | 12,109 (×1.2) | 10,025 (×1.0) |
| 20 ms | 35,522 (×1.8) | 23,845 (×1.2) | 20,050 (×1.0) |

The Akuma column is **flat at ~35.5 ms** — the requested duration barely matters
below 20 ms. That flatness is the whole finding: it is a floor, not an overshoot.
Note also the tight ceiling (`max` ≈ 37,0xx µs on every row), i.e. a quantised
wait, not jitter.

### 2. The floor scales with runnable threads

`sleep(1 ms)`, 10 ms tick, varying the number of competing CPU hogs
(`(while true; do :; done) &`):

| competing runnable threads | actual |
|---|---|
| 0 extra | 24,041 µs |
| 4 extra | **76,978 µs** |

≈ **+13 ms per additional runnable thread**, ≈ one 10 ms timeslice plus overhead.
This is what identifies the mechanism as round-robin scheduling latency rather
than timer granularity.

### 3. It is the terminal path, and networking is not the cause

`termbench` writes 1500 × 1024 B to stdout — the same path nca's TUI uses
(pipe → sshd → TCP) — and reports the latency **tail**, because stutter is a tail
phenomenon, not a mean one:

| | 10 ms tick, idle | 10 ms tick, concurrent download | 1 ms tick, idle |
|---|---|---|---|
| wall for 1500 writes | 18,657 ms | 24,427 ms | **1,002 ms** |
| p50 | 2 µs | 3 µs | 1 µs |
| p90 | 36,727 µs | 48,379 µs | **2,005 µs** |
| p99 | 37,255 µs | 59,224 µs | 2,008 µs |
| max | 47,058 µs | 68,922 µs | 3,018 µs |
| **writes > 10 ms** | **526** | **526** | **0** |

The stall **count is identical** with and without 137 MB of concurrent download.
Network load deepens each stall (37 → 48 ms at p90) because it adds runnable
threads; it creates none. p50 = 2 µs shows the write itself is cheap — the cost is
entirely waiting for sshd's next scheduling turn.

### 4. What the 1 ms tick costs: nothing measurable, and it helps

| workload | 10 ms tick | 1 ms tick |
|---|---|---|
| `pipebench` (write+read round trip) | 7.15 µs/iter | **2.85 µs/iter** |
| `md5sum` of 75 MB `/bin/crush`, warm | 1.27 / 2.23 s | **0.48 s** |
| boot self-test suite | — | **283 PASSED, 0 FAILED** |

The expected cost of 10× the timer IRQ rate did not appear even on CPU-bound
work. The likely reason is that blocking syscalls stop costing a scheduler round,
which dominates the extra interrupt overhead at this tick rate. **This needs
confirming on a long workload — see below.**

### 5. Every poll-interval knob in the kernel is currently inert

`sys_epoll_pwait` caps each loop iteration's sleep at
`effective_poll_interval_us` — 10 ms normally (`BLOCKING_POLL_INTERVAL_US`), 1 ms
for rump fds (`RUMP_BLOCKING_POLL_INTERVAL_US`) — and re-scans. Measured against
an fd that never becomes ready, 10 ms tick:

| requested timeout | Akuma | Linux |
|---|---|---|
| 1 ms | 35,334 µs (×35.3) | 1,975 µs (×2.0) |
| 2 ms | 35,689 µs (×17.8) | 2,994 µs (×1.5) |
| 5 ms | 36,040 µs (×7.2) | 6,948 µs (×1.4) |
| 10 ms | 35,928 µs (×3.6) | 12,588 µs (×1.3) |
| 50 ms | 71,170 µs (×1.4) | 54,082 µs (×1.1) |

**1 ms and 10 ms are indistinguishable**, and 50 ms takes *two* rounds. The cap
determines when a thread becomes **eligible**, not when it **runs** — so lowering
it below the round-robin period buys nothing. `RUMP_BLOCKING_POLL_INTERVAL_US`
cannot deliver the 1 ms cadence its comment describes, and neither can any future
knob of the same shape.

This also corrects a claim made in
[`../runbooks/debug-async-subprocess-hang.md`](../runbooks/debug-async-subprocess-hang.md)
and [`debug-delayed-first-byte.md`](../runbooks/debug-delayed-first-byte.md):
that a poller "re-checks every fd at least every 10 ms whether or not a `Waker`
fires". The *logic* survives — a watcher cannot sleep through a state change
forever, so a hang is still never a lost wakeup — but the **number is wrong in
practice**: it is ~35 ms, and it grows with runnable-thread count.

## What this is *not*

Ruled out with evidence, recorded because the next person will suspect the same
things:

| Hypothesis | Verdict |
|---|---|
| Networking blocks the terminal via a shared lock / I/O guard | **No** — stall count identical with and without network load (§3) |
| The `epoll_on_fd_drained` call added to the pipe read path on 2026-08-17 | **No** — A/B of three kernels (as-committed / optimised / call deleted) was noise-dominated: medians 7.20 / 7.15 / 7.33 µs per iter, within-variant spread ±2×. Deleting the call outright is not faster |
| Host sleep/wake stalls | **No** — only 6 `[WATCHDOG] Time jump` events in 798 s of log |
| BKL contention / lock storm | **No** — no `[BKL] stuck`, no `POOL contended`, no deadlock lines anywhere in the logs |
| nca doing something pathological | **No** — its own profile is unremarkable: `pgfault=247`, FSCACHE 91 % hit, FPCACHE `evict=0` |

## Resolution (2026-08-18)

Both changes landed, after a three-arm A/B on `release` SMP=1 (medians,
round-1 discarded, 5 rounds probes / 5 downloads; host on AC, load ~2):

- **`WAKE_DEADLINE_PREEMPT`** (`crates/akuma-exec/src/threading/mod.rs`): the
  scheduler's deadline wake-pass now arms `PREEMPT_WAKE_TID` for the
  earliest-deadline thread it promoted, so an expired sleeper runs **next**
  instead of joining the back of the round-robin queue. The hint machinery
  already existed for `ThreadWaker::wake` (compiled out behind
  `WAKEUP_LOCALITY_HINT`); the fix arms it from the one path that never set
  it. When several sleepers expire in one pass the earliest deadline wins the
  hint; the rest rotate as before. Fairness is preserved: the hint fires once
  per wake and the preempted thread keeps its rotation position.
- **`TIMER_INTERVAL_US` 10 000 → 1 000**, **profile-gated**
  (`#[cfg(not(kernel_profile_extreme))]`): `extreme-size` keeps 10 ms — a
  4 MB single-core box pays for every interrupt, has no `sc-epoll`, and the
  1 ms arm was not measurable there (see Matrix B note).

### Release, SMP=1

| metric | base (10 ms, no preempt) | B: preempt only | AB: preempt + 1 ms |
|---|---|---|---|
| `sleepbench` 1 ms actual | ~29–43 ms | 14.75 ms (tight) | **1.06 ms** |
| `pollbench` 1 ms actual | ~41 ms | 12–51 ms (bimodal) | **1.01 ms** |
| `pipebench` µs/iter | 10.4 | 10.1 | **3.25** |
| `termbench` p90 / stalls >10 ms | 36.3 ms / 1010 | 51.3 ms / 477 | **2.0 ms / 0** |
| `termbench --net` p90 / stalls | 58.9 ms / 526 | 61.5 ms / 526 | **4.0 ms / 0** |
| 128 MB download | 6.3 s | 9.5 s | **3.4 s** |
| boot suite | 284/0 | 284/0 | **284/0** |
| idle `[Heartbeat]`/35 s | 1 | 1 | 1 |

Readings worth recording:

- **B alone (preemption at 10 ms tick) was not good enough and one axis
  regressed.** Sleep latency hits the tick floor (~14.7 ms = one 10 ms tick +
  overhead), `pollbench` went bimodal (a missed hint costs a full 10 ms
  round; consistent with competing hints at coarse granularity — unproven,
  made moot by AB), and the download slowed ~50 % (6.3 → 9.5 s). The
  preemption alone is not the fix; at a 10 ms tick it mostly moves threads
  mid-slice without delivering sub-tick latency.
- **AB improved every axis, including the two that regressed B.** Download
  3.4 s vs base 6.3 s — the preemption tax seen in B does not exist at 1 ms;
  shorter quanta mean each preemption costs less and deadline wakes land
  sooner. `termbench --net` moved fewer KiB (~137 vs ~180 MB) only because
  the probe finished ~10× faster; the concurrent download had less time to
  run. Not a regression.
- **An invalid first pass is preserved as a trap record**: the initial base+B
  runs were taken on battery power; arm B then showed a uniform ~40 %
  degradation across unrelated axes (pipe, download, termbench together),
  which is the signature of host-side throttling, not a scheduler effect.
  Re-run on AC: base numbers moved, B's cross-axis regression shrank to the
  download-only signal above. **Check power source, not just host load.**

### Release, SMP=4

| metric | base (10 ms, no preempt) | fixed (1 ms + preempt) |
|---|---|---|
| `sleepbench` 1 ms actual | 5.57 ms (5.42–7.13) | **1.01 ms** (1.012–1.019) |
| `pollbench` 1 ms actual | 3.94 ms (3.66–5.10) | **1.02 ms** (1.013–1.019) |
| `pipebench` | 1.43 µs/iter (1.15–1.65) | 1.51 µs/iter (1.44–1.59) |
| `termbench` p90 / stalls | 62 µs / 2 | 91 µs / 0 |
| 128 MB download | **1.23 s** (1.23–1.26) | 2.48 s (2.41–2.50) |
| boot suite | 291/0 lines, `[BKL] stuck` 96 | 291/0 lines, `[BKL] stuck` 93 |

### devbox-smoltcp, SMP=4 (the config nca runs on)

| metric | base (10 ms, no preempt) | fixed (1 ms + preempt) |
|---|---|---|
| `sleepbench` 1 ms actual | 4.38 ms (4.10–5.44) | **1.02 ms** (1.017–1.021) |
| `pollbench` 1 ms actual | 5.87 ms (3.73–7.10) | **1.03 ms** (1.021–1.034) |
| `pipebench` | 1.06 µs/iter (1.03–1.09) | 1.71 µs/iter (1.58–1.83) |
| `termbench` p90 / stalls | 46 µs / 0 | 81 µs / 0 |
| 128 MB download | **1.20 s** (1.20–1.24) | 2.55 s (2.53–2.57) |
| in-VM `cargo build --release` of `/root/hello` (`-j1`/`-j4`) | 17.4 / 17.4 s | 17.7 / 17.7 s — **flat**, within noise, 0 OOM / 0 `[BKL] stuck` both arms |

### The SMP=4 columns say something the SMP=1 columns do not

The base arms for both SMP=4 configs were collected **after** the fix was
already committed, and they change the story materially. Two findings:

- **The catastrophic floor is an SMP=1 phenomenon.** At SMP=4 the *unfixed*
  kernel already slept in 4.4–5.6 ms (not ~35 ms) and already wrote the
  terminal at 46–62 µs p90 with 0–2 stalls. More cores means more chances
  to be picked in a round, so the round-robin tax mostly disappears. The
  fix still tightens sleep/poll to a hard 1.0 ms, but it is buying
  *predictability* there, not rescuing a broken axis.
- **Bulk throughput regressed ~2× at SMP=4, on both configs**: 1.23 → 2.48 s
  (release) and 1.20 → 2.55 s (devbox) for the same 128 MB. Within-arm
  spread is a few percent in every cell, so this is not measurement noise;
  the arms were not interleaved, though, which is the one confound left
  (see open item 6). devbox `pipebench` moved the same way (1.06 → 1.71
  µs/iter) while release `pipebench` stayed flat.

  Mechanism, unproven but consistent with the SMP=1 B arm: at 1 ms the
  kernel takes 10× the timer interrupts and the wake hint fires far more
  often, so a bulk-copy loop is interrupted mid-slice ~10× as much. At
  SMP=1 that cost was invisible because the *baseline* was so much worse
  (6.27 → 3.40 s, a win); at SMP=4 there is no broken baseline left to
  rescue, so only the cost shows.

The two `[BKL] stuck` counts settle open item 5: the base arm logged **96**
lines to the fixed arm's **93** under the same load. The count is a property
of SMP=4 contention, not of the tick or the wake hint.

The cargo result is *flat, not faster*, and the crate explains it: `hello`
is one file, so the build is a single CPU-bound rustc+link run with no
cross-process pipe traffic — the axis that was never taxed (CPU-bound
`md5sum` was flat-to-better in the original evidence too). The
"builds get faster" prediction applies to multi-crate `-j4` builds where
rustc↔cargo pipe round-trips dominate; that is the full self-host kernel
build, left as the follow-up below.

### extreme-size (10 ms tick + preemption)

The probe cannot run there: fetching the 1.2 MB static musl binary into guest
RAM on a 4 MB box drops `pmm_free` to 125/1024 pages and every session fork
fails (`clone: fork failed: Kernel memory low`); at 8 MB the probe's own
2 MB heap allocation is OOM-killed. Both are memory-floor facts, not
scheduler findings. What was verified at the real 4 MB floor (SMP=1,
10 ms + preemption kernel): boots to sshd, 6/6 `fork + busybox exec` rounds
over ssh, 0 fork-failures, 0 `[OOM]`, idle 1 heartbeat/30 s — same idle
profile as every other arm, so the retained 10 ms tick costs nothing
observable at idle. Scheduler-latency behaviour at 10 ms + preemption is
characterized by the release-B arm (sleep ≈ one tick + overhead).

### Full matrix, all arms in one table

Medians, round 1 discarded. Every arm booted from a saved kernel binary
(`shasum`-distinct), same host, on AC. Reproduce or extend with
`scripts/sched_audit_matrix.py` (`run` / `report`).

| arm | SMP | RAM | tick | wake-preempt | sleep 1 ms | poll 1 ms | pipe µs/iter | term p90 | stalls >10 ms | term+net p90 | 128 MB dl | suite | `[BKL] stuck` |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| release base | 1 | 2048 | 10 ms | off | 29.18 ms | 40.65 ms | 10.43 | 36.35 ms | 1010 | 58.9 ms | 6.27 s | 284/0 | 0 |
| release B | 1 | 2048 | 10 ms | **ON** | 14.75 ms | 44.42 ms | 10.12 | 51.33 ms | 477 | 61.5 ms | 9.54 s | 284/0 | 0 |
| release fixed | 1 | 2048 | **1 ms** | **ON** | **1.06 ms** | **1.01 ms** | **3.25** | **2.01 ms** | **0** | **4.0 ms** | **3.40 s** | 284/0 | 0 |
| release base | 4 | 2048 | 10 ms | off | 5.57 ms | 3.94 ms | 1.43 | 62 µs | 2 | — | **1.23 s** | 291/0 | 96 |
| release fixed | 4 | 2048 | **1 ms** | **ON** | **1.01 ms** | **1.02 ms** | 1.51 | 91 µs | **0** | — | 2.48 s | 291/0 | 93 |
| devbox base | 4 | 4096 | 10 ms | off | 4.38 ms | 5.87 ms | **1.06** | 46 µs | 0 | — | **1.20 s** | n/a | 0 |
| devbox fixed | 4 | 4096 | **1 ms** | **ON** | **1.02 ms** | **1.03 ms** | 1.71 | 81 µs | 0 | — | 2.55 s | n/a | 0 |
| rump base | 4 | 4096 | 10 ms | off | 3.88 ms | 3.69 ms | 1.83 | — | — | — | **never completed** | n/a | **5225** |
| rump fixed | 4 | 4096 | **1 ms** | **ON** | **1.01 ms** | **1.01 ms** | 2.22 | — | — | — | 104.4 s | n/a | **0** |
| extreme 4 MB | 1 | 4 | 10 ms | ON | — | — | — | — | — | — | — | boots, 6/6 forks, 0 OOM | 0 |
| extreme 8 MB | 1 | 8 | 10 ms | ON | — | — | — | — | — | — | — | probe OOM-killed | — |

Bold = best cell in its config group. `n/a` = devbox builds with `no-tests`,
so there is no boot suite to count. The `extreme-size` rows carry no
scheduler numbers at all — the probe does not fit (see below), so that
profile's 10 ms tick is a *decision by default*, not a measured one.

Read down the columns, not across configs: the SMP=1 rows and SMP=4 rows
measure different machines, and the download column in particular is not
comparable between them (four cores move more bytes).

In-VM `cargo build --release` of `/root/hello` on devbox, `-j1` / `-j4`:
17.4 / 17.4 s base vs 17.7 / 17.7 s fixed — flat.

## The fix candidate

> **Historical section — written 2026-08-17/18 before the fix landed.** The
> decision it argues through was taken; see [Resolution](#resolution-2026-08-18).
> The rump-recollection below motivated the profile gate that shipped.

```rust
// src/config.rs
pub const TIMER_INTERVAL_US: u64 = 10_000;   // -> 1_000
```

The constant has been `10_000` since the early `ed04578b "fixed sleep"` commit,
carries only the comment `// Timer interval in microseconds`, and has no rationale
in `scheduler.md` or `config-flags.md`.

**Do not read that silence as "nobody chose it".** The author's recollection
(2026-08-18) is that it was probably **set empirically**, and possibly **because
10 ms was compatible with the rump kernel** — held with low confidence, but it is
first-hand and outranks the absence of a comment. Two consequences:

- The empirical part means some workload once argued for 10 ms. If the matrix
  below comes back clean, that workload was either not represented in it or no
  longer applies — worth a moment's thought before assuming it was arbitrary.
- The rump part is a **latent risk that this investigation will not close**: rump
  is out of scope for the measurements (deferred work, handled separately), so a
  global change ships with that question open. If rump later misbehaves on
  timing, this is the first thing to revisit. A profile-dependent constant would
  sidestep it entirely.

An earlier draft of this doc called it "an unexamined default"; that was my
inference from absence of documentation and it was wrong.

**Shortening the tick treats the symptom.** The underlying issue is that an
expired sleeper has no priority over threads that never yielded. Alternatives,
roughly in increasing order of effort:

1. **Wake preemption** — when a sleep deadline expires, make that thread eligible
   to run *next* rather than at the end of the round. Same latency win without
   10× the interrupts. This is the principled fix.
2. **Shorter tick** (the one-liner above) — measured, effective, blunt.
3. **Tickless / one-shot timer** programmed to the earliest pending deadline.
   Best latency, largest change.
4. **Fix the consumer** — sshd's bridge polls with `sleep_ms(1)` instead of
   blocking on readiness. A real `ppoll` on {child fd, socket fd} would be woken
   by data. Helps the terminal specifically, but every other poll loop in the
   system keeps the same floor, and a woken thread *still* waits its round-robin
   turn — so this is complementary to 1, not a substitute.

## What still needs measuring

> Superseded 2026-08-18 — Matrix A (SMP=1), the SMP=4 column, devbox-smoltcp
> and the 4 MB extreme floor were run; see [Resolution](#resolution-2026-08-18).
> The ground rules and traps below are kept verbatim because they are the
> method, and the remaining gaps in [What is still open](#what-is-still-open)
> still demand them. What is left here is the original plan, unfilled cells
> and all.

### Ground rules

- **Two arms per row: `TIMER_INTERVAL_US = 10_000` (current) and `1_000`.** Build
  each once, keep both binaries, and label them. Verify you booted the arm you
  think you did: `cmp` the two `.bin`s and check the boot banner.
- **After any manual `cargo build`, re-run `rust-objcopy`.** `cargo build` does
  not regenerate `akuma.bin`; only `cargo run` does. See
  [Traps](#traps-all-of-which-cost-time-in-this-investigation) — this already
  produced one wrong measurement in this investigation.
- **≥5 rounds per cell, report the median, discard the first** (cold page cache).
  Record min/max too — a median without a spread is not interpretable here.
- **Check host load before and after each cell** (`uptime`). This host sat at
  load 4.3 mid-measurement because Spotlight was indexing a fresh build tree, and
  that alone moves these numbers more than the effect being measured. Quiesce
  first; re-run any cell taken under load.
- **Anything under ~20 % is below the noise floor** of wall-clock microbenchmarks
  here. Don't report it as a difference.

### Matrix A — `release` profile, the full workload set

`cargo build --release`; `SMP=N cargo run --release`. Four cells per row.

| Workload | metric to record | 10 ms<br>SMP=1 | 1 ms<br>SMP=1 | 10 ms<br>SMP=4 | 1 ms<br>SMP=4 |
|---|---|---|---|---|---|
| `ncaprobe sleepbench` | actual µs for a 1 ms request (median of 40) | 35,702 | 2,004 | | |
| `ncaprobe termbench` | p90 µs, and count of writes > 10 ms | 36,727 / 526 | 2,005 / 0 | | |
| `ncaprobe termbench --net` | p90 µs, count > 10 ms, KiB moved | 48,379 / 526 | | | |
| `ncaprobe pollbench` | actual µs for a 1 ms `epoll_wait` timeout | 35,334 | | | |
| `ncaprobe pipebench` | µs/iter (median of 5) | 7.15 | 2.85 | | |
| **Big-file download ×5** | MB/s per round, median + spread | | | | |
| **Clean `cargo build --release` ×3** | wall seconds per round, median | | | | |
| Boot self-test suite | PASSED / FAILED counts | | 283 / 0 | | |
| Idle CPU | `[Heartbeat] Loop` delta per 30 s, VM otherwise idle | | | | |

Pre-filled cells are this investigation's measurements, all `-smp 1`; treat them
as one sample, not a baseline — re-measure them under the ground rules above.

The two rows that matter most for the decision are the last four, because they
are where 10× the timer IRQ rate could plausibly *cost* something. The download
and the build are the ones that would veto the change.

### Matrix B — other profiles, probes + download only

Per the request: no cargo builds here, just the probe set and the big-file
download.

| Profile / how to build + run | 10 ms | 1 ms |
|---|---|---|
| **devbox-smoltcp** — `scripts/build_devbox_smoltcp.sh` then `overlays/devbox/run-smoltcp.sh` (`SMP=4`, `MEMORY=4096`, `devbox.img`). The configuration nca is actually used on, so this is the one that has to improve for the change to be worth anything. | | |
| **extreme-size** — `scripts/build_extreme_size.sh`. **The arm most likely to object:** a 4 MB, single-core box pays for every interrupt, and it has no `sc-epoll`. If the tick costs measurably here, the constant should become profile-dependent rather than global — which changes the shape of the fix. | | |

Record for each cell: `sleepbench` 1 ms row, `termbench` p90 + stall count,
`pipebench` median, and download MB/s.

**rump is deliberately out of scope here** — it is deferred work and its owner
will handle it separately. Note the risk rather than testing it: if 10 ms was
picked partly for rump compatibility (see
[The fix candidate](#the-fix-candidate)), a global change could surface there
later.

An earlier draft argued rump would be undisturbed because its poll path already
uses a 1 ms cadence (`RUMP_BLOCKING_POLL_INTERVAL_US`). [§5](#5-every-poll-interval-knob-in-the-kernel-is-currently-inert)
retires that argument in the opposite direction: that knob is **inert** — 1 ms and
10 ms caps produce the same ~35 ms wake. The author's recollection is that it was
*measured* when rump was built — **on an SMP=1 configuration, per the author,
2026-08-18** — which is worth reconciling, because on today's scheduler it
cannot do what it was set to do *even single-core*: the base arm of the
Resolution A/B is SMP=1 and still shows `pollbench` 1 ms → ~41 ms. Either the
measurement predates something that changed, or the improvement it captured
came from elsewhere in the same change. Whoever picks rump back up should
treat that constant as unverified rather than load-bearing — and note that
after the fix, the 1 ms `TIMER_INTERVAL_US` finally gives that knob the
sub-10 ms cadence it always assumed it had.

## Post-fix: nca still freezes (2026-08-18)

After `WAKE_DEADLINE_PREEMPT` + the 1 ms tick landed, real-world use of `nca`
(the terminal-rendering client this investigation was originally chasing)
still shows the freeze/stutter symptom. This contradicts the "FIXED" verdict
above, which was based entirely on `ncaprobe` synthetic workloads
(`sleepbench`, `termbench`, `pipebench`) and the full-matrix A/B, not on nca
itself.

Observed: freezes appear to get **worse**, not better, when network traffic
and terminal rendering happen **at the same time** — i.e. concurrent
sshd-forwarded traffic plus active screen redraw is the trigger, not either
alone. This is consistent with the fixed round being *shorter* in isolation
but the round still growing under combined load (more runnable threads →
longer round, [§2](#2-the-floor-scales-with-runnable-threads)), or with a
different mechanism entirely that the probes never modeled — the probes
exercise sleep/pipe/terminal paths independently, not simultaneously under
real sshd + nca traffic patterns.

Not yet root-caused. Nothing here has been reverted. Next steps: reproduce
under nca itself (not `ncaprobe`) with traffic and rendering concurrent,
capture `PSTATS` and BKL contention during a live freeze, and check whether
this is the same "expired sleepers join the back of the round-robin queue"
mechanism at a different multiplier, or a distinct bug the fix didn't touch.

## What is still open

1. **Full in-VM kernel-build A/B** (`scripts/run_selfhost_kernelbuild.py`,
   or `cargo build --release -j4` in the `/root/akuma` devbox clone). The
   `hello` crate was flat (single CPU-bound rustc run, no pipe traffic);
   the multi-crate build is where round-trip savings should appear, and it
   is the last veto-class workload nobody re-measured. ~30+ min per arm.
2. **Rump** — partly answered on 2026-08-18 at SMP=4 (item 7 below,
   [`RUMP_ON_SMP.md`](RUMP_ON_SMP.md)), where the 10 ms arm is the one that
   fails. Still owed: the same comparison at **SMP=1**, rump's historical
   configuration and the one the "10 ms was chosen for rump" recollection
   refers to. Until that exists, the profile gate hedges a risk that has
   been contradicted at 4 cores and never tested at 1.
3. **Tick sweep** (500 µs / 2 ms / 5 ms) on the best and worst rows — is
   2 ms as good as 1 ms at half the interrupts? The knee was never found.
4. **`extreme-size` on a 1 ms tick** — **DEFERRED by decision, 2026-08-18.**
   The profile keeps 10 ms; the gate in `src/config.rs` stays. This is a
   deliberate hold, not an oversight: the case for flipping is real but
   entirely inferential, and nobody wanted to spend the boots today.

   *The case for flipping, when someone picks this up:*

   - `extreme-size` is **SMP=1**, and the one measured cost of the 1 ms tick
     (bulk throughput, ~2×) appeared only at **SMP=4**. At SMP=1 the 1 ms
     arm was *better* on throughput too — 128 MB download 6.27 → 3.40 s.
   - The axis where 10× the interrupt rate should hurt most is CPU-bound
     work, and there the 1 ms tick won at SMP=1 as well
     ([§4](#4-what-the-1-ms-tick-costs-nothing-measurable-and-it-helps):
     `md5sum` 1.27 / 2.23 s → 0.48 s).
   - Extreme currently runs the **B-arm configuration** — 10 ms tick *with*
     `WAKE_DEADLINE_PREEMPT` — which measured a **~14.7 ms** sleep floor. It
     is the only profile still paying a floor an order of magnitude above
     the rest of the matrix.

   *Why it is not a foregone conclusion:*

   - Extreme is not "release with less RAM". It builds
     `--features no-tests,smoltcp,extreme,userspace-sshd`, i.e. **no
     `sc-epoll`**. `pollbench` is therefore structurally unmeasurable there,
     and userspace sshd's polling path — the consumer this whole
     investigation started from — is a *different* path on that profile.
     The mechanism the fix exploits may not behave the same way.
   - The real risk is interrupt overhead on a small, slow single core. That
     is a CPU cost, not a memory one, so the 4 MB floor is not what is in
     danger; throughput is.

   *The experiment that would settle it* (~4 boots, ~30 min): the blocker
   was never the tick, it was the probe — 1.2 MB of binary plus a 2 MB heap
   does not fit a 4-8 MB box. Boot the **extreme kernel at 32 MB** (a
   scheduler test, explicitly not a floor test) and A/B `sleepbench` +
   `pipebench` + a CPU-bound `busybox time` run across both ticks; then one
   **4 MB functional boot per tick** (ssh up, fork + exec rounds, 0 OOM,
   idle heartbeat cadence) to confirm the floor is unaffected. If that comes
   back clean, drop the gate and use 1 ms everywhere.
5. ~~**SMP=4 `[BKL] stuck` count**~~ — **closed 2026-08-18.** The 10 ms
   baseline logged **96** lines to the fixed arm's **93** under the same
   load. Contention noise, unrelated to the tick or the wake hint.
6. **The SMP=4 bulk-throughput regression** (128 MB download 1.2 s → 2.5 s
   on both release and devbox; see "The SMP=4 columns say something…").
   Reproduced under an interleaved A/B, so it is not drift — but the
   mechanism is unproven and no attempt has been made to recover it. The
   obvious levers, cheapest first: (a) a 2 ms tick, if the tick sweep shows
   the latency knee is below it; (b) suppress the run-next hint when the
   preempted thread is inside a bulk copy (no cheap signal exists today);
   (c) accept it — 2.5 s for 128 MB is still ~50 MB/s and better than every
   SMP=1 arm. This is the one axis where the fix is strictly worse than
   what it replaced.
7. **Rump at SMP=4** — **measured 2026-08-18, and it inverts the risk this
   investigation hedged against.** Full write-up:
   [`RUMP_ON_SMP.md`](RUMP_ON_SMP.md). Rump had never run multi-core at all
   (`scripts/build_devbox.sh` is `--no-default-features`, which drops
   `smp-shared`); with that feature added, the **fixed** kernel boots 4/4
   cores, passes rump's boot tests, sleeps/polls at 1.01 ms and completes a
   128 MB download over the sysproxy stack 5/5 times with **0** `[BKL]
   stuck` lines. The **10 ms baseline** on the same image never completed a
   single download and logged **5225** `tag=511` stuck lines. Caveats: one
   run each, base arm on battery, and `smp-shared`+rump is itself a new
   combination — but the direction is the opposite of the feared one.
   Remaining: the HTTPS A/B (probe wired, unrun), a 10 ms SMP=1 third point,
   and an AC re-run.
8. **nca still freezes with the fix landed** — **new, top priority.** See
   [Post-fix: nca still freezes](#post-fix-nca-still-freezes-2026-08-18).
   Real nca usage, not `ncaprobe`, still stutters, and appears to worsen
   specifically when traffic and terminal rendering overlap. Unlike items
   1-7, this calls into question whether the fixed mechanism was the actual
   cause of the symptom nca users see, not just an untested axis.

### Running the workloads

**Probes** — build once on the host, fetch in the guest (do **not** repopulate
`disk.img` under a live VM):

```bash
userspace/ncaprobe/build-musl.sh --serve        # host, serves on :8899
```
```bash
# guest
curl -s -o /tmp/nb http://10.0.2.2:8899/ncaprobe && chmod +x /tmp/nb
/tmp/nb sleepbench
/tmp/nb termbench
/tmp/nb termbench --net
/tmp/nb pipebench
```

**Linux control** — the same static binary, which is what makes any Akuma number
arguable rather than absolute:

```bash
cd userspace/ncaprobe/target/aarch64-unknown-linux-musl/release
docker run --rm --platform linux/arm64 -v "$PWD":/p:ro alpine /p/ncaprobe sleepbench
```

**Big-file download** — serve a fixed file from the host and pull it 5×. Use one
large enough that TCP leaves slow-start and the transfer dominates connection
setup; 256 MB was not tried, pick a size that takes ≥10 s on the slower arm and
keep it identical across every cell:

```bash
# host, once
mkdir -p /tmp/bigserve && dd if=/dev/urandom of=/tmp/bigserve/big.bin bs=1m count=256
python3 -m http.server 8899 --bind 0.0.0.0 --directory /tmp/bigserve
```
```bash
# guest, 5 rounds
for i in 1 2 3 4 5; do
  /bin/busybox time /bin/busybox wget -q -O /dev/null http://10.0.2.2:8899/big.bin
done
```

`wget`/`curl` to `/dev/null` keeps the disk out of the measurement. If you want
the disk in it, write to a file and say so in the cell — but keep it consistent.

**Clean `cargo build --release`** — this is the in-guest build, the heavy
multi-process CPU + I/O workload where extra timer interrupts would show up if
they cost anything. Pin one target and use it for every cell; a mid-size crate is
better than the whole kernel because it fits in a reasonable measurement window:

```bash
# guest
cd <fixed crate>
cargo clean && /bin/busybox time cargo build --release -j1
```

Choices that must be pinned identically across all four cells, and stated in the
result: **which crate**, and **`-j`**. `-j1` isolates the scheduler question;
`-j4` is more realistic but adds the fork/exec and thread-spawn paths, which have
their own history (`docs/archive/`, self-host notes). Running both would be
better than choosing. `scripts/run_selfhost_kernelbuild.py` is the heavier
alternative if a bigger signal is needed.

### Reading the result

> **Outcome 2026-08-18:** landed as a *combination* — wake preemption **and**
> the 1 ms tick, gated off for `extreme-size`. The rules below were applied
> with one amendment learned from the B arm: preemption at a 10 ms tick
> regressed the download ~50 %, so "reject the tick, keep preemption" was
> never actually on the menu — the two changes only work together. The tick
> sweep and the full kernel-build row remain open; see
> [What is still open](#what-is-still-open).

- **Land the one-liner** if the download and build rows are flat or better on the
  1 ms arm across `release` SMP=1/4 *and* devbox-smoltcp, and `extreme-size` shows
  no idle-CPU regression.
- **Make it profile-dependent** if only `extreme-size` regresses.
- **Reject it and go to wake preemption** (option 1 above) if the build or
  download regresses on `release` — that would mean the interrupt cost is real
  and the latency win has to come from scheduling policy instead of tick rate.
- Either way, the **tick sweep** (500 µs / 1 ms / 2 ms / 5 ms) on the single best
  and single worst row finds the knee. If 2 ms is as good as 1 ms for latency at
  half the interrupts, that is the better default.

### Traps, all of which cost time in this investigation

- **`akuma.bin` goes stale.** `cargo build` does **not** regenerate it; only
  `scripts/cargo_runner.sh` (i.e. `cargo run`) objcopies. Booting
  `-kernel target/aarch64-unknown-none/release/akuma.bin` by hand after a plain
  `cargo build` runs the *previous* kernel. This bit this investigation: the ELF
  was 3.80 MB / 23:45 while the `.bin` was 1.68 MB / 23:34. Always
  `rust-objcopy -O binary target/aarch64-unknown-none/release/akuma{,.bin}`
  after a manual build, and `cmp` against the other variant's binary to prove you
  booted the arm you think you did.
- **Variance swamps small effects.** The `epoll_reset_edge` A/B above was
  noise-dominated: ±2× within a single variant. Take medians of ≥5 runs, discard
  the first (cold cache), and check host load (`uptime`) — Spotlight indexing a
  fresh build tree put this host at load 4.3 mid-measurement. Any claim under
  ~20 % needs a different method than wall-clock microbenchmarks.
- **Check the host is on AC power** (`pmset -g batt`), not just its load. The
  first base+B pass of the Resolution A/B ran on battery and showed a uniform
  ~40 % degradation across *unrelated* axes on one arm — the signature of
  host-side throttling, not a scheduler effect. It moved every number enough
  to have flipped the decision the other way.
- **`PSTATS` is per-thread, not per-process.** "The process never calls `read`"
  read off the main thread's line is wrong; the reads were on a worker.
- **Never write `disk.img` under a live QEMU.** Use `cp -c` (APFS COW clone) for
  an isolated image, private ports, and fetch binaries over HTTP from
  `10.0.2.2` rather than repopulating the disk.
- `interest_fds=` in `[epoll] pwait ret` is hardcoded `0` and means nothing.

## Background

- [`TOKIO_PIPE_EPOLL_HANG.md`](TOKIO_PIPE_EPOLL_HANG.md) — the investigation
  immediately preceding this one, which introduced `userspace/ncaprobe` and whose
  fix was briefly (and wrongly) suspected of causing this stutter.
- [`../runbooks/debug-async-subprocess-hang.md`](../runbooks/debug-async-subprocess-hang.md)
  — probe usage and the epoll-edge debugging ladder.
- [`../../userspace/ncaprobe/README.md`](../../userspace/ncaprobe/README.md) — the
  probe, its subcommands, and the Linux A/B method.
- [`../reference/subsystems/scheduler.md`](../reference/subsystems/scheduler.md) —
  scheduler reference; contains no rationale for the 10 ms tick, which is itself
  part of the finding.
