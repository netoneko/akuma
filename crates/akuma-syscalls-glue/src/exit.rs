//! The end of a process, the same on both kernels.
//!
//! [`finish_process`] is what each kernel does once per process after the group's other threads
//! are gone and its writable shared mappings are released. Those two steps stay with the
//! caller because their place in the sequence is the architecture's:
//!
//! - AArch64 (`proc::sys_exit_group`) releases the mappings *before* `kill_thread_group`, which
//!   tears the address space down.
//! - amd64 (`run_process`) releases them *after* `thread::drain`, so no sibling can fault a page
//!   back in behind the release.
//!
//! How the calling thread then leaves the CPU is also the caller's: AArch64 marks its thread
//! terminated and parks, amd64's task returns to the scheduler.
//!
//! `proposals/AKUMA_ONE_EXIT.md` step 1.

use akuma_exec::process::{Pid, Process, ProcessState};
use core::sync::atomic::Ordering;

/// Close the process's descriptors, mark it exited, hand its children to init, and tell its
/// parent. In that order, and the order is load-bearing:
///
/// 1. **Descriptors first.** A pipe end released here is what lets a peer see EOF or `EPIPE`
///    (`yes | head -n 1`). Releasing them after the publish would let the parent reap the
///    process while its fds are still charged to the shared table.
/// 2. **`exited`, `exit_code`, `Zombie`.** `/proc`, `ps` and `cleanup_process_fds`'s sharer
///    count read these; `wait4` does not.
/// 3. **Reparent every member's children to init**, in both views of parenthood
///    (`reparent_children_to`). A child records the *spawning thread's* pid as its parent, so
///    `members` must be the whole group, collected before the caller tore the siblings down.
///    Done before the publish, as Linux's `exit_notify` does, so a parent woken by the publish
///    never races a reparent of its grandchildren.
/// 4. **Publish the exit last.** `publish_child_exit` marks the child channel exited, which is
///    the one thing glue's `wait4`/`waitpid`/`waitid` reap on (`has_exited`), and then raises
///    `SIGCHLD`. A reap therefore never runs while any step above is in progress. A non-leader
///    calling `exit_group` publishes the leader's channel too, which is the one the parent
///    waits on.
/// 5. Complete a `vfork`: wake the parent parked on this child, if any.
///
/// Idempotent: a second call finds an empty table, an already-exited channel (no second
/// `SIGCHLD`) and no children to move.
pub fn finish_process(proc: &Process, members: &[Pid], code: i32) {
    proc.fds.close_all();
    proc.exit_code.store(code, Ordering::Release);
    proc.exited.store(true, Ordering::Release);
    proc.state.store(ProcessState::Zombie(code));
    for &member in members {
        let _ = akuma_exec::process::reparent_children_to(member, 1);
    }
    akuma_exec::process::publish_child_exit(proc.pid, code);
    if proc.tgid != proc.pid {
        akuma_exec::process::publish_child_exit(proc.tgid, code);
    }
    crate::proc::vfork_complete(proc.pid);
}

/// Every pid in `proc`'s thread group, the members [`finish_process`] reparents for.
///
/// Call it before the siblings are torn down: a retired row is invisible to the table walk.
#[must_use]
pub fn group_members(proc: &Process) -> alloc::vec::Vec<Pid> {
    let tgid = proc.tgid;
    akuma_exec::process::collect_pids(|q| q.tgid == tgid)
}
