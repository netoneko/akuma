# Timers

Source: `crates/akuma-timer` (hardware access + tick policy — extracted
2026-08-18), `src/timer.rs` (scheduler-tick ISR + boot probe wiring + UTC
presentation), `akuma_exec::alarms`
(`crates/akuma-exec/src/alarms.rs` — async alarm queue, formerly
`src/kernel_timer.rs`). For IRQ delivery, see [`gic.md`](gic.md); for what
the scheduler does on each tick, see [`../scheduler.md`](../scheduler.md).

> **Stability: A (stable, trust it).** Restructured 2026-08-18: hardware +
> self-tuning tick policy extracted to `crates/akuma-timer`, the async alarm
> queue moved to `akuma_exec::alarms`, the scheduler-tick ISR stayed in
> `src/timer.rs` (`archive/AKUMA_TIME_EXTRACTION.md`). The CNTP→CNTV
> unification has been stable since 2026-06-09. The recurring lesson: **the
> ARM physical timer/counter (CNTP) is owned by the hypervisor under QEMU
> HVF** — an EL1 guest must program the virtual timer (CNTV) instead, or
> accessing `CNTP_CVAL_EL0` traps as an undefined instruction (EC=0x0).

## Three pieces, one hardware source

| Piece | Role | Registers used |
|---|---|---|
| `crates/akuma-timer` | **Hardware owner**: CNTV access, arm/disarm, PL031 RTC seconds, UTC offset atomic, tick registry, boot WFI probe + runtime governor policy (host-testable over a mocked `Hw` trait) | `CNTV_CVAL_EL0`, `CNTV_CTL_EL0`, `CNTVCT_EL0`, `CNTFRQ_EL0` |
| `src/timer.rs` | Scheduler-tick ISR (re-arm, alarm servicing, watchdog, scheduler SGI), boot-probe wiring, `DateTime`/ISO-8601 presentation | via `akuma-timer` |
| `akuma_exec::alarms` | Async alarm queue (`Timer::after`, `schedule_wake`) — **no hardware access**: time via the runtime `uptime_us` hook, expiry polled by the tick ISR | none |

One accessor now: the duplicated `CNTVCT`/`CNTFRQ` reads that used to live in
`kernel_timer.rs` are gone — `alarms::now_us()` delegates to the runtime hook,
same source as the scheduler's wake-pass.

## The self-tuning tick

`TIMER_INTERVAL_US` (`config.rs`) is only the *fallback*; at boot the BSP
runs `akuma_timer::policy::pick_tick` (via `timer::probe_host_tick`, with a
NOP IRQ-27 handler swapped in): for candidates {1, 2, 3, 5} ms, 8 one-shot
WFI samples each, pass when ≥6/8 halt for ≥ interval/2; smallest passing
candidate wins, none → 10 ms. This exists because under QEMU HVF on
darwin/arm64 the host declines to sleep vCPU threads for deadlines below
~2.5 ms — a sub-floor tick makes WFI a no-op and burns one saturated host
core per guest core (`archive/CPU_LOAD_REGRESSION_INVESTIGATION.md`,
`archive/AKUMA_TIME_EXTRACTION.md`). At runtime the governor
(`policy::governor_observe`, fed by the `timer::NETPOLL_ITERS` sensor)
demotes the tick to 5 ms if the host stops honouring WFI after boot —
latched, never re-promotes. Since 2026-08-19 the latch itself is
`akuma_kacho::Latch`, the shared primitive behind every self-tuning policy in
the tree; the thresholds and the verdict are unchanged.
`kernel_profile_extreme` skips the probe and keeps 10 ms.

> **Caveat: the sensor is desensitised at SMP>1 (OPEN, 2026-08-19).**
> `NETPOLL_ITERS` is incremented only by the BSP netpoll loop
> (`src/main.rs:1445`), but `GOVERNOR_TICKS` is incremented by **every** core —
> the timer handler is one shared dispatch table (`src/timer.rs:134`). Over a
> `GOVERNOR_WINDOW_TICKS = 2000` window at SMP=4 only ~500 real ticks of BSP
> wall time have elapsed, while the threshold is still
> `2000 x SPIN_ITER_PER_TICK`, so the effective trip point is 40 iterations per
> real tick instead of 10 — **4x desensitised, scaling with core count.**
> Nothing has been attributed to it, and fixing it changes behaviour on a
> demotion path, so it is recorded rather than changed. Do not read this
> governor's verdict as evidence at SMP>1 until it is fixed
> (`archive/AKUMA_SCHEDULING_EXTRACTION.md` §6).

## Why CNTV, not CNTP

Originally the design split the two ARM generic timers: CNTP (PPI 30) armed
the preemption tick, CNTV (PPI 27) served the async alarm queue. Under QEMU
HVF on Apple Silicon, the **physical** timer belongs to the hypervisor — an
EL1 guest programming `CNTP_CVAL_EL0` traps as an undefined instruction
(EC=0x0), crashing the kernel. `CNTVOFF` is nonzero under HVF, so `CNTPCT`
and `CNTVCT` read different time bases; the fix
(`archive/QEMU_HVF_ISV_BUG.md` root cause 2) was to unify both roles onto
the single virtual timer. PPI 30 is registered nowhere; `main.rs` documents
this explicitly next to the PPI 27 registration.

## One hardware timer, two jobs

There is exactly one virtual-timer compare register (`CNTV_CVAL_EL0`), and
the scheduler tick (`timer::timer_irq_handler` in `src/timer.rs`) is the
sole owner. Each tick, in order:

1. Re-arm `CNTV_CVAL_EL0` for entry + current tick (probe/governor choice,
   re-read every tick so a demotion lands next interrupt) and defensively
   re-enable `CNTV_CTL_EL0=1` — guards against the control register ever
   getting corrupted into a permanently masked state.
2. `akuma_exec::alarms::on_timer_interrupt()` — service the async alarm
   queue (below).
3. If `ENABLE_PREEMPTION_WATCHDOG` (`config-flags.md`), check for a thread
   that's held preemption disabled too long; rate-limited warning print.
4. `gic::trigger_sgi(SGI_SCHEDULER)` — hand off to the scheduler (see
   [`gic.md`](gic.md) "IRQ dispatch to the scheduler").

Because there's only one compare register, `alarms::schedule_wake` **cannot**
also arm the hardware for its own deadlines: doing so would push the next
preemption tick out to whatever far-future alarm was just scheduled (e.g. a
5 s `Timer::after`) and freeze preemption entirely. This is why
`alarms::update_hardware_timer` is an intentional no-op — alarms are instead
polled once per tick, giving async timers the scheduler quantum (~3 ms) as
their effective resolution. Coarse but adequate for the SSH read timeouts
and periodic monitors that use `Timer::after`. Giving the queue the one-shot
deadline (tickless idle) is the recorded next step
(`archive/TRIMMING_FAT_SCHEDULER.md`).

## Alarm queue

`akuma_exec::alarms::ALARM_QUEUE` is a fixed 8-slot array under its own
`Spinlock` (a real cross-core lock since BKL Phase 7a — the old
`critical_section` impl gave no cross-core exclusion at all).
`schedule_wake` reuses a slot for the same waker (`Waker::will_wake`) or
evicts the earliest-deadline slot if all 8 are full; there is no queueing
beyond 8 outstanding wakers. `on_timer_interrupt` collects expired wakers
inside the lock but calls `.wake()` **outside** it, to avoid deadlocking a
waker that itself needs the lock. It also calls the runtime's
`check_itimers` hook — ITIMER_REAL/`alarm()` SIGALRM delivery rides the
tick.

## Wall clock

The PL031 RTC read lives in `akuma-timer` (`rtc::unix_seconds`, fixed QEMU
`virt` address `0x9010000`); `init_utc_from_rtc` (in `src/timer.rs`) reads
it once to compute the UTC offset (uptime-relative), after which
`utc_time_us()`/`utc_seconds()` derive wall-clock time from
`uptime_us() + offset` without touching the RTC again. Used by TLS
certificate verification and `DateTime`/ISO-8601 formatting for
userspace-visible timestamps.

`akuma-timer` only ever *reads* the offset that `set_utc_time_us` writes;
the writers — `clock_settime`/`adjtimex`/`clock_adjtime`, and the boot-time
SNTP fallback for platforms with no PL031 (Firecracker) — live in the
sibling `akuma-time` crate. See
[`../syscalls/time.md`](../syscalls/time.md) § "boot-time clock source and
the Firecracker fallback". Don't confuse the two crates by name: `akuma-timer`
is the hardware/tick-policy crate this file documents, `akuma-time` is the
syscall/NTP crate.

## Background

- `archive/AKUMA_TIME_EXTRACTION.md` — the 2026-08-18 extraction, the HVF
  WFI floor measurements, probe/governor design and validation.
- `archive/CPU_LOAD_REGRESSION_INVESTIGATION.md` — the idle-CPU regression
  the self-tuning tick exists to prevent.
- `archive/TRIMMING_FAT_SCHEDULER.md` — the unification plan (scheduler
  deadline machinery as the core; `alarms` as a client).
- `archive/QEMU_HVF_ISV_BUG.md` "Root cause 2" — the CNTP→CNTV unification.
- `archive/RUST_TOOLCHAIN_ISSUES.md` §3 — `sgi_scheduler_handler_with_sp`'s
  `try_lock` on the SGI path (why a blocking lock in IRQ context would
  freeze the box).
