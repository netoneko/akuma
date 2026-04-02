use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::types::{Pid, ProcessState, SignalAction, MAX_SIGNALS};
use crate::process::table::PROCESS_TABLE;
use crate::process::channel::{remove_channel, get_channel};
use crate::process::children::{lookup_process, clear_lazy_regions};
use crate::process::cleanup_process_fds;
use crate::runtime::with_irqs_disabled;
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
    let child_pids: Vec<Pid> = with_irqs_disabled(|| {
        let table = PROCESS_TABLE.lock();
        table
            .iter()
            .filter_map(|(&child_pid, p)| {
                if p.parent_pid == pid {
                    Some(child_pid)
                } else {
                    None
                }
            })
            .collect()
    });
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
    
    // Mark process as killed by SIGKILL
    proc.exited = true;
    proc.exit_code = -9; // negative = killed by signal (SIGKILL)
    proc.state = ProcessState::Zombie(-9);
    
    // Clear lazy region metadata before dropping the process.
    // Without this, the LAZY_REGION_TABLE BTreeMap entry leaks.
    clear_lazy_regions(pid);

    // Unregister from process table and DROP the Box<Process>
    let _dropped_process = crate::process::table::unregister_process(pid);
    
    // Remove and notify the process channel
    if let Some(tid) = thread_id {
        if let Some(channel) = remove_channel(tid) {
            channel.set_exited(-9);
        }

        // Mark the thread as terminated so scheduler stops scheduling it
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

    clear_lazy_regions(pid);
    let _dropped = crate::process::table::unregister_process(pid);

    if let Some(tid) = thread_id {
        if let Some(channel) = remove_channel(tid) {
            channel.set_exited(exit_code);
        }
        threading::mark_thread_terminated(tid);
    }

    Ok(())
}
