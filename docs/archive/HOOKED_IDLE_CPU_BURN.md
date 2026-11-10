# An idle guest burned a whole host CPU under the hooked scheduler

Date: 2026-11-10. Branch `akuma-multiarch-attempt-0`, found while checking the hooked scheduler on
extreme-size before making it the AArch64 default (`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`).

## Symptom

`/proc/stat` on an idle hooked guest counted all of its time as `user`: 5 s of idleness added
+508 `user` and +3 `idle` ticks. On SGI the same period is almost all `idle`. That was not
just an accounting error. The QEMU process really was busy:

| idle guest, host CPU of its QEMU | SGI | hooked |
|---|---|---|
| extreme-size, 8 MB, HVF | 0.2 % | 99–100 % |
| devbox, SMP=1, Lima `fc` (lifetime average, boot included) | 34 % | 97 % |

## Cause

Two kernel threads on the core have nothing to do between packets: the async-main loop
(`run_async_main*`, ending each lap in `threading::idle_halt`) and the netpoll loop. On the SGI
path both reach a `wfi`, and the tick or an RX interrupt ends it.

On the hooked path neither halted:

- `idle_halt` under `yield-takes-bkl` yields instead of halting whenever anything else is
  runnable. That rule was added so a socket waiter would not hold the only core at SMP=1, but
  the netpoll thread is always runnable.
- The netpoll loop's hooked branch halts only when *nothing* is runnable, and the async-main
  thread always is, since it yielded rather than parking. Without `smp-shared` (extreme-size)
  there was no hooked branch at all, just a bare `yield_now()` loop.

So the two handed the core back and forth for ever. The idle thread never ran, and every
microsecond was billed to one of them.

The hooked idle loop also called a raw `wfi`, and on this path the switch bills a slot for its
whole residency. So even a genuinely halted core would have been reported busy. amd64 had already
hit that one and fixed it with `threading::credit_halted_time`.

## Fix

1. **`idle_halt` parks instead of yielding** on the hooked path (`yield-takes-bkl`), for every
   caller but an idle thread. The park is `sleep_us(IDLE_HALT_PARK_US)`, 1 ms, a floor and not a
   period. A parked caller is not runnable, so anything that is runs, the idle thread halts when
   nothing is, and the first wake pass after the deadline brings the caller back, at the latest
   on the next tick. That is the "until the next interrupt" an SGI `wfi` gives, and it still
   covers the SMP=1 case the yield was added for: the socket waiter leaves the core.
2. **The netpoll loop's hooked branch applies without `smp-shared` too:** halt when nothing else
   is runnable, then yield. Its BKL calls are no-ops without `smp-shared`.
3. **The hooked idle loop credits its halt** (`credit_halted_time` around the `wfi`), as amd64's
   idle loop does.

The SGI build does not change. The `idle_halt` edit is under `yield-takes-bkl`, which only
`hooked-sched` enables. The netpoll `cfg`s give a non-hooked build the same branches as before.

## Verification

Extreme-size, 8 MB, HVF: idle host CPU 0.1 % (was 100 %); `/proc/stat` over 5 s idle is
+1 `user`, +533 `idle`.

Devbox and the gates: see the multiarch log, section "Hooked: the idle CPU burn".

## Background

- `docs/archive/SECOND_LISTENER_SMP1_FREEZE.md`: why a non-idle halter must not hold the core.
- `docs/archive/KNOWN_ISSUES.md` issue #11: the same ping-pong on the SGI path, fixed there by
  `idle_halt`'s `wfi`.
- `amd64/src/sched.rs`: `credit_halted_time` in amd64's idle loop.
