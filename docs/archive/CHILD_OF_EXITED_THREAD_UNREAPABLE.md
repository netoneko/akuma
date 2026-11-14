# A child forked by a thread that has since exited can never be reaped (Claude's input stall) — 2026-11-09

**Status: fixed and verified on the trashcan (kernel built there from this tree + the fix).** Root cause of the long-open Claude Code input stall
(`AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`).

## Symptom

`claude` (Bun) on the trashcan, with **no ssh and no wifi in the path** (`claude_input_lag_local.py` runs it under a
pty on the box itself): keys echo in 17-45 ms for ~30-60 s after start, then, after idle, **nothing a key does
shows for minutes**, claude writes zero bytes, the pty stays raw (`ICANON=0`) and a bare `\n` releases nothing.
So it was never the transport, the line buffer or output volume.

## What the stalled process was doing

Main claude process (`R`, 115,000 syscalls/s, about half of it in the kernel): per loop, three `wait4`, three
`epoll_ctl`, `clock_gettime`s, one `epoll_pwait` and a `pipe`. It held three **pidfds**, and its three children
(`rg --version`, `rg --files ... .orphaned_at /root/.claude/plugins/cache`, `rg --files ... /root` — Claude's
file-index scan) were all **zombies** (`Z`, ppid = the claude process) that were never reaped.

## Cause

`register_child_channel` stored the **pid of the forking thread** as the child's parent. `wait4(pid)` checks
`is_child_of_group(child, waiter_tgid)`, which resolves that thread to its thread group with `find_process`.
Bun forks `rg` from a **pool thread**; pool threads exit when idle. After that `find_process` finds nothing,
`is_child_of_group` is false, and `wait4` answers `ECHILD` for a child that exists, has exited and belongs to
the caller. Nothing can reap it. Its **pidfd**, meanwhile, is `pidfd_can_read` = the child channel says exited =
readable. So `epoll` reports the pidfd ready at once, `wait4` says there is no such child, Bun polls again:
a hot loop that never reaches the stdin handler. The ~45-60 s onset is the pool threads' idle exit.

Reproduced without claude (python on the trashcan, `scripts` scratch `reap2.py`):

| spawn / wait | result before the fix |
|---|---|
| main spawns, main waits | reaped |
| thread A spawns and stays alive, thread B waits | reaped |
| **thread A spawns and exits, thread B (or main) waits** | **`ECHILD`** |

Linux keeps the child with the thread group, so the third case is reaped there.

Also found: `waitid(2)` was `ENOSYS` on amd64 (`os.waitid` and `waitid(P_PIDFD, ...)` both), though
`akuma-syscalls-glue` has an implementation. Wired the same evening: `Syscall::Waitid` in `crates/akuma-kernel-amd64/src/usermode.rs` goes to glue like
`wait4` and sweeps the reaped `SPAWN` rows. Checked on the trashcan (python/ctypes): `P_PID` with `WNOWAIT` leaves the child,
the next call reaps it (`CLD_EXITED`, status 7), then `ECHILD`; `WNOHANG` on a live child returns 0 with `si_pid` 0;
`P_PIDFD` blocks until exit; `P_ALL` finds an exited child; `P_PIDFD` on the caller's own pidfd gives `ECHILD` (Go's probe).

## Fix

- `akuma-exec` `register_child_channel` records the forking thread's **thread group** (`tgid`) when the thread is
  known, so a child belongs to the process.
- `akuma-syscalls-glue` `sys_wait4`: the `pid == -1 / 0` arm now looks children up by the waiter's thread group
  (`has_children`, `find_exited_child`, `add_poller_to_all_children`) instead of the calling thread's pid, so a
  wait from any thread sees what any thread of its process forked.

## Verification

Trashcan, kernel built on the box (`kbuild -j 4 --features no-tests`, incremental 2 m 18 s) and installed with `kinstall`.

- Reproducer (`reap2.py`): "thread A spawns then EXITS, thread B waits" went `ECHILD` -> reaped `(pid, 0)`; the other
  rows are unchanged.
- Claude through the in-box probe (`scripts/benchmarks/claude_input_lag_local.py`, keys typed one at a time, idle between):

| | keys after idle | echo after idle |
|---|---|---|
| before the fix (4 runs: 45 s, 45 s, 150 s, 150 s idle) | 0 of 22 shown, claude silent for minutes | n/a |
| after the fix: 150 s idle | 8 of 8 | median 21 ms (4-277) |
| after the fix: 100 s idle, run 1 | 6 of 6 | median 21.6 ms (max 45) |
| after the fix: 100 s idle, run 2 | 6 of 6 | median 21.6 ms (max 49) |

Not fixed by this and still visible: the **first minute** after start is slow (first key 0.3-3.8 s, median up to
1.9 s in two runs) while claude's startup `rg --files /root` scan and the pool threads run on a very large `/root`.
That is a separate cost, not a stall.

Still open: the unit test `reaping_a_buffered_child_keeps_its_output_but_not_the_child`
fails when run alone under a `children` filter (needs `akuma_exec::init`, fails on the pristine tree too); no host
test yet covers "forker thread gone" because it needs a process table.

## Background

`docs/archive/AMD64_CLAUDE_LOGIN_INPUT_STUCK.md`, `crates/akuma-exec/src/process/children.rs`
(`is_child_of_group`, `reparent_children_to`), `scripts/benchmarks/claude_input_lag_local.py`.
