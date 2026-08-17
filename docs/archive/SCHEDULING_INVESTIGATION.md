# Terminal stutter: every short sleep costs a full round-robin pass

**Date:** 2026-08-17/18
**Status:** **OPEN** — root cause identified and a one-line fix candidate measured,
but **not landed** and **not validated beyond one configuration**. The measurement
matrix in [What still needs measuring](#what-still-needs-measuring) is the handoff.
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

## The fix candidate

```rust
// src/config.rs
pub const TIMER_INTERVAL_US: u64 = 10_000;   // -> 1_000
```

The constant has been `10_000` since the early `ed04578b "fixed sleep"` commit,
carries only the comment `// Timer interval in microseconds`, and has no rationale
in `scheduler.md` or `config-flags.md`. It reads as an unexamined default rather
than a considered tradeoff — but that is an inference from absence, not evidence,
and is worth a second opinion from whoever remembers.

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

None of the following has been run. This is the handoff.

**Configurations.** Everything above is `-smp 1`, one host, one disk image.

1. `SMP=4` shared-kernel (`cargo build --release`, `SMP=4 cargo run --release`).
   Does the round-robin latency divide by core count, and does 10× the IRQ rate
   cost more with real cores contending?
2. **devbox-smoltcp** (`scripts/build_devbox_smoltcp.sh` +
   `overlays/devbox/run-smoltcp.sh`, `SMP=4`, `MEMORY=4096`, `devbox.img`) — the
   configuration nca is actually used on. Note `run-smoltcp.sh` does its own
   `cargo run --features`, so confirm the feature set it ends up with.
3. **`extreme-size`** (`scripts/build_extreme_size.sh`). This is the one with a
   real reason to object: a 4 MB / single-core box pays for every interrupt.
   Measure idle CPU with both ticks; if the tick costs meaningfully there, the
   constant may need to be profile-dependent rather than global.
4. **A long build** — `scripts/run_selfhost_kernelbuild.py`, or `acceptance/11`.
   The short `md5sum` test is not enough to rule out interrupt overhead over
   tens of minutes. Compare wall time, both ticks, several runs.

**Sweep.** 10 ms and 1 ms are two points. Try 500 µs / 1 ms / 2 ms / 5 ms and
find the knee — if 2 ms is as good as 1 ms for latency at half the interrupts,
that is the better default.

**The principled fix.** Prototype wake preemption (option 1) and compare it
against the 1 ms tick on the same four workloads. If it matches the latency at
the 10 ms tick rate, it wins outright.

### Exact reproduction

```bash
userspace/ncaprobe/build-musl.sh --serve       # host, serves on :8899
```
```bash
# guest
curl -s -o /tmp/nb http://10.0.2.2:8899/ncaprobe && chmod +x /tmp/nb
/tmp/nb sleepbench                              # the floor
/tmp/nb termbench                               # terminal tail, idle
/tmp/nb termbench --net                         # ... with a concurrent download
/tmp/nb pipebench                               # pipe round-trip cost
```
```bash
# the Linux control — SAME binary, this is what makes the numbers arguable
cd userspace/ncaprobe/target/aarch64-unknown-linux-musl/release
docker run --rm --platform linux/arm64 -v "$PWD":/p:ro alpine /p/ncaprobe sleepbench
```

Thread-count scaling (the measurement that identifies the mechanism):

```bash
# guest, 10 ms tick
for h in 0 4; do
  i=0; while [ $i -lt $h ]; do (while true; do :; done) & i=$((i+1)); done
  sleep 1; /tmp/nb sleepbench | grep '1000 ->'
  kill %1 %2 %3 %4 2>/dev/null
done
```

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
