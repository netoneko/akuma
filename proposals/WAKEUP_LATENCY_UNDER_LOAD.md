# Wakeup latency under load (proposal, 2026-11-11)

**Status: proposal, not started.** Deferred to after the big merge. The measurements below are
the baseline any change here has to beat.

## The problem

A thread whose sleep deadline passes while other threads keep the cores busy is not run until a
**tick** notices it. Idle, both kernels sleep accurately. Under load, a 1 ms `nanosleep` takes
roughly one tick per runnable competitor on a full machine, and up to one tick even when a core
is free.

## Measurements (2026-11-11)

`ncaprobe sleepbench` (median of 40 per row), with CPU hogs `(while true; do :; done) &`. "1 ms"
is the median actual sleep for a 1 ms request.

| kernel | machine | idle | 3 hogs | 7 hogs |
|---|---|---|---|---|
| AArch64 `da505df7` | QEMU HVF, SMP=1, 1 ms tick | 1.27 ms | **4.29 ms** | — |
| AArch64 `da505df7` | QEMU HVF, SMP=4, 1 ms tick | 1.31 ms | 1.00 ms | 2.02 ms |
| amd64 `da505df7` | trashcan (i5-4460, 4 cores), 10 ms tick | 1.01 ms | **10.0 ms** | **11.5 ms** |
| amd64, the kernel trashcan ran that morning (source unknown) | same | 1.02 ms | 1.0 ms once, **6.0 ms** on a repeat | **23.9 ms** |

- **The single 1.0 ms reading was luck.** One run of the morning kernel read 1.0 ms with 3 hogs;
  the repeat read 6.0 ms. The first comparison against it reported a regression that did not
  exist. Every cell here needs several runs before it means anything (see "Method").
- **For reference:** AArch64 with the SGI scheduler's `WAKE_DEADLINE_PREEMPT` and a 1 ms tick
  measured 1.06 ms at SMP=1 regardless of load; before any fix it was ~35 ms
  ([`SCHEDULING_INVESTIGATION.md`](../docs/archive/SCHEDULING_INVESTIGATION.md)).
- **Signals:** `pthread_kill_eintr` passed 10/10 on HVF at SMP=1 and SMP=4, idle and with
  3 hogs. On trashcan it was flaky on both kernels (see "Related").
- **Untested:** the rump sysproxy round trip, which needs the rump devbox. It used to rely on
  the network-thread boost.

## Why

Both kernels pick with `akuma_threading::pick_next`: plain round-robin over `READY` slots, with
the `ON_CPU` claim. An expired sleeper becomes `READY` only when some core runs `wake_pass`, and
it runs only when that core next schedules. Three things are missing.

1. **Busy cores ignore deadlines.** An idle core arms its timer for the earliest parked deadline
   before it halts (amd64 `lapic::arm_deadline`; AArch64's hooked idle threads arm the same
   value). A core running a hog arms only its periodic tick, so it notices the expired sleeper
   up to a tick late: 10 ms on amd64, the probed 1–3 ms on AArch64.
2. **A halted core may be armed for an older deadline.** It computed its one-shot before the
   sleeper parked, and nothing tells it to re-arm. A sleeper that parks with an earlier deadline
   than every halted core's timer waits for whichever fires first.
3. **A woken thread goes to the back of the queue.** Once `READY`, it waits behind every other
   runnable thread, one slice each. This is what the SGI scheduler's run-next hint
   (`PREEMPT_WAKE_TID`, armed by `WAKE_DEADLINE_PREEMPT` and `SIGNAL_WAKE_PREEMPT`) fixed, and it
   went with that scheduler ([`SGI_SCHEDULER_RESIDUE.md`](../docs/archive/SGI_SCHEDULER_RESIDUE.md)).
   It matters only when the machine is oversubscribed. With a free core (3 hogs on 4 cores),
   items 1 and 2 alone should give ~1 ms.

## Steps, each measured on its own

| step | what | expected |
|---|---|---|
| 1 | **Deadline timer on busy cores.** Each time a core resumes a thread, program its timer for `min(next tick, earliest_wake_time())`, as the idle path already does. amd64: `lapic` one-shot/periodic juggling (`arm_deadline`, `restore_periodic`). AArch64: `CNTV_CVAL` | 3 hogs on 4 cores: ~1 ms on trashcan |
| 2 | **Kick a halted core on an earlier timed park.** When `publish_waiting_and_take_pending_wake` stores a deadline earlier than what the halted cores are armed for, kick one (`smp::kick_halted_core` on amd64, a wake SGI on AArch64) so it re-arms | removes the residual tail of step 1 |
| 3 | **A run-next hint in `pick_next`, per core.** A thread readied by `wake_pass` or a signal is preferred by the next pick on the core that readied it (or the core it last ran on). It must not bypass the `ON_CPU` claim; several cores scanning at once is why a single global slot is wrong here | 7 hogs on 4 cores and SMP=1 with 3 hogs: ~1 ms |

Also check before step 1: amd64's 10 ms tick. AArch64 probes its tick at boot (1 ms on HVF here).
amd64 runs 10 ms with idle one-shots. Steps 1–2 make the tick matter less for latency, but it
still sets the slice length for step 3's fairness.

## Method

- At least 5 runs per cell, reporting median and spread. Where a runtime toggle allows it, run
  A/B/A on the same boot, so both arms share a machine state.
- Cells: idle, 3 hogs, 7 hogs; SMP=1 and SMP=4 on HVF; trashcan (4 cores, metal).
- Also: `ncaprobe pollbench` (the epoll timeout path), `pthread_kill_eintr` ×20, and bulk
  throughput (the 128 MB download from `SCHEDULING_INVESTIGATION.md`). The SGI fix cost 2× bulk
  throughput at SMP=4 at one point, so throughput is a gate too.
- The probes: `userspace/ncaprobe` (`build-musl.sh` for AArch64; for x86_64,
  `cargo build --release --target x86_64-unknown-linux-musl` with the musl-gcc linker and
  `-C link-arg=-static`) and `userspace/forktest/c_stress/pthread_kill_eintr.c`
  (`x86_64-linux-musl-gcc -static -O2 -pthread`).

## Related, found while measuring (separate problems, also open)

- **amd64 thread-directed signals look unreliable under load.** During a `-j4` kernel build on
  trashcan (`da505df7`):
  - a `rustc` died of signal 10. That is SIGUSR1 on x86_64, which the jobserver helper thread
    uses to wake itself;
  - a build-script `rustc` sat for over 1,000 s waiting for a wakeup;
  - two `rustc` processes survived `kill -9`.

  `pthread_kill_eintr` also fails intermittently on both kernels at phase 2 (an `SA_RESTART`
  read returns `EINTR`). A misdelivered or lost thread-directed signal fits all of these.
  Unconfirmed: no trace yet.
- **main's kernel (`44526152`, built on the ryzen) hangs on trashcan's multiboot2 path** right
  after `pmm: … free frames`, before the framebuffer double buffer, `hda::init`, `kvm::init`
  and the watchdog. The user has seen a similar hang in the past few days. The branch kernel
  boots.
- **Ryzen → trashcan HTTP transfer ran at ~230 KB/s** (busybox `wget` on Akuma, Python
  `http.server` on Pop). Either the ryzen's link or Akuma's TCP receive path.
- **trashcan has no `taskset` applet** (`/bin/taskset` is a busybox link to an applet that isn't
  built), so placement experiments (pinning the hogs away from the sleeper) were not possible.

## Background

- [`docs/archive/SGI_SCHEDULER_RESIDUE.md`](../docs/archive/SGI_SCHEDULER_RESIDUE.md): what
  the SGI picker took with it.
- [`docs/archive/SCHEDULING_INVESTIGATION.md`](../docs/archive/SCHEDULING_INVESTIGATION.md):
  the original ~35 ms floor and the 2026-08-18 fix.
- [`docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`](../docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md)
- [`docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md`](../docs/archive/RUMP_SYSPROXY_LATENCY_FIX.md)
- [`docs/archive/HOOKED_IDLE_TICK.md`](../docs/archive/HOOKED_IDLE_TICK.md): AArch64's
  tickless idle.
- `amd64/src/lapic.rs` (`arm_deadline`, `arm_idle_shot`), `amd64/src/sched.rs` (`idle_loop`),
  `crates/akuma-threading/src/lib.rs` (`pick_next`, `wake_pass`, `schedule_blocking`).
