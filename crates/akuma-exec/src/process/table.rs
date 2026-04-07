use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::AtomicU32;
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, LazyRegion};
use crate::sync::RwSpinlock;
use crate::runtime::with_irqs_disabled;

/// Next available PID
pub static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Process table: maps PID to shared Process wrapped in per-process lock.
///
/// Uses `RwSpinlock` for the outer table: readers (lookups, iteration) can
/// proceed concurrently; only insert/remove takes the write lock.
/// Each Process is behind its own `Spinlock` inside an `Arc` for safe sharing.
///
/// When `unregister_process` is called, the `Arc<Spinlock<Process>>` is removed.
/// When the last `Arc` reference is dropped, `Process::drop()` runs, which triggers
/// `UserAddressSpace::drop()` to free all physical pages.
pub static PROCESS_TABLE: RwSpinlock<BTreeMap<Pid, Arc<Spinlock<Process>>>> =
    RwSpinlock::new(BTreeMap::new());

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

/// Register a process in the table (takes ownership)
pub fn register_process(pid: Pid, proc: Box<Process>) {
    let arc = Arc::new(Spinlock::new(*proc));
    with_irqs_disabled(|| {
        let t0 = crate::process::diag::lock_timer_start();
        PROCESS_TABLE.write().insert(pid, arc);
        crate::process::diag::lock_timer_end("register", t0);
    })
}

/// Unregister a process from the table
///
/// Returns the Arc<Spinlock<Process>> so the caller controls when it is dropped.
/// When the last Arc reference is dropped, Process::drop() runs, freeing all
/// memory including the UserAddressSpace and all its physical pages.
pub fn unregister_process(pid: Pid) -> Option<Arc<Spinlock<Process>>> {
    with_irqs_disabled(|| {
        let t0 = crate::process::diag::lock_timer_start();
        let result = PROCESS_TABLE.write().remove(&pid);
        crate::process::diag::lock_timer_end("unregister", t0);
        result
    })
}

/// Get an Arc handle to a process by PID (read-locks the table briefly).
pub fn get_process(pid: Pid) -> Option<Arc<Spinlock<Process>>> {
    with_irqs_disabled(|| {
        PROCESS_TABLE.read().get(&pid).cloned()
    })
}

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
