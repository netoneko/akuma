use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicPtr, Ordering};
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, LazyRegion};
use crate::runtime::{config, runtime, with_irqs_disabled};

/// Maximum number of concurrent processes.
pub const MAX_PROCESSES: usize = 256;

/// Next available PID (monotonically increasing, never recycled)
pub static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Slot states for the lock-free process table.
pub mod slot_state {
    pub const FREE: u8 = 0;
    pub const ACTIVE: u8 = 1;
    /// Reaped (`unregister_process`'d) but not yet freed. Invisible to
    /// `get_process_ptr`/`for_each_process`/`register_process`'s free-slot scan —
    /// exactly like ACTIVE is invisible to allocation and FREE is invisible to
    /// lookup. The `Process` and its address space stay live in memory until
    /// `reclaim_retired_processes` frees them; see that function's docs for why.
    pub const RETIRED: u8 = 2;
}

/// Per-slot state: FREE, ACTIVE, or RETIRED.
static SLOT_STATES: [AtomicU8; MAX_PROCESSES] = {
    const INIT: AtomicU8 = AtomicU8::new(slot_state::FREE);
    [INIT; MAX_PROCESSES]
};

/// Per-slot process pointer. Non-null when ACTIVE or RETIRED, null when FREE.
/// Points to a heap-allocated Process (from Box::into_raw).
static PROCESS_SLOTS: [AtomicPtr<Process>; MAX_PROCESSES] = {
    const INIT: AtomicPtr<Process> = AtomicPtr::new(core::ptr::null_mut());
    [INIT; MAX_PROCESSES]
};

/// Per-slot retirement timestamp (uptime_us), valid while the slot is RETIRED.
/// Read by `reclaim_retired_processes` to enforce the cooldown; see its docs.
static RETIRE_TIME: [AtomicU64; MAX_PROCESSES] = {
    const INIT: AtomicU64 = AtomicU64::new(0);
    [INIT; MAX_PROCESSES]
};

/// Register a process in the table (takes ownership via Box).
///
/// Finds a free slot via CAS, stores the Process pointer.
/// The Process is kept alive until `unregister_process` + `reclaim_retired_processes`
/// reclaim it.
pub fn register_process(_pid: Pid, proc: Box<Process>) {
    let ptr = Box::into_raw(proc);
    if try_claim_free_slot(ptr) {
        return;
    }
    // Miss: deliberately NOT calling `reclaim_retired_processes` here, even though
    // the table may simply be full of cooled-down zombies awaiting periodic
    // reclamation rather than genuinely out of slots (the same shape
    // `spawn_user_thread_fn_internal`'s on-demand `reclaim_terminated_slots` retry
    // solves for THREAD_STATES). `register_process` is called from deep inside
    // fork/clone/spawn paths whose lock context it doesn't control; reclaiming runs
    // arbitrary `Process::drop` code (frees page-table frames, releases an ASID —
    // `UserAddressSpace::drop`), which needs its own locks. A caller already holding
    // one of those locks (or anything downstream of them) self-deadlocks the instant
    // reclaim tries to take it again on the same core — this is exactly the boot
    // hang found and root-caused while landing this phase (real fork/exec/pipe
    // traffic through `test_sigpipe_terminate_no_deadlock`; see
    // docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md §"the on-demand reclaim that
    // wasn't"). The periodic `netpoll_maint` collector (100ms cadence, a call site
    // with no such ambient lock context) is the only reclaimer. If a spawn storm
    // ever starves it the way `THREAD_STATES` was starved before
    // `reclaim_terminated_slots` dropped its caller-identity gate, the fix must be a
    // *different* collector call site — not this one.
    unsafe { drop(Box::from_raw(ptr)); }
    panic!("Process table full ({} slots)", MAX_PROCESSES);
}

/// Try to claim one FREE slot and install `ptr` into it. Returns whether it succeeded.
fn try_claim_free_slot(ptr: *mut Process) -> bool {
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].compare_exchange(
            slot_state::FREE,
            slot_state::ACTIVE,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ).is_ok() {
            PROCESS_SLOTS[i].store(ptr, Ordering::Release);
            return true;
        }
    }
    false
}

/// Retire a process from the table: makes it invisible to `get_process_ptr`,
/// `for_each_process`, `current_process`, and every other lookup, and ineligible
/// for `register_process`'s free-slot scan — but does **not** free the `Process`
/// or drop its address space yet. Returns `true` if `pid` was found and this call
/// is the one that retired it (a racing second `unregister_process(pid)` for the
/// same pid loses the CAS below and returns `false`).
///
/// # Why deferred
/// A BKL-dropped window (`no-bkl-vfs`/`no-bkl-mm`/`no-bkl-process`) lets a peer
/// core hold a raw `*mut`/`*const Process` (from `lookup_process`/`get_process_ptr`/
/// `lookup_process_shared`) across real work with the BKL released. If this pid's
/// `Process` were freed here — as it used to be, synchronously, via
/// `Box::from_raw` + drop — that peer core's pointer becomes a use-after-free the
/// instant this call returns, with nothing but `with_irqs_disabled` (mutual
/// exclusion on one core) standing between them. Retiring instead of freeing keeps
/// the memory valid until `reclaim_retired_processes` frees it well after any such
/// window could still be open. See docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md
/// (Phase 7e, "Free" half) and docs/archive/BKL_PHASE7_AUDIT.md §2.1/§2.1.1.
pub fn unregister_process(pid: Pid) -> bool {
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
            continue;
        }
        let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
        if ptr.is_null() {
            continue;
        }
        if unsafe { (*ptr).pid } != pid {
            continue;
        }
        // Claim the ACTIVE -> RETIRED transition exclusively. Losing this CAS means
        // another thread already retired (or is retiring) this exact slot; the
        // scalar `pid` match above can't tell those apart from us, the CAS can.
        if SLOT_STATES[i].compare_exchange(
            slot_state::ACTIVE,
            slot_state::RETIRED,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ).is_err() {
            continue;
        }
        RETIRE_TIME[i].store((runtime().uptime_us)(), Ordering::Release);
        // Mark the process's thread as TERMINATED before unregistering.
        // This prevents orphaned threads that stay READY forever after
        // their process is reaped. Without this, kthreads shows "user-process"
        // threads with no corresponding process, and switching to them hangs.
        //
        // IMPORTANT: Only mark terminated if this is NOT the current thread.
        // Tests call unregister_process from the same thread to clean up,
        // and marking ourselves terminated would cause Thread 0's cleanup
        // to zero our context while we're still running.
        if let Some(tid) = unsafe { (*ptr).thread_id } {
            let current_tid = crate::threading::current_thread_id();
            if tid != current_tid {
                crate::threading::mark_thread_terminated(tid);
            }
        }
        return true;
    }
    false
}

/// Free the memory of every RETIRED slot whose cooldown
/// (`config().process_reclaim_cooldown_us`) has elapsed. Returns the count freed.
///
/// No caller-identity gate (unlike thread-slot cleanup's "only thread 0" default) —
/// there is no steady-state collector here that only runs when the system is idle,
/// so there is nothing to relax. Called *only* periodically, from `netpoll_maint` —
/// `register_process` deliberately does not reclaim on a full-table miss; see the
/// comment there for the self-deadlock that ruled that call site out.
///
/// # Why a time cooldown and not a caller-identity or reference-count gate
/// Same reasoning as `threading::reclaim_terminated_slots`: the thing that makes
/// freeing safe is that any BKL-dropped window holding a stale pointer has had time
/// to close, not who calls this function. `process_reclaim_cooldown_us` must outlast
/// any such window; those windows are single bounded I/O ops or PTE-chunk copies, not
/// open-ended, so the same order of magnitude as `THREAD_CLEANUP_COOLDOWN_US` (10ms)
/// is generous. See docs/archive/BKL_PHASE7E_PROCESS_TABLE_RECLAIM.md.
pub fn reclaim_retired_processes() -> usize {
    reclaim_retired_processes_internal(false)
}

/// Reclaim regardless of cooldown. **Tests only** — never safe in production, the
/// same way `threading::cleanup_terminated_force` is test-only: it removes the
/// margin that guarantees no peer core still holds a stale pointer into the slot
/// being freed.
pub fn reclaim_retired_processes_force() -> usize {
    reclaim_retired_processes_internal(true)
}

fn reclaim_retired_processes_internal(ignore_cooldown: bool) -> usize {
    let now = (runtime().uptime_us)();
    let cooldown_us = config().process_reclaim_cooldown_us;
    let mut count = 0;
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::RETIRED {
            continue;
        }
        if !ignore_cooldown {
            let retire_time = RETIRE_TIME[i].load(Ordering::Acquire);
            if retire_time > 0 && now.saturating_sub(retire_time) < cooldown_us {
                continue;
            }
        }
        // Extract the pointer BEFORE releasing the slot for reuse: the state stays
        // RETIRED throughout this swap, so `register_process` (which only claims
        // FREE slots) can never observe a half-freed slot or race the pointer we're
        // about to free. Two reclaimers racing the same slot (`reclaim_retired_processes`
        // has no single-collector gate): exactly one gets the real pointer via this
        // atomic swap, the other gets null and skips — mirrors how the old synchronous
        // `unregister_process` handled a racing second call on the same slot.
        let old = PROCESS_SLOTS[i].swap(core::ptr::null_mut(), Ordering::AcqRel);
        if old.is_null() {
            continue;
        }
        SLOT_STATES[i].store(slot_state::FREE, Ordering::Release);
        RETIRE_TIME[i].store(0, Ordering::Relaxed);
        unsafe { drop(Box::from_raw(old)); }
        count += 1;
    }
    count
}

/// Number of RETIRED slots awaiting `reclaim_retired_processes`. Diagnostics/tests only.
pub fn retired_process_count() -> usize {
    let mut count = 0;
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) == slot_state::RETIRED {
            count += 1;
        }
    }
    count
}

/// Access a process by PID within a callback. **This is the safe API.**
///
/// The callback runs with IRQs disabled, guaranteeing the Process pointer
/// is valid for the entire duration (no other thread can free it).
///
/// The callback MUST NOT allocate on the heap. For operations that need
/// allocation, copy scalar fields inside the callback and allocate outside.
///
/// # Example
/// ```ignore
/// let name = table::with_process(pid, |p| p.name.clone()); // OK for short strings
/// let exit_code = table::with_process(pid, |p| p.exit_code); // preferred for scalars
/// ```
#[inline]
pub fn with_process<T, F: FnOnce(&mut Process) -> T>(pid: Pid, f: F) -> Option<T> {
    with_irqs_disabled(|| {
        let ptr = get_process_ptr_inner(pid)?;
        Some(f(unsafe { &mut *ptr }))
    })
}

/// Look up a process by PID. Returns a raw pointer.
///
/// # Safety
/// The pointer is valid only while IRQs are disabled or no other thread
/// can call `unregister_process`. Prefer `with_process()` for safe access.
/// This function exists for the 218+ legacy call sites that use
/// `lookup_process() -> &'static mut Process`.
pub fn get_process_ptr(pid: Pid) -> Option<*mut Process> {
    with_irqs_disabled(|| get_process_ptr_inner(pid))
}

/// Inner scan (no IRQ guard — caller must ensure IRQs disabled).
fn get_process_ptr_inner(pid: Pid) -> Option<*mut Process> {
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
            continue;
        }
        let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
        if !ptr.is_null() && unsafe { (*ptr).pid } == pid {
            return Some(ptr);
        }
    }
    None
}

/// Iterate all active processes, calling `f` for each.
///
/// Runs entirely with IRQs disabled — the callback MUST NOT allocate.
/// For iteration that needs allocation, use `collect_pids` + per-PID lookup.
#[inline]
pub fn for_each_process<F: FnMut(&Process)>(mut f: F) {
    with_irqs_disabled(|| {
        for i in 0..MAX_PROCESSES {
            if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
                continue;
            }
            let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
            if !ptr.is_null() {
                f(unsafe { &*ptr });
            }
        }
    });
}

/// Iterate all active processes, calling `f` for each. Returns early if `f` returns Some.
///
/// Runs entirely with IRQs disabled — the callback MUST NOT allocate.
#[inline]
pub fn find_process<T, F: FnMut(&Process) -> Option<T>>(mut f: F) -> Option<T> {
    with_irqs_disabled(|| {
        for i in 0..MAX_PROCESSES {
            if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
                continue;
            }
            let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
            if !ptr.is_null() {
                if let Some(result) = f(unsafe { &*ptr }) {
                    return Some(result);
                }
            }
        }
        None
    })
}

/// Collect PIDs matching a predicate.
///
/// Two-phase: scan with IRQs disabled (no allocation), then collect PIDs
/// into a Vec with IRQs enabled. Safe because PIDs are just u32 values
/// copied out during the scan.
pub fn collect_pids<F: FnMut(&Process) -> bool>(mut pred: F) -> Vec<Pid> {
    // Phase 1: scan into fixed-size stack buffer (no heap allocation)
    let mut buf = [0u32; MAX_PROCESSES];
    let mut count = 0usize;
    with_irqs_disabled(|| {
        for i in 0..MAX_PROCESSES {
            if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
                continue;
            }
            let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
            if !ptr.is_null() {
                let p = unsafe { &*ptr };
                if pred(p) && count < MAX_PROCESSES {
                    buf[count] = p.pid;
                    count += 1;
                }
            }
        }
    });
    // Phase 2: copy to Vec with IRQs enabled (safe to allocate)
    buf[..count].to_vec()
}

/// Collect (PID, thread_id, extra_field) tuples matching a predicate.
///
/// Same two-phase approach as `collect_pids` but captures additional fields.
/// Stack buffer holds up to MAX_PROCESSES entries.
pub fn collect_process_info<T: Copy + Default, F>(mut f: F) -> Vec<T>
where
    F: FnMut(&Process) -> Option<T>,
{
    let mut buf: [core::mem::MaybeUninit<T>; MAX_PROCESSES] = unsafe {
        core::mem::MaybeUninit::uninit().assume_init()
    };
    let mut count = 0usize;
    with_irqs_disabled(|| {
        for i in 0..MAX_PROCESSES {
            if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
                continue;
            }
            let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
            if !ptr.is_null() {
                if let Some(val) = f(unsafe { &*ptr }) {
                    if count < MAX_PROCESSES {
                        buf[count] = core::mem::MaybeUninit::new(val);
                        count += 1;
                    }
                }
            }
        }
    });
    let mut result = Vec::with_capacity(count);
    for item in &buf[..count] {
        result.push(unsafe { item.assume_init() });
    }
    result
}

/// Number of active processes.
pub fn process_count() -> usize {
    let mut count = 0;
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) == slot_state::ACTIVE {
            count += 1;
        }
    }
    count
}

// ── Thread PID map and lazy regions (unchanged) ─────────────────────────

/// Maps kernel thread IDs to PIDs for CLONE_THREAD children.
/// Needed because thread clones share the parent's ProcessInfo page, so
/// read_current_pid() would return the parent's PID.
pub static THREAD_PID_MAP: Spinlock<BTreeMap<usize, Pid>> =
    Spinlock::new(BTreeMap::new());

/// Global lazy region table, keyed by PID then by start_va.
/// The inner BTreeMap allows O(log n) range lookups via `range(..=va).next_back()`.
/// Stored separately from Process to avoid aliasing/corruption issues
/// with &mut Process references from current_process().
pub static LAZY_REGION_TABLE: Spinlock<BTreeMap<Pid, BTreeMap<usize, LazyRegion>>> =
    Spinlock::new(BTreeMap::new());

pub fn register_thread_pid(tid: usize, pid: Pid) {
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().insert(tid, pid);
    });
}

pub fn unregister_thread_pid(tid: usize) {
    with_irqs_disabled(|| {
        THREAD_PID_MAP.lock().remove(&tid);
    });
}
