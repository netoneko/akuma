# Taoxichuan: `exit_group` "killed" a sibling thread that was still running (2026-11-01)

**Stability: B**: fixed and verified on the Mac TCG rig at `smp=4`; not yet run on the ryzen.

## Symptom

Found by the B7 gate (Taoxichuan's `futex` moved onto glue's), `tokioprobe` twice in a row at `smp=4`. The first run
passed; its exit left this on the console:

```
[kill] tid=4 (pid=6) terminated by tid=3 (pid=5) victim_state=3
[futex] WARNING: futex key degraded to tgid=0 (read_current_pid returned None) ...
[REVIVE] tid=9 TERMINATED->WAITING at=crates/akuma-threading/src/lib.rs:5828 by_tid=9
```

and the second run died: `[Fault] ring3 PageFault ... cr2=0x34 -- task killed`, then `[BKL] cpu N stuck: owner
cpu 0` on three cores. At `smp=1` and in the battery nothing showed.

## Cause

`child::kill_siblings` (the `exit_group` path) ended every other thread of the group by marking it TERMINATED,
purging its futex entries, clearing its slot and retiring its `Process`. That is safe for a thread that is not
executing (parked or ready): it never runs again. It is not safe for a thread RUNNING on another core, which at
`smp>1` is in ring 3 or waiting for the kernel lock at syscall entry: the mark does not stop it. Thread 9 kept going,
made a `futex` call with its `Process` already retired (the degraded key), and parked, and `schedule_blocking`
overwrote its own TERMINATED with WAITING (`[REVIVE]`). A thread of a dead process was then alive in the scheduler,
in an address space being torn down.

The race predates B7. Taoxichuan's own futex never parked on this path in a way that showed it; glue's, which
parks through the same scheduler with different timing, did on the first `smp=4` run.

## Fix

The AArch64 model (`akuma_threading::request_thread_kill`), at Taoxichuan's two kernel entries:

- `kill_siblings` ends a non-running sibling in place as before (`end_sibling`). A RUNNING one gets
  `request_thread_kill` instead, and the exiting thread waits, with the kernel lock released
  (`smp::bkl_unlocked`), until each such sibling is gone (TERMINATED, or its slot reissued). The bound is 2 s, after
  which a sibling is ended in place with an `[exit_group] slot N never reached the kernel` line.
- The syscall dispatcher checks `child::kill_requested()` after taking the lock and again before returning to ring 3;
  the timer tick checks it when it interrupted ring 3 (a compute loop makes no syscall). A requested thread runs
  `child::finish_killed`: purge, clear the slot, retire the `Process`, `x86_finish_current`. No exit status: the
  exiting thread publishes the group's.
- glue's `futex` wait already returns `EINTR` on `has_pending_kill`, so a sibling that parks just before the request
  lands is released, and leaves at the dispatcher's exit check.

## Verified

Mac TCG rig, the B7 tree: `tokioprobe` (4 workers) three times at `smp=4`: `RESULT PASS` x3, and 0 `[Fault]`,
`[REVIVE]`, degraded-key, `[exit_group]` or `BKL stuck` lines. Battery 10/10 + goose and the toolchain gate (rustc,
cargo build) with the same 0s.

Not shown: that the request path ran in those runs (there is no counter; a sibling RUNNING at the exact moment is a
timing question). The evidence is the absence of the failure that the old path produced on the first try. Not run on
the ryzen or in a fixed point yet.

## Background

- [`TAOXICHUAN_CLEANUP_PLAN.md`](TAOXICHUAN_CLEANUP_PLAN.md): step B7.
- [`AKUMA_AMD64_SIGKILL_NATIVE_PATH.md`](AKUMA_AMD64_SIGKILL_NATIVE_PATH.md) is not this: that was amd64's `kill -9` taking the
  AArch64 hard-kill path.
