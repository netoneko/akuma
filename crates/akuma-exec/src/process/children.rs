use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, ProcessInfo, PROCESS_INFO_ADDR, LazyRegion, LazySource, MmapRegion, ProcessInfo2, ProcessState};
use crate::process::channel::{ProcessChannel, get_channel};
use crate::process::table::{LAZY_REGION_TABLE, THREAD_PID_MAP, find_process};
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
    if let Some(proc) = current_process() {
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
    current_channel()
        .map(|ch| ch.is_interrupted())
        .unwrap_or(false)
}

/// Interrupt a process by thread ID
///
/// Used by the SSH shell to send Ctrl+C signal to a running process.
pub fn interrupt_thread(thread_id: usize) {
    if let Some(channel) = get_channel(thread_id) {
        channel.set_interrupted();
    }
}

/// Read the current process PID from the process info page
///
/// During a syscall, TTBR0 is still set to the user's page tables,
/// so reading from PROCESS_INFO_ADDR gives us the calling process's PID.
/// This prevents PID spoofing since the page is read-only for userspace.
///
/// Returns None if TTBR0 points to boot page tables (no user process context).
pub fn read_current_pid() -> Option<Pid> {
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
            return Some(crate::process::table::with_process(pid, |p| p.tgid).unwrap_or(pid));
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
    
    // Read from the fixed address in the current address space
    // SAFETY: TTBR0 is user page tables, so PROCESS_INFO_ADDR is mapped
    let pid = unsafe { (*(PROCESS_INFO_ADDR as *const ProcessInfo)).pid };
    if pid == 0 { None } else { Some(pid) }
}

/// Look up a process by PID.
///
/// # Safety warning
/// Returns `&'static mut Process` that is ONLY valid while the process stays
/// registered. If another thread calls `unregister_process` between this call
/// and your use of the reference, you get use-after-free.
///
/// **Prefer `crate::process::table::with_process(pid, |p| ...)` for safe access.**
///
/// This function exists for the 218+ legacy call sites in syscall handlers.
/// Most are safe in practice because syscall handlers run in a single thread
/// context and the process can't be freed during a syscall by its own thread.
pub fn lookup_process(pid: Pid) -> Option<&'static mut Process> {
    let ptr = crate::process::table::get_process_ptr(pid)?;
    crate::process::diag::borrow_inc(pid);
    Some(unsafe { &mut *ptr })
}

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
    /// Slot was free (or already held by us) and acquired cleanly.
    Acquired,
    /// Slot was reclaimed from a holder thread that had already died
    /// (TERMINATED/FREE) without releasing it — the root-cause poison recovery.
    /// Carries the dead holder's thread id.
    ReclaimedDead(usize),
    /// Slot was force-reclaimed after spinning past the safety bound: the holder
    /// neither released nor visibly died (wedged, or its slot was recycled to a
    /// live thread). Carries the stale holder id. Should be vanishingly rare.
    ReclaimedWedged(usize),
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
/// one [`fault_slot_release`] — normally via an RAII guard.
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
            let proc = match lookup_process(as_owner) {
                Some(p) => p,
                None => return Some(FaultSlot::NoProc),
            };
            let mut faults = proc.fault_mutex.lock();
            match faults.get(&page_va).copied() {
                None => {
                    faults.insert(page_va, my_tid);
                    Some(FaultSlot::Acquired)
                }
                Some(holder) if holder == my_tid => Some(FaultSlot::Acquired),
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
        if let Some(proc) = lookup_process(as_owner) {
            let mut faults = proc.fault_mutex.lock();
            if faults.get(&page_va).copied() == Some(my_tid) {
                faults.remove(&page_va);
            }
        }
    });
}

/// Get the current process (for syscall handlers).
///
/// For CLONE_THREAD children, uses the thread-to-PID map since they share
/// the parent's ProcessInfo page. Otherwise reads PID from the process info page.
///
/// Same safety caveats as `lookup_process`. Prefer `with_process` for new code.
pub fn current_process() -> Option<&'static mut Process> {
    let tid = crate::threading::current_thread_id();
    let thread_pid = with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().get(&tid).copied()
    });
    if let Some(pid) = thread_pid {
        return lookup_process(pid);
    }
    let pid = read_current_pid()?;
    lookup_process(pid)

}

/// Resolve the current process PID (checking THREAD_PID_MAP first, then ProcessInfo page).
pub fn current_pid() -> Option<Pid> {
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
    current_process().map(|p| p.terminal_state.clone())
}

/// Allocate mmap region for current process
/// Returns the address or 0 on failure
pub fn alloc_mmap(size: usize) -> usize {
    // Use address-space owner so CLONE_VM threads share allocation state.
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let proc = match lookup_process(pid) {
        Some(p) => p,
        None => {
            (runtime().print_str)("[mmap] ERROR: No current process\n");
            return 0;
        }
    };

    // Use per-process memory tracking
    match proc.memory.alloc_mmap(size) {
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
    if let Some(proc) = lookup_process(pid) {
        proc.vm_with_regions(|r| r.push(MmapRegion::owned(start_va, frames)));
    }
}

/// Record a lazy mmap region — VA reserved, no physical pages.
/// `page_flags` = 0 for PROT_NONE (needs mprotect), non-zero for demand-paged.
pub fn record_lazy_region(start_va: usize, size: usize, page_flags: u64) {
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    if let Some(proc) = lookup_process(pid) {
        proc.lazy_regions.push(LazyRegion { start_va, size, flags: page_flags, source: LazySource::Zero });
    }
}

/// Check if a virtual address falls within any lazy region of the current process.
/// Returns `(flags, source, region_start, region_size)` if found.
/// The source is cloned so the caller can release the table lock before performing I/O.
pub fn lazy_region_lookup(va: usize) -> Option<(u64, LazySource, usize, usize)> {
    let pid = address_space_owner_pid_for_fault()?;
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            // O(log n): last region whose start_va <= va, then range-check.
            if let Some((_key, r)) = regions.range(..=va).next_back() {
                if va < r.start_va + r.size {
                    return Some((r.flags, r.source.clone(), r.start_va, r.size));
                }
            }
        }
        None
    })
}

/// Like lazy_region_lookup but takes an explicit PID (for tests and non-current-process use).
pub fn lazy_region_count_for_pid(pid: Pid) -> usize {
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        table.get(&pid).map_or(0, |r| r.len())
    })
}

pub fn lazy_region_lookup_for_pid(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            // O(log n): find the last region whose start_va <= va, then range-check.
            if let Some((_key, r)) = regions.range(..=va).next_back() {
                if va < r.start_va + r.size {
                    return Some((r.flags, r.source.clone(), r.start_va, r.size));
                }
            }
        }
        None
    })
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

    // Snapshot the file-backed regions onto the stack — no heap allocation, since
    // we are already under memory pressure and a Vec growth could recurse into
    // the allocator's OOM handler. 64 regions is ample (llama uses ~37 total).
    let mut regions: [(usize, usize); 64] = [(0, 0); 64];
    let mut n = 0usize;
    with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        if let Some(map) = table.get(&pid) {
            for r in map.values() {
                if matches!(r.source, LazySource::File { .. }) && n < regions.len() {
                    regions[n] = (r.start_va, r.size);
                    n += 1;
                }
            }
        }
    });
    if n == 0 { return 0; }

    let proc = match lookup_process(pid) {
        Some(p) => p,
        None => return 0,
    };

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
            if let Some(frame) = proc.address_space.try_evict_ro_page(va) {
                (runtime().free_page)(frame);
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
/// share one [`Process::fault_mutex`] and match [`LAZY_REGION_TABLE`] (see `clone_lazy_regions`,
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
    current_process().map(|p| p.tgid).or_else(read_current_pid)
}

/// Like [`lazy_region_lookup_for_pid`], but resolves demand-paging metadata keyed by the
/// thread-group id ([`Process::tgid`]) first — the same key as `sys_mmap` uses via `proc.tgid`
/// — then falls back to `pid` (e.g. [`read_current_pid`] from EL0).
///
/// Ordering matters when `LAZY_REGION_TABLE` only has entries under the leader but the caller
/// passes another thread id (clone snapshot keys, or stale ProcessInfo).
pub fn lazy_region_lookup_for_page_fault(pid: Pid, va: usize) -> Option<(u64, LazySource, usize, usize)> {
    if let Some(owner) = address_space_owner_pid_for_fault() {
        if let Some(r) = lazy_region_lookup_for_pid(owner, va) {
            return Some(r);
        }
    }
    lazy_region_lookup_for_pid(pid, va)
}

/// Stack-local writer for visible kernel output without heap allocation.
struct LazyDebugWriter<const N: usize> {
    buf: [u8; N],
    pos: usize,
}
impl<const N: usize> LazyDebugWriter<N> {
    const fn new() -> Self { Self { buf: [0; N], pos: 0 } }
    fn flush(&mut self) {
        if let Ok(s) = core::str::from_utf8(&self.buf[..self.pos]) {
            (runtime().print_str)(s);
        }
        self.pos = 0;
    }
}
impl<const N: usize> core::fmt::Write for LazyDebugWriter<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = N - self.pos;
        let len = core::cmp::min(bytes.len(), remaining);
        self.buf[self.pos..self.pos + len].copy_from_slice(&bytes[..len]);
        self.pos += len;
        Ok(())
    }
}

pub fn lazy_region_debug(va: usize) {
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    with_irqs_disabled(|| {
        use core::fmt::Write;
        let table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&pid) {
            let mut w = LazyDebugWriter::<256>::new();
            let _ = write!(w, "[DP] lazy miss: pid={} va={:#x} regions={} [", pid, va, regions.len());
            for (i, (_, r)) in regions.iter().enumerate().take(8) {
                if i > 0 { let _ = w.write_str(","); }
                let _ = write!(w, "{:#x}+{:#x}", r.start_va, r.size);
            }
            let _ = w.write_str("]\n");
            w.flush();
        } else {
            let mut w = LazyDebugWriter::<128>::new();
            let _ = writeln!(w, "[DP] lazy miss: pid={} va={:#x} no entry in table", pid, va);
            w.flush();
        }
    });
}

pub fn push_lazy_region(pid: Pid, start_va: usize, size: usize, page_flags: u64) -> usize {
    push_lazy_region_with_source(pid, start_va, size, page_flags, LazySource::Zero)
}

pub fn push_lazy_region_with_source(pid: Pid, start_va: usize, size: usize, page_flags: u64, source: LazySource) -> usize {
    let len = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let regions = table.entry(pid).or_insert_with(alloc::collections::BTreeMap::new);
        regions.insert(start_va, LazyRegion { start_va, size, flags: page_flags, source });
        regions.len()
    });
    len
}

/// Copy every lazy-region descriptor registered for `parent_pid` into a fresh
/// entry for `child_pid`.
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
/// Returns the number of regions copied (0 if the parent has none registered).
pub fn propagate_lazy_regions_to_child(parent_pid: Pid, child_pid: Pid) -> usize {
    let parent_regions: alloc::vec::Vec<LazyRegion> = with_irqs_disabled(|| {
        let table = LAZY_REGION_TABLE.lock();
        table.get(&parent_pid)
            .map(|regions| regions.values().cloned().collect())
            .unwrap_or_default()
    });
    let count = parent_regions.len();
    if count > 0 {
        with_irqs_disabled(|| {
            let mut table = LAZY_REGION_TABLE.lock();
            let child_regions = table.entry(child_pid).or_insert_with(alloc::collections::BTreeMap::new);
            for region in parent_regions {
                child_regions.insert(region.start_va, region);
            }
        });
    }
    count
}

/// Derive a CoW-forked child's `mmap_regions` from its parent's.
///
/// The child maps every page of every parent region (read-only, CoW-shared by
/// `cow_share_range`) but *owns* none of them — frames are shared, and a write
/// fault allocates the child a private frame tracked in `user_frames`. So each
/// child region carries the parent's extent with an empty frame list.
///
/// Carrying the **extent** across is the part that matters, and the part that
/// used to be dropped: the child's regions were built with
/// `Vec::with_capacity(frames.len())`, which is a *length-zero* Vec, and every
/// consumer derived the region's size from `frames.len()`. A child forked from
/// such a child therefore saw four zero-length regions, `cow_share_range` skipped
/// all of them, and the grandchild had no mapping at all for the VAs its parent
/// was about to hand it live pointers into — a deterministic write to an unmapped
/// page (`docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`). The shell shape
/// `( cmd; cmd ) &` produces exactly that lineage: the shell mmaps musl's first
/// malloc arena, forks a subshell, and the subshell forks again to exec `cmd`.
pub fn inherit_mmap_regions_for_cow_child(parent_regions: &[MmapRegion]) -> alloc::vec::Vec<MmapRegion> {
    parent_regions
        .iter()
        .map(|r| MmapRegion::inherited(r.start_va, r.pages))
        .collect()
}

/// Update flags on all lazy regions that overlap [range_start, range_start+range_size).
pub fn update_lazy_region_flags(pid: Pid, range_start: usize, range_size: usize, new_flags: u64) {
    let range_end = range_start + range_size;
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get_mut(&pid) {
            // Collect keys of regions that overlap [range_start, range_end).
            // Any overlapping region must have start_va < range_end AND start_va + size > range_start.
            let keys: alloc::vec::Vec<usize> = regions
                .range(..range_end)
                .filter(|x| *x.0 + x.1.size > range_start)
                .map(|x| *x.0)
                .collect();

            for key in keys {
                let r_start = key;
                let r_size = regions[&key].size;
                let r_end = r_start + r_size;
                let r_flags = regions[&key].flags;
                let r_source = regions[&key].source.clone();

                let clip_start = r_start.max(range_start);
                let clip_end = r_end.min(range_end);

                if clip_start == r_start && clip_end == r_end {
                    // Fully contained: update in place.
                    regions.get_mut(&key).unwrap().flags = new_flags;
                } else {
                    // Partially overlapping: remove and re-insert up to 3 pieces.
                    regions.remove(&key);
                    // "before" tail keeps old flags.
                    if clip_start > r_start {
                        regions.insert(r_start, LazyRegion {
                            start_va: r_start,
                            size: clip_start - r_start,
                            flags: r_flags,
                            source: r_source.clone(),
                        });
                    }
                    // Overlapping slice gets new flags.
                    regions.insert(clip_start, LazyRegion {
                        start_va: clip_start,
                        size: clip_end - clip_start,
                        flags: new_flags,
                        source: r_source.clone(),
                    });
                    // "after" tail keeps old flags.
                    if clip_end < r_end {
                        regions.insert(clip_end, LazyRegion {
                            start_va: clip_end,
                            size: r_end - clip_end,
                            flags: r_flags,
                            source: r_source,
                        });
                    }
                }
            }
        }
    });
}

pub fn remove_lazy_region(pid: Pid, start_va: usize) -> Option<LazyRegion> {
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get_mut(&pid) {
            regions.remove(&start_va)
        } else {
            None
        }
    })
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
    let result = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let regions = table.get_mut(&pid)?;

        // Find the first region overlapping [range_start, range_end).
        // A region overlaps if start_va < range_end AND start_va + size > range_start.
        let key = regions
            .range(..range_end)
            .filter(|x| *x.0 + x.1.size > range_start)
            .map(|x| *x.0)
            .next()?;

        let reg_start = key;
        let reg_size = regions[&key].size;
        let reg_end = reg_start + reg_size;
        let reg_flags = regions[&key].flags;
        let reg_source = regions[&key].source.clone();

        let clip_start = range_start.max(reg_start);
        let clip_end = range_end.min(reg_end);

        if clip_start == reg_start && clip_end == reg_end {
            regions.remove(&key);
            Some(('F', reg_start, reg_size / 4096))
        } else if clip_start == reg_start {
            // Trim prefix: remove old entry, insert remainder at new start_va.
            regions.remove(&key);
            regions.insert(clip_end, LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: reg_flags,
                source: reg_source,
            });
            let freed = (clip_end - clip_start) / 4096;
            Some(('P', clip_start, freed))
        } else if clip_end == reg_end {
            // Trim suffix: shorten the existing entry in place (key unchanged).
            regions.get_mut(&key).unwrap().size = clip_start - reg_start;
            let freed = (reg_end - clip_start) / 4096;
            Some(('S', clip_start, freed))
        } else {
            // Middle split: shorten left piece, insert right piece.
            regions.get_mut(&key).unwrap().size = clip_start - reg_start;
            regions.insert(clip_end, LazyRegion {
                start_va: clip_end,
                size: reg_end - clip_end,
                flags: reg_flags,
                source: reg_source,
            });
            let freed = (clip_end - clip_start) / 4096;
            Some(('M', clip_start, freed))
        }
    });

    if let Some((op, freed_start, freed_pages)) = result {
        log::debug!("[LR{}] pid={} munmap {:#x}+{:#x} ({} pages)",
            op as char, pid, freed_start, freed_pages * 4096, freed_pages);
        Some((freed_start, freed_pages))
    } else {
        None
    }
}

pub fn clear_lazy_regions(pid: Pid) {
    let count = with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        let count = table.get(&pid).map_or(0, |r| r.len());
        table.remove(&pid);
        count
    });
    if count > 0 {
        log::debug!("[LR!] clear pid={} ({} regions)", pid, count);
    }
}

pub fn clone_lazy_regions(from_pid: Pid, to_pid: Pid) {
    with_irqs_disabled(|| {
        let mut table = LAZY_REGION_TABLE.lock();
        if let Some(regions) = table.get(&from_pid) {
            let cloned = regions.clone();
            let len = cloned.len();
            table.insert(to_pid, cloned);
            log::debug!("[LR] clone pid={}->{} ({} regions)", from_pid, to_pid, len);
        }
    });
}

/// Check if a virtual address falls within any lazy region.
pub fn is_in_lazy_region(va: usize) -> bool {
    lazy_region_lookup(va).is_some()
}

/// Remove and return mmap region starting at the given VA
pub fn remove_mmap_region(start_va: usize) -> Option<Vec<PhysFrame>> {
    let pid = address_space_owner_pid_for_fault().unwrap_or(0);
    let proc = lookup_process(pid)?;
    
    // Find & remove the region under vm_lock (pure Vec op).
    let region = proc.vm_with_regions(|r| {
        r.iter().position(|reg| reg.start_va == start_va).map(|idx| r.remove(idx))
    })?;

    // RECLAIM: Add the freed range to free_regions. Size from `pages` (the
    // authoritative extent) so a CoW-inherited region — which owns no frames —
    // still recycles its full VA range rather than zero bytes.
    proc.memory.free_regions.push((region.start_va, region.len_bytes()));

    Some(region.frames)
}

/// Get stack bounds for current process
pub fn get_stack_bounds() -> (usize, usize) {
    match current_process() {
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
        let (name, args) = if let Some(proc) = lookup_process(info.pid) {
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

    /// `ProcessChannel::write` reads the global `config()` (for a debug-print
    /// gate), which panics if unregistered. The crate has no shared test harness,
    /// so register a no-op stub runtime + zeroed config once (OnceCopy::set is
    /// idempotent — first call wins, the rest are ignored, so this is safe under
    /// parallel test execution).
    fn ensure_test_runtime() {
        use crate::runtime::{ExecRuntime, ExecConfig, register};
        let rt = ExecRuntime {
            uptime_us: || 0,
            disable_irqs: || {},
            enable_irqs: || {},
            end_of_interrupt: |_| {},
            trigger_sgi: |_| {},
            wake_remote_idle: || {},
            alloc_page_zeroed: || None,
            alloc_page: || None,
            free_page: |_| {},
            pmm_stats: || (0, 0, 0),
            track_frame: |_, _| {},
            free_count: || 0,
            total_count: || 0,
            alloc_pages_contiguous_zeroed: |_| None,
            free_pages_contiguous: |_, _| {},
            heap_stats: || (0, 0),
            is_memory_low: || false,
            exec_bkl_drop_enabled: || false,
            read_file: |_| Err(0),
            read_at: |_, _, _| Err(0),
            resolve_inode: |_| Err(0),
            read_at_by_inode: |_, _, _| Err(0),
            on_process_exit: |_| {},
            remove_socket: |_| {},
            socket_clone_ref: |_| {},
            futex_wake: |_, _, _| {},
            pipe_close_write: |_| {},
            pipe_close_read: |_| {},
            pipe_clone_ref: |_, _| {},
            eventfd_close: |_| {},
            eventfd_clone_ref: |_| {},
            epoll_destroy: |_| {},
            pidfd_close: |_| {},
            resolve_symlinks: |_| alloc::string::String::new(),
            file_size: |_| Ok(0),
            get_box_namespace: |_| None,
            set_spawn_namespace: |_| {},
            clear_spawn_namespace: || {},
            print_str: |_| {},
            cow_ref_inc: |_| {},
            cow_ref_dec: |_| false,
            cow_ref_get: |_| 0,
            cow_fault_lock: |_| {},
            cow_fault_unlock: |_| {},
            prepare_user_address_space: None,
            remote_fd_close: None,
        };
        let cfg = ExecConfig {
            max_threads: 64,
            reserved_threads: 1,
            kernel_stack_size: 0,
            boot_stack_base: 0,
            boot_stack_top: 0,
            default_thread_stack_size: 0,
            system_thread_stack_size: 0,
            user_thread_stack_size: 0,
            user_stack_size: 0,
            enable_stack_canaries: false,
            stack_canary: 0,
            canary_words: 0,
            network_thread_ratio: 0,
            deferred_thread_cleanup: false,
            thread_cleanup_cooldown_us: 0,
            syscall_debug_info_enabled: false,
            fork_brk_serial_progress: false,
            enable_sgi_debug_prints: false,
            proc_stdin_max_size: 1 << 20,
            proc_stdout_max_size: 1 << 20,
            cow_fork_enabled: false,
            vfork_fastpath_enabled: false,
            prefer_whole_file_load: false,
        };
        register(rt, cfg);
    }

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
    use super::*;

    fn clear(pid: Pid) {
        LAZY_REGION_TABLE.lock().remove(&pid);
    }

    #[test]
    fn copies_all_parent_regions_to_child() {
        let parent: Pid = 900_001;
        let child: Pid = 900_002;
        clear(parent);
        clear(child);

        push_lazy_region(parent, 0x1000_0000, 0x1000, 0x1);
        push_lazy_region_with_source(parent, 0x2000_0000, 0x2000, 0x2, LazySource::File {
            path: alloc::string::String::from("/bin/busybox"),
            inode: 42,
            file_offset: 0,
            filesz: 0x2000,
            segment_va: 0x2000_0000,
        });

        let copied = propagate_lazy_regions_to_child(parent, child);
        assert_eq!(copied, 2);

        let table = LAZY_REGION_TABLE.lock();
        let child_regions = table.get(&child).expect("child should have a lazy-region entry");
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
        drop(table);
        clear(parent);
        clear(child);
    }

    #[test]
    fn parent_with_no_regions_copies_nothing() {
        let parent: Pid = 900_003;
        let child: Pid = 900_004;
        clear(parent);
        clear(child);

        let copied = propagate_lazy_regions_to_child(parent, child);
        assert_eq!(copied, 0);
        assert!(LAZY_REGION_TABLE.lock().get(&child).is_none());
    }

    #[test]
    fn does_not_clobber_childs_existing_regions_at_other_vas() {
        let parent: Pid = 900_005;
        let child: Pid = 900_006;
        clear(parent);
        clear(child);

        // Child already has its own region (e.g. from an earlier setup step) at a
        // VA the parent doesn't use; propagation must not wipe it out.
        push_lazy_region(child, 0x3000_0000, 0x1000, 0x1);
        push_lazy_region(parent, 0x1000_0000, 0x1000, 0x1);

        let copied = propagate_lazy_regions_to_child(parent, child);
        assert_eq!(copied, 1);

        let table = LAZY_REGION_TABLE.lock();
        let child_regions = table.get(&child).unwrap();
        assert_eq!(child_regions.len(), 2);
        assert!(child_regions.contains_key(&0x1000_0000));
        assert!(child_regions.contains_key(&0x3000_0000));
        drop(table);
        clear(parent);
        clear(child);
    }
}

#[cfg(test)]
mod mmap_region_inheritance_tests {
    //! Regression tests for the grandchild-loses-its-mmap-regions bug
    //! (docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md).
    //!
    //! A CoW-forked child owns none of its inherited regions' frames, so its
    //! frame lists are empty. When the region's extent was *derived* from those
    //! lists, a child's own fork computed a zero-length range for every inherited
    //! region and shared none of them, leaving the grandchild with no mapping for
    //! VAs its parent had resident. `MmapRegion::pages` carries the extent
    //! independently of frame ownership so that chain holds up.
    use super::*;
    use crate::runtime::PhysFrame;

    fn frames(n: usize) -> alloc::vec::Vec<PhysFrame> {
        (0..n).map(|i| PhysFrame::new(0x4000_0000 + i * 4096)).collect()
    }

    /// The generation that called `mmap` owns its frames; extent == frame count.
    #[test]
    fn owned_region_extent_matches_frames() {
        let r = MmapRegion::owned(0x2012_0000, frames(3));
        assert_eq!(r.pages, 3);
        assert_eq!(r.len_bytes(), 3 * 4096);
        assert!(r.contains(0x2012_0000));
        assert!(r.contains(0x2012_2fff));
        assert!(!r.contains(0x2012_3000));
        assert_eq!(r.frame_for(0x2012_1000).map(|f| f.addr), Some(0x4000_1000));
    }

    /// A CoW child keeps the extent but owns no frames — the exact state whose
    /// extent used to be lost.
    #[test]
    fn cow_child_inherits_extent_without_owning_frames() {
        let parent = alloc::vec![
            MmapRegion::owned(0x2012_0000, frames(1)),
            MmapRegion::owned(0x2012_1000, frames(1)),
            MmapRegion::owned(0x2012_2000, frames(2)),
            MmapRegion::owned(0x2012_4000, frames(1)),
        ];

        let child = inherit_mmap_regions_for_cow_child(&parent);

        assert_eq!(child.len(), 4);
        for (c, p) in child.iter().zip(parent.iter()) {
            assert_eq!(c.start_va, p.start_va);
            assert_eq!(c.pages, p.pages, "extent must survive the CoW fork");
            assert!(c.frames.is_empty(), "a CoW child owns no per-region frames");
        }
        // Total extent preserved: 1+1+2+1 = 5 pages — the five pages the
        // grandchild used to be missing.
        assert_eq!(child.iter().map(|r| r.pages).sum::<usize>(), 5);
    }

    /// The actual regression: fork the child again. Every region must still
    /// present a non-zero range to share, or the grandchild faults on first touch.
    #[test]
    fn grandchild_still_inherits_full_extent() {
        let parent = alloc::vec![MmapRegion::owned(0x2012_0000, frames(1))];
        let child = inherit_mmap_regions_for_cow_child(&parent);
        let grandchild = inherit_mmap_regions_for_cow_child(&child);

        assert_eq!(grandchild.len(), 1);
        assert_eq!(grandchild[0].start_va, 0x2012_0000);
        assert_eq!(grandchild[0].pages, 1);
        assert!(
            grandchild[0].len_bytes() > 0,
            "a zero-length range is skipped by cow_share_range — this is the bug"
        );
        // The faulting address from the original report lands in this region.
        assert!(grandchild[0].contains(0x2012_0338));
    }

    /// An inherited region has no owned frame to re-map from, so the eager
    /// demand-paging fallback must decline rather than index an empty list.
    #[test]
    fn inherited_region_has_no_frame_to_remap() {
        let r = MmapRegion::inherited(0x2012_0000, 2);
        assert!(r.contains(0x2012_0338));
        assert_eq!(r.frame_for(0x2012_0338), None);
        assert_eq!(r.frame_for(0x9999_0000), None);
    }
}
