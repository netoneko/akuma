# SysV message queues outlived their box (2026-11-11)

## Symptom

None observed at runtime. Found by reading:
- the 2026-08-07 dead-code sweep recorded it
  ([`DEAD_CODE_SWEEP_FINDINGS.md`](DEAD_CODE_SWEEP_FINDINGS.md) §1);
- the 2026-11-11 pre-merge sweep found it still open
  ([`AKUMA_FEATURE_AUDIT.md`](AKUMA_FEATURE_AUDIT.md), "Dead-code sweep before the merge").

## Cause

`cleanup_box_queues(box_id)` existed in `akuma-syscalls-ipc`, said "called from sys_kill_box",
and had no caller. `sys_kill_box` dropped the box's VFS namespace but not its queues. The table
is keyed `(box_id, msqid)` and herd derives a box id from the box's name, so:

1. Every box that used a queue and was killed leaked the queue and its messages until reboot.
2. A box restarted under the same name inherited the previous generation's queues, undelivered
   messages included.

The function is `pub`, which is why the `dead_code` lint never reported it.

## Fix

`akuma-syscalls-glue::container::sys_kill_box` calls `cleanup_box_queues(box_id)` after the
kill succeeds (behind the crate's `sc-sysv-ipc` feature, next to `remove_box_namespace`).

Nobody is woken. Every thread that could be parked on one of those queues belongs to the box
and has just been killed. Queues are reachable only from their own box, because the key
includes the caller's box id.

## Verification

Boot test `test_kill_box_removes_its_msgqueues` (`akuma-kernel-tests-aarch64`, under
`sc-containers`):
1. registers a box;
2. creates a queue in it as a process inside the box, then pushes a message;
3. kills the box from the host;
4. asserts no queue with that box id remains.

HVF boot suite results are in `AKUMA_FEATURE_AUDIT.md`. amd64 has no `kill_box` dispatch arm
(only `mount`/`umount2` of `sc-containers` are wired there), so the path is AArch64-only today.

## Still open

The five msgqueue waker tests remain disabled (`DEAD_CODE_SWEEP_FINDINGS.md` §2). The
`IPC_RMID`/send/receive wake paths are still covered only by production use.
