# Timers

Source: `src/timer.rs` (hardware tick + wall clock), `src/kernel_timer.rs`
(async alarm queue). For IRQ delivery, see [`gic.md`](gic.md); for what the
scheduler does on each tick, see [`../scheduler.md`](../scheduler.md).

> **Stability: B (watch).** The CNTP→CNTV unification (below) has been stable
> since 2026-06-09. A scheduling freeze surfaced again 2026-07-05
> ("fixes the annoying freeze") — its root cause was in
> `akuma-exec/threading` (preemption-watchdog accounting), not in these two
> files, but it's a reminder that this subsystem sits directly on the
> single-timer design's one soft spot (see "One hardware timer, two jobs"
> below). The recurring lesson: **the ARM physical timer/counter (CNTP) is
> owned by the hypervisor under QEMU HVF** — an EL1 guest must program the
> virtual timer (CNTV) instead, or accessing `CNTP_CVAL_EL0` traps as an
> undefined instruction (EC=0x0).

## Two timers, one hardware source

Both files read/write the same ARM generic timer, but for different purposes:

| File | Role | Registers used |
|---|---|---|
| `timer.rs` | Owns the **hardware**: arms the periodic preemption tick, reads wall-clock/uptime, reads the PL031 RTC | `CNTV_CVAL_EL0`, `CNTV_CTL_EL0`, `CNTVCT_EL0`, `CNTFRQ_EL0` |
| `kernel_timer.rs` | Software alarm queue for **async** code (`with_timeout`, `Timer::after`) — no hardware access of its own | reads `CNTVCT_EL0`/`CNTFRQ_EL0` only, via its own `read_counter`/`read_frequency` |

Both independently read the counter/frequency rather than sharing one
accessor — harmless (they're pure MMIO/system-register reads with no side
effects), but a reader looking for "the" clock source should know it's
duplicated.

## Why CNTV, not CNTP

Originally the design split the two ARM generic timers: CNTP (PPI 30) armed
the preemption tick, CNTV (PPI 27) served the async alarm queue. Under QEMU
HVF on Apple Silicon, the **physical** timer belongs to the hypervisor — an
EL1 guest programming `CNTP_CVAL_EL0` traps as an undefined instruction
(EC=0x0), crashing the kernel. `CNTVOFF` is nonzero under HVF, so `CNTPCT` and
`CNTVCT` read different time bases; the fix (`archive/QEMU_HVF_ISV_BUG.md`
root cause 2) was to unify both roles onto the single virtual timer:

- `timer.rs::enable_timer_interrupts` (`timer.rs:30-45`) and `timer_irq_handler`
  (`timer.rs:48-110`) program `CNTV_CVAL_EL0`/`CNTV_CTL_EL0` and fire on PPI 27.
- `kernel_timer.rs` reads `CNTVCT_EL0`/`CNTFRQ_EL0` for `now_us()`
  (`kernel_timer.rs:66-101`) — the same time base the preemption tick uses, so
  deadlines computed by one are directly comparable to time read by the other.
- PPI 30 (physical timer) is registered nowhere; `main.rs` documents this
  explicitly next to the PPI 27 registration (`main.rs:877-882`).

## One hardware timer, two jobs

There is exactly one virtual-timer compare register (`CNTV_CVAL_EL0`), and
`timer::timer_irq_handler` (`timer.rs:48-110`) is the sole owner. Each tick,
in order:

1. Re-arm `CNTV_CVAL_EL0` for `now + TIMER_INTERVAL_US` (default 10 ms,
   `config.rs:573`) and defensively re-enable `CNTV_CTL_EL0=1` — guards
   against the control register ever getting corrupted into a permanently
   masked state (`timer.rs:56-63`).
2. `kernel_timer::on_timer_interrupt()` — service the async alarm queue
   (below).
3. If `ENABLE_PREEMPTION_WATCHDOG` (`config-flags.md`), check for a thread
   that's held preemption disabled too long; rate-limited warning print.
4. `gic::trigger_sgi(SGI_SCHEDULER)` — hand off to the scheduler (see
   [`gic.md`](gic.md) "IRQ dispatch to the scheduler").

Because there's only one compare register, `kernel_timer::schedule_wake`
**cannot** also arm the hardware for its own deadlines: doing so would push
the next preemption tick out to whatever far-future alarm was just scheduled
(e.g. a 5 s `Timer::after`) and freeze preemption entirely. This is why
`kernel_timer::update_hardware_timer` is an intentional no-op
(`kernel_timer.rs:177-185`) — alarms are instead polled once per tick in
`on_timer_interrupt`, giving async timers the scheduler quantum (~10 ms) as
their effective resolution. That's coarse but adequate for the SSH read
timeouts and periodic monitors that use `with_timeout`/`Timer::after`; nothing
in the kernel currently needs sub-tick async timer precision.

## Alarm queue

`kernel_timer::AlarmQueue` is a fixed 8-slot array (`QUEUE_SIZE`,
`kernel_timer.rs:107-136`) guarded by a `critical_section::Mutex` (IRQs
disabled, nesting-counted — `kernel_timer.rs:316-354`). `schedule_wake`
reuses a slot for the same waker (`Waker::will_wake`) or evicts the
earliest-deadline slot if all 8 are full; there is no queueing beyond 8
outstanding wakers. `on_timer_interrupt` collects expired wakers inside the
critical section but calls `.wake()` **outside** it, to avoid deadlocking a
waker that itself needs the critical section.

## Wall clock

`timer.rs` also owns UTC tracking, independent of the preemption/alarm
machinery above: `init()` sets up a PL031 RTC at the fixed QEMU `virt` address
`0x9010000` (`timer.rs:16-23`); `init_utc_from_rtc` reads it once to compute
`UTC_OFFSET_US` (uptime-relative), after which `utc_time_us()`/`utc_seconds()`
derive wall-clock time from `uptime_us() + offset` without touching the RTC
again. Used by TLS certificate verification and `DateTime`/ISO-8601
formatting for userspace-visible timestamps.

## Background

- `archive/QEMU_HVF_ISV_BUG.md` "Root cause 2" — the CNTP→CNTV unification.
- `archive/RUST_TOOLCHAIN_ISSUES.md` §3 — `sgi_scheduler_handler_with_sp`'s
  `try_lock` on the SGI path (why a blocking lock in IRQ context would freeze
  the box); relevant context for the 2026-07-05 freeze fix.
