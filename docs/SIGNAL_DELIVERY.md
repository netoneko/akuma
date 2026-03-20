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

### The `SA_RESTART` / ELR backup bug (§48)

A subtle but critical bug was found in the `SA_RESTART` implementation. When a
signal was delivered *after* a syscall completed but *before* the syscall's
return value was processed, the `SA_RESTART` logic would rewind `ELR` by 4 bytes,
assuming the syscall had been interrupted and needed to be restarted.

This was incorrect for syscalls that had already completed successfully.  For
example, a `FUTEX_WAKE` syscall that wakes one waiter returns `1`. If a signal
arrived at this exact moment, the sequence was:
1. `sys_futex` returns `1`.
2. `try_deliver_signal` is called.
3. `SA_RESTART` logic sees the flag, assumes an interrupted syscall, and does
   `elr_el1 -= 4`, backing the PC up to the `SVC` instruction.
4. The signal handler runs and returns via `rt_sigreturn`.
5. Execution resumes at the `SVC` instruction, but with `x0` now holding the
   *return value* (`1`) from the first call, not the original `uaddr` argument.
6. `sys_futex` is re-executed with `uaddr=1`, which is unaligned, causing an
   `EINVAL` error.

The fix, implemented in `try_deliver_signal` in `src/exceptions.rs`, is to
gate the `ELR` backup. The backup now only occurs if the syscall's return value
is `-4` (EINTR) or `-512` (ERESTARTSYS), indicating it was genuinely
interrupted. For any other return value (success or a different error), `ELR`
is not modified.

This is verified by the `test_sa_restart_not_applied_to_successful_futex_wake`
and `test_futex_sequential_wake_no_einval` tests in `src/sync_tests.rs`.

### Remaining risk: signal masking during asyncPreempt

After the fix, a second SIGURG that arrives **while asyncPreempt is running**
is blocked by `proc.signal_mask` (set in `try_deliver_signal` at delivery
time).  `rt_sigreturn` restores `uc_sigmask` from the saved frame — which does
**not** include SIGURG in the blocked set — so after sigreturn SIGURG is
unblocked again.  The pending-signal check added by the fix then sees that
second SIGURG and re-delivers it.

Whether Go handles this re-entrant delivery safely depends on
`gp.asyncSafePoint` state.  On Linux x86 this is fine because Go's SIGURG
handler checks `asyncSafePoint` before calling `pushCall`.  On AArch64 the
same guard applies.  No crash has been observed from this path, but it remains
a theoretical concern if a goroutine is not at an async-safe point when the
second SIGURG fires.

### Signal mask bit-numbering convention

Signal N uses bit `1u64 << (N-1)` in the 64-bit `uc_sigmask`:

| Signal | Number | Mask bit | Hex value |
|--------|--------|----------|-----------|
| SIGHUP | 1 | 0 | `0x0000_0001` |
| SIGKILL | 9 | 8 | `0x0000_0100` |
| SIGSTOP | 19 | 18 | `0x0004_0000` |
| SIGURG | 23 | 22 | `0x0040_0000` |

SIGKILL (9) and SIGSTOP (19) bypass the mask check entirely in
`take_pending_signal` — they are delivered regardless of `proc.signal_mask`.

### Single pending-signal slot limitation

`PENDING_SIGNAL[tid]` is a single `AtomicU32`.  A second `pend_signal_for_thread`
call overwrites the first.  If two signals arrive between two consecutive
`take_pending_signal` calls, only the later one survives.  This is acceptable
for SIGURG (Go tolerates dropped async-preemption attempts) but would be
wrong for SIGTERM + SIGKILL sequencing.  The limitation is documented in
`test_pending_signal_overwrite` in `src/sync_tests.rs`.

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
| r11/r12 contain ASCII of "futexwakeup addr=" | crash happened during Go's `throw` string build |
| goroutine 0 sp outside stack bounds | register corruption occurred before the crash |

---

## Test coverage (src/sync_tests.rs)

| Test | What it verifies |
|------|-----------------|
| `test_pending_signal_drained_by_take` | signal consumed exactly once |
| `test_peek_pending_signal` | peek is non-destructive |
| `test_futex_wait_eintr_signal_preserved` | signal survives FUTEX_WAIT EINTR |
| `test_take_pending_signal_sigurg_masked` | SIGURG is not taken when bit 22 of mask is set |
| `test_take_pending_signal_sigkill_ignores_mask` | SIGKILL/SIGSTOP bypass the mask |
| `test_pending_signal_overwrite` | second pend overwrites first (single-slot limit) |
| `test_signal_mask_bit_numbering` | bit positions for key signals |
| `test_futex_wake_sigurg_pending_x0_not_reused` | SIGURG pending after FUTEX_WAKE(1) returns 1 |
| `test_futex_wake_returns_exact_count_three_waiters` | FUTEX_WAKE(1) returns ≤1, not 3 |
| `test_futex_einval_uaddr_one` | uaddr=1 returns EINVAL cleanly |
| `test_sa_restart_not_applied_to_successful_futex_wake` | SA_RESTART only triggers for EINTR/ERESTARTSYS |
| `test_futex_sequential_wake_no_einval` | two sequential successful wakes do not fault |
| `test_pipe_epipe_for_nonexistent_pipe_id` | write to invalid pipe returns EPIPE |
| `test_pipe_multi_process_lifecycle` | pipe survives across shared FD tables and fork/exec |
| `test_rt_sigreturn_pending_redelivery` | signal redelivered after rt_sigreturn |

---

## Relevant source locations

- `src/exceptions.rs` — `do_rt_sigreturn`, `try_deliver_signal`, and the
  pending-signal delivery check after `rt_sigreturn`.
- `src/syscall/sync.rs` — `sys_futex`, including the EINVAL log with ELR.
- `crates/akuma-exec/src/threading/mod.rs` — `take_pending_signal`,
  `peek_pending_signal`, `pend_signal_for_thread` (lines ~2312–2368).
- `src/sync_tests.rs` — unit tests for the futex EINVAL paths and the
  pending-signal drain invariant.
