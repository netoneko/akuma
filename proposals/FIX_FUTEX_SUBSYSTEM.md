# Plan: SA_RESTART ELR bug fix + additional tests (§48)

## Context

After §46 (pending signal not delivered after rt_sigreturn), the crash
`[futex] EINVAL: uaddr=0x1` still appears with the following log sequence:

```
[signal] deliver sig=23 slot=12 handler=... elr=0x1009f14c sa_flags=0x18000004
[sigreturn] restoring: pc=0x1009f14c
[futex] EINVAL: uaddr=0x1 op=129 elr=0x1009f150
[pipe] write WARN: pipe id=25 not found (len=17)
```

The pipe WARN immediately after the EINVAL confirms the crash causes a Go goroutine
to exit before it finishes provisioning a pipe fd that other goroutines then write to.

## Root cause (identified)

`sa_flags=0x18000004` = `SA_RESTART (0x10000000) | SA_ONSTACK (0x08000000) | SA_SIGINFO (0x4)`.

**`try_deliver_signal` (`src/exceptions.rs` lines 793–801):**

```rust
const SA_RESTART: u64 = 0x10000000;
if action.flags & SA_RESTART != 0 {
    let esr: u64;
    unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr); }
    if (esr >> 26) == 0x15 { // EC_SVC_LOWER
        unsafe { (*frame).elr_el1 -= 4; } // ← backs up to the SVC instruction
    }
}
```

The ELR-4 backup fires for **every syscall** when SA_RESTART is set, regardless
of whether the syscall was interrupted. SA_RESTART semantics require the backup
only when the syscall returned EINTR (-4) or ERESTARTSYS (-512).

Crash sequence with the bug:
1. `FUTEX_WAKE(addr=real_addr)` completes successfully, returns `x0 = 1`.
2. Post-syscall check: SIGURG is pending. `try_deliver_signal` is called.
3. SA_RESTART set, EC=SVC_LOWER → ELR backed up 4: now points at the SVC instruction.
4. Signal frame: `mcontext.pc = SVC instruction`, `mcontext.x0 = 1`.
5. Go's `doSigPreempt` → `pushCall(asyncPreempt)`: `LR = mcontext.pc = SVC instruction`.
6. `rt_sigreturn`: restores all regs. `ELR_EL1 = SVC instruction`, `x0 = 1`.
7. `asyncPreempt` runs (via modified `mcontext.pc`), returns via `LR = SVC instruction`.
8. SVC re-executes with `x0 = 1` → `FUTEX_WAKE(uaddr=1)` → EINVAL.
9. Go `throw` → goroutine dies → pipe fd never provisioned → EPIPE for other writers.

## Fix — `src/exceptions.rs` line 799

Gate the ELR-4 backup on the syscall return value being an interrupted-syscall
error. The syscall return value is already in `(*frame).x0` at this point (set
by the caller before invoking `try_deliver_signal`):

```rust
const SA_RESTART: u64 = 0x10000000;
if action.flags & SA_RESTART != 0 {
    let esr: u64;
    unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr); }
    if (esr >> 26) == 0x15 { // EC_SVC_LOWER
        // Only restart the syscall if it was actually interrupted.
        // SA_RESTART must NOT apply to successful syscalls — backing up ELR
        // for a completed FUTEX_WAKE (ret=1) causes it to re-execute with
        // x0=1 (the return value), producing EINVAL (uaddr=1 is unaligned).
        let ret_val = unsafe { (*frame).x0 as i64 };
        if ret_val == -4 || ret_val == -512 { // EINTR or ERESTARTSYS
            unsafe { (*frame).elr_el1 -= 4; }
        }
    }
}
```

## Tests — `src/sync_tests.rs`

### 1. `test_sa_restart_not_applied_to_successful_futex_wake`
Single-threaded. Directly verifies the fixed condition:
- After a FUTEX_WAKE with no waiters (returns 0), assert that x0 = 0 does NOT
  satisfy `ret == -4 || ret == -512`, so ELR would NOT be backed up.
- After a FUTEX_WAKE with one waiter (returns 1), same: 1 ≠ -4/-512, no backup.
- After a wait that returns EINTR (-4, simulated), assert EINTR satisfies the
  condition (ELR WOULD be backed up).
This test validates the gate condition in isolation without needing full signal
delivery infrastructure.

### 2. `test_futex_sequential_wake_no_einval` (multi-threaded)
Regression for the exact crash:
1. Spawn a waiter on FUTEX_WORD_SEQ (value 0, waits for non-zero).
2. Main: set FUTEX_WORD_SEQ=1, call FUTEX_WAKE(1) → returns woken (0 or 1).
3. Main: immediately call FUTEX_WAKE on a SECOND valid aligned address (no
   waiters) — must return 0, NOT EINVAL.
Without the fix, the first WAKE's return value (1) would corrupt x0 for the
second call via SA_RESTART ELR rewind + asyncPreempt. With the fix, the second
WAKE succeeds.
Note: in kernel tests there is no live signal delivery between steps 2 and 3,
so this test documents the invariant rather than simulating the delivery race.
Add a comment explaining the limitation.

### 3. `test_pipe_epipe_for_nonexistent_pipe_id` (single-threaded)
Tests the clean EPIPE path seen in the crash log. Using NR_PIPE2 and NR_WRITE
via `handle_syscall`:
1. Create a pipe with NR_PIPE2 → get fd_r, fd_w, pipe_id.
2. Close fd_r (NR_CLOSE) so the read end is gone. (Or: directly test by writing
   to a pipe_id that was never created — pipe_id 99999.)
3. Write to fd_w → must return EPIPE (-32), not crash.
4. Write to a pipe fd whose pipe_id doesn't exist → must return EPIPE.
This is the exact recovery path exercised after the Go goroutine dies.

### 4. `test_rt_sigreturn_pending_redelivery`
Verifies the §46 fix is in place. Because calling NR_RT_SIGRETURN would change
the test thread's SP/PC, use the state-level approach:
- Pend SIGURG (23) on current thread slot.
- Verify `take_pending_signal(0)` returns `Some(23)` (the §46 fix drains it).
- Verify a second `take_pending_signal(0)` returns `None` (drained).
- Pend SIGURG again, set mask to block SIGURG (bit 22), verify `take_pending_signal(mask)` returns None (masked, §46 doesn't deliver masked signals).
This isolates the "take on sigreturn" invariant without needing real sigreturn.

## Critical files

| File | Change |
|------|--------|
| `src/exceptions.rs` lines 793–801 | Gate ELR-4 on `ret_val == -4 \|\| ret_val == -512` |
| `src/sync_tests.rs` | Add 4 tests + register in `run_all_tests()` |
| `docs/SIGNAL_DELIVERY.md` | Add §48 (SA_RESTART bug + fix) |
| `docs/GOLANG_MISSING_SYSCALLS.md` | Add §48 entry |

## Verification

```bash
cargo check
cargo test --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
```

In QEMU: run sync tests (all 4 new pass); then run `go build` — the
`[futex] EINVAL: uaddr=0x1` log should no longer appear.
