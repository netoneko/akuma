# Unifying kernel timing under the scheduler (2026-08-18)

**Status: proposal — no code moved.** Grows out of
[`AKUMA_TIME_EXTRACTION.md`](AKUMA_TIME_EXTRACTION.md); that doc extracted the
*hardware* half of the timer into `crates/akuma-timer`. This one decides which
*mechanism* owns kernel timing going forward, and what per-CPU structure (if
any) the tickless endgame needs.

## The two mechanisms, as they stand

After the extraction there are exactly two ways anything in the kernel waits
for a time T:

1. **Thread deadlines** (`akuma-exec`, `threading/mod.rs`):
   `sleep_us` parks a thread by storing an absolute deadline in the global
   `WAKE_TIMES[MAX_THREADS]` array (`:524`). The periodic tick's scheduler SGI
   runs the deadline wake-pass in `schedule_indices` (`:2473`): scan, promote
   expired WAITING threads to READY — `WAKE_DEADLINE_PREEMPT` makes the
   earliest-deadline sleeper run *next*, which is where the measured latency
   win of the 1 ms experiment actually lives (and it is tick-independent).
   Consumers: nanosleep/clock_nanosleep, poll/epoll timeouts, futex timed
   waits, blocking reads, `blocking_relax` loops in akuma-net. I.e. everything
   syscall-visible.

2. **Async wakers** (`src/kernel_timer.rs`): futures (`Timer::after`) park
   their `Waker` in an 8-slot `ALARM_QUEUE`; the tick ISR calls
   `on_timer_interrupt()`, which fires expired wakers and rides
   `check_itimers` (SIGALRM). Consumers today: two `Timer::after` sites in
   `main.rs` (netpoll warmup, memory monitor) plus whatever async futures
   register timeouts internally. Its `update_hardware_timer` is an
   intentional no-op — the scheduler owns CVAL — so its resolution is the
   tick, same as the threads path.

They never talk to each other. Both are serviced by the same ISR, both get
tick resolution, and the ISR re-arm (`src/timer.rs`) already re-reads the
tick from the `akuma-timer` registry so governor demotions land next tick.

## Decision: the scheduler's deadline machinery is the core; kernel_timer becomes a client

Rationale:

- All timing ultimately *is* a scheduling decision — "make something runnable
  at T". The wake-pass already owns the run-next decision (`WAKE_DEADLINE_PREEMPT`);
  the alarm queue cannot influence scheduling directly, it can only wake a
  task and hope the executor runs.
- The syscall-visible set already funnels through thread deadlines. The queue
  serves two call sites.
- The scan cost is measured free: `body_us=0` over 40 000 ticks (256 atomic
  loads/tick). Nothing forces a cleverer structure.
- Linux is the existence proof for the shape: one hrtimer core feeding
  scheduler wakeups; sleepers/poll/futex all sit on it.

Concrete shape: the wake-pass grows a second deadline source — the waker
entries (keep the 8-slot queue or a dedicated array), checked in the same
pass; expired ones `waker.wake()` and become eligible for the same preempt
hint. `kernel_timer` keeps its API (`Timer::after`, `schedule_wake`) but stops
owning an expiry mechanism: it registers deadlines, the scheduler fires them.
`check_itimers`/SIGALRM is already a rider on the ISR and moves with it.

## Per-CPU queues: no (for now). Per-CPU *arming*: yes, tickless forces it

| | today | tickless (the endgame) |
|---|---|---|
| deadline storage | global (`WAKE_TIMES[256]` + queue), scan free | **keep global** — sleeping threads aren't core-bound; per-CPU queues need migration rules on every switch, the first per-CPU scheduler state in an otherwise shared design, bought against no measured pain |
| expiry check | every core's tick runs the pass | the one core holding the tick/BKL runs it |
| timer arming | every core fires PPI 27 periodically | **per-CPU by nature**: a core with runnable threads ticks; an idle core arms a one-shot at the earliest deadline it must observe (`akuma_timer::arm_oneshot_ticks` — the seam the extraction left) and takes **zero** interrupts |

Per-CPU *queues* only pay off with thousands of timers or a profile showing
cacheline bouncing on the deadline arrays; current load is 256 thread slots +
8 wakers. Cross-core wakeups already have machinery (`wake_remote_idle` SGI
nudge), so a deadline expiring on core A can nudge a sleeper on core B without
either queue being per-CPU.

## Sequence

1. **Unify**: wake-pass consumes waker deadlines (above). Mechanical, no
   behaviour change, gate with `scripts/measure_idle_cpu.py` + boot suite.
2. **Tickless idle**: earliest deadline over {threads, wakers, itimers} arms
   the one-shot; periodic tick only while ≥1 thread is runnable on that core.
   Idle cost goes from "1 interrupt per tick" to "0 interrupts". The boot WFI
   probe's floor logic still governs the *periodic* fallback, so the HVF
   cliff stays handled.
3. **Then stop**: revisit sharding only if a profile ever shows the scan.
   Per-CPU arming from step 2 is the only locality that matters.

## Background

- [`AKUMA_TIME_EXTRACTION.md`](AKUMA_TIME_EXTRACTION.md) — the hardware/policy
  extraction, the HVF WFI floor, the boot probe + governor.
- [`SCHEDULING_INVESTIGATION.md`](SCHEDULING_INVESTIGATION.md) — Matrix A:
  which wins are tick-scaled vs wake-path (WAKE_DEADLINE_PREEMPT survives any
  tick choice).
- [`CPU_LOAD_REGRESSION_INVESTIGATION.md`](CPU_LOAD_REGRESSION_INVESTIGATION.md)
  — why tick rate and idle cost were ever coupled.
- `TRIM_FAT_EMBARRESSING_DUPLICATIONS.md` § "Deferred audit" — the original
  extraction blueprint (gic/gic_v3/ramfb/irq still unextracted).
