# `pause()` ignored a group exit (2026-11-12)

## Symptom

On amd64, every run of `userspace/forktest/c_stress/groupexit_thread.c` printed one line:

```
[thread] DRAIN INCOMPLETE: 1 thread(s) still live in group 1828 — the reaper may free a live address space
```

The probe exited with the right status (3), so its own check passed. The probe's `sleeper`
thread, which sits in `for (;;) pause();`, stayed in `ps` after its group was reaped, still
standing in an address space the reaper had freed.

During exit steps 1 and 2 (`proposals/AKUMA_ONE_EXIT.md`) the three lines were blamed on a
stale `THREADS` row left behind by `lifeprobe`'s SIGKILL. Step 3 deleted that table and the
lines stayed. Once `drain` reported by tgid, they named `groupexit_thread`'s group.

## Cause

Glue's `suspend_until_signal` is the wait loop behind both `pause()` and `rt_sigsuspend`. It
re-parks in 10 ms slices and leaves only when:

- a signal its mask does not block is pending, or
- the interrupt flag (`is_current_interrupted`) is set.

A group exit sets neither. amd64's `thread::wake_group` and AArch64's `kill_thread_group` ask a
sibling to leave with a **deferred kill request** (`request_thread_kill` -> `PENDING_KILL`),
which is what `should_interrupt_blocking_syscall` reads for every other blocking wait.
`suspend_until_signal` never read it. The sleeper woke, found nothing deliverable and parked
again, so it never reached the syscall boundary where it would have left. `drain` spun its
100 000 rounds and gave up.

AArch64 hid this: `kill_thread_group` hard-terminates a sibling once its grace expires.

## Fix

`suspend_until_signal` returns `EINTR` when `has_pending_kill(slot)` is set:

- It restores the saved mask first, as for a spurious interrupt, because no signal frame will
  consume the restore-sigmask.
- It does not consume the request. On amd64 the thread leaves at its next syscall entry
  (`should_leave_now`). On AArch64 the syscall-return path takes the request and terminates
  the thread.

## Verified

- On amd64 under QEMU (TCG), SMP=4, `groupexit_thread 0/1/2` all return rc=3.
- The run printed no `DRAIN INCOMPLETE` lines, and no probe thread remained in `ps` afterwards.

## Still open

`busybox timeout`'s watcher shows up as a zombie under pid 1 on the amd64 probe rig. That is a
property of the rig, not this bug: the rig boots `sshd` as init, and `sshd` does not reap
orphans. The AArch64 rig boots `herd`, which does.
