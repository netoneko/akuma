# Signal Delivery in Akuma

## Overview

Signals are delivered to userspace processes at syscall boundaries.  When a
syscall completes, `rust_sync_el0_handler` in `src/exceptions.rs` checks the
per-thread pending-signal queue and, if a signal is ready and not masked,
calls `try_deliver_signal` to push an `rt_sigframe` onto the user stack and
redirect ELR to the registered handler.

---

## Go goroutine async preemption and rt_sigreturn

### How Go uses SIGURG

Go's goroutine scheduler uses **SIGURG** (signal 23) for asynchronous
preemption.  When the runtime wants to preempt a running goroutine it sends
SIGURG to that OS thread.  The signal handler (`doSigPreempt`) uses
`pushCall` to inject `asyncPreempt` into the goroutine's execution stream:

1. Decrements `mcontext.sp` by 8 (pushes the original LR onto the goroutine
   stack).
2. Sets `mcontext.regs[30]` (LR) to the PC after the interrupted SVC so that
   `asyncPreempt`'s `RET` returns to the right place.
3. Sets `mcontext.pc` to the `asyncPreempt` address.

The kernel then runs `rt_sigreturn` to restore the modified context, and
execution resumes inside `asyncPreempt`.

### The bug: missing signal delivery after rt_sigreturn

Linux delivers pending signals on **every** return to user mode, including
after `rt_sigreturn`.  Prior to the fix, Akuma only checked for pending
signals in the normal syscall return path — but `rt_sigreturn` returned
*early* (before that check), so any signal that arrived while the first
SIGURG handler was running was silently deferred.

The exact crash sequence:

1. `futexwakeup(addr=0xc4047158)` → `sys_futex(WAKE)` returns 1 (woke 1
   waiter).  The kernel sets `frame.x0 = 1` (the return value).
2. A pending SIGURG is found.  The kernel pushes a signal frame, saves
   `frame.x0 = 1` in `mcontext.regs[0]`, and redirects ELR to the handler.
3. Go's `doSigPreempt` runs; `pushCall` modifies `mcontext.sp/.pc/.regs[30]`
   to inject `asyncPreempt`.
4. `rt_sigreturn` SVC: `do_rt_sigreturn` restores the modified frame (x0 is
   restored to `1` — the syscall return value).
5. **Before the fix**: the early `return saved_x0` skipped the pending-signal
   check.  A second SIGURG that arrived during step 3 was left in the queue.
6. `asyncPreempt` runs with the goroutine stack shifted by -8 (pushCall).
7. The second SIGURG is deferred to the *next* syscall.  By that time the
   goroutine's stack has been shifted; the futex call made at that point has
   `x0 = 1` (the `mutex_locked` sentinel) instead of the real address.
8. Result: `[futex] EINVAL: uaddr=0x1`, `futexwakeup returned -22`, and
   eventually SIGSEGV at 0x1006 (Go's deliberate crash on unexpected futex
   failure).

### The fix

After `do_rt_sigreturn` succeeds, check for pending signals using the same
logic as the normal syscall return path (`src/exceptions.rs`).  Key points:

- `do_rt_sigreturn` has already restored the full register set in `*frame`,
  so `try_deliver_signal` sees the correct SP/PC.
- `frame.x0` must be set to `saved_x0` *before* calling `try_deliver_signal`
  so that the nested signal frame saves the right value; when the nested
  handler calls `rt_sigreturn`, the original syscall return value is
  correctly restored.

### Diagnosis aid

The `[futex] EINVAL` log message now includes the caller's ELR (return
address of the faulting SVC instruction), making it much easier to identify
whether a corrupted uaddr originated from a goroutine preemption race:

```
[futex] EINVAL: uaddr=0x1 op=129 elr=0x... (null or unaligned)
```

---

## Symptom summary

| Symptom | Meaning |
|---------|---------|
| `[futex] EINVAL: uaddr=0x1` | x0 was corrupted to the `mutex_locked` sentinel |
| `futexwakeup addr=... returned -22` | Go's runtime detected the unexpected EINVAL |
| `SIGSEGV fault=0x1006` | Go's deliberate crash (`throw`) triggered by futex failure |

---

## Relevant source locations

- `src/exceptions.rs` — `do_rt_sigreturn`, `try_deliver_signal`, and the
  pending-signal delivery check after `rt_sigreturn`.
- `src/syscall/sync.rs` — `sys_futex`, including the EINVAL log with ELR.
- `src/sync_tests.rs` — unit tests for the futex EINVAL paths and the
  pending-signal drain invariant.
