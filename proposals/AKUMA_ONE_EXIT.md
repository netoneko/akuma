# One process exit for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. The last lifecycle piece of the convergence in
[`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`](../docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md), after
one execve, one spawn, one waitpid and one mm ([`AKUMA_MM_CONVERGENCE.md`](AKUMA_MM_CONVERGENCE.md)).*

## Status

Step 1 done (2026-11-12): `akuma_syscalls_glue::exit::finish_process`, called by glue's
`sys_exit_group` and amd64's `run_process`.

Step 2 done (2026-11-12): the group's exit flag and status live on its `SharedSignalTable`
(`begin_group_exit`/`group_exiting`, `GroupExit::{Code, Signal}`, `end_group_exit_for_exec`).
amd64's `GROUP_EXIT`/`GROUP_EXIT_STATUS` arrays and every slot clear (spawn, `bind_child_task`,
exec) are gone; the status is typed, so the sign-before-flag decode at its two readers is gone
too. `spawn_stale_group_state_test` (a boot test that poisoned the next spawn slot) has nothing
left to poison and is replaced by host tests (`akuma_exec::process::signal::group_exit_tests`).
Gates: amd64 suite 826/836/832 (the retired test's 5 checks; SMP=4's further 4 are the
by-design spawn skip), AArch64 312/320/318; probes and ssh exit status equal to step 1 on
amd64 and all-pass on AArch64. The 3 `DRAIN INCOMPLETE` lines appear on the step-1 kernel
too. *Corrected 2026-11-12:* they were blamed here on a stale `THREADS` row after `lifeprobe`'s
SIGKILL. Step 3 deleted the table, the lines stayed, and once `drain` reported by tgid they
named `groupexit_thread`'s group: its `pause()` sibling never saw the group exit
([`docs/archive/AKUMA_PAUSE_IGNORES_GROUP_EXIT.md`](../docs/archive/AKUMA_PAUSE_IGNORES_GROUP_EXIT.md),
fixed in cc546fd2).

Step 3 done (2026-11-12): amd64's `THREADS` table is gone. A group's live threads are the
process table's rows with its `tgid` and a live `THREAD_PID_MAP` slot
(`akuma_exec::process::{for_each_live_group_thread, live_group_threads}`); "is this task a
thread" is `pid != tgid`; a thread's exit clears its `Process`'s `clear_child_tid`.
`clone_thread` refuses a thread once the group is exiting (`GROUP_EXITING`), and `drain` marks
before it counts, so no thread joins after the count reaches zero. `UserCtx::thread_slot` is a
reserved word (the assembly's offsets past it are fixed). Gates: amd64 suite 826/836/832,
AArch64 312/320/318, both equal to step 2.

Step 4 done (2026-11-12): amd64's `SPAWN` table, `PROC_SLOTS`, `UserCtx::proc_slot` (now a
reserved word, for the assembly's offsets) and `seed_proc_slot` are gone. "Is this a user task"
is "does it have a registered `Process`"; `bind_child_task` only sets the `CR3` root; the
sweep's last job moved to `after_reap` as `akuma_exec::process::sweep_dead_thread_identities`,
which drops a `THREAD_PID_MAP` row only when its slot is not live *and* its pid is not
registered, and only if it still names that pid. A fork no longer fails at 121 processes with
"no free SPAWN row"; the process table is the only ceiling. Gates: amd64 suite 826/836/832,
AArch64 312/320/318; no `DRAIN INCOMPLETE` anywhere; probes and ssh exit status equal to
step 3. Still open on amd64: `orphanthr`'s thread-child case (a child of an already-exited
worker keeps the dead ppid). `lifeprobe`'s orphan check fails only because that rig boots
`sshd` as pid 1, which does not reap.

### What step 1 found

- **The reap reads the channel, confirmed.** glue's `wait4`/`waitpid`/`waitid` reap on
  `ProcessChannel::has_exited` only (`reap_exited` behind `get_child_channel`/`find_exited_child`).
  `Process::exited` is read by `/proc`, `ps`, the load counters, `cleanup_process_fds`'s sharer
  count and the AArch64 EL0 syscall return. So the order of the `exited` store does not decide
  when a reap may run. The publish does, and it is last in `finish_process`.
- **The sequence is** close fds -> `exited`/`exit_code`/`Zombie` -> reparent every group
  member's children to init -> `publish_child_exit` (and the leader's channel when a non-leader
  exits the group) -> `vfork_complete`. Two changes against what each kernel did:
  - AArch64 reparented *after* the publish; it now reparents before, as Linux's `exit_notify`
    does, so a woken parent never races the reparent.
  - AArch64 collected the group's pids *after* `kill_thread_group`; it now collects before, the
    way amd64 always did before `drain` (a retired row is invisible to the table walk).
- **Kept per kernel, on purpose.** `release_shared_write_mappings` stays at each call site:
  AArch64 releases before `kill_thread_group` (which tears the space down), amd64 after `drain`
  (so no sibling faults a page back in). Sibling teardown and how the thread leaves the CPU stay
  per kernel. amd64 keeps `fbdev::release`.
- **Pinned difference.** AArch64 still also marks the caller exited before `kill_thread_group`;
  the siblings' `cleanup_process_fds` has always counted on that. `finish_process` stores the
  same values again.
- **amd64 now marks every process it ends**, including the boot suite's and init (slots below
  `SPAWN_SLOT_BASE`), which `spawn_record_exit` skipped. Their publish is a no-op (no child
  channel). The stale-row diagnostic went with `spawn_record_exit`: the exit now names the
  exiting process, not whatever pid a row held, so that bug class cannot reach the publish.
- **Open on amd64, found by step 1's gate and older than it:** a child forked by a worker thread
  that exits before its process keeps the dead thread's pid as its parent
  (`userspace/forktest/c_stress/orphanthr.c`; AArch64 and Linux pass). `finish_process`
  reparents for every member *still in the table*, and an exited worker's row is already gone.
  The fix belongs in thread exit (reparent to a live group member, as Linux does), which is
  step 3's territory. `lifeprobe`'s "orphan of a SIGKILLed parent was reaped" fails on amd64
  for the same parentage reason, before and after step 1; it passes on AArch64.
- **Gates (step 1):** boot suites amd64 831/841/841, AArch64 312/320/318 (SMP 1/2/4), both
  equal to the previous commit. At SMP=2 on both kernels: `segvgroup`, `thrprobe`,
  `cloneflags`, `futextest`, `groupexit_thread` 0/1/2, `grandfork` and `killtree` pass; ssh
  exit status 10/10 sessions, `exit 1..5`, 143 and 7. amd64's results equal the previous
  commit's probe for probe. Self-host builds not yet run.
- **Not folded:** `return_to_kernel` (fault and OOM kills on AArch64) lives in `akuma-exec`,
  below glue, and keeps its own `cleanup_process_fds` + `publish_child_exit`. Nor glue's
  `sys_exit` leader path, which is a thread exit, not a process one.

## Goal

A process ends through **one** sequence on both kernels, and amd64 loses the bookkeeping that
exists only because its exit is its own:

- the `SPAWN` table and the "proc slot" index (`usermode::{SPAWN, PROC_SLOTS, SPAWN_SLOT_BASE}`,
  `UserCtx::proc_slot`, `bind_child_task`'s row, `spawn_record_exit`, `sweep_reaped_spawn_rows`);
- the group-exit arrays indexed by that slot (`thread::{GROUP_EXIT, GROUP_EXIT_STATUS}`);
- the second thread table (`thread::THREADS`, `live_count`, `drain`), where akuma-exec already
  gives every thread a `Process` of its own (`clone_thread`, `tgid = parent.tgid`).

What stays per architecture is how a thread **leaves the CPU**. amd64's lifecycle returns:
`enter_user` comes back to `run_process` and the task finishes. AArch64's does not: the thread is
marked terminated on its slot and parks until the scheduler stops picking it. That difference is a
hook, the way `ExecArch` is for execve.

## Where the two kernels stand

The same work, in the order each kernel does it. "glue" is `crates/akuma-syscalls-glue/src/proc.rs`.

| Step | AArch64 | amd64 |
|---|---|---|
| A thread leaves (`exit`) | glue `sys_exit`: mark exited/zombie, clear and futex-wake `clear_child_tid`, notify channel; the leader also releases shared writable mappings and closes fds; then `mark_thread_terminated` and park | dispatch `Exit`: `mark_leave`, unwind to `run_process` (main) or `thread::run_thread` (others) |
| The group ends (`exit_group`) | glue `sys_exit_group`: `SignalActions::claim_group_exit` picks one caller; `kill_thread_group`; `return_to_kernel` | dispatch `ExitGroup`: `thread::exit_group_from_thread` from a non-leader; `exit_current_group` sets `GROUP_EXIT[proc_slot]` and unwinds |
| Siblings stop | `kill_thread_group` terminates them (grace for a sibling in the kernel) | `thread::drain(proc_slot)`: set the flag, wake the group, spin until `live_count` is 0 (prints `DRAIN INCOMPLETE` otherwise) |
| Shared writable pages | `mem::release_shared_write_mappings` (leader) | the same function, in `run_process` |
| Descriptors | `close_all` (leader) / `cleanup_process_fds` in `return_to_kernel` | `close_all` in `run_process` |
| Exit status visible | `publish_child_exit`, `notify_child_channel_exited` | `spawn_record_exit`: `exited`/`exit_code`/`state` on the `Process`, then the child channel (`publish_child_exit`) |
| Orphans | (in the shared exit helpers) | `reparent_children_to(member, 1)` for every group member |
| Device ownership | none | `fbdev::release(tgid)` |
| The task's I/O channel | `remove_channel(tid).set_exited` | the same |
| Reclaim | `drain_retired_if_requested` | the same |
| Reap side | glue `wait4`/`waitpid`/`waitid` (`reap_exited`) | glue's, then `after_reap`: the `SPAWN` row sweep |

Already one copy, and not to be re-audited: `wait4`/`waitpid`/`waitid`, `publish_child_exit` and
the "done" signal (the child channel, set after `close_all`), `reparent_children_to`,
`hold_exited_thread_slot`, `clone_thread`.

## Target

1. `akuma_syscalls_glue::exit::finish_process(proc, code)`: the steps both kernels already take
   once per process, in one order:
   1. release shared writable mappings;
   2. close descriptors;
   3. mark the process exited (`exited`, `exit_code`, `Zombie`);
   4. reparent the group's children to init;
   5. publish the exit (`publish_child_exit`, child channel);
   6. complete a `vfork`.

   An `ExitArch` hook supplies the per-kernel extras (amd64: `fbdev::release`) and the departure.
2. The group-exit state lives on the group's shared `SignalActions`, beside `claim_group_exit`:
   one flag and one status for every thread of the tgid. amd64's slot-indexed arrays go.
3. "Which threads are still running in this group" is asked of the process table: the
   `Process` records whose `tgid` matches and whose thread has not terminated. amd64's `THREADS`
   table keeps only what the table cannot answer yet (`clear_child_tid` is already on `Process`).
4. With 1–3 done, a `SPAWN` row carries nothing the `Process` does not (`pid`, and the task slot
   that `Process::thread_id` already records). The row, the proc slot and the sweep go. The one
   job the sweep has left, removing a stale `THREAD_PID_MAP` entry, moves to glue's reap.

## Steps

Each step is one commit and keeps both kernels building and passing.

1. **`finish_process` in glue, called by both.** AArch64's `sys_exit` leader path and
   `return_to_kernel` call it. amd64's `run_process` calls it in place of its inline steps and of
   `spawn_record_exit`'s body.
   - Pin every ordering difference found while diffing, in the commit.
   - The one already known: amd64 marks `exited` inside `spawn_record_exit`, after `close_all`;
     glue's `sys_exit` marks it before.
   - The waiters read the channel, which both set last, so the two orders are equivalent for
     `wait4`. Confirm that and do not assume it.
2. **Group exit on `SignalActions`.**
   - `exit_group_from_thread`, `set_group_exiting`, `group_exiting`, `should_leave_now` and the
     status word read the tgid's shared state.
   - `GROUP_EXIT`/`GROUP_EXIT_STATUS` are deleted, and so is the "clear the old slot's flag"
     work in `bind_child_task` (2026-09-12's `segvgroup` bug; there is nothing per-slot left to
     clear).
3. **`live_count` from the process table.** `drain` waits on the table's answer. `THREADS` shrinks
   to what `run_thread` and the futex wake on exit still need, or goes.
4. **Delete the `SPAWN` table.**
   - `bind_child_task` keeps the `CR3` root and the thread-slot seed.
   - `proc_entry`'s "no slot" check becomes "no registered process", which it already checks next.
   - `after_reap` keeps only the reclaim drain.

## Gates

- Both boot suites at SMP 1/2/4 (amd64 TCG, AArch64 HVF); the amd64 ring-3 harness.
- Process-lifecycle probes on both kernels: `segvgroup`, `thrprobe` (8 × 12 threads),
  `cloneflags`, `futextest`, `lifeprobe`, `forktest`.
- The ssh exit-status harness: 10 sessions each return their own status, and a shell sees
  `exit 1..5`, `SIGTERM` as 143 and a background `wait` as 7.
- Self-host: AArch64 devbox `-j4` and the ryzen amd64 Firecracker build, both `EXIT=0`. Exit
  ordering is what a parallel build exercises hardest.

## Risks

- **Reaping mid-exit.** The channel is the "done" signal because a reap that runs while the child
  is still in `close_all` frees what the child is using (the "One waitpid" section). Any
  reordering in step 1 must keep the channel last.
- **A live address space freed.** `drain`'s whole purpose is that no sibling runs in a space its
  reaper frees ("DRAIN INCOMPLETE"). Step 3 must keep that guarantee, not just the count.
- **Stale identity on a recycled task slot.** The `stale SPAWN row` trace (2026-10-17, ash
  returning to its prompt while `claude` ran on) is the bug class the row created. Deleting the row
  removes the class, but the `THREAD_PID_MAP` removal in step 4 needs the same "only if it still
  names this pid" guard the sweep has.
- **AArch64's group kill is load-bearing for Go and rustc.** `kill_thread_group`'s grace for
  in-kernel siblings came from self-host incidents. Step 2 moves state, not that policy.
