# An idle SMP guest burned 1.5 host cores: polling sshd, a 1 kHz tick on every core

Date: 2026-11-11. Branch `akuma-multiarch-attempt-0`, after the hooked scheduler took EL1
preemption (`docs/archive/HOOKED_EL1_PREEMPTION.md`).

## Symptom

An idle devbox at SMP=4 kept its QEMU at about 150 % host CPU on both schedulers (Lima `fc`,
which is KVM inside the macOS hypervisor, so every guest exit is expensive). At SMP=1 it was
about 40 %. Idle cost grew with core count.

## Attribution

The 30 s heartbeat already counts per-core exception entries (`[EXC]`), IRQs by INTID
(`[IRQS]`), vector classes (`[EXCC]`) and per-process syscalls (`[PSTATS]`). Deltas between two
heartbeats of an idle guest, hooked, SMP=4:

| | per second |
|---|---|
| EL0 syscalls, all from `/bin/sshd` | 2272 |
| virtual-timer IRQs (INTID 27) | 3034 (~760 per core) |
| scheduler SGIs (INTID 0) | 3034 |

Three causes, each fixed in turn:

1. **sshd polled.** Its fork-sessions accept loop did `wait4(WNOHANG)`, a non-blocking
   `accept` and `nanosleep(1 ms)` per lap: three syscalls a millisecond, forever.
2. **Every core ticked at 1 kHz, idle or not, and every tick sent a self-SGI.** The host WFI
   probe picks a 1 ms tick; the timer handler re-armed it on every core unconditionally and
   then rang `SGI_SCHEDULER` at itself. That is a second interrupt, and under KVM a trapped GIC
   write, per tick.
3. **Two kernel threads halted with `wfi` instead of parking:** the boot thread's housekeeping
   loop (via `idle_halt`) and the netpoll loop. A core halted inside a non-idle thread keeps
   its tick, so those two cores stayed at 1 kHz even after the idle threads stopped ticking.

## Fix

1. **sshd blocks in `poll`** on the listener with a 1 s timeout, so a finished session is
   reaped within a second. libakuma gained `poll` over `ppoll` (asm-generic 73, x86_64 271).
2. **Tickless idle, hooked path.** Before an idle thread halts it arms its core's timer for
   the earliest thing due: a parked thread's timeout (`x86_earliest_wake_time`), the alarm
   queue's next deadline (`alarms::next_deadline`, new), capped at 10 ms because itimers have
   no deadline to ask for. It re-arms the periodic tick when the halt ends, and `switch_to`
   re-arms it when an idle thread is switched out, since an SGI can do that before the idle
   thread reaches its own re-arm. New work reaches a halted core by SGI, as before
   (`ThreadWaker::wake`, the netpoll doorbell).
3. **No self-SGI per tick, hooked path.** `irq_hooked` treats the timer interrupt itself as the
   preemption point (`akuma_gic::TIMER_PPI`, now public).
4. **The two kernel loops park, hooked path.** The boot thread sleeps 10 ms between
   housekeeping laps. The netpoll loop parks for up to 10 ms
   (`schedule_blocking`); the doorbell, which the NIC interrupt and loopback pushes ring, now
   also wakes it (`threading::{register,wake}_netpoll_parker`, new). A wake that lands before
   the park is kept by `schedule_blocking`'s sticky woken flag.

The SGI build is unchanged except for sshd, which both run: the timer, idle and loop changes
are under `hooked-sched`.

## Results

Idle, hooked, SMP=4, Lima `fc`, 100 s:

| step | host CPU | IRQs/s | EL0 syscalls/s |
|---|---|---|---|
| before | 155–169 % | 6068 | 2272 |
| sshd polls | 169 % | 5540 | 3 |
| + tickless idle, no self-SGI | 62 % | 1496 | 3 |
| + kernel loops park | 25.5 % | 317 | 3 |

What is left is about 80 timer interrupts a second per core: the 10 ms caps and park periods.

## Verification

Hooked, Lima `fc`, new sshd:

- Boot suite: 314/0, 322/0 and 320/0 at SMP=1/2/4.
- futex suite passes at SMP=2 and 4; epoll suite at SMP=4: 17 PASS, 0 FAIL, the 1 known
  DIVERGE; sigspin 5/5.
- Idle CPU: 5.4 % at SMP=1 (was 42.5 %) and 22.9 % at SMP=4.
- Pipe wake p50: 74 µs at SMP=1 (was 2.9 ms).
- Extreme-size idle: 0.4 %.

Tables: the multiarch log, section "Idle cost at SMP=4".

## Background

- `docs/archive/HOOKED_IDLE_CPU_BURN.md`: the SMP=1 half of the idle cost.
- `docs/archive/HOOKED_EL1_PREEMPTION.md`: the preemption this relies on.
- `amd64/src/lapic.rs` `arm_deadline`: amd64's idle deadline, the model for this one.
