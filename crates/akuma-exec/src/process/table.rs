use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicPtr, Ordering};
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, LazyRegion};
use crate::runtime::with_irqs_disabled;

/// Maximum number of concurrent processes.
pub const MAX_PROCESSES: usize = 256;

/// Next available PID (monotonically increasing, never recycled)
pub static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Slot states for the lock-free process table.
pub mod slot_state {
    pub const FREE: u8 = 0;
    pub const ACTIVE: u8 = 1;
}

/// Per-slot state: FREE or ACTIVE.
static SLOT_STATES: [AtomicU8; MAX_PROCESSES] = {
    const INIT: AtomicU8 = AtomicU8::new(slot_state::FREE);
    [INIT; MAX_PROCESSES]
};

/// Per-slot process pointer. Non-null when ACTIVE, null when FREE.
/// Points to a heap-allocated Process (from Box::into_raw).
static PROCESS_SLOTS: [AtomicPtr<Process>; MAX_PROCESSES] = {
    const INIT: AtomicPtr<Process> = AtomicPtr::new(core::ptr::null_mut());
    [INIT; MAX_PROCESSES]
};

/// Register a process in the table (takes ownership via Box).
///
/// Finds a free slot via CAS, stores the Process pointer.
/// The Process is kept alive until `unregister_process` reclaims it.
pub fn register_process(_pid: Pid, proc: Box<Process>) {
    let ptr = Box::into_raw(proc);
    // Claim a free slot
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].compare_exchange(
            slot_state::FREE,
            slot_state::ACTIVE,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ).is_ok() {
            PROCESS_SLOTS[i].store(ptr, Ordering::Release);
            return;
        }
    }
    // No free slot — reclaim the Box to avoid leak, then panic
    unsafe { drop(Box::from_raw(ptr)); }
    panic!("Process table full ({} slots)", MAX_PROCESSES);
}

/// Unregister a process from the table.
///
/// Returns the owned Box<Process> so the caller controls when it is dropped.
/// Dropping the Box triggers UserAddressSpace::drop() to free all physical pages.
pub fn unregister_process(pid: Pid) -> Option<Box<Process>> {
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
            continue;
        }
        let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
        if ptr.is_null() {
            continue;
        }
        if unsafe { (*ptr).pid } == pid {
            // Found it — swap to null and mark FREE
            let old = PROCESS_SLOTS[i].swap(core::ptr::null_mut(), Ordering::AcqRel);
            SLOT_STATES[i].store(slot_state::FREE, Ordering::Release);
            if !old.is_null() {
                return Some(unsafe { Box::from_raw(old) });
            }
        }
    }
    None
}

/// Look up a process by PID. Returns a raw pointer (lock-free read).
///
/// The pointer is valid as long as the process remains registered.
/// Callers must not hold the pointer across `unregister_process`.
pub fn get_process_ptr(pid: Pid) -> Option<*mut Process> {
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
/// Lock-free: scans the slot array, reads each pointer atomically.
/// The callback receives `(slot_index, &Process)`.
#[inline]
pub fn for_each_process<F: FnMut(&Process)>(mut f: F) {
    for i in 0..MAX_PROCESSES {
        if SLOT_STATES[i].load(Ordering::Relaxed) != slot_state::ACTIVE {
            continue;
        }
        let ptr = PROCESS_SLOTS[i].load(Ordering::Acquire);
        if !ptr.is_null() {
            f(unsafe { &*ptr });
        }
    }
}

/// Iterate all active processes, calling `f` for each. Returns early if `f` returns Some.
#[inline]
pub fn find_process<T, F: FnMut(&Process) -> Option<T>>(mut f: F) -> Option<T> {
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
}

/// Collect PIDs matching a predicate (lock-free scan).
pub fn collect_pids<F: FnMut(&Process) -> bool>(mut pred: F) -> Vec<Pid> {
    let mut pids = Vec::new();
    for_each_process(|p| {
        if pred(p) {
            pids.push(p.pid);
        }
    });
    pids
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
