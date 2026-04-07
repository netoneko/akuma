use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::types::{Pid, ProcessState, SignalAction, MAX_SIGNALS};
use crate::process::table;
use crate::process::channel::{remove_channel, get_channel};
use crate::process::children::lookup_process;
use crate::process::cleanup_process_fds;
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
    // Kill direct children first so parent-kill semantics cascade and avoid
    // leaving orphaned workers running after the parent exits.
    let child_pids: Vec<Pid> = table::collect_pids(|p| p.parent_pid == pid);
    for child_pid in child_pids {
        if child_pid != pid {
            let _ = kill_process(child_pid);
        }
    }

    // Look up the process
    let proc = lookup_process(pid).ok_or("Process not found")?;

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
    proc.exited = true;
    proc.exit_code = -9;
    proc.state = ProcessState::Zombie(-9);
    proc.thread_id = None; // prevent entry_point_trampoline from matching this zombie

    // Notify the CHILD channel so the parent's wait4 unblocks
    if let Some(ch) = crate::process::get_child_channel(pid) {
        ch.set_exited(-9);
    }

    // Remove and notify the thread channel, terminate the thread
    if let Some(tid) = thread_id {
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
    let proc = lookup_process(pid).ok_or("Process not found")?;
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
    proc.exited = true;
    proc.exit_code = exit_code;
    proc.state = ProcessState::Zombie(exit_code);
    proc.thread_id = None;

    // Do NOT unregister — leave zombie for wait4 to reap.

    // Notify the CHILD channel so the parent's wait4 unblocks
    if let Some(ch) = crate::process::get_child_channel(pid) {
        ch.set_exited(exit_code);
    }

    if let Some(tid) = thread_id {
        if let Some(channel) = remove_channel(tid) {
            channel.set_exited(exit_code);
        }
        threading::mark_thread_terminated(tid);
    }

    Ok(())
}
