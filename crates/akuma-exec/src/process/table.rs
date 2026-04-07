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
