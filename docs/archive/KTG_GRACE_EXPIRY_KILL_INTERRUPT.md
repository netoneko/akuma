# `exit_group` paid 2 s per multithreaded process because no wait was kill-interruptible

**Date:** 2026-08-30
**Status:** **FIXED** — `should_interrupt_blocking_syscall` and `sys_futex` both
consult the deferred-kill flag. Gates: `test_pending_kill_interrupts_blocking_wait`
(boot suite) and `userspace/forktest/c_stress/futexkill.c` via
`scripts/futex_suite.py`.
**Found by:** gating the console flood in
[`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md) §13 — not by looking for it.

---

## 1. How it surfaced

`[KTG-STALE]` was 9.5% of the remaining serial log after the print gating, which
looked like a tripwire firing too loosely. It was not: the tripwires are correct
and were doing their job. What the quieter log exposed was the line *next* to
them —

```
[W] [ktg] pid=364: grace expired, hard-terminating 1 straggler(s)
```

**60 of those in a 90-second guest `cargo build`.** In the pre-gating capture of
the same workload there were 61, so this is not a new regression; it is a
long-standing cost that 4,418 lines of console noise had made unreadable.

The early ones are the tell — a strictly serialized chain, one every ~2.1 s:

| pid | grace-expiry at |
|---|---|
| 364 | 8.41 s |
| 366 | 11.06 s |
| 369 | 13.18 s |
| 372 | 15.26 s |
| 375 | 17.31 s |

## 2. Why every one of those is exactly 2 seconds

Not an inference from log timestamps — from the control flow.
`kill_thread_group` (`crates/akuma-exec/src/process/mod.rs`) only reaches that
`log::warn!` on the timeout arm:

```rust
const KILL_GRACE_US: u64 = 2_000_000; // 2 s
loop {
    if all_done { break; }
    if (runtime().uptime_us)() - started > KILL_GRACE_US {
        log::warn!("[ktg] pid={}: grace expired, hard-terminating {} straggler(s)", ...);
```

So the line *is* the 2 s. Sixty of them is ~120 s of blocked `exit_group` inside
a 90 s wall-clock window, and every one of them is a `rustc` that cargo's `wait4`
could not reap yet.

## 3. Root cause: the wake was delivered, then discarded by the loop it was for

`kill_thread_group` deliberately does **not** hard-terminate siblings — a thread
stopped mid-critical-section leaks its spinlocks, which is the sshd-"freeze" root
cause. Instead `request_thread_kill`:

```rust
PENDING_KILL[tid].store(true, Ordering::Release);
get_waker_for_thread(tid).wake();
```

sets a flag and *wakes* the thread, expecting it to run to its EL1→EL0 boundary
where `take_thread_kill_request` fires and it self-terminates.

A thread parked in an **untimed** `FUTEX_WAIT` never gets there. It wakes,
re-evaluates, and re-parks — because the re-evaluation never looked at the flag:

```rust
// src/syscall/sync.rs, before
let signal_pending = akuma_exec::threading::peek_pending_signal(tid) != 0;
match akuma_syscalls_sync::step(located, signal_pending, key, deadline, now)
```

No signal, futex word unchanged → `Step::Revalidate` → re-enqueue → park. The
wake was consumed by the very loop it was meant to end. The only remaining exit
is the 2 s grace expiry and the hard kill.

**And it was not just futex.** The yarn-driven families (`epoll_pwait`, `ppoll`,
`pselect6`, socket waits) route their interrupt input through
`should_interrupt_blocking_syscall`, which combined exactly two things:

```rust
pub fn should_interrupt_blocking_syscall() -> bool {
    if is_current_interrupted() { return true; }              // Ctrl-C / sys_kill
    config().pthread_kill_eintr_enabled
        && current_thread_has_pending_interrupt()              // pthread_kill
}
```

Both are *signal* paths. A grep for `has_pending_kill` across the tree found it
in `kill_thread_group`'s own bookkeeping and in tests — **and nowhere else**. No
blocking wait in the kernel was interruptible by a deferred thread-kill.

That is why the code comments describing this mechanism read as fatalistic: the
previous fix in this area correctly stopped the grace loop from breaking *early*
(accepting "flag consumed" as "thread dead" let a live sibling be left running
with its `Process` un-reaped). Correct, but it left the thread with no prompt way
out — only a reliable slow one.

## 4. The fix

One arm in each of the two readers, because they are genuinely separate paths:

- `should_interrupt_blocking_syscall` gains a kill check, **first**, covering
  every yarn-driven family at once.
- `sys_futex` does not call that helper — it *peeks* rather than consumes, and
  against an explicit tid — so it gains the same term where it builds `step`'s
  interrupt input.

Returning `EINTR` is safe precisely because the thread is dying: it unwinds to
the EL1→EL0 boundary, `take_thread_kill_request` fires, and it self-terminates
without the errno ever reaching userspace.

### 4.1 Three things checked before believing that

1. **No restart loop.** `SA_RESTART` rewinds `ELR` to re-execute the `svc` when a
   syscall returned `EINTR` — which would spin forever against a flag that is only
   consumed at the boundary. It does not apply here: that rewind lives *inside*
   signal delivery (`src/exceptions.rs`, guarded by a `UserFn` handler), and a
   kill-driven `EINTR` delivers no signal. Worst case, when a thread has both a
   pending signal *and* a pending kill, the syscall restarts once and the kill is
   taken at the next boundary — bounded, not a hang.
2. **No stale-flag hazard.** `PENDING_KILL[i]` is cleared both on slot reset and
   on recycle (`threading/mod.rs`), so a recycled slot's next occupant cannot
   inherit a spurious interrupt. This mattered more than usual: the flag is
   *peeked*, not consumed, by the new arms.
3. **The neighbourhood is race-sensitive.**
   [`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`](PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md)
   records that adding a `safe_print!` to `should_interrupt_blocking_syscall` was
   enough to make a failing probe pass — the function is on a path whose timing is
   load-bearing. The new arm is one relaxed atomic load placed before two heavier
   checks, so it should shorten that path rather than lengthen it. That document
   also records the trap that **SMP=4 cannot see this class**: verify at SMP=1.

## 5. What `[KTG-STALE]` actually was

Worth stating plainly, because it is what sent us here and it is **not** a bug:

- `[KTG-STALE]` / `[KTG-STALE-CH]` fire when a recorded sibling `thread_id` has
  been recycled to an unrelated process, and they are the guards that refuse to
  kill or exit-stamp the new occupant. They prevented two documented incidents:
  an innocent cargo worker killed as a "straggler" (`[PROC-ORPHAN]` for the rest
  of the boot), and a forged `exit(0)` on a live `ld` that hung a `-j4` build in
  `read()` forever.
- They fired 26 times each in the sample, always in 1:1 pairs. Only 18 of the 60
  grace-expiring pids produced one, so they are a *consequence* of slow exits
  under load, not the cause.

They stay ungated and unchanged. Fewer grace expiries should mean fewer of them,
which is a side effect of the fix rather than its goal.

## 6. Verification

```bash
scripts/futex_suite.py --port 2222        # includes the new futexkill probe
```

`futexkill` forks a child, parks N siblings in an untimed `FUTEX_WAIT`, has the
child signal readiness through a pipe immediately before `exit_group`, and times
`exit_group` → reaped from the parent. A regressed kernel reports ~2000 ms per
round; a fixed one reports single-digit ms. The limit is 500 ms — far below the
2 s grace, far above any healthy exit.

The boot-suite half is `test_pending_kill_interrupts_blocking_wait`, which asserts
**both** readers see an armed kill, and consumes the request before returning so
the test thread never carries it to a boundary.

Also re-check `[ktg] grace expired` count over a fixed guest-build window — that
is the number this was about, and it should go to approximately zero.

## Background

- [`CONSOLE_LOG_COST.md`](CONSOLE_LOG_COST.md) §13 — the print gating that made
  this readable, and the measurement method.
- [`PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md`](PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md)
  — the other half of "what ends a blocking syscall", and why SMP=1 is the arm
  that can see this class.
- [`STALE_THREAD_SLOT_KILL.md`](STALE_THREAD_SLOT_KILL.md) — why the `[KTG-STALE]`
  guards exist and what they prevented.
