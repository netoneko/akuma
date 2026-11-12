# Signals never reached a thread in a compute loop (AArch64) — fixed on the hooked path

Date: 2026-11-10. Branch `akuma-multiarch-attempt-0`, step W3c of
`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`.

## Symptom

`kill -TERM` of an AArch64 process in a pure compute loop did nothing. The same was true of
any catchable signal: a `SIGUSR1` handler on a spinning thread never ran. Only `SIGKILL`
worked, because it does not go through delivery at all: `deliver_signal` hard-kills the
thread group (`akuma-exec/src/process/signal.rs`).

Probe: `userspace/forktest/c_stress/sigspin.c`. It forks a spinner, signals it after 100 ms,
and waits 3 s for the expected status. Before the fix, Lima `fc` (KVM), devbox, SMP=2, 3 rounds
per case:

| case          | SGI scheduler       | hooked scheduler    |
|---------------|---------------------|---------------------|
| term          | 0/3 (not reaped)    | 0/3 (not reaped)    |
| handler       | 0/3 (never ran)     | 0/3 (never ran)     |
| kill9         | 3/3                 | 3/3                 |
| thread-term   | 0/3                 | 0/3                 |
| thread-kill9  | 3/3                 | 3/3                 |

Neither scheduler delivered, so this was not a regression from the hooked work. Linux delivers
on every return to user mode, so every case there finishes in about one tick.

## Cause

On AArch64, pending signals were taken at exactly three places, all in `sync_el0`:

- syscall return;
- `rt_sigreturn`;
- the JIT icache-retry path.

A thread that makes no syscall leaves EL0 only on an interrupt. Neither IRQ path ever looked at
the pending set:

- SGI: `irq_with_sp`;
- hooked: `irq_hooked`.

The pend itself worked. `pend_signal_to_group` set the bit and the interrupt flag, and nothing
ever consumed them.

amd64 did not have this hole: there, a thread leaves ring 3 through its own syscall return or a
tick, and both deliver (`crates/akuma-kernel-amd64/src/signal.rs`).

## Fix (hooked scheduler only)

`irq_hooked` now takes the EL0 frame as `Option<FrameHandle>`. Before an interrupt from EL0
releases the BKL, the thread about to resume into that frame runs `deliver_on_irq_return`. If
the interrupt switched threads, that is the incoming thread.

1. A **deferred kill** from a peer's `kill_thread_group` (`take_thread_kill_request`) ends the
   thread with the same mark-and-yield the syscall boundary uses.
2. One **pending, unmasked signal** goes into the frame through `try_deliver_signal`, or takes
   its default action. It follows the syscall-return rules, with these differences:
   - The ESR is 0, so `SA_RESTART` never moves `ELR` back onto an `svc` that was never executed.
   - `x0` is the interrupted code's live register. The signal frame saves it as it stands, and
     it is replaced only by the handler's argument.
   - There is no `sigframe_active` bound. That bound protects a syscall return userspace has not
     seen yet, and an interrupt carries none.
   - Go's SIGURG-before-sigaltstack is re-pended, as on syscall return.
3. Interrupts are open during delivery, as they are around `sync_el0`. Building the frame may
   fault in the user stack. The EOI has already been written.

The work allocates nothing: two atomic loads in the common case.

The hooked front end makes this cheap. The kit's IRQ frame is the full 832-byte user frame, the
same layout as the syscall frame (`akuma-entry/src/hooked.rs` pins it). The SGI path's IRQ frame
is a different, smaller layout that `try_deliver_signal` cannot write.

**The SGI path is not fixed.** It is to be deleted once hooked becomes the AArch64 default (the
next step of the multiarch work), so the fix was not ported to its frame.

## Verification

`sigspin 5` on `hooked-w3c-devbox`, Lima `fc`. All rounds passed at every SMP level. The times
are from the signal to the reap:

| case          | SMP=1        | SMP=2         | SMP=4         |
|---------------|--------------|---------------|---------------|
| term          | 7.7–8.0 ms   | 11.3–14.3 ms  | 5.4–9.4 ms    |
| handler       | 1.0–1.1 ms   | 4.1–11.5 ms   | 1.2–1.5 ms    |
| kill9         | 5.2–25.3 ms  | 17–28 µs      | 15–191 µs     |
| thread-term   | 8.3–8.6 ms   | 6.6–14.9 ms   | 5.6–33.8 ms   |
| thread-kill9  | 35.4–35.5 ms | 28 µs–4.5 ms  | 1.3–2.4 ms    |

Also checked:

- clippy is clean with and without `hooked-sched`;
- boot suite SMP=1 314/0, SMP=2 322/0, SMP=4 319/1 (the known `test_epoll_socket_waker`);
- epoll and futex suites pass at SMP=2 and 4.

## Open

- The SGI scheduler still drops signals to spinners. It goes away with the SGI path.
- amd64 with `sigspin`: run it on the trashcan to confirm the table above matches the
  tick-delivery path there.

## Background

- `docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`: the `sigframe_active` bound.
- `docs/archive/AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`: amd64 pends every signal and delivers on
  the tick.
- `docs/archive/HOOKED_TPIDR_EL0_SWITCH.md`: the previous hooked-path defect.
