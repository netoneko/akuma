# Tokio kernels: a threaded `execve` target never finished (2026-11-22)

Found by the ecosystem demo's Denver trial (`AKUMA_ECOSYSTEM_DEMO_AGENT_TRIAL.md`, round 2): agent C's `scala-cli run` of a program
that exits stalled for 40 minutes. Kernels: Denver (amd64), and by construction Taoxichuan and Shanghai (they share
`akuma-tokio-kernel-core`).

## Symptom

`scala-cli run` of any program that terminates (a JDK-only hello, a `ujson` hello, a project with compile errors) printed its
output and then **never completed**: the command stayed in `ps` forever, the parent never got a wait status. Servers were
unaffected, which is why every earlier Denver check of `scala-cli run` (they ran the board) passed.
`git bisect` over the kernel commits back to `e00744db` hung on every one: not a regression, a defect that no test of a
*terminating* program had touched.

## Cause

`execve` from a **non-leader thread** ends the group's other threads, leader included (`exec_strs` -> `kill_siblings`), and the
caller continues as the group's only thread, on a `Process` of its own, while the leader keeps its `Process` (the group's
identity and wait status) and its ring slot. But a stopped leader is a `TERMINATED` slot, and `claim_slot` hands `TERMINATED`
slots to the next `clone`. The new image's first thread took the leader's slot. At the image's `exit_group`,
`group_leader(cur)` looked for the slot whose pid is the group's `tgid`, found none, and fell back to `cur`: the exit was
published on the **caller's thread record** (pid 75), to its parent, which is the leader (pid 68), which never waits. The real
parent waits on pid 68 and heard nothing.

`task_exit_code` already avoids this for `exit_group`: it takes `hold_exited_thread_slot(lead_pid, lead)` *before* it stops the
siblings, for exactly this reason ("a leader stopped by then is a TERMINATED slot a fork on another core could claim"). The exec path
stopped the leader through the same `kill_siblings` and took no hold.

## How it was found

A static musl probe, `userspace/forktest/execthread/execpost.rs` (harness `overlays/ecosystem-demo/execpost.py`): four busy worker
threads, a non-leader thread `exec`s the probe itself, the new image spawns and joins three threads, prints `POST_JOIN_OK` and exits
with 7. On Linux the parent sees rc=7. On Denver the new image ran to the end (all three joins, `POST_JOIN_OK`) and the parent never
heard (3 of 3 runs). Temporary prints in `task_exit_code` showed `cur proc pid=75 tgid=68 ppid=68` with `lead=11 lead_pid=75`, and
a slot dump showed the leader's slot (4) already reused by the image's own threads (`TRAMP-MISMATCH tid=4 ... found 68`).
The earlier theories (cross-thread `wait4`, `pthread_join`'s clear-child-tid wake, the socket-wait changes) were each tested and
ruled out: all four spawn/wait thread layouts reap, and the image's joins worked.

## Fix

`exec_strs`: when the caller is not the leader, `hold_exited_thread_slot(slot_pid(owner), owner)` before `kill_siblings`, as
`task_exit_code` does; the reap releases it. One call, no allocation.

## Verification

| | before | after |
|---|---|---|
| `execpost` `libc` exit (3 runs / 2) | hang, 3 of 3 | rc=7, full output, 2 of 2 |
| `execpost` raw `exit_group` | hang | rc=7, 2 of 2 |
| `scala-cli run`, JDK-only hello, plain | hangs | completes in 27 s |
| `scala-cli run`, `ujson` hello, plain | hangs | completes in 29 s |
| `execthread-probe.py` (host and box, ok and fail, 3 each) | 0 failures | 0 failures |

Dependents of `akuma-tokio-kernel-core`: Denver, Taoxichuan, Shanghai (amd64 and aarch64). The change applies only to an `execve`
whose caller is not the group leader.

## Still open

* **The separate Codex PTY hang** was traced to unwired PTY reference hooks; see [TOKIO_PTY_REFERENCE_HANG.md](TOKIO_PTY_REFERENCE_HANG.md).
* **A raw `exit` (not `exit_group`) of the last remaining thread** of an exec'd image still never ends the process
  (`execpost raw_exit`); `task_exit_thread` publishes no group status. musl's `pthread_exit` of the last thread calls `exit`, so JVMs
  and Rust do not reach it.
* The main amd64 kernel's detach divergence (the exec'd program has a new pid; the parent sees the old group's exit) is E7b in
  `proposals/FUTURE_WORK.md`; in `execpost` it shows as rc=0 after `POST-START`.
