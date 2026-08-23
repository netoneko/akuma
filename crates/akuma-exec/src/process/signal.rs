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

    /// A private copy of this table, for `fork`/`vfork`.
    ///
    /// POSIX: **`fork` inherits every signal disposition**; only `execve`
    /// resets caught handlers (see `Process::load_image`, which does that part
    /// correctly). `fork` used to hand the child a `new()` table instead —
    /// all-`Default` — silently un-installing every handler the parent had
    /// registered.
    ///
    /// That is invisible for the common `fork`+`exec` pair, because `exec`
    /// would have reset the handlers anyway, which is why it survived so long.
    /// It bites a process that forks and **stays** in the same image: a
    /// master/worker daemon. nginx installs its `SIGTERM` handler in the master
    /// before forking, so on Akuma the worker's disposition was `Default`, and
    /// the consequences ran in both directions:
    ///
    /// - `current_thread_has_pending_interrupt` only reports an interrupt for a
    ///   `UserFn` handler, so a `Default` disposition never broke the worker
    ///   out of `epoll_pwait` — `SIGTERM` sat pending indefinitely and the
    ///   worker looked immune to `kill`.
    /// - When the syscall did eventually return for an unrelated reason, the
    ///   `Default` action **terminated** the worker instead of running nginx's
    ///   graceful-shutdown handler — killing the in-flight request with it.
    ///
    /// The copy is deep by construction: `SignalAction` is `Copy`, so the child
    /// gets its own array and a later `sigaction` on either side is invisible to
    /// the other, which is exactly what `fork` (as opposed to `CLONE_SIGHAND`)
    /// requires.
    #[must_use]
    pub fn clone_for_fork(&self) -> Self {
        Self {
            actions: Spinlock::new(*self.actions.lock()),
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

/// Deliver `sig` to `pid`'s whole thread group (target + CLONE_THREAD siblings),
/// or hard-kill it if it has no live thread to interrupt. This is `sys_kill`'s
/// per-pid delivery logic, factored out so [`kill_process_group`] can reuse it
/// for each member of a process group instead of duplicating it.
///
/// Returns `true` if the process was found (delivered to a live thread, or
/// hard-killed via the fallback) — `sys_kill` maps that to its 0-vs-`ESRCH`
/// return value.
pub fn deliver_signal(pid: Pid, sig: u32) -> bool {
    let Some(proc) = lookup_process_shared(pid) else {
        return kill_process_with_signal(pid, sig).is_ok();
    };
    let tgid = proc.tgid;
    let l0_phys = proc.address_space.l0_phys();

    // SIGKILL (9) is unconditional — bypass signal delivery entirely. On
    // Linux, SIGKILL cannot be caught or ignored. Hard-kill the thread group.
    if sig == 9 {
        crate::process::kill_thread_group(pid, l0_phys, -9);
        let _ = kill_process_with_signal(pid, 9);
        return true;
    }

    // Collect ALL thread IDs in the group (target + siblings) FIRST.
    // `for_each_process` runs its callback with IRQs disabled, which forbids
    // allocation — a fixed array bounded by `MAX_PROCESSES` (there can never
    // be more live threads than that) sidesteps it instead of a `Vec` that
    // would grow inside the callback.
    let mut all_tids = [0usize; table::MAX_PROCESSES];
    let mut tid_count = 0;
    if let Some(tid) = proc.thread_id {
        all_tids[tid_count] = tid;
        tid_count += 1;
    }
    table::for_each_process(|p| {
        if p.pid != pid && p.tgid == tgid
            && let Some(tid) = p.thread_id
            && tid_count < all_tids.len() {
                all_tids[tid_count] = tid;
                tid_count += 1;
            }
    });
    let all_tids = &all_tids[..tid_count];

    // Set ALL interrupted flags FIRST — before any wake() call. This prevents
    // a race where a thread wakes from schedule_blocking, checks
    // is_current_interrupted() (false — not set yet), and re-enters
    // schedule_blocking before we set the flag.
    for &tid in all_tids {
        crate::process::interrupt_thread(tid);
    }

    // NOW pend signals and wake. pend_signal_for_thread calls wake() internally.
    // The interrupted flag is already set, so when the thread wakes and checks
    // is_current_interrupted(), it sees true.
    for &tid in all_tids {
        threading::pend_signal_for_thread(tid, sig);
    }

    true
}

/// Deliver `sig` to every process whose `.pgid` is `pgid` (POSIX `kill(-pgid,
/// sig)` group semantics).
///
/// `fork`/`clone` propagate `.pgid` from parent to child (a plain
/// non-job-control shell never calls `setpgid`, so it and everything it execs
/// share one `pgid`), so this reaches a shell's foreground child without
/// needing to track "whichever pid is currently in the foreground" — the
/// mechanism a terminal's INTR character (Ctrl-C) needs. See
/// `write_to_process_stdin`'s ISIG handling and
/// `docs/archive/CTRL_C_SIGINT_DELIVERY.md`.
///
/// Deliberately excludes the group **leader** (`p.pid == pgid` — by
/// construction, in every case this is called for, that's the interactive
/// login shell itself: `spawn_process_with_channel_ext` sets a freshly
/// spawned top-level process's own `.pgid` to its own `pid`,
/// `crates/akuma-exec/src/process/image.rs:296`). Live-tested 2026-08-24:
/// broadcasting to the leader too killed the shell (and the whole SSH
/// session with it) on roughly 2 of 3 runs — real Unix relies on the shell
/// protecting itself with `SIG_IGN`, and something in that path is not
/// reliable here (`SIG_IGN` is correctly stored and honored, and `fork`/
/// `vfork` both correctly give the child a private `signal_actions` copy —
/// see `deliver_signal`'s and `SharedSignalTable::clone_for_fork`'s doc
/// comments — so the exact mechanism is still unattributed; this exclusion
/// sidesteps it rather than fixes it). Excluding the leader is safe for
/// every caller of this function today: it exists solely for the terminal's
/// INTR-character handling in `write_to_process_stdin`, not as a general
/// `kill(-pgid, sig)` primitive, and the thing that should die on Ctrl-C is
/// the foreground job, never the shell running it.
pub fn kill_process_group(pgid: Pid, sig: u32) {
    // Fixed array, not a `Vec`: `for_each_process`'s callback runs with IRQs
    // disabled and forbids allocation, and there can never be more than
    // `MAX_PROCESSES` matches.
    let mut targets = [0 as Pid; table::MAX_PROCESSES];
    let mut count = 0;
    table::for_each_process(|p| {
        if p.pgid == pgid && p.pid != pgid && count < targets.len() {
            targets[count] = p.pid;
            count += 1;
        }
    });
    for &pid in &targets[..count] {
        deliver_signal(pid, sig);
    }
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

/// `fork` inherits signal dispositions; `CLONE_SIGHAND` shares them.
///
/// The regression is the difference between the two. A `fork` child must get a
/// *copy* it can diverge from, and it must not start life with every handler
/// un-installed — which is what a fresh `SharedSignalTable::new()` gave it, and
/// what made nginx's worker unkillable by `SIGTERM` while parked in
/// `epoll_pwait` (`docs/archive/FORK_LOSES_SIGNAL_HANDLERS.md`).
#[cfg(test)]
mod fork_signal_inheritance_tests {
    use super::SharedSignalTable;
    use crate::process::types::{SignalAction, SignalHandler};

    /// SIGTERM is signal 15, and `actions` is indexed by `sig - 1`.
    const SIGTERM_IDX: usize = 14;

    fn table_with_sigterm_handler() -> SharedSignalTable {
        let table = SharedSignalTable::new();
        table.actions.lock()[SIGTERM_IDX] = SignalAction {
            handler: SignalHandler::UserFn(0x1234),
            flags: 0x4,
            mask: 0,
            restorer: 0x5678,
        };
        table
    }

    #[test]
    fn a_fork_child_inherits_every_disposition() {
        let parent = table_with_sigterm_handler();
        let child = parent.clone_for_fork();
        let action = child.actions.lock()[SIGTERM_IDX];
        assert!(
            matches!(action.handler, SignalHandler::UserFn(0x1234)),
            "fork must carry the parent's handler over, not reset it to Default"
        );
        assert_eq!(action.flags, 0x4, "flags travel with the handler");
        assert_eq!(action.restorer, 0x5678);
    }

    /// The other half: it is a *copy*, so `sigaction` on either side after the
    /// fork is invisible to the other. Sharing is `CLONE_SIGHAND`'s job, and
    /// that path passes the parent's `Arc` instead of calling this.
    #[test]
    fn the_copy_is_private_to_the_child() {
        let parent = table_with_sigterm_handler();
        let child = parent.clone_for_fork();

        child.actions.lock()[SIGTERM_IDX] = SignalAction::default();
        assert!(
            matches!(parent.actions.lock()[SIGTERM_IDX].handler, SignalHandler::UserFn(_)),
            "the child resetting its handler must not touch the parent's"
        );

        parent.actions.lock()[SIGTERM_IDX] = SignalAction {
            handler: SignalHandler::Ignore,
            ..SignalAction::default()
        };
        assert!(
            matches!(child.actions.lock()[SIGTERM_IDX].handler, SignalHandler::Default),
            "and the parent changing its own must not touch the child's"
        );
    }

    /// Untouched signals stay `Default` — the copy must not invent anything.
    #[test]
    fn untouched_signals_stay_default() {
        let child = table_with_sigterm_handler().clone_for_fork();
        let actions = child.actions.lock();
        for (i, action) in actions.iter().enumerate() {
            if i == SIGTERM_IDX {
                continue;
            }
            assert!(matches!(action.handler, SignalHandler::Default), "signal {} changed", i + 1);
        }
    }
}
