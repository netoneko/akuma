use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, ProcessInfo, PROCESS_INFO_ADDR, LazyRegion, LazySource, MmapRegion, ProcessInfo2, ProcessState};
use crate::process::channel::{ProcessChannel, get_channel};
use crate::process::table::{THREAD_PID_MAP, find_process};
use crate::runtime::{with_irqs_disabled, runtime, PhysFrame};
use akuma_terminal as terminal;

/// Registry mapping child PIDs to (ProcessChannel, parent_pid)
/// Used by parent processes to read child stdout via ChildStdout FD
/// and by wait4(-1) to find children of a specific parent.
static CHILD_CHANNELS: Spinlock<BTreeMap<Pid, (Arc<ProcessChannel>, Pid)>> =
    Spinlock::new(BTreeMap::new());

/// Register a child process channel (called when spawning via syscall)
pub fn register_child_channel(child_pid: Pid, channel: Arc<ProcessChannel>, parent_pid: Pid) {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().insert(child_pid, (channel, parent_pid));
    })
}

/// Get a child process channel by PID
pub fn get_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).map(|(ch, _)| ch.clone())
    })
}

/// True when `child_pid` is registered as a child of the thread group `waiter_tgid`.
///
/// The registered parent is the pid of whichever thread called fork/clone; a
/// multithreaded parent (e.g. the Go runtime) may wait from a *different* thread
/// of the same group, so the comparison is by thread group, not raw pid. Linux
/// `wait*` on a process that is not your child fails with ECHILD — the wait4 /
/// waitid paths use this to enforce that. Notably Go's os/exec pidfd probe
/// calls `waitid(P_PIDFD, <pidfd of itself>)` and *requires* ECHILD; blocking
/// on a non-child instead deadlocks the caller against its own exit.
pub fn is_child_of_group(child_pid: Pid, waiter_tgid: Pid) -> bool {
    let ppid = with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().get(&child_pid).map(|(_, ppid)| *ppid)
    });
    let Some(ppid) = ppid else { return false };
    if ppid == waiter_tgid {
        return true;
    }
    // The recorded parent may be a non-leader thread; resolve its thread group.
    find_process(|p| if p.pid == ppid { Some(p.tgid) } else { None })
        .is_some_and(|tgid| tgid == waiter_tgid)
}

/// The pid recorded as `child_pid`'s parent at fork time, if it is still a
/// registered child. This is the *forking thread's* pid, which may be a
/// non-leader thread of a multithreaded parent (e.g. the Go runtime's M
/// threads) — resolve its thread group before using it as a signal target
/// (see [`sigchld_target_thread`]).
pub fn parent_pid_of(child_pid: Pid) -> Option<Pid> {
    with_irqs_disabled(|| CHILD_CHANNELS.lock().get(&child_pid).map(|(_, ppid)| *ppid))
}

/// Pick which thread of group `tgid` receives a process-directed signal.
///
/// Linux delivers a process-directed signal to any thread that does not block
/// it. We approximate that with an explicit preference order, because two of
/// our delivery-path guards silently drop a signal aimed at the wrong thread:
///
///   1. a thread not blocking SIGCHLD **and** with a sigaltstack configured —
///      preferred because `try_deliver_signal` (src/exceptions.rs) re-pends a
///      signal whose handler is `SA_ONSTACK` when the target thread's
///      `alt_sp == 0` (a Go M that has not reached `mstart`'s `sigaltstack`
///      call); targeting such a thread would re-pend SIGCHLD at every syscall
///      return forever and never deliver;
///   2. any thread not blocking SIGCHLD;
///   3. the thread-group leader (the blocked-signal fallback — Linux would pick
///      *some* thread and leave the bit pending; we pin it to the leader so a
///      blocked SIGCHLD stays pending on a real thread of the group until the
///      mask is cleared).
fn sigchld_target_thread(tgid: Pid) -> Option<usize> {
    const SIGCHLD_BIT: u64 = 1u64 << (17 - 1);
    let mut best_with_altstack: Option<usize> = None;
    let mut best_unblocked: Option<usize> = None;
    let mut leader_tid: Option<usize> = None;

    crate::process::table::for_each_process(|p| {
        if p.tgid != tgid { return; }
        let Some(tid) = p.thread_id else { return; };
        if p.pid == tgid { leader_tid = Some(tid); }
        let blocks_sigchld = crate::threading::thread_signal_mask_of(tid) & SIGCHLD_BIT != 0;
        if !blocks_sigchld {
            if best_unblocked.is_none() { best_unblocked = Some(tid); }
            let (sp, _size, flags) = crate::threading::get_sigaltstack(tid);
            if sp != 0 && flags != 2 /* SS_DISABLE */ && best_with_altstack.is_none() {
                best_with_altstack = Some(tid);
            }
        }
    });

    best_with_altstack.or(best_unblocked).or(leader_tid)
}

/// Raise SIGCHLD on the parent of `child_pid`, if it has a live parent process.
///
/// MUST be called *after* the child's channel is marked exited (see
/// [`publish_child_exit`]): shells respond to the handler by calling
/// `waitpid(WNOHANG)`, which has to already see the zombie or the shell
/// concludes nothing happened and re-suspends. The ordering is structural in
/// [`publish_child_exit`]; callers that invoke this directly must honour it.
///
/// Records `exit_code` in the per-thread SIGCHLD siginfo side-channel so an
/// `SA_SIGINFO` handler reads a real `si_pid`/`si_status` instead of zeros.
///
/// Never sets the interrupted flag (`interrupt_thread`) — that is Ctrl+C's
/// channel and would turn every child exit into a spurious `EINTR` storm across
/// the parent's unrelated blocking syscalls. Pending + wake is enough: the
/// pending bit is what `sys_rt_sigsuspend` polls, and delivery happens at the
/// parent's next syscall-return boundary.
pub fn raise_sigchld_for_parent(child_pid: Pid, exit_code: i32) {
    const SIGCHLD: u32 = 17;
    let Some(ppid) = parent_pid_of(child_pid) else { return };
    // The forking thread may be a non-leader of a multithreaded parent; resolve
    // its thread group so we can target any of its threads.
    let Some(tgid) = find_process(|p| if p.pid == ppid { Some(p.tgid) } else { None }) else { return };
    // Kernel-thread parents (e.g. the in-kernel sshd bridge) have no live child
    // entry and no userspace to signal — silently skip.
    if let Some(tid) = sigchld_target_thread(tgid) {
        crate::threading::set_last_sigchld(tid, child_pid, exit_code);
        crate::threading::pend_signal_for_thread(tid, SIGCHLD);
    }
}

/// Publish a child's exit atomically: mark the child channel exited (waking any
/// `wait4`/pidfd pollers) and then raise SIGCHLD on the parent, **iff this call
/// is the one that published the exit**.
///
/// Ordering is load-bearing: the channel MUST be marked exited before SIGCHLD
/// is pended, because the parent's SIGCHLD handler immediately re-polls with
/// `waitpid(WNOHANG)` and must observe the zombie. Routing every exit site
/// through this helper makes that ordering structural instead of a comment
/// repeated at each call site.
///
/// SIGCHLD is raised only on the first publish — a child that exits cleanly and
/// is then torn down (`return_to_kernel` / `kill_child_processes`) must not
/// raise two SIGCHLDs for one death. A duplicate is harmless for shells (which
/// re-poll with `WNOHANG` and get `ECHILD`/0) but would confuse a
/// signal-counting handler, so the guard is worth keeping.
pub fn publish_child_exit(child_pid: Pid, exit_code: i32) {
    let published = match get_child_channel(child_pid) {
        Some(ch) if !ch.has_exited() => {
            ch.set_exited(exit_code);
            true
        }
        _ => false,
    };
    if published {
        raise_sigchld_for_parent(child_pid, exit_code);
    }
}

/// Remove a child process channel (called when the parent CLOSES its
/// `ChildStdout` read fd, or on `execve`/teardown of the reading process).
pub fn remove_child_channel(child_pid: Pid) -> Option<Arc<ProcessChannel>> {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().remove(&child_pid).map(|(ch, _)| ch)
    })
}

/// Reap a child's channel on the `wait*` (waitpid/wait4/waitid) path.
///
/// This is distinct from [`remove_child_channel`], which fires when the parent
/// closes its `ChildStdout` read fd. Reaping a zombie must NOT discard stdout the
/// child wrote right before exiting: the parent's `ChildStdout` fd resolves the
/// channel by pid via [`get_child_channel`] on every read, so if `wait*` removed
/// the channel the instant it reaped, a parent that reads stdout *after*
/// observing the exit would find it gone (EBADF) and lose all buffered output.
///
/// That is exactly the sshd interactive bridge: it checks `waitpid` first, then
/// drains the child's stdout. A fully-buffered shell (busybox flushes stdio at
/// `_exit`) loses everything; an unbuffered one (toybox) loses only its final
/// pre-exit write. So here we only drop the channel if its stdout buffer is
/// already empty; otherwise we keep it and let the parent's `close()` (or process
/// teardown) remove it via [`remove_child_channel`] once drained.
///
/// Race-free: the child is confirmed exited before reaping, so no further writes
/// can arrive — an empty buffer stays empty, and a non-empty one only shrinks as
/// the reader drains it. Returns `true` if the channel was removed, `false` if it
/// was kept (data still buffered) or was absent.
pub fn reap_child_channel(child_pid: Pid) -> bool {
    with_irqs_disabled(|| {
        let mut map = CHILD_CHANNELS.lock();
        let has_data = matches!(map.get(&child_pid), Some((ch, _)) if ch.has_stdout_data());
        if has_data {
            false
        } else {
            map.remove(&child_pid).is_some()
        }
    })
}

/// Find any exited child of the given parent. Returns (child_pid, channel).
pub fn find_exited_child(parent_pid: Pid) -> Option<(Pid, Arc<ProcessChannel>)> {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (&child_pid, (ch, ppid)) in channels.iter() {
            if *ppid == parent_pid && ch.has_exited() {
                return Some((child_pid, ch.clone()));
            }
        }
        None
    })
}

/// Register `poller_tid` as a poller on every child channel of `parent_pid`.
/// When any child exits, `set_exited()` wakes the poller.
pub fn add_poller_to_all_children(parent_pid: Pid, poller_tid: usize) {
    with_irqs_disabled(|| {
        let channels = CHILD_CHANNELS.lock();
        for (ch, ppid) in channels.values() {
            if *ppid == parent_pid {
                ch.add_poller(poller_tid);
            }
        }
    })
}

/// Check if the given parent has any children registered.
pub fn has_children(parent_pid: Pid) -> bool {
    with_irqs_disabled(|| {
        CHILD_CHANNELS.lock().values().any(|(_, ppid)| *ppid == parent_pid)
    })
}

/// Get channel for the current thread (used by syscall handlers)
pub fn current_channel() -> Option<Arc<ProcessChannel>> {
    if let Some(proc) = current_process_shared() {
        if let Some(ref ch) = proc.channel {
            return Some(ch.clone());
        }
    }
    
    // Fallback to thread-ID based lookup for legacy system threads
    let thread_id = crate::threading::current_thread_id();
    get_channel(thread_id)
}

/// Check if the current process has been interrupted (Ctrl+C)
///
/// Called by syscall handlers to detect interrupt signal.
/// Returns true if the process should terminate.
pub fn is_current_interrupted() -> bool {
    // Borrowed read: this runs on every syscall (handle_syscall prologue), and
    // `current_process_shared` is an identity-cache hit, so the whole check is
    // a couple of loads. The old shape cloned the channel `Arc` here.
    if let Some(proc) = current_process_shared() {
        if let Some(ref ch) = proc.channel {
            return ch.is_interrupted();
        }
        // No channel on the process → legacy kernel-thread fallback below.
    }
    let thread_id = crate::threading::current_thread_id();
    get_channel(thread_id).map(|ch| ch.is_interrupted()).unwrap_or(false)
}

/// Interrupt a process by thread ID
///
/// Used by the SSH shell to send Ctrl+C signal to a running process.
pub fn interrupt_thread(thread_id: usize) {
    if let Some(channel) = get_channel(thread_id) {
        channel.set_interrupted();
    }
}

/// Does the *current thread* have a pending signal that must abort a blocking
/// syscall with `EINTR`?
///
/// This is the per-thread counterpart to [`is_current_interrupted`], which reads
/// `ProcessChannel::is_interrupted` — a flag set only by Ctrl-C and `sys_kill`.
/// That makes it structurally blind to `tkill`/`tgkill` (i.e. `pthread_kill`),
/// which pend a signal on one thread slot and wake it. Without this check a woken
/// thread re-tests its loop predicate, still sees "not interrupted", and blocks
/// again — so `pthread_kill` could never interrupt a blocking syscall.
///
/// Linux reports `EINTR` when a signal is actually *delivered*: pending, not
/// blocked, and carrying a userspace handler. Two deliberate exclusions:
///
/// - **Blocked signals** stay pending silently and interrupt nothing.
/// - **`SA_RESTART` handlers** get the syscall transparently restarted instead.
///   Every caller of this helper is a "retry until the predicate holds" loop, so
///   *not reporting* the interrupt makes the loop take another pass — which is
///   exactly a restart. (Go installs its SIGURG preemption handler with
///   `SA_RESTART`; reporting `EINTR` for it would break every blocking syscall a
///   Go program makes.)
///
/// A `Default`-disposition signal needs no check here: `sys_tkill` applies a
/// fatal default action inline and only pends it when blocked — and blocked is
/// already excluded above.
///
/// The motivating case is jobserver-rs's `Helper::join`, which sends SIGUSR1 with
/// `SA_SIGINFO` and *no* `SA_RESTART` specifically to break its helper thread out
/// of a blocking pipe `read`; every rustc that reaches codegen leaks that thread
/// otherwise.
pub fn current_thread_has_pending_interrupt() -> bool {
    // Hot path: one relaxed-ish atomic load for the overwhelmingly common
    // "nothing pending" case, before any lookup or lock.
    let tid = crate::threading::current_thread_id();
    // Two sources, and the second is why `pthread_kill` can interrupt a blocking
    // syscall at all under a fast signal source:
    //
    //   pending    not yet delivered — the ordinary case.
    //   delivered  ALREADY delivered since this syscall began. `take_pending_signal`
    //              cleared the pending bit at the return-to-EL0 path, so without
    //              this the blocking loop has no way to learn it was interrupted.
    //              The deliver -> handler -> rt_sigreturn -> deliver chain can stay
    //              saturated indefinitely, and the loop never gets a look in.
    //              See `threading::DELIVERED_SIGNALS` and
    //              docs/archive/PTHREAD_KILL_EINTR_DELIVERY_STARVATION.md.
    let pending = crate::threading::pending_signals_raw(tid);
    let delivered = crate::threading::delivered_signals_raw(tid);
    if pending | delivered == 0 {
        return false;
    }
    let deliverable = (pending | delivered) & !crate::threading::thread_signal_mask();
    if deliverable == 0 {
        return false;
    }

    let Some(pid) = read_current_pid() else { return false };
    let Some(proc) = crate::process::lookup_process_shared(pid) else { return false };

    /// AArch64 Linux `SA_RESTART`.
    const SA_RESTART: u64 = 0x1000_0000;

    let actions = proc.signal_actions.actions.lock();
    let mut bits = deliverable;
    while bits != 0 {
        // Bit `i` is signal `i + 1`, and `actions` is indexed by `sig - 1`, so
        // the bit index indexes `actions` directly.
        let idx = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        if idx >= crate::process::types::MAX_SIGNALS {
            continue;
        }
        let action = actions[idx];
        if matches!(action.handler, crate::process::SignalHandler::UserFn(_))
            && action.flags & SA_RESTART == 0
        {
            // Consume the delivered record for THIS signal only, and only once
            // we have decided it produces an EINTR — otherwise a single delivery
            // could interrupt several later syscalls in a row. `pending` bits are
            // left alone: the delivery path still owns those.
            crate::threading::consume_delivered_signals(tid, 1u64 << idx);
            return true;
        }
    }
    false
}

/// Should the current blocking syscall give up and return `EINTR`?
///
/// Combines the process-wide Ctrl-C / `sys_kill` path
/// ([`is_current_interrupted`]) with the per-thread `pthread_kill` path
/// ([`current_thread_has_pending_interrupt`]). Blocking loops should call this
/// rather than either half.
pub fn should_interrupt_blocking_syscall() -> bool {
    if is_current_interrupted() {
        return true;
    }
    crate::runtime::config().pthread_kill_eintr_enabled
        && current_thread_has_pending_interrupt()
}

/// Count of `read_current_pid` tgid resolutions that fell back to the thread's own pid
/// because the process table would not resolve the mapped pid. Non-zero is the signature
/// of the identity-degradation window described at the fallback site.
pub static TGID_RESOLVE_MISSES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Read the current process PID from the process info page.
///
/// During a syscall, TTBR0 is still set to the user's page tables, so reading
/// from `PROCESS_INFO_ADDR` gives us the calling process's PID. This prevents PID
/// spoofing since the page is read-only for userspace.
///
/// Returns `None` when there is no user process context: TTBR0 is the boot page
/// tables, or the live TTBR0 has no process info page mapped (a bare
/// `UserAddressSpace`), or the page says pid 0.
pub fn read_current_pid() -> Option<Pid> {
    // Identity cache fast path: one validated slot-state load replaces the
    // lock + map walk + tgid table-scan this function used to pay on every
    // resolution (several per syscall — see `table::THREAD_IDENTITY`).
    if let Some((tgid, _)) = crate::process::table::current_thread_tgid_process() {
        return Some(tgid);
    }
    // vfork fast-path: a shared-AS child reads the *parent's* PROCESS_INFO page,
    // so the page no longer uniquely identifies the caller.  THREAD_PID_MAP is
    // authoritative for every user thread; resolve it to the owning process's
    // tgid.  This is behavior-preserving for normal threads (page pid == tgid
    // leader, so callers including getpid see the same value) and gives a vfork
    // child its own pid (its tgid == its pid).  Gated so toggling the fast-path
    // off restores the exact prior page-only behavior.
    if crate::runtime::config().vfork_fastpath_enabled {
        let tid = crate::threading::current_thread_id();
        let mapped = with_irqs_disabled(|| THREAD_PID_MAP.lock().get(&tid).copied());
        if let Some(pid) = mapped {
            return Some(match crate::process::table::with_process(pid, |p| p.tgid) {
                Some(tgid) => tgid,
                // The map named a pid the process table will not resolve. `with_process`
                // only matches slots in state ACTIVE, so this is the window between
                // `unregister_process`'s ACTIVE→RETIRED CAS and the thread's last
                // instruction — the identity silently degrades from tgid to the thread's
                // OWN pid for whatever runs in it.
                //
                // That matters most for `futex_key_tgid`: a non-leader thread degrading
                // here parks on key `(own_pid, uaddr)` while every waker publishes to
                // `(tgid, uaddr)`, which is a lost wakeup that only affects multi-threaded
                // processes. Counted and logged (first 10) rather than silently absorbed —
                // an earlier pass wrongly blamed the sibling `unwrap_or(0)` for this class
                // without ever confirming it fired.
                None => {
                    let n = TGID_RESOLVE_MISSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                    if n <= 10 {
                        (runtime().print_str)(
                            "[identity] WARNING: THREAD_PID_MAP pid not ACTIVE in process table \
                             — tgid degraded to own pid (futex keys may not match wakers)\n");
                    }
                    pid
                }
            });
        }
        // No THREAD_PID_MAP entry → fall through to the page read below
        // (early boot, or a thread not yet registered).
    }
    // CRITICAL: Check TTBR0 before reading from user address space!
    //
    // PROCESS_INFO_ADDR (0x1000) is only mapped in USER page tables.
    // With boot TTBR0, address 0x1000 is in the device memory region (0x0-0x40000000)
    // and reading from it returns garbage, causing FAR=0x5 crashes.
    let ttbr0: u64;
    #[cfg(target_os = "none")]
    unsafe {
        core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
    }
    #[cfg(not(target_os = "none"))]
    { ttbr0 = 0; }
    
    // Compare against actual boot TTBR0, not a range check.
    // User page tables are allocated from the same physical memory pool,
    // so they can have addresses in the same range as boot tables.
    let boot_ttbr0 = crate::mmu::get_boot_ttbr0();
    let ttbr0_addr = ttbr0 & 0x0000_FFFF_FFFF_FFFF; // Mask off ASID bits
    if ttbr0_addr == boot_ttbr0 {
        return None; // Boot TTBR0 - no user process context
    }

    // "Not the boot address space" is NOT "there is a process here". A bare
    // `UserAddressSpace::new()` — which several boot tests construct and
    // `activate()` — is a non-boot TTBR0 with nothing mapped at 0x1000, so the
    // check above passes and the read below is a wild EL1 access that wedges the
    // VM with no output (docs/archive/USER_COPY_FOLD.md §7; the first version of
    // `test_kernel_va_rejected_as_user_pointer` died exactly this way, reaching
    // here through `address_space_owner_pid_for_fault`'s fallback).
    //
    // So ask the page tables directly, which is the question the `unsafe` below
    // actually depends on. §7 proposed `owner_pid_for_l0_phys` ("is this L0 owned
    // by a live process") instead; that answers a strictly different question —
    // identity, not mappedness — and costs an O(MAX_PROCESSES) table scan on a
    // path kernel threads take repeatedly, where this costs a four-level walk.
    // It also would not have caught anything this misses: an L0 with no process
    // info page mapped is exactly the case that wedges.
    //
    // The AP-gated predicate rather than the presence one because this is a read
    // *of user memory from EL1* — the same question `validate_user_range` asks of
    // any syscall buffer. Reaching it through `user_access` would recurse
    // (`validate_user_range` → `address_space_owner_pid_for_fault` → here), hence
    // the raw `mmu` call. The page is mapped `user_flags::RO` (`AP_RO_ALL`, EL0
    // bit set) at every site that maps it — `image.rs` on exec, `mod.rs` on fork
    // and on the post-CoW re-map — so it passes.
    if !crate::mmu::is_current_user_range_mapped(
        PROCESS_INFO_ADDR,
        core::mem::size_of::<ProcessInfo>(),
    ) {
        return None;
    }

    // Read from the fixed address in the current address space
    // SAFETY: checked directly above — PROCESS_INFO_ADDR is mapped and EL0-accessible
    // in the live TTBR0, so this read cannot fault.
    let pid = unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid };
    if pid == 0 { None } else { Some(pid) }
}

// `lookup_process(pid) -> Option<&'static mut Process>` is GONE (Phase 7e
// "Access" half, 2026-08-01): two cores could each materialize `&'static mut`
// to the same `Process` (aliasing UB), and the `'static` lifetime structurally
// outlives the RETIRED→FREE deferred reclamation the "Free" half introduced.
// Use [`lookup_process_shared`] for reads and `&self` methods,
// `table::with_process` for short IRQ-masked field writes, and
// `table::with_process_exclusive` (unsafe, enumerated call sites only) for the
// execve/first-run lifecycle windows Phase 7f owns.

/// Look up a process by PID, returning a **shared** `&'static Process`.
///
/// The shared-kernel-SMP (M5b) BKL-free page-fault path uses this instead of
/// [`lookup_process`] so two cores faulting in different address spaces don't both
/// materialize `&'static mut` to the same object (aliasing UB). Every address-space
/// mutation the fault path needs is a `&self` method (`track_user_frame`,
/// `track_page_table_frame`, `vm_with_regions`, `with_as_locked`) or a free function
/// (`mmu::map_user_page*`); the actual cross-core mutual exclusion on the raw
/// page-table writes comes from [`Process::as_lock`], not from `&mut` exclusivity.
///
/// # Safety warning
/// Same lifetime caveat as [`lookup_process`]: valid only while the process stays
/// registered. The fault fast path only ever looks up its **own** live thread-group
/// leader (`as_owner`), which cannot be freed while the faulting thread runs, so the
/// reference is sound there. Foreign-PID lookups must stay on the BKL slow path.
pub fn lookup_process_shared(pid: Pid) -> Option<&'static Process> {
    let ptr = crate::process::table::get_process_ptr(pid)?;
    crate::process::diag::borrow_inc(pid);
    Some(unsafe { &*ptr })
}

/// Outcome of [`fault_slot_acquire`] — how the per-page demand-paging slot was won.
pub enum FaultSlot {
    /// No address-space-owner process is registered; caller skips serialization.
    NoProc,
    /// Slot was free and acquired cleanly.
    Acquired,
    /// **This thread already held the slot** for this page — a re-entrant acquire.
    ///
    /// Unreachable in this tree (all three call sites are mutually exclusive
    /// branches of `rust_sync_el0_handler_inner`, which cannot re-enter itself: a
    /// fault taken while it runs comes from EL1 and takes the EL1 vector, and
    /// nothing on that path touches a fault slot). It exists to make the outcome
    /// *nameable*, because the alternative was silently wrong: this used to return
    /// [`FaultSlot::Acquired`], so the inner RAII guard's release removed the
    /// **outer** guard's entry and the page ran unserialized for the rest of the
    /// outer critical section, with the outer release a no-op.
    ///
    /// The caller must **not** pair this with a [`fault_slot_release`] — the
    /// outermost holder still owns the release. See `COW_PILE_AUDIT.md` §9 F6.
    AlreadyHeld,
    /// Slot was reclaimed from a holder thread that had already died
    /// (TERMINATED/FREE) without releasing it — the root-cause poison recovery.
    /// Carries the dead holder's thread id.
    ReclaimedDead(usize),
    /// Slot was force-reclaimed after spinning past the safety bound: the holder
    /// neither released nor visibly died (wedged, or its slot was recycled to a
    /// live thread). Carries the stale holder id. Should be vanishingly rare.
    ReclaimedWedged(usize),
}

/// Why a [`FaultSlot`] acquisition had to take the slot away from its recorded
/// holder. Carried out of [`FaultSlot::reclaim_report`] so the caller can pick a
/// message without re-matching the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimCause {
    /// The holder thread had already died without releasing — the root-cause
    /// poison recovery, and the smoking gun for a build-script deadlock.
    Dead,
    /// The holder neither released nor visibly died within
    /// [`FAULT_SLOT_SPIN_BOUND`]; the bounded fallback fired.
    Wedged,
}

impl FaultSlot {
    /// The decision behind the reclaim trace: `Some((cause, holder_tid))` for the
    /// two reclaim outcomes, `None` for the hot `Acquired` / `NoProc` paths that
    /// must print nothing.
    ///
    /// Split out from the caller's `safe_print!` so the *decision* is
    /// host-testable while the *effect* stays on the fault path — the same shape
    /// as `trace_snippet` in `process/channel.rs`. Callers must keep the silent
    /// arm silent: this runs on every demand-paging fault, and a print here is a
    /// console write per page.
    #[must_use]
    pub fn reclaim_report(&self) -> Option<(ReclaimCause, usize)> {
        match *self {
            FaultSlot::ReclaimedDead(holder) => Some((ReclaimCause::Dead, holder)),
            FaultSlot::ReclaimedWedged(holder) => Some((ReclaimCause::Wedged, holder)),
            FaultSlot::NoProc | FaultSlot::Acquired | FaultSlot::AlreadyHeld => None,
        }
    }
}

/// Spin bound before [`fault_slot_acquire`] force-reclaims a slot. Generous: any
/// legitimate concurrent demand-paging of the same page completes in well under
/// this many cooperative yields; reaching it means the holder is wedged.
const FAULT_SLOT_SPIN_BOUND: u32 = 200_000;

/// Acquire the per-page demand-paging serialization slot for `page_va` on the
/// address-space-owner process `as_owner`, recording the calling thread as the
/// holder. Serializes concurrent faults on the same page across CLONE_VM threads
/// (the leader holds the shared `fault_mutex`).
///
/// Unlike the previous raw `BTreeSet` spin-loop, this can never deadlock: if the
/// recorded holder thread has died (its RAII release guard never ran because a
/// kernel thread teardown abandons the stack rather than unwinding), a sibling
/// reclaims the slot instead of spinning forever. A bounded fallback also covers
/// a wedged or slot-recycled holder.
///
/// The caller MUST pair a successful (`Acquired`/`Reclaimed*`) return with exactly
/// one [`fault_slot_release`] — normally via an RAII guard. [`FaultSlot::NoProc`]
/// may be released harmlessly (the release is holder-gated), but
/// [`FaultSlot::AlreadyHeld`] must **not** be: the outer holder owns that entry,
/// and releasing it here is exactly the bug that variant exists to name.
pub fn fault_slot_acquire(as_owner: Pid, page_va: usize) -> FaultSlot {
    let my_tid = crate::threading::current_thread_id();
    let mut spins: u32 = 0;
    loop {
        // IRQ-safe critical section. `fault_mutex` is a shared spinlock on the
        // EL0 demand-paging path; like every other such lock here it must be
        // taken with IRQs disabled, otherwise a holder could be preempted by the
        // timer/SGI mid-section while a CLONE_VM sibling (which shares the
        // leader's one `fault_mutex`) contended on it — and a contender that
        // reached the lock with IRQs already masked (a nested/EL1-side fault,
        // or any call site inside an `IrqGuard`) would spin on a preempted
        // holder that can never be rescheduled (timer masked). Masking here
        // guarantees the holder can never be preempted while holding the slot
        // on a single CPU. `with_irqs_disabled` is reentrant and nests fine
        // with the IRQ-safe heap lock that `BTreeMap::insert` may touch. The
        // `yield_now()` below stays OUTSIDE the IRQ-disabled region so the
        // scheduler + IRQs keep making progress while we wait. (Correct hygiene
        // — but note this was investigated and is *not* the `curl https` freeze;
        // see docs/OPTIONAL_SMOLTCP.md: that was `clone_thread` handing the
        // child a stale TTBR0.)
        let outcome = with_irqs_disabled(|| {
            let proc = match lookup_process_shared(as_owner) {
                Some(p) => p,
                None => return Some(FaultSlot::NoProc),
            };
            let mut faults = proc.fault_mutex.lock();
            match faults.get(&page_va).copied() {
                None => {
                    faults.insert(page_va, my_tid);
                    Some(FaultSlot::Acquired)
                }
                // Re-entrant acquire by the holder itself. NOT `Acquired`: the
                // caller must not release, or the inner guard drops the outer
                // guard's entry (see `FaultSlot::AlreadyHeld`).
                Some(holder) if holder == my_tid => Some(FaultSlot::AlreadyHeld),
                Some(holder) => {
                    if crate::threading::is_thread_terminated(holder) {
                        faults.insert(page_va, my_tid);
                        return Some(FaultSlot::ReclaimedDead(holder));
                    }
                    if spins >= FAULT_SLOT_SPIN_BOUND {
                        faults.insert(page_va, my_tid);
                        return Some(FaultSlot::ReclaimedWedged(holder));
                    }
                    None // contended — retry after yielding (IRQs on)
                }
            }
        });
        if let Some(slot) = outcome {
            return slot;
        }
        spins = spins.wrapping_add(1);
        // Wait for the slot holder to release, DROPPING the Big Kernel Lock under
        // shared-kernel SMP. The holder may be a CLONE_VM sibling doing its fault
        // block I/O on a peer core (M5b BKL-dropped file-fault path); it must be able
        // to re-take the BKL to release the slot, which it can't if we spin holding it
        // (the bounded `FAULT_SLOT_SPIN_BOUND` reclaim above only papers over that).
        crate::threading::blocking_relax();
    }
}

/// Release the per-page demand-paging slot for `page_va`, but only if the calling
/// thread still owns it. If a sibling reclaimed the slot (because we were assumed
/// dead/wedged), we must NOT remove its entry — the reclaimer releases it.
pub fn fault_slot_release(as_owner: Pid, page_va: usize) {
    let my_tid = crate::threading::current_thread_id();
    // IRQ-safe critical section — same discipline as `fault_slot_acquire`. Reached
    // from the EL0 demand-paging fault path (IRQs-enabled), and contended across
    // CLONE_VM siblings sharing the leader's one `fault_mutex`.
    with_irqs_disabled(|| {
        if let Some(proc) = lookup_process_shared(as_owner) {
            let mut faults = proc.fault_mutex.lock();
            if faults.get(&page_va).copied() == Some(my_tid) {
                faults.remove(&page_va);
            }
        }
    });
}

// `current_process() -> Option<&'static mut Process>` is GONE — see the note
// above `lookup_process_shared`. Use [`current_process_shared`] /
// [`with_current_process`].

/// Get the current process as a **shared** `&'static Process` — the
/// [`lookup_process_shared`] counterpart of [`current_process`], resolving the
/// PID the same way (thread-to-PID map first, then the ProcessInfo page).
///
/// This is the accessor for the Phase 7e "Access" migration: reads and calls
/// to `&self` methods (fd table, `vm_*`, `with_as_locked`, `Arc` fields) go
/// through this; plain-field *writes* go through [`with_current_process`] /
/// `table::with_process` instead, so no call site materializes a long-lived
/// `&'static mut Process` (aliasing UB when two cores hold one to the same
/// object). Same lifetime caveat as [`lookup_process_shared`]: valid only
/// while the process stays registered.
pub fn current_process_shared() -> Option<&'static Process> {
    // Identity cache fast path (own-process half): the map value's `Process`
    // in two validated loads — same resolution, same fallbacks.
    if let Some((_, proc)) = crate::process::table::current_thread_own_process() {
        return Some(proc);
    }
    lookup_process_shared(current_pid()?)
}

/// Run `f` on the current process with IRQs disabled — the current-process
/// counterpart of `table::with_process`, resolving the PID exactly like
/// [`current_process`] (thread-to-PID map first, then the ProcessInfo page).
///
/// Same contract as `with_process`: the callback runs with IRQs disabled and
/// MUST NOT allocate on the heap (moving a pre-built value into a field, or
/// letting the replaced value drop, is fine — deallocation cannot re-enter the
/// OOM path). Use it for short plain-field writes and scalar copies; use
/// [`current_process_shared`] for reads and `&self` methods.
pub fn with_current_process<T>(f: impl FnOnce(&mut Process) -> T) -> Option<T> {
    crate::process::table::with_process(current_pid()?, f)
}

/// Resolve the current process PID (checking THREAD_PID_MAP first, then ProcessInfo page).
pub fn current_pid() -> Option<Pid> {
    // Identity cache fast path — the map's own-pid value, without the lock.
    if let Some((pid, _)) = crate::process::table::current_thread_own_process() {
        return Some(pid);
    }
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().get(&tid).copied()
    });
    if thread_pid.is_some() { return thread_pid; }
    read_current_pid()
}

/// Get the current process's TerminalState (for syscall handlers)
///
/// Returns a mutable reference to the TerminalState if found.
pub fn current_terminal_state() -> Option<Arc<Spinlock<terminal::TerminalState>>> {
    // 1. Try thread-ID based lookup (for system threads or overridden processes)
    let tid = crate::threading::current_thread_id();
    if let Some(state) = crate::process::channel::get_terminal_state(tid) {
        return Some(state);
    }

    // 2. Fallback to process table
    current_process_shared().map(|p| p.terminal_state.clone())
}

/// Allocate mmap region for current process
/// Returns the address or 0 on failure
pub fn alloc_mmap(size: usize) -> usize {
    // Use address-space owner so CLONE_VM threads share allocation state.
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let proc = match lookup_process_shared(pid) {
        Some(p) => p,
        None => {
            (runtime().print_str)("[mmap] ERROR: No current process\n");
            return 0;
        }
    };

    // Use per-process memory tracking
    match proc.vm_alloc_mmap(size) {
        Some(addr) => addr,
        None => {
            log::debug!("[mmap] REJECT: pid={} size=0x{:x} next=0x{:x} limit=0x{:x}",
                proc.pid, size, proc.memory.next_mmap.load(core::sync::atomic::Ordering::Relaxed), proc.memory.mmap_limit);
            0
        }
    }
}

/// Record a new mmap region for the current process
///
/// Called by sys_mmap after allocating frames.
/// The frames Vec should contain all physical frames for this region.
pub fn record_mmap_region(start_va: usize, frames: Vec<PhysFrame>) {
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    if let Some(proc) = lookup_process_shared(pid) {
        proc.vm_with_regions(|r| r.push(MmapRegion::owned(start_va, frames)));
    }
}

// ==========================================================================
// LazyRegionMap — the per-process lazy-region bookkeeping.
//
// This is the pure data-structure layer, lifted out of the former global
// `LAZY_REGION_TABLE` so it can live as a field on `Process` (`lazy_regions:
// Spinlock<LazyRegionMap>`). Owning the map per-process is what closes the
// rule-2 hang class for this subsystem: a `BTreeMap::insert` that OOMs under
// the lock routes through `alloc_error_handler` → `return_to_kernel`, whose
// teardown used to re-enter the *global* `LAZY_REGION_TABLE` (`clear_lazy_regions`)
// and spin forever (the abandoned `SpinlockGuard` is never dropped — no
// unwinding). With the map owned by the dying `Process`, the field drops inside
// `Process::drop` on the existing reclaim path and teardown no longer
// re-acquires it. See docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md.
//
// Keeping the logic in a standalone newtype also means it is unit-testable
// without a registered `Process` (the host regression tests construct one
// directly), preserving the coverage the old global-table tests gave us.
// ==========================================================================

/// A process's set of demand-paged lazy regions, keyed by `start_va`.
pub struct LazyRegionMap {
    regions: BTreeMap<usize, LazyRegion>,
}

impl LazyRegionMap {
    pub const fn new() -> Self {
        Self { regions: BTreeMap::new() }
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// O(log n) lookup of the region covering `va`, if any. The `LazySource` is
    /// cloned out so the caller can drop the lock before doing I/O.
    pub fn lookup(&self, va: usize) -> Option<(u64, LazySource, usize, usize)> {
        let (_key, r) = self.regions.range(..=va).next_back()?;
        (va < r.start_va + r.size)
            .then(|| (r.flags, r.source.clone(), r.start_va, r.size))
    }

    /// Insert/replace a region. Returns the new region count.
    pub fn push(&mut self, start_va: usize, size: usize, flags: u64, source: LazySource) -> usize {
        self.regions.insert(
            start_va,
            LazyRegion { start_va, size, flags, source },
        );
        self.regions.len()
    }

    pub fn remove(&mut self, start_va: usize) -> Option<LazyRegion> {
        self.regions.remove(&start_va)
    }

    pub fn clear(&mut self) -> usize {
        let n = self.regions.len();
        self.regions.clear();
        n
    }

    /// Deep-clone of the whole map (fork propagation snapshots).
    pub fn clone_all(&self) -> BTreeMap<usize, LazyRegion> {
        self.regions.clone()
    }

    /// Snapshot every `LazySource::File` region's `(start_va, size)` onto a
    /// stack buffer for `reclaim_clean_file_pages` (which cannot allocate — it
    /// runs on the OOM path). `out` is filled up to `N` entries; returns the
    /// count written.
    pub fn snapshot_file_regions<const N: usize>(
        &self,
        out: &mut [(usize, usize); N],
    ) -> usize {
        let mut n = 0usize;
        for r in self.regions.values() {
            if matches!(r.source, LazySource::File { .. }) && n < N {
                out[n] = (r.start_va, r.size);
                n += 1;
            }
        }
        n
    }

    /// Merge every region in `src` into `self`, replacing any existing entry at
    /// the same `start_va` and leaving entries at other VAs alone (fork's
    /// `propagate_lazy_regions_to_child`). Returns the number of entries in
    /// `self` after the merge.
    pub fn extend_from_slice(&mut self, src: &[LazyRegion]) -> usize {
        for r in src {
            self.regions.insert(r.start_va, r.clone());
        }
        self.regions.len()
    }

    /// Replace `self`'s contents with a deep clone of `src` (fork's
    /// `clone_lazy_regions`).
    pub fn replace_with_clone(&mut self, src: &BTreeMap<usize, LazyRegion>) {
        self.regions = src.clone();
    }

    /// Update flags on all regions overlapping `[range_start, range_end)`,
    /// splitting partially-overlapping regions as needed.
    pub fn update_flags(&mut self, range_start: usize, range_size: usize, new_flags: u64) {
        let range_end = range_start + range_size;
        // Collect keys of regions that overlap [range_start, range_end).
        let keys: alloc::vec::Vec<usize> = self
            .regions
            .range(..range_end)
            .filter(|x| *x.0 + x.1.size > range_start)
            .map(|x| *x.0)
            .collect();

        for key in keys {
            let r_start = key;
            let r_size = self.regions[&key].size;
            let r_end = r_start + r_size;
            let r_flags = self.regions[&key].flags;
            let r_source = self.regions[&key].source.clone();

            let clip_start = r_start.max(range_start);
            let clip_end = r_end.min(range_end);

            if clip_start == r_start && clip_end == r_end {
                // Fully contained: update in place.
                self.regions.get_mut(&key).unwrap().flags = new_flags;
            } else {
                // Partially overlapping: remove and re-insert up to 3 pieces.
                self.regions.remove(&key);
                if clip_start > r_start {
                    self.regions.insert(r_start, LazyRegion {
                        start_va: r_start,
                        size: clip_start - r_start,
                        flags: r_flags,
                        source: r_source.clone(),
                    });
                }
                self.regions.insert(clip_start, LazyRegion {
                    start_va: clip_start,
                    size: clip_end - clip_start,
                    flags: new_flags,
                    source: r_source.clone(),
                });
                if clip_end < r_end {
                    self.regions.insert(clip_end, LazyRegion {
                        start_va: clip_end,
                        size: r_end - clip_end,
                        flags: r_flags,
                        source: r_source,
                    });
                }
            }
        }
    }

    /// Clip/remove the first region overlapping `[range_start, range_end)` for
    /// munmap. Returns `Some((op, freed_start_va, freed_pages))` if a region was
    /// touched, so the caller can loop for further overlaps. `op` is the clip
    /// shape — `F`ull / `P`refix / `S`uffix / `M`iddle-split — carried out for the
    /// caller's `[LR*]` debug line; region splitting is where this code has gone
    /// wrong before, and the shape is the first thing you want in the log.
    pub fn munmap_one_overlap(
        &mut self,
        range_start: usize,
        range_end: usize,
    ) -> Option<(char, usize, usize)> {
        let key = self
            .regions
            .range(..range_end)
            .filter(|x| *x.0 + x.1.size > range_start)
            .map(|x| *x.0)
            .next()?;

        let reg_start = key;
        let reg_size = self.regions[&key].size;
        let reg_end = reg_start + reg_size;
        let reg_flags = self.regions[&key].flags;
        let reg_source = self.regions[&key].source.clone();

        let clip_start = range_start.max(reg_start);
        let clip_end = range_end.min(reg_end);

        if clip_start == reg_start && clip_end == reg_end {
            self.regions.remove(&key);
            Some(('F', reg_start, reg_size / 4096))
        } else if clip_start == reg_start {
            // Trim prefix: remove old entry, insert remainder at new start_va.
            self.regions.remove(&key);
            self.regions.insert(clip_end, LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: reg_flags,
                source: reg_source,
            });
            Some(('P', clip_start, (clip_end - clip_start) / 4096))
        } else if clip_end == reg_end {
            // Trim suffix: shorten the existing entry in place (key unchanged).
            self.regions.get_mut(&key).unwrap().size = clip_start - reg_start;
            Some(('S', clip_start, (reg_end - clip_start) / 4096))
        } else {
            // Middle split: shorten left piece, insert right piece.
            self.regions.get_mut(&key).unwrap().size = clip_start - reg_start;
            self.regions.insert(clip_end, LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: reg_flags,
                source: reg_source,
            });
            Some(('M', clip_start, (clip_end - clip_start) / 4096))
        }
    }

    /// Debug iterator (first 8 regions) for `lazy_region_debug`.
    pub fn for_each_debug<F: FnMut(usize, usize)>(&self, mut f: F) {
        for r in self.regions.values().take(8) {
            f(r.start_va, r.size);
        }
    }
}

/// Check if a virtual address falls within any lazy region of the current process.
/// Returns `(flags, source, region_start, region_size)` if found.
/// The source is cloned so the caller can release the lock before performing I/O.
pub fn lazy_region_lookup(va: usize) -> Option<(u64, LazySource, usize, usize)> {
    let pid = address_space_owner_pid_for_fault()?;
    lookup_process_shared(pid).and_then(|p| with_irqs_disabled(|| p.lazy_regions.lock().lookup(va)))
}

/// Number of lazy regions registered for `pid` (0 if the pid isn't registered).
pub fn lazy_region_count_for_pid(pid: Pid) -> usize {
    lookup_process_shared(pid).map_or(0, |p| with_irqs_disabled(|| p.lazy_regions.lock().len()))
}

pub fn lazy_region_lookup_for_pid(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    lookup_process_shared(pid).and_then(|p| with_irqs_disabled(|| p.lazy_regions.lock().lookup(va)))
}

/// Rotating sweep cursor (VA) for [`reclaim_clean_file_pages`], so successive
/// reclaims page out across the whole file region (clock-like) instead of always
/// hitting the same low addresses.
static RECLAIM_CURSOR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Evict up to `want` clean, read-only, **file-backed** pages of the current
/// address space and return them to the PMM — the page-reclaim half of demand
/// paging that lets a file mmap larger than physical RAM make progress under
/// memory pressure (model weights are paged out and re-faulted from the file).
///
/// Only pages inside `LazySource::File` lazy regions are candidates, and only
/// those still mapped read-only (`try_evict_ro_page` re-checks the PTE), so anon
/// memory (stack/heap/compute buffers) and any CoW-dirtied page are never
/// touched. Allocates nothing (it runs on the OOM path): regions are snapshotted
/// onto the stack and frames are freed via the runtime hook. Returns the number
/// of pages freed. Called from `pmm::alloc_page_zeroed_user` before it declares
/// OOM.
pub fn reclaim_clean_file_pages(want: usize) -> usize {
    if want == 0 { return 0; }
    use core::sync::atomic::Ordering;

    let pid = match address_space_owner_pid_for_fault() {
        Some(p) => p,
        None => return 0,
    };

    let proc = match lookup_process_shared(pid) {
        Some(p) => p,
        None => return 0,
    };

    // Snapshot the file-backed regions onto the stack — no heap allocation, since
    // we are already under memory pressure and a Vec growth could recurse into
    // the allocator's OOM handler. 64 regions is ample (llama uses ~37 total).
    let mut regions: [(usize, usize); 64] = [(0, 0); 64];
    let n = with_irqs_disabled(|| proc.lazy_regions.lock().snapshot_file_regions(&mut regions));
    if n == 0 { return 0; }

    // Cap pages scanned per call so a sparse (mostly-unmapped) region set can't
    // spin; eviction is the slow path, but it must still bound its own work.
    const MAX_SCAN: usize = 262_144; // up to ~1 GB of VA scanned per reclaim
    let cursor = RECLAIM_CURSOR.load(Ordering::Relaxed);
    let mut freed = 0usize;
    let mut scanned = 0usize;
    let mut next_cursor = 0usize; // 0 ⇒ wrap to the start next time

    'sweep: for i in 0..n {
        let (start, size) = regions[i];
        let end = start + size;
        // Resume from the cursor; regions are stored sorted by start_va.
        let mut va = if start < cursor { cursor & !0xFFF } else { start };
        if va >= end { continue; }
        while va < end {
            if freed >= want || scanned >= MAX_SCAN {
                next_cursor = va;
                break 'sweep;
            }
            scanned += 1;
            // `try_evict_ro_page` walks and clears a live PTE, same class of edit as
            // every other page-table mutation in this address space — it needs
            // `as_lock` to exclude a concurrent BKL-free fault (or a future BKL-free
            // mmap-family syscall) on the same page, exactly like the fault handler's
            // own per-page `as_lock` holds. One hold per page (not one hold spanning
            // the whole up-to-262144-page scan): this loop is a bounded but
            // potentially long sweep, and `as_lock_hold` masks IRQs for its duration —
            // holding it across the entire sweep would starve this core's timer for
            // however long the scan runs, exactly the "mask per attempt, never across
            // an unbounded wait" rule (docs/reference/subsystems/locking.md).
            let evicted = proc.with_address_space(|aspace| aspace.try_evict_ro_page(va));
            if let Some(frame) = evicted {
                akuma_pmm::free_page(frame.addr, akuma_primitives::preempt::current_tid() as u32);
                freed += 1;
            }
            va += 0x1000;
        }
    }
    RECLAIM_CURSOR.store(next_cursor, Ordering::Relaxed);
    freed
}

/// Find the PID of the non-shared process whose address space's L0 page-table frame
/// matches `l0_phys`. CLONE_THREAD goroutines share an address space (is_shared==true),
/// so this returns the thread-group leader (the owner of the real page tables).
fn owner_pid_for_l0_phys(l0_phys: usize) -> Option<Pid> {
    find_process(|p| {
        if !p.address_space.is_shared() && p.address_space.l0_phys() == l0_phys {
            Some(p.pid)
        } else {
            None
        }
    })
}

/// Thread group leader PID for page-fault / CoW paths: all `CLONE_VM` threads in a group must
/// share one [`Process::fault_mutex`] and resolve to the leader's [`Process::lazy_regions`]
/// (see `clone_lazy_regions`,
/// forktest / GO_FORKTEST_DEBUG).
///
/// Uses TTBR0-derived lookup as the primary mechanism: the current TTBR0_EL1 unambiguously
/// identifies the running address space regardless of THREAD_PID_MAP state.  Stale
/// THREAD_PID_MAP entries (e.g. when a kernel thread slot is reused for a different process)
/// would otherwise cause the demand-pager to look up lazy regions under the wrong PID,
/// triggering an EL1 copy-path fault and delivering a spurious SIGSEGV to the wrong process.
pub fn address_space_owner_pid_for_fault() -> Option<Pid> {
    // TTBR0 identifies the running address space with certainty.  Find the non-shared
    // process (i.e. the thread-group leader) that owns this L0 frame.
    let ttbr0 = crate::mmu::get_current_ttbr0() as usize;
    let boot_ttbr0 = crate::mmu::get_boot_ttbr0() as usize;
    let l0_phys = ttbr0 & 0x0000_FFFF_FFFF_F000;
    if l0_phys != 0 && l0_phys != boot_ttbr0 {
        if let Some(pid) = owner_pid_for_l0_phys(l0_phys) {
            return Some(pid);
        }
    }
    // Fallback: THREAD_PID_MAP tgid, then ProcessInfo page.
    current_process_shared().map(|p| p.tgid).or_else(read_current_pid)
}

/// Like [`lazy_region_lookup_for_pid`], but resolves demand-paging metadata keyed by the
/// thread-group id ([`Process::tgid`]) first — the same key as `sys_mmap` uses via `proc.tgid`
/// — then falls back to `pid` (e.g. [`read_current_pid`] from EL0).
///
/// Ordering matters when only the leader's `Process` holds the regions but the caller
/// passes another thread id (clone snapshot keys, or stale ProcessInfo).
pub fn lazy_region_lookup_for_page_fault(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    if let Some(owner) = address_space_owner_pid_for_fault() {
        if let Some(r) = lazy_region_lookup_for_pid(owner, va) {
            return Some(r);
        }
    }
    lazy_region_lookup_for_pid(pid, va)
}

// `LazyDebugWriter` was a third copy of the stack writer; `lazy_region_debug`
// below uses the shared `akuma_primitives::console::StackWriter` instead.

pub fn lazy_region_debug(va: usize) {
    use core::fmt::Write;
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let Some(proc) = lookup_process_shared(pid) else {
        crate::safe_print!(128, "[DP] lazy miss: pid={} va={:#x} no process entry\n", pid, va);
        return;
    };
    with_irqs_disabled(|| {
        let g = proc.lazy_regions.lock();
        let count = g.len();
        let mut w = akuma_primitives::console::StackWriter::<256>::new();
        let _ = write!(w, "[DP] lazy miss: pid={} va={:#x} regions={} [", pid, va, count);
        let mut i = 0;
        g.for_each_debug(|sv, sz| {
            if i > 0 { let _ = w.write_str(","); }
            let _ = write!(w, "{:#x}+{:#x}", sv, sz);
            i += 1;
        });
        let _ = w.write_str("]\n");
        w.flush();
    });
}

pub fn push_lazy_region(pid: Pid, start_va: usize, size: usize, page_flags: u64) -> usize {
    push_lazy_region_with_source(pid, start_va, size, page_flags, LazySource::Zero)
}

pub fn push_lazy_region_with_source(pid: Pid, start_va: usize, size: usize, page_flags: u64, source: LazySource) -> usize {
    let Some(proc) = lookup_process_shared(pid) else { return 0 };
    with_irqs_disabled(|| proc.lazy_regions.lock().push(start_va, size, page_flags, source))
}

/// Copy every lazy-region descriptor in `parent_regions` into `child`.
///
/// `child` is taken by reference rather than by pid deliberately: `fork_process`
/// builds the child as a local `Box<Process>` and only calls `register_process`
/// at the very end, so a pid-keyed variant would silently find nothing to write
/// to and drop the propagation on the floor (which is exactly the SIGSEGV this
/// function exists to prevent). Both fork call sites already hold a
/// `Vec<LazyRegion>` snapshot of the parent, so the parent is read once.
///

/// `fork_process`'s CoW-sharing (`cow_share_range`) only shares pages that are
/// *currently resident* in the parent — a lazy region the parent registered but
/// hasn't fully touched yet (a `.data`/`.bss` page nobody wrote to since exec, a
/// stack page deeper than the parent's current usage, ...) has nothing resident
/// to share. Without also copying the region *descriptors* themselves, the child
/// has no lazy-region entry for that VA either: not resident (nothing was shared)
/// and not lazy (no entry to demand-page from) — an unconditional SIGSEGV on first
/// touch. A single fork off a long-lived, fully-warmed-up process rarely hits this;
/// forking off a process that was itself freshly forked (a shell subshell
/// backgrounding a real command) hits it far more often, since the intermediate
/// process hasn't had time to fault every lazy page in yet.
/// See docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md.
///
/// Returns the child's total region count after the merge (0 if the parent had
/// none — the child is left untouched in that case).
pub fn propagate_lazy_regions_to_child(parent_regions: &[LazyRegion], child: &Process) -> usize {
    if parent_regions.is_empty() {
        return 0;
    }
    // Only the child's lock is taken, and the parent snapshot was produced
    // before this call — the two maps' locks never nest.
    with_irqs_disabled(|| child.lazy_regions.lock().extend_from_slice(parent_regions))
}

/// Derive a CoW-forked child's `mmap_regions` from its parent's, and `munmap`'s
/// clip-and-split over a region list — both re-exported from `akuma-mmap`.
///
/// Neither ever took a process: one maps a slice, the other rewrites a `&mut Vec`.
/// They moved with `MmapRegion` and took their ~230 lines of host tests with them,
/// which is the whole point — region splitting is where this code has gone wrong
/// before (`docs/archive/CARGO_HEAP_NULL_RC.md` D8/D9,
/// `docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`) and every shape that
/// matters is now pinned without a live process. The pid-keyed accessors below
/// (`eager_region_flags_for_page_fault`, `update_eager_region_flags`,
/// `munmap_lazy_regions_in_range`, …) stay here: each resolves a process and takes
/// `vm_lock` before it can touch a region list.
pub use akuma_mmap::{detach_eager_regions_in_range, inherit_mmap_regions_for_cow_child};

/// Protection recorded for the **eager** mmap region covering `va`, if any.
///
/// The eager counterpart of [`lazy_region_lookup_for_page_fault`], and resolved the
/// same way: eager regions live on the address-space owner's `mmap_regions`, so a
/// CLONE_VM worker must look them up under the owner, not its own pid.
///
/// Used by the EL0 write-permission-fault handler to decide whether a read-only PTE
/// inside an eager mapping is a repairable inconsistency or a genuine access
/// violation.
pub fn eager_region_flags_for_page_fault(pid: Pid, va: usize) -> Option<u64> {
    let lookup = |p: Pid| -> Option<u64> {
        let proc = lookup_process_shared(p)?;
        proc.vm_with_regions(|r| r.iter().find(|reg| reg.contains(va)).map(|reg| reg.flags))
    };
    if let Some(owner) = address_space_owner_pid_for_fault()
        && let Some(f) = lookup(owner)
    {
        return Some(f);
    }
    lookup(pid)
}

/// The protection an eager region **states** for `va`, if it states one.
///
/// The *deny*-side counterpart of [`eager_region_flags_for_page_fault`]. That one
/// answers "what flags does the record hold", which is right for granting a write
/// — the unrecorded default is `NONE` and grants nothing. This one answers "did
/// the region actually say anything", which is what a refusal needs: treating an
/// unrecorded region as read-only refuses legitimate CoW breaks and kills `rustc`.
///
/// See `MmapRegion::prot_recorded` for the `NONE`-is-two-facts problem this exists
/// to resolve.
pub fn eager_region_recorded_prot_for_page_fault(pid: Pid, va: usize) -> Option<u64> {
    let lookup = |p: Pid| -> Option<u64> {
        let proc = lookup_process_shared(p)?;
        proc.vm_with_regions(|r| {
            r.iter().find(|reg| reg.contains(va)).and_then(MmapRegion::recorded_prot)
        })
    };
    if let Some(owner) = address_space_owner_pid_for_fault()
        && let Some(f) = lookup(owner)
    {
        return Some(f);
    }
    lookup(pid)
}

/// Every eager region covering `va`, as `(start_va, pages, flags)`.
///
/// [`eager_region_flags_for_page_fault`] answers from the **first** `Vec` match,
/// so if two regions cover one VA the winner is decided by insertion order and an
/// obsolete record can shadow the live one — which is indistinguishable, from the
/// fault handler's side, from a page whose protection was legitimately recorded.
/// This exists so an anomaly report can say how many regions actually claim the
/// address: more than one is a bookkeeping bug, not a close call. See
/// docs/archive/CARGO_HEAP_NULL_RC.md.
pub fn eager_regions_containing(pid: Pid, va: usize) -> alloc::vec::Vec<(usize, usize, u64)> {
    let owner = address_space_owner_pid_for_fault().unwrap_or(pid);
    let collect = |p: Pid| -> alloc::vec::Vec<(usize, usize, u64)> {
        lookup_process_shared(p).map_or_else(alloc::vec::Vec::new, |proc| {
            proc.vm_with_regions(|r| {
                r.iter()
                    .filter(|reg| reg.contains(va))
                    .map(|reg| (reg.start_va, reg.pages, reg.flags))
                    .collect()
            })
        })
    };
    let owned = collect(owner);
    if owned.is_empty() && owner != pid { collect(pid) } else { owned }
}

/// Update flags on all **eager** mmap regions overlapping
/// `[range_start, range_start+range_size)`.
///
/// `mprotect`'s eager counterpart to [`update_lazy_region_flags`]. Unlike the lazy
/// map this does not split regions: `MmapRegion` keys its frame list to `start_va`,
/// and splitting one would have to split `frames` in step. A sub-range `mprotect`
/// therefore records the *new* protection for the whole region, which is
/// deliberately conservative in the safe direction — the fault handler only ever
/// uses these flags to grant a write, so widening the recorded range of a
/// downgrade can never turn a legitimate SIGSEGV into a silent success. Recording
/// nothing at all, which is what happened before, is what could.
pub fn update_eager_region_flags(pid: Pid, range_start: usize, range_size: usize, new_flags: u64) {
    let Some(proc) = lookup_process_shared(pid) else { return };
    let range_end = range_start.saturating_add(range_size);
    // SPLITS now, rather than recording the new protection against the whole
    // overlapping region. The old widening was justified as safe "because the
    // fault handler only ever uses these flags to grant a write" — true for
    // granting, false for refusing, and the day the write-fault handler started
    // refusing on this record a guard page `mprotect(PROT_NONE)`-ed inside a
    // larger mapping marked every page of it and killed `rustc` mid-build.
    //
    // The algebra is `akuma_mmap::mprotect_eager_regions_in_range`, host-tested
    // against every shape (head/middle/tail, fully covered, multi-region,
    // CoW-inherited, and that neighbours keep whatever the original region said).
    // Splitting frames in step is the same walk `detach_eager_regions_in_range`
    // has always done — the old "we cannot split `frames`" objection was never
    // true. See docs/reference/subsystems/syscalls/mem.md.
    let touched = proc.vm_with_regions(|r| {
        akuma_mmap::mprotect_eager_regions_in_range(r, range_start, range_end, new_flags)
    });
    if touched > 0 {
        EAGER_FLAG_WIDENED.fetch_add(0, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Count of `mprotect` upgrades that recorded "writable" for pages outside the
/// requested range (see [`update_eager_region_flags`]). Every one of them is a page
/// the fault handler will silently grant a write on.
pub static EAGER_FLAG_WIDENED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Update flags on all lazy regions that overlap [range_start, range_start+range_size).
pub fn update_lazy_region_flags(pid: Pid, range_start: usize, range_size: usize, new_flags: u64) {
    let Some(proc) = lookup_process_shared(pid) else { return };
    with_irqs_disabled(|| proc.lazy_regions.lock().update_flags(range_start, range_size, new_flags));
}

pub fn remove_lazy_region(pid: Pid, start_va: usize) -> Option<LazyRegion> {
    let proc = lookup_process_shared(pid)?;
    with_irqs_disabled(|| proc.lazy_regions.lock().remove(start_va))
}

/// Handle munmap across all lazy regions overlapping [unmap_addr, unmap_addr+unmap_len).
pub fn munmap_lazy_regions_in_range(pid: Pid, unmap_addr: usize, unmap_len: usize) -> Vec<(usize, usize)> {
    let unmap_end = unmap_addr + unmap_len;
    let mut results = Vec::new();

    loop {
        if let Some(result) = munmap_lazy_region_overlapping(pid, unmap_addr, unmap_end) {
            results.push(result);
        } else {
            break;
        }
    }
    results
}

fn munmap_lazy_region_overlapping(pid: Pid, range_start: usize, range_end: usize) -> Option<(usize, usize)> {
    let proc = lookup_process_shared(pid)?;
    let (op, freed_start, freed_pages) =
        with_irqs_disabled(|| proc.lazy_regions.lock().munmap_one_overlap(range_start, range_end))?;
    log::debug!("[LR{}] pid={} munmap {:#x}+{:#x} ({} pages)",
        op, pid, freed_start, freed_pages * 4096, freed_pages);
    Some((freed_start, freed_pages))
}

/// Clear every lazy region registered for `pid`. No-op if `pid` isn't a registered
/// process. Note: the per-process teardown paths (`return_to_kernel*`,
/// `teardown_forked_process_thread_group`) deliberately do NOT call this — the
/// field drops inside `Process::drop` on the reclaim path, so calling it from the
/// exit/OOM-kill path would re-enter the very lock an OOM'd mutator frame is
/// still holding (the rule-2 hang in
/// docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md). It is kept for the
/// zombie-reap paths (`sys_wait4`/`sys_waitid` in `syscall/proc.rs`), which release
/// a *child's* map early from the reaping parent's syscall context — a context that
/// holds no lazy-region lock, so there is nothing to re-enter. Purely an
/// optimization there: the map would drop with the `Process` anyway.
///
/// The execve image-replacement path does not come through here — `replace_image`
/// owns `&mut self` and clears `self.lazy_regions` directly.
pub fn clear_lazy_regions(pid: Pid) {
    let Some(proc) = lookup_process_shared(pid) else { return };
    let count = with_irqs_disabled(|| proc.lazy_regions.lock().clear());
    if count > 0 {
        log::debug!("[LR!] clear pid={} ({} regions)", pid, count);
    }
}

pub fn clone_lazy_regions(from_pid: Pid, to_pid: Pid) {
    let (Some(from), Some(to)) = (lookup_process_shared(from_pid), lookup_process_shared(to_pid)) else {
        return;
    };
    // Snapshot under the parent's lock, clone under the child's — never nested.
    let snapshot = with_irqs_disabled(|| from.lazy_regions.lock().clone_all());
    if snapshot.is_empty() {
        return;
    }
    let len = snapshot.len();
    with_irqs_disabled(|| to.lazy_regions.lock().replace_with_clone(&snapshot));
    log::debug!("[LR] clone pid={}->{} ({} regions)", from_pid, to_pid, len);
}

/// Owned snapshot of a pid's lazy regions (`None` if the pid isn't registered).
/// Diagnostic/test replacement for the former direct
/// `LAZY_REGION_TABLE.lock().get(&pid)` inspection (that global is gone) — allocates
/// (clones the map),
/// so call only from non-pressure contexts.
pub fn lazy_regions_snapshot(pid: Pid) -> Option<BTreeMap<usize, LazyRegion>> {
    lookup_process_shared(pid).map(|p| with_irqs_disabled(|| p.lazy_regions.lock().clone_all()))
}

/// Check if a virtual address falls within any lazy region.
pub fn is_in_lazy_region(va: usize) -> bool {
    lazy_region_lookup(va).is_some()
}

/// Remove and return mmap region starting at the given VA
pub fn remove_mmap_region(start_va: usize) -> Option<Vec<PhysFrame>> {
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let proc = lookup_process_shared(pid)?;
    
    // Find & remove the region under vm_lock (pure Vec op).
    let region = proc.vm_with_regions(|r| {
        r.iter().position(|reg| reg.start_va == start_va).map(|idx| r.remove(idx))
    })?;

    // RECLAIM: Add the freed range to free_regions. Size from `pages` (the
    // authoritative extent) so a CoW-inherited region — which owns no frames —
    // still recycles its full VA range rather than zero bytes.
    proc.vm_free_mmap(region.start_va, region.len_bytes());

    Some(region.frames)
}

/// Get stack bounds for current process
pub fn get_stack_bounds() -> (usize, usize) {
    match current_process_shared() {
        Some(p) => (p.memory.stack_bottom, p.memory.stack_top),
        None => (0, 0),
    }
}


/// List all running processes.
///
/// Collects scalar fields with IRQs disabled (safe from use-after-free),
/// then does a second pass to clone Strings per PID.
/// The String clone uses lookup_process which re-validates the pointer.
pub fn list_processes() -> Vec<ProcessInfo2> {
    // Phase 1: collect scalar fields atomically (IRQs disabled, no allocation)
    #[derive(Copy, Clone, Default)]
    struct Info {
        pid: u32,
        ppid: u32,
        box_id: u64,
        state: u8, // 0=ready 1=running 2=blocked 3=zombie
        current_syscall: u64,
        last_syscall: u64,
    }
    let infos = crate::process::table::collect_process_info(|p| {
        let st = match p.state {
            ProcessState::Ready => 0u8,
            ProcessState::Running => 1,
            ProcessState::Blocked => 2,
            ProcessState::Zombie(_) => 3,
        };
        Some(Info {
            pid: p.pid,
            ppid: p.parent_pid,
            box_id: p.box_id,
            state: st,
            current_syscall: p.current_syscall.load(core::sync::atomic::Ordering::Relaxed),
            last_syscall: p.last_syscall.load(core::sync::atomic::Ordering::Relaxed),
        })
    });

    // Phase 2: clone Strings per PID (IRQs enabled, safe to allocate).
    // lookup_process re-validates the pointer; if the process was freed
    // between phase 1 and 2, lookup returns None and we use fallback values.
    let mut result = Vec::with_capacity(infos.len());
    for info in &infos {
        let state_str = match info.state {
            0 => "ready", 1 => "running", 2 => "blocked", _ => "zombie",
        };
        let (name, args) = if let Some(proc) = lookup_process_shared(info.pid) {
            if proc.name.len() <= 4096 && proc.args.len() <= 256 {
                (proc.name.clone(), proc.args.clone())
            } else {
                (alloc::string::String::from("?"), Vec::new())
            }
        } else {
            (alloc::string::String::from("?"), Vec::new())
        };
        result.push(ProcessInfo2 {
            pid: info.pid,
            ppid: info.ppid,
            box_id: info.box_id,
            name,
            state: state_str,
            current_syscall: info.current_syscall,
            last_syscall: info.last_syscall,
            args,
        });
    }
    result
}

/// Find a process PID by thread ID (lock-free scan).
pub fn find_pid_by_thread(thread_id: usize) -> Option<Pid> {
    crate::process::table::find_process(|p| {
        if p.thread_id == Some(thread_id) { Some(p.pid) } else { None }
    })
}

#[cfg(test)]
mod child_channel_drain_tests {
    //! Regression tests for the sshd interactive-shell "lost output" bug: a child
    //! that wrote stdout and exited (busybox/toybox login shell over sshd) had its
    //! buffered output discarded because `wait*` called `remove_child_channel` the
    //! instant it reaped the zombie, before the parent's bridge could drain it.
    //! `reap_child_channel` keeps the channel until its stdout is drained.
    use super::*;
    use crate::process::channel::ProcessChannel;
    use crate::test_support::ensure_test_runtime;


    #[test]
    fn reap_keeps_channel_until_buffered_stdout_is_drained() {
        ensure_test_runtime();
        // High, test-local pids so the shared CHILD_CHANNELS registry can't collide
        // with other parallel host tests.
        let pid: Pid = 0x7000_0001;
        let parent: Pid = 0x7000_0002;

        let ch = Arc::new(ProcessChannel::new());
        // Child writes output, then exits (mirrors busybox flushing stdio at _exit).
        ch.write(b"HELLO_FROM_CHILD");
        ch.set_exited(0);
        register_child_channel(pid, ch.clone(), parent);

        // The wait* path reaps the zombie. Output is still buffered, so the channel
        // MUST be kept (returns false = not removed) — otherwise the parent's
        // ChildStdout fd would resolve to nothing and lose the output.
        assert!(
            !reap_child_channel(pid),
            "reap must KEEP the channel while stdout is still buffered"
        );
        let surviving = get_child_channel(pid)
            .expect("channel must survive the reap while data is pending");

        // Parent drains the buffered output (exactly what sshd's bridge does after
        // observing the child's exit).
        let mut buf = [0u8; 64];
        let n = surviving.read(&mut buf);
        assert_eq!(&buf[..n], b"HELLO_FROM_CHILD", "buffered child output preserved");

        // Now that it is drained, a subsequent reap removes the channel.
        assert!(reap_child_channel(pid), "reap removes the channel once drained");
        assert!(get_child_channel(pid).is_none(), "channel gone after drained reap");
    }

    #[test]
    fn reap_removes_immediately_when_no_buffered_stdout() {
        let pid: Pid = 0x7000_0011;
        let parent: Pid = 0x7000_0012;

        let ch = Arc::new(ProcessChannel::new());
        ch.set_exited(0); // exited with no pending output (or already drained)
        register_child_channel(pid, ch, parent);

        // Nothing buffered → reap removes it right away, so callers that waitpid
        // without ever reading the ChildStdout fd don't leak channels.
        assert!(reap_child_channel(pid), "empty channel is removed on reap");
        assert!(get_child_channel(pid).is_none());
    }

    #[test]
    fn is_child_of_group_matches_registered_parent_only() {
        let pid: Pid = 0x7000_0031;
        let parent: Pid = 0x7000_0032;
        let stranger: Pid = 0x7000_0033;

        register_child_channel(pid, Arc::new(ProcessChannel::new()), parent);

        assert!(is_child_of_group(pid, parent), "registered parent may wait");
        assert!(!is_child_of_group(pid, stranger), "non-parent gets ECHILD");
        // The Go os/exec pidfd probe: a process waitid()s a pidfd of ITSELF and
        // must get ECHILD (it is not its own child), never block.
        assert!(!is_child_of_group(pid, pid), "self-wait is not a child wait");

        remove_child_channel(pid);
    }

    #[test]
    fn is_child_of_group_unregistered_pid_is_not_a_child() {
        assert!(!is_child_of_group(0x7000_0041, 0x7000_0042));
    }

    #[test]
    fn reap_absent_channel_is_a_noop() {
        // Reaping a pid with no registered channel (a process spawned without a
        // stdout pipe) must not panic and reports "not removed".
        assert!(!reap_child_channel(0x7000_0021));
    }
}

#[cfg(test)]
mod lazy_region_propagation_tests {
    //! Regression tests for `propagate_lazy_regions_to_child`
    //! (docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md): `fork_process`'s
    //! `cow_share_range` only shares pages already resident in the parent, so the
    //! child also needs the parent's lazy-region *descriptors* copied over —
    //! otherwise a page the parent registered but hadn't touched yet is neither
    //! resident (nothing to share) nor lazy (no entry to demand-page from) for the
    //! child, and the first touch is an unconditional SIGSEGV.
    //!
    //! These exercise [`LazyRegionMap`] rather than the pid-keyed wrapper: since
    //! the per-process move (docs/archive/LAZY_REGION_TABLE_ALLOC_UNDER_LOCK_HANG.md)
    //! `propagate_lazy_regions_to_child` is a null-check plus one
    //! `extend_from_slice` call, and building a real `Process` on the host needs a
    //! page allocator the stub test runtime doesn't have.
    use super::*;

    /// The parent-side snapshot both fork arms take before propagating.
    fn snapshot(map: &LazyRegionMap) -> alloc::vec::Vec<LazyRegion> {
        map.clone_all().into_values().collect()
    }

    /// Mount id 1 throughout: these tests are about region propagation across
    /// fork, not about which filesystem the inode came from.
    fn file_source(path: &str, inode: u32) -> LazySource {
        LazySource::file(alloc::string::String::from(path), 1, inode, 0, 0x2000, 0x2000_0000)
    }

    #[test]
    fn copies_all_parent_regions_to_child() {
        let mut parent = LazyRegionMap::new();
        let mut child = LazyRegionMap::new();

        parent.push(0x1000_0000, 0x1000, 0x1, LazySource::Zero);
        parent.push(0x2000_0000, 0x2000, 0x2, file_source("/bin/busybox", 42));

        let total = child.extend_from_slice(&snapshot(&parent));
        assert_eq!(total, 2);

        let child_regions = child.clone_all();
        assert_eq!(child_regions.len(), 2);

        let r1 = &child_regions[&0x1000_0000];
        assert_eq!(r1.size, 0x1000);
        assert_eq!(r1.flags, 0x1);
        assert!(matches!(r1.source, LazySource::Zero));

        let r2 = &child_regions[&0x2000_0000];
        assert_eq!(r2.size, 0x2000);
        assert_eq!(r2.flags, 0x2);
        match &r2.source {
            LazySource::File { path, inode, .. } => {
                assert_eq!(path, "/bin/busybox");
                assert_eq!(*inode, 42);
            }
            _ => panic!("expected a File-backed lazy source to survive propagation"),
        }

        // The parent keeps its own regions — propagation is a copy, not a move.
        assert_eq!(parent.len(), 2);
    }

    // ── Inode pins follow region lifetime ───────────────────────────
    //
    // A `LazySource::File` region holds an `InodePin` that stops the filesystem
    // freeing its inode underneath it (`SELFHOST_ZERO_PAGE_HUNT.md` §14). Nothing
    // reads that field, so nothing would notice it going wrong — these tests are
    // the check that every mutation on this map leaves the count balanced.
    //
    // Each test uses its own inode number: the pin table is a process-wide
    // static, so shared numbers would make peers interfere.

    use akuma_primitives::inode_pin::pin_count;

    #[test]
    fn a_file_region_pins_its_inode_until_it_is_removed() {
        let ino = 50_001;
        let mut map = LazyRegionMap::new();
        assert_eq!(pin_count(ino), 0);

        map.push(0x1000_0000, 0x1000, 0x1, file_source("/lib.so", ino));
        assert_eq!(pin_count(ino), 1, "a live region must pin its inode");

        map.remove(0x1000_0000);
        assert_eq!(pin_count(ino), 0, "removing the region must release it");
    }

    #[test]
    fn dropping_the_whole_map_releases_every_pin() {
        // The `Process::drop` path: a dying process must not leave its inodes
        // pinned forever, or their disk space is never reclaimed.
        let ino = 50_002;
        {
            let mut map = LazyRegionMap::new();
            map.push(0x1000_0000, 0x1000, 0x1, file_source("/lib.so", ino));
            map.push(0x2000_0000, 0x1000, 0x1, file_source("/lib.so", ino));
            assert_eq!(pin_count(ino), 2);
        }
        assert_eq!(pin_count(ino), 0, "map drop must release all of them");
    }

    #[test]
    fn clear_releases_every_pin() {
        // `exec` replacing the image: same requirement as drop.
        let ino = 50_003;
        let mut map = LazyRegionMap::new();
        map.push(0x1000_0000, 0x1000, 0x1, file_source("/lib.so", ino));
        assert_eq!(pin_count(ino), 1);

        map.clear();
        assert_eq!(pin_count(ino), 0);
    }

    #[test]
    fn fork_propagation_takes_a_reference_of_its_own() {
        // The child maps the same file, so the inode must stay pinned until
        // *both* are gone — a child outliving its parent is the ordinary case.
        let ino = 50_004;
        let mut parent = LazyRegionMap::new();
        parent.push(0x1000_0000, 0x1000, 0x1, file_source("/lib.so", ino));

        let mut child = LazyRegionMap::new();
        child.extend_from_slice(&snapshot(&parent));
        assert_eq!(pin_count(ino), 2, "parent and child hold one each");

        parent.clear();
        assert_eq!(pin_count(ino), 1, "the child's mapping still needs the file");

        child.clear();
        assert_eq!(pin_count(ino), 0);
    }

    #[test]
    fn replacing_a_region_at_the_same_va_swaps_the_pin() {
        // `push` over an existing key drops the old region — its pin must go
        // with it, or a re-mmap'd VA leaks a pin on the previous file.
        let old = 50_005;
        let new = 50_006;
        let mut map = LazyRegionMap::new();
        map.push(0x1000_0000, 0x1000, 0x1, file_source("/old.so", old));
        map.push(0x1000_0000, 0x1000, 0x1, file_source("/new.so", new));

        assert_eq!(pin_count(old), 0, "the replaced region must release its pin");
        assert_eq!(pin_count(new), 1);
        map.clear();
        assert_eq!(pin_count(new), 0);
    }

    #[test]
    fn munmap_clip_shapes_keep_the_count_matching_the_pieces() {
        // Every clip shape rebuilds regions by remove-then-insert, which is
        // exactly where a hand-maintained count would drift.
        let ino = 50_007;
        let mut map = LazyRegionMap::new();
        map.push(0x1000_0000, 0x4000, 0x1, file_source("/lib.so", ino));
        assert_eq!(pin_count(ino), 1);

        // Middle split: one region becomes two, so two pins.
        map.munmap_one_overlap(0x1000_1000, 0x1000_2000);
        assert_eq!(pin_count(ino), 2, "a middle split leaves two live regions");

        // Full unmap of everything that is left.
        while map.munmap_one_overlap(0x1000_0000, 0x1000_4000).is_some() {}
        assert_eq!(pin_count(ino), 0, "unmapping it all must release every pin");
    }

    #[test]
    fn mprotect_splitting_a_region_keeps_the_pin() {
        // `update_flags` also removes and re-inserts up to three pieces.
        let ino = 50_008;
        let mut map = LazyRegionMap::new();
        map.push(0x1000_0000, 0x4000, 0x1, file_source("/lib.so", ino));

        map.update_flags(0x1000_1000, 0x1000, 0x7);
        assert_eq!(pin_count(ino), 3, "split into three pieces, three pins");

        map.clear();
        assert_eq!(pin_count(ino), 0);
    }

    #[test]
    fn anonymous_regions_pin_nothing() {
        // `LazySource::Zero` has no file behind it; it must not touch the table.
        let mut map = LazyRegionMap::new();
        map.push(0x1000_0000, 0x1000, 0x1, LazySource::Zero);
        // An inode-0 file region is the "read by path" case and is equally inert.
        map.push(0x2000_0000, 0x1000, 0x1, file_source("/by-path", 0));
        assert_eq!(pin_count(0), 0);
        map.clear();
    }

    #[test]
    fn parent_with_no_regions_copies_nothing() {
        let parent = LazyRegionMap::new();
        let mut child = LazyRegionMap::new();

        assert_eq!(child.extend_from_slice(&snapshot(&parent)), 0);
        assert!(child.is_empty());
    }

    #[test]
    fn does_not_clobber_childs_existing_regions_at_other_vas() {
        let mut parent = LazyRegionMap::new();
        let mut child = LazyRegionMap::new();

        // Child already has its own region (e.g. from an earlier setup step) at a
        // VA the parent doesn't use; propagation must not wipe it out.
        child.push(0x3000_0000, 0x1000, 0x1, LazySource::Zero);
        parent.push(0x1000_0000, 0x1000, 0x1, LazySource::Zero);

        assert_eq!(child.extend_from_slice(&snapshot(&parent)), 2);

        let child_regions = child.clone_all();
        assert!(child_regions.contains_key(&0x1000_0000));
        assert!(child_regions.contains_key(&0x3000_0000));
    }

    #[test]
    fn parent_region_replaces_a_child_region_at_the_same_va() {
        // Same `start_va` on both sides: the parent's descriptor wins, so a child
        // forked off a parent that re-mmap'd a VA inherits the *current* mapping
        // rather than a stale one.
        let mut parent = LazyRegionMap::new();
        let mut child = LazyRegionMap::new();

        child.push(0x1000_0000, 0x1000, 0x1, LazySource::Zero);
        parent.push(0x1000_0000, 0x8000, 0x7, file_source("/bin/dash", 7));

        assert_eq!(child.extend_from_slice(&snapshot(&parent)), 1);

        let (flags, source, start_va, size) =
            child.lookup(0x1000_4000).expect("the parent's larger region now covers this VA");
        assert_eq!((start_va, size, flags), (0x1000_0000, 0x8000, 0x7));
        assert!(matches!(source, LazySource::File { inode: 7, .. }));
    }
}

#[cfg(test)]
mod fault_slot_tests {
    //! Host unit tests for [`FaultSlot::reclaim_report`] — the pure half of the
    //! per-page demand-paging slot's observability. `fault_slot_acquire` itself
    //! needs a live thread slot and a registered process, so it stays in the boot
    //! suite (`src/process_tests.rs`); the classification does not.
    use super::*;

    /// The hot outcomes must stay silent: this runs on every demand-paging fault,
    /// so a `Some` here is a console write per page.
    #[test]
    fn acquired_and_noproc_report_nothing() {
        assert_eq!(FaultSlot::Acquired.reclaim_report(), None);
        assert_eq!(FaultSlot::NoProc.reclaim_report(), None);
    }

    /// Both reclaim outcomes report, and each carries the *stale holder's* tid —
    /// not the reclaiming thread's. That tid is the whole diagnostic value: it
    /// names the thread that died or wedged mid-fault.
    #[test]
    fn both_reclaims_report_their_stale_holder() {
        assert_eq!(
            FaultSlot::ReclaimedDead(41).reclaim_report(),
            Some((ReclaimCause::Dead, 41))
        );
        assert_eq!(
            FaultSlot::ReclaimedWedged(42).reclaim_report(),
            Some((ReclaimCause::Wedged, 42))
        );
    }

    /// The two reclaim causes must stay distinguishable: `Dead` is the expected
    /// recovery from a thread that died mid-fault, `Wedged` means the bounded
    /// fallback fired and "should be vanishingly rare" — collapsing them would
    /// hide the second behind the first in the logs.
    #[test]
    fn dead_and_wedged_are_distinct_causes() {
        let dead = FaultSlot::ReclaimedDead(7).reclaim_report();
        let wedged = FaultSlot::ReclaimedWedged(7).reclaim_report();
        assert_ne!(dead, wedged);
    }

    /// Holder tid 0 is a legal thread slot, so it must not be confused with "no
    /// reclaim" — an `Option<(_, usize)>` keyed on the tid being non-zero would
    /// silently drop the reclaim of slot 0.
    #[test]
    fn holder_tid_zero_still_reports() {
        assert_eq!(
            FaultSlot::ReclaimedDead(0).reclaim_report(),
            Some((ReclaimCause::Dead, 0))
        );
    }

    /// A nested acquire is not a reclaim: nothing was taken from anyone, so the
    /// `[FAULT-RECLAIM]` trace must stay silent for it. The caller distinguishes
    /// it from `Acquired` by matching the variant, not by this report — the two
    /// differ in who owns the *release*, which `reclaim_report` says nothing about
    /// (F6, `COW_PILE_AUDIT.md` §9).
    #[test]
    fn already_held_reports_nothing() {
        assert_eq!(FaultSlot::AlreadyHeld.reclaim_report(), None);
    }

    /// ...but it must not be *spelled* `Acquired`, which is the shape of the F6
    /// defect: one variant for "you now own the release" and "someone else still
    /// does" is what let the inner RAII guard drop the outer guard's entry. This
    /// pins the discriminants apart so a future `reclaim_report`-driven refactor
    /// cannot quietly merge them back.
    #[test]
    fn already_held_is_not_acquired() {
        assert!(!matches!(FaultSlot::AlreadyHeld, FaultSlot::Acquired));
        assert!(!matches!(FaultSlot::Acquired, FaultSlot::AlreadyHeld));
    }
}

#[cfg(test)]
mod sigchld_delivery_tests {
    //! Host unit tests for the SIGCHLD delivery edge added to fix the hanging
    //! `wait` builtin (docs/archive/SIGCHLD_DELIVERY_PLAN.md). The kernel boot
    //! self-test exercises the in-VM reproducer; these cover the pure-logic
    //! helpers (`parent_pid_of`, `raise_sigchld_for_parent`, `publish_child_exit`,
    //! and the `LAST_SIGCHLD` siginfo side-channel) that have no dependency on a
    //! live thread slot.
    //!
    //! Every assertion here relies only on the shared `CHILD_CHANNELS` registry
    //! and the static per-slot arrays; high test-local pids (0x7000_005x) avoid
    //! collisions with the parallel sibling test modules.
    use super::*;
    use crate::process::channel::ProcessChannel;
    use crate::threading;

    /// `parent_pid_of` mirrors the lookup `is_child_of_group` already does, but
    /// returns the raw forking-thread pid (which may be a non-leader of a
    /// multithreaded parent — resolution happens later, in `raise_sigchld_for_parent`).
    #[test]
    fn parent_pid_of_returns_registered_parent_then_none_after_remove() {
        let child: Pid = 0x7000_0050;
        let parent: Pid = 0x7000_0051;
        register_child_channel(child, Arc::new(ProcessChannel::new()), parent);
        assert_eq!(parent_pid_of(child), Some(parent));
        remove_child_channel(child);
        assert_eq!(parent_pid_of(child), None);
    }

    /// Raising SIGCHLD for a child with no registered channel (reaped, or a
    /// double-published exit) must be a silent no-op — never a panic.
    #[test]
    fn raise_sigchld_for_unregistered_child_is_noop() {
        raise_sigchld_for_parent(0x7000_0052, 0);
        raise_sigchld_for_parent(0x7000_0052, -9);
    }

    /// The in-kernel sshd bridge spawns children whose recorded parent is a
    /// kernel system thread with no `Process` entry. Resolving the parent's
    /// thread group must fail cleanly (no signal, no panic) rather than index a
    /// missing process.
    #[test]
    fn raise_sigchld_for_parent_with_no_process_is_noop() {
        let child: Pid = 0x7000_0053;
        let kernel_thread_parent: Pid = 0x7000_0054; // not in the process table
        register_child_channel(child, Arc::new(ProcessChannel::new()), kernel_thread_parent);
        // No registered Process → find_process returns None → no signal pended.
        raise_sigchld_for_parent(child, 0);
        remove_child_channel(child);
    }

    /// With an empty process table, every thread-group preference falls through
    /// and `sigchld_target_thread` returns `None` — the signal simply isn't
    /// raised. (The preference-order test against registered Processes is a
    /// kernel boot self-test: it needs live thread slots, which host tests lack.)
    #[test]
    fn sigchld_target_thread_is_none_with_no_processes() {
        assert!(sigchld_target_thread(0x7000_0055).is_none());
    }

    /// The load-bearing invariant: `publish_child_exit` marks the channel exited
    /// (so a racing `waitpid(WNOHANG)` in the parent's handler sees the zombie)
    /// and is safe even though the SIGCHLD itself is a no-op here (no parent
    /// Process registered). A second publish must NOT overwrite the real exit
    /// code with a teardown code like 137 — that is the "one death, one signal"
    /// guard, and it is what keeps `go build`'s buildID honest when a goroutine
    /// crashes after the leader already published a clean exit.
    #[test]
    fn publish_child_exit_marks_channel_and_dedups() {
        let child: Pid = 0x7000_0056;
        let parent: Pid = 0x7000_0057;
        let ch = Arc::new(ProcessChannel::new());
        register_child_channel(child, ch.clone(), parent);

        // First publish: real exit code 0, channel transitions to exited.
        publish_child_exit(child, 0);
        assert!(ch.has_exited(), "channel must be marked exited by publish");
        assert_eq!(ch.exit_code(), 0);

        // Second publish (e.g. return_to_kernel / subtree teardown with code 137):
        // the `has_exited` guard suppresses the overwrite AND the duplicate
        // SIGCHLD. The real exit code survives.
        publish_child_exit(child, 137);
        assert_eq!(ch.exit_code(), 0, "second publish must not clobber the real code");

        remove_child_channel(child);
    }

    /// `publish_child_exit` on a pid with no registered child channel (a process
    /// spawned without a stdout pipe, or one already reaped) is a silent no-op.
    #[test]
    fn publish_child_exit_for_absent_channel_is_noop() {
        publish_child_exit(0x7000_0058, 0);
        publish_child_exit(0x7000_0058, -9);
    }

    /// The SIGCHLD `siginfo` side-channel packs `(child_pid, exit_code)` into one
    /// u64 and must round-trip both clean exits (code >= 0) and signal kills
    /// (code < 0, where `-code` is the signal number). Slot 63 is unused by any
    /// other host test.
    #[test]
    fn last_sigchld_roundtrips_clean_and_signaled_exits() {
        let slot = 63usize;

        threading::set_last_sigchld(slot, 1234, 0);
        assert_eq!(threading::peek_last_sigchld(slot), Some((1234, 0)));

        threading::set_last_sigchld(slot, 4321, -9);
        assert_eq!(threading::peek_last_sigchld(slot), Some((4321, -9)),
            "negative exit codes (signal kills) must round-trip");

        threading::set_last_sigchld(slot, 7, 42);
        assert_eq!(threading::peek_last_sigchld(slot), Some((7, 42)));
    }

    /// `peek_last_sigchld` on a slot that was never written returns `None`, so
    /// the SIGCHLD siginfo path falls back to zeros gracefully.
    #[test]
    fn peek_last_sigchld_unset_is_none() {
        // Slot 62 is untouched by the round-trip test (which uses 63), and no
        // other host test writes LAST_SIGCHLD, so it is reliably the zero init.
        assert_eq!(threading::peek_last_sigchld(62), None);
    }
}
