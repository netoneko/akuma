# One process exit for both kernels

*2026-11-12, branch `akuma-multiarch-attempt-0`. The last lifecycle piece of the convergence in
[`docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`](../docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md), after
one execve, one spawn, one waitpid and one mm ([`AKUMA_MM_CONVERGENCE.md`](AKUMA_MM_CONVERGENCE.md)).*

## Status

Proposal. No step has started.

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
