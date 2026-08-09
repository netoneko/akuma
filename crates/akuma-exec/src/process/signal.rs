use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::types::{Pid, ProcessState, SignalAction, MAX_SIGNALS};
use crate::process::table;
use crate::process::channel::{remove_channel, get_channel};
use crate::process::children::lookup_process_shared;
use crate::process::cleanup_process_fds;
use crate::process::lifecycle::LifecycleGuard;
use crate::threading;

/// Shared signal action table for CLONE_SIGHAND semantics.
///
/// When threads are created with CLONE_THREAD (pthreads), they share this table
/// via Arc — matching Linux CLONE_SIGHAND behavior. Fork/Spawn creates a fresh table.
pub struct SharedSignalTable {
    pub actions: Spinlock<[SignalAction; MAX_SIGNALS]>,
}

impl SharedSignalTable {
    pub fn new() -> Self {
        Self {
            actions: Spinlock::new([SignalAction::default(); MAX_SIGNALS]),
        }
    }
}

/// Kill a process by PID
///
/// Terminates the process and cleans up all associated resources:
/// - Closes all open sockets and file descriptors
/// - Removes process from process table
/// - Removes process channel
/// - Marks the thread as terminated
///
/// # Arguments
/// * `pid` - Process ID to kill
///
/// # Returns
/// * `Ok(())` if the process was successfully killed
/// * `Err(message)` if the process was not found or could not be killed
pub fn kill_process(pid: Pid) -> Result<(), &'static str> {
    // Serialize against concurrent lifecycle ops — reentrant when called from
    // `return_to_kernel`'s teardown (which already holds the lock). See
    // `process/lifecycle.rs`.
    let _lifecycle = LifecycleGuard::acquire();
    // Kill direct children first so parent-kill semantics cascade and avoid
    // leaving orphaned workers running after the parent exits.
    let child_pids: Vec<Pid> = table::collect_pids(|p| p.parent_pid == pid);
    for child_pid in child_pids {
        if child_pid != pid {
            let _ = kill_process(child_pid);
        }
    }

    // Look up the process
    let proc = lookup_process_shared(pid).ok_or("Process not found")?;

    // Get thread_id before cleanup (needed for channel removal and thread termination).
    // Some synthetic test processes don't have a started thread yet; still allow
    // kill/unregister for those entries.
    let thread_id = proc.thread_id;

    // Set the interrupt flag FIRST - this allows blocked syscalls (like accept())
    // to detect the interrupt and properly abort their sockets before we clean up.
    if let Some(tid) = thread_id {
        if let Some(channel) = get_channel(tid) {
            channel.set_interrupted();
        }

        // Yield a few times to give the blocked thread a chance to detect the interrupt.
        for _ in 0..5 {
            threading::yield_now();
        }
    }

    // Clean up all open FDs for this process
    cleanup_process_fds(proc);

    // Mark process as zombie — do NOT unregister from the table.
    // The parent's wait4 needs to find the zombie to collect exit status.
    // The zombie is reaped by on_thread_cleanup when the thread slot is recycled,
    // or by return_to_kernel if the thread reaches it.
    // (Bug #24 + #31: eager unregister caused ECHILD in wait4)
    table::with_process(pid, |p| {
        p.exited = true;
        p.exit_code = -9;
        p.state = ProcessState::Zombie(-9);
        p.thread_id = None; // prevent entry_point_trampoline from matching this zombie
    });

    // Notify the CHILD channel so the parent's wait4 unblocks, and raise
    // SIGCHLD so a shell parked in `sigsuspend` (busybox ash `wait`) wakes.
    // `publish_child_exit` is a no-op if the child already exited cleanly, so
    // a defensive `kill -9` on an already-reaped zombie does not overwrite the
    // real exit code or raise a duplicate SIGCHLD.
    crate::process::publish_child_exit(pid, -9);

    // Remove and notify the thread channel, terminate the thread.
    //
    // `thread_id` was snapshotted before the five `yield_now()`s and the fd cleanup
    // above, so it can be stale by now: the target may have exited under us, had its
    // slot recycled (~10 ms cooldown) and re-claimed by an unrelated process. Acting
    // on the stale number kills an innocent thread and strands its process with no
    // thread at all. `slot_still_owned_by` re-checks the slot's current owner.
    if let Some(tid) = thread_id
        && slot_still_owned_by(tid, pid)
    {
        if let Some(channel) = remove_channel(tid) {
            channel.set_exited(-9);
        }
        threading::mark_thread_terminated(tid);
    }

    log::debug!("[kill] Killed PID {} (thread {:?})", pid, thread_id);

    Ok(())
}

/// Kill a process with a specific signal number.
/// The exit code is set to -(signal) so encode_wait_status reports the correct signal.
pub fn kill_process_with_signal(pid: Pid, sig: u32) -> Result<(), &'static str> {
    // Serialize against concurrent lifecycle ops — reentrant. See `process/lifecycle.rs`.
    let _lifecycle = LifecycleGuard::acquire();
    let proc = lookup_process_shared(pid).ok_or("Process not found")?;
    let thread_id = proc.thread_id;

    if let Some(tid) = thread_id {
        if let Some(channel) = get_channel(tid) {
            channel.set_interrupted();
        }
        for _ in 0..5 {
            threading::yield_now();
        }
    }

    cleanup_process_fds(proc);

    let exit_code = -(sig as i32);
    table::with_process(pid, |p| {
        p.exited = true;
        p.exit_code = exit_code;
        p.state = ProcessState::Zombie(exit_code);
        p.thread_id = None;
    });

    // Do NOT unregister — leave zombie for wait4 to reap.

    // Notify the CHILD channel so the parent's wait4 unblocks, and raise
    // SIGCHLD (e.g. `kill -9 <child>` from a shell must wake its `wait`).
    crate::process::publish_child_exit(pid, exit_code);

    // Same staleness hazard as `kill_process` — see the comment there.
    if let Some(tid) = thread_id
        && slot_still_owned_by(tid, pid)
    {
        if let Some(channel) = remove_channel(tid) {
            channel.set_exited(exit_code);
        }
        threading::mark_thread_terminated(tid);
    }

    Ok(())
}

/// Does thread slot `tid` still belong to `pid`?
///
/// Thread slots are recycled (`cleanup_terminated_internal`, ~10 ms cooldown), so a
/// `thread_id` snapshotted before any yielding operation can name a slot that has
/// since been handed to an unrelated process. Terminating such a slot kills an
/// innocent thread and leaves its process alive with no thread: unschedulable, unable
/// to exit, never reaped, with its parent's `wait4` blocked forever.
///
/// `THREAD_PID_MAP` records the slot's current owner. Only an entry naming a
/// *different* pid proves the slot was reassigned; a missing entry means nobody has
/// claimed it, and terminating it is still correct (the orphaned-READY-thread case
/// `table::unregister_process` describes).
fn slot_still_owned_by(tid: usize, pid: Pid) -> bool {
    let owner = crate::runtime::with_irqs_disabled(
        || table::THREAD_PID_MAP.lock().get(&tid).copied(),
    );
    match owner {
        Some(owner) if owner != pid => {
            crate::safe_print!(112, "[kill] pid={} stale tid={} now owned by pid={}\n",
                pid, tid, owner);
            false
        }
        _ => true,
    }
}
