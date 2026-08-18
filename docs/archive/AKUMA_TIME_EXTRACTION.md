# Timer extraction + self-tuning ticker (2026-08-18)

**Status: landed on `nca-terminal-fixes`; validated on devbox-smoltcp SMP=1/4.**

This closes [`CPU_LOAD_REGRESSION_INVESTIGATION.md`](CPU_LOAD_REGRESSION_INVESTIGATION.md)
(that doc's "Open questions" 1–3 are all answered here) and executes the
deferred-audit row of
[`TRIM_FAT_EMBARASSING_DUPLICATIONS.md`](TRIM_FAT_EMBARRESSING_DUPLICATIONS.md)
§ "GIC/timer/ramfb/irq → a leaf crate" for the timer half.

All measurements: devbox-smoltcp, QEMU `-accel hvf`, darwin/arm64, host
otherwise idle, via `scripts/measure_idle_cpu.py` (differenced `ps -o time=`,
window after the sshd marker).

## The mechanism, confirmed

The regression was a **host WFI floor**, not tick-handler cost:

| SMP=1 tick | idle CPU | netpoll iters/sec (TICKPROBE) |
|---|---|---|
| 1 ms | 100.0% | ~1,800,000 (WFI is a no-op; loop spins at natural speed) |
| 2 ms | 100.0% | ~1,815,000 |
| 3 ms | 1.6% | ~670 (~1 per tick: WFI sleeps) |
| 5 ms | 1.1% | — |
| 10 ms | 0.6% | — |

- **Cliff, not slope** — a hard threshold between 2 and 3 ms on this host.
  The investigation doc's "superlinearity" (100x CPU for 10x tick rate) was a
  red herring: it is a binary regime switch between "vCPU thread sleeps
  between ticks" and "vCPU thread spins forever".
- **The cost is not the tick.** Handler body <1 µs, period exact and stable.
  Decisive arms: 10 ms + WFI-off → **100%**; 10 ms + WFI → **0.9%**. WFI is
  the *only* thing sleeping the vCPU; below the floor it does nothing.
- **Under HVF the host declines to park a vCPU thread for deadlines below
  ~2.5 ms.** The guest's `wfi` traps and returns immediately; both idle loops
  (BSP netpoll `main.rs`, secondary cores `smp_shared.rs`) become busy-polls.
  One saturated host core per guest core; SMP=4 read ~330% (sublinear —
  residual host-side serialization, not characterised further once irrelevant).
- This is an undocumented HVF heuristic, not an architectural limit: it can
  move with macOS/QEMU versions, which is why the fix measures instead of
  hardcoding.

The in-flight inference correction in the investigation doc held up: the
`wfi_per_2000t` counter only saw `idle_halt` entries; the netpoll loop's raw
`wfi` (invisible to that counter) was where the spin lived.

## The fix: `akuma-timer` + self-tuning tick

Two layers, both owning their decision, both host-testable:

### Boot probe (`akuma_timer::policy::pick_tick`)

At boot, before the periodic tick is armed, the BSP measures each candidate
tick `{1, 2, 3, 5} ms`: 8 one-shot-armed WFI samples per candidate; a
candidate passes when ≥6/8 show a halt ≥ interval/2 (fraction, not min — an
unrelated IRQ can win one race even on a good host). Smallest passing
candidate wins; none passing → 10 ms fallback. `kernel_profile_extreme`
skips the probe entirely and keeps its 10 ms constant.

**Gotcha recorded for posterity:** the probe runs with a NOP IRQ handler
installed on IRQ 27. A fired one-shot keeps its level asserted
(`CVAL <= counter`, enabled) — a NOP handler that leaves it armed makes the
GIC re-forward IRQ 27 forever after EOI and wedges the machine
(boot stall observed at 21:13 builds). The NOP handler must `disarm()`
(mask); each sample re-arms. This cost one debug cycle and is why
`akuma_timer::disarm` exists as public API.

### Runtime governor (`policy::governor_observe`)

The host's behaviour can change after boot (host load, heuristic shift). The
BSP netpoll loop increments `timer::NETPOLL_ITERS`; every 2000 ticks the
TICKPROBE block feeds the count to the governor. Healthy ≈ 1 iteration/tick;
a no-op WFI shows ~1000x that (measured 1.8M/s at 1 ms). Two consecutive
spinny windows demote the tick to 5 ms — latched, never re-promoted.

**Demote-only by design.** The risk is asymmetric: stuck at 5 ms costs ~2 ms
of sleep-floor granularity (cosmetic); a bad re-promotion re-enters the burn
for ~4 s until the governor catches it, and passive re-learning is impossible
(you cannot observe "3 ms would sleep fine now" while sleeping 5 ms — only
deliberately re-entering the burn state teaches that). Future work if a
pessimistic boot ever matters: re-probe failed candidates once at end of
boot, not runtime re-promotion.

### Validation

| arm | result |
|---|---|
| default boot, SMP=4 | probe picks **3000 µs** (matches the manually-measured cliff), idle **5.6–5.8%** |
| forced 1 ms override, SMP=4 (pre-cleanup build) | governor trips in ~4 s: `WFI spin detected, tick -> 5000 us`, idle **3.8%** |
| TICKPROBE at 3 ms, SMP=4 | `period_us=917, idle_iters≈500/2000t` — per-core halts flowing, no spin |
| host tests (`cargo test -p akuma-timer`) | 7/7: never-sleeps→fallback, honours-1ms→1ms, HVF-floor(2.5ms)→3ms, racing-IRQ tolerance, freq=0→fallback, governor window logic |

## What moved where

`src/timer.rs` was two unrelated things in one file — scheduler-ISR logic
wearing a driver's filename (the deferred-audit row's words). Now:

**`crates/akuma-timer/`** (hardware + policy, no_std, host-testable, every
export `#[inline]` — the ISR path crosses the codegen-unit boundary and
ThinLTO only saves explicitly-marked callees):

- CNTV access (`read_counter`/`read_frequency`/`uptime_us`), virtual-timer
  arm/disarm — virtual not physical: CNTPCT is hypervisor-owned under HVF and
  traps (`QEMU_HVF_ISV_BUG.md`)
- tick registry (`set_tick_us`/`tick_us`) — the ISR re-reads it every
  interrupt, so a demotion lands on the next tick with no broadcast
- PL031 RTC seconds read
- UTC offset atomic (kept lock-free; the old comment about the BKL-free
  `FUTEX_WAIT_BITSET|CLOCK_REALTIME` read path moved with it)
- `policy`: `pick_tick`, `Governor` — pure over a mockable `Hw` trait
  (`ArchHw` = real asm, `target_os = "none"` only)

**`src/timer.rs` stays** (fused to the bin crate, cannot move):

- `timer_irq_handler` — re-arm, `kernel_timer::on_timer_interrupt`,
  preemption watchdog, scheduler SGI (reaches `akuma_exec`/`gic`)
- `probe_host_tick` + `probe_irq_nop` — the probe's GIC/DAIF dance is
  bin-crate wiring (register-handler swap around `pick_tick`)
- `DateTime`/`utc_iso8601` presentation (allocates; console-facing)

Call sites unchanged: ~190 `timer::uptime_us` re-export consumers, and
secondary cores now arm from `timer::current_tick_us()` (the shared choice)
instead of the raw config constant. `arm_pl031` moved from the bin's
dependencies to the crate's.

`config::TIMER_INTERVAL_US` is now the *fallback/default* the probe overrides
(non-extreme default is 3 ms — same value the probe picks on the measured
host, so behaviour is identical even if the probe were disabled). The 1 ms
constant from commit `0e4ba1b9` is gone: it was strictly worse on this host
and the probe recovers it automatically on hosts that honour short WFIs.

## Temporary, still in tree (remove before landing the branch)

- `src/timer.rs` TICKPROBE block (`PROBE_TICKS`/`PROBE_LAST_ENTRY`/
  `PROBE_BODY_SUM`/`PROBE_PERIOD_SUM` + the print). The `NETPOLL_ITERS`
  sensor and governor call are permanent.
- `akuma_exec::threading::PROBE_WFI` (`idle_halt` entry counter).
- Fixed `PROBE_WFI`'s cfg-theft of `idle_halt` (the static was inserted
  between `#[cfg(target_os = "none")]` and its fn, silently un-gating it —
  found by the pre-commit hook's host clippy pass, E0428).

## Open items

1. **One-shot/tickless idle** is the real endgame the investigation doc
   named: arm the timer at the earliest pending wake deadline, take zero
   interrupts when idle. `akuma-timer`'s `arm_oneshot_ticks` is the seam;
   the natural next step is letting `kernel_timer`'s alarm queue own the
   hardware deadline (replacing its intentionally-no-op
   `update_hardware_timer`) and only falling back to a periodic tick when
   threads are runnable.
2. Pessimistic-boot mitigation (re-probe failed candidates at end of boot)
   if CI-under-load machines ever hit it.
3. `gic`/`gic_v3`/`ramfb`/`irq` extraction — the rest of the deferred-audit
   row, still not started, now unblocked pattern-wise by this crate.

## Background

- [`CPU_LOAD_REGRESSION_INVESTIGATION.md`](CPU_LOAD_REGRESSION_INVESTIGATION.md)
  — the regression bisect and the (correct) mechanism hypotheses this doc
  closed out.
- [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) — why the tick
  went to 1 ms in the first place; which wins are tick-scaled vs
  wake-path (`WAKE_DEADLINE_PREEMPT` is tick-independent and survives any
  tick choice).
- [`TRIM_FAT_EMBARRESSING_DUPLICATIONS.md`](TRIM_FAT_EMBARRESSING_DUPLICATIONS.md)
  § "Deferred audit" — the extraction's pre-existing blueprint.
- Idle-CPU gate: `scripts/measure_idle_cpu.py`, A/B rule per
  `docs/runbooks/verify-trim-fat-change.md`.
