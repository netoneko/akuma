# Hooked scheduler: a thread returned from a blocking syscall with another thread's TLS (2026-11-10)

**Scope:** the AArch64 root kernel built with `--features hooked-sched` (M6, default off). The SGI
scheduler, amd64 and the tokio kernels are not affected.

## Symptom

A two-thread musl program, the userspace probe `userspace/epollprobe/c/epoll_wake_latency.c`,
died with `SIGSEGV` on every hooked devbox boot at SMP=2 and SMP=4, after 0.4–1.2 s:

```
[Fault] Data abort from EL0 at FAR=0x10462f40, ELR=0x404bbc, ISS=0x7
[Fault]  SP_EL0=0x201ffffa80 SPSR=0x60000000 TPIDR_EL0=0x10462fe8
[Fault] Process 10 (/tmp/ewl) SIGSEGV after 1.19s
```

The faulting address sits just below `TPIDR_EL0`. That `TPIDR_EL0` is in the mmap region where musl
puts a *thread's* TLS, not the main thread's static TLS (`x0=0x420110` is the main thread's data).
The syscall trace just before it ends `futex`, `futex`, `munmap`: `pthread_join` waits for the
thread, then unmaps its stack and TLS. So the main thread came back to EL0 with the exited thread's
TLS as its own, and its first TLS access hit the unmapped page. A `[BKL] stuck` report from a peer
core came with it each time. The SGI build and pre-refactor `main` ran the probe cleanly at SMP=2
and SMP=4. The hooked kernel was not run at SMP=1.

## Cause

The syscall epilogue in `akuma-exceptions` (`sync_el0`, the "SYNC TLS" block) copies the **live**
`TPIDR_EL0` register into the thread's trap frame after every syscall. That is how
`set_tpidr_el0` takes effect, since the register is reloaded from the frame on the way back to EL0.

The copy is only correct if the live register still holds this thread's value when its syscall
finishes. On the SGI path it does: the IRQ entry that switches threads saves and restores
`TPIDR_EL0` around every switch (`akuma-entry/src/exceptions.rs`). On the hooked path a switch is a
function call, and `switch_to` (`akuma-kernel-glue/src/hooked.rs`) installed only `TTBR0_EL1` and
the slot name. Its module doc stated that `TPIDR_EL0` "needs nothing per switch" because the frame
covers it, which overlooked this epilogue. So a thread that blocked inside a syscall (the main
thread in `pthread_join`'s futex wait) resumed after another thread had run on that core and set the
register on its way to EL0. The epilogue then stamped that other thread's TLS into the resumed
thread's frame.

Any syscall that blocks on a core where another user thread then runs can hit this, not only
`pthread_join`, and at any SMP, including 1.

## Fix

`switch_to` saves the outgoing slot's live `TPIDR_EL0` and installs the incoming slot's, from a new
per-slot `SAVED_TLS` array (one `AtomicU64` per slot, no allocation). A slot that never ran, or a
recycled one, installs a stale value. That is harmless, because its first crossing to EL0 restores
its own frame's value before EL0 can read the register. Both operations are the safe
`akuma_cpu::sysreg` functions; no `unsafe` was added.

## Verification

- The probe on the fixed hooked devbox kernel, SMP=2 and SMP=4, spin and sleep modes, 300
  iterations each: completes, no `SIGSEGV`, no `[BKL] stuck`. Before the fix it crashed in every one
  of those four runs.
- Hooked boot suite and the devbox session stress: see `AKUMA_MULTIARCH_ATTEMPT_0.md`, W3b
  follow-up.

## Still open

- Whether the rare devbox ssh `exit 255` seen before the fix was this bug. The session stress after
  the fix answers it either way (see the log).
- The epilogue's copy of the live register is the fragile part. A design that writes the frame only
  from `set_tpidr_el0` itself would not depend on the scheduler preserving the register. Changing it
  touches the SGI path's default build, so it was left alone.

Background: `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md` (W3b), where the probe was written to
compare wake latency across kernels and found this instead.
