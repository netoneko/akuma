use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::sync::atomic::AtomicU32;
use spinning_top::Spinlock;

use crate::process::Process;
use crate::process::types::{Pid, LazyRegion};
use crate::runtime::with_irqs_disabled;

/// Next available PID
pub static NEXT_PID: AtomicU32 = AtomicU32::new(1);

/// Process table: maps PID to owned Process
///
/// Processes are stored here when created and removed when they exit.
/// Syscall handlers use read_current_pid() + lookup_process() to find
/// the calling process.
///
/// IMPORTANT: The table owns the Process via Box. When unregister_process
/// is called, the Box<Process> is returned and dropped, which triggers
/// UserAddressSpace::drop() to free all physical pages. This prevents
/// memory leaks when processes exit.
pub static PROCESS_TABLE: Spinlock<BTreeMap<Pid, Box<Process>>> =
    Spinlock::new(BTreeMap::new());

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
    with_irqs_disabled(|| {
        PROCESS_TABLE.lock().insert(pid, proc);
    })
}

/// Unregister a process from the table
///
/// Returns the owned Process so it can be dropped, freeing all memory
/// including the UserAddressSpace and all its physical pages.
pub fn unregister_process(pid: Pid) -> Option<Box<Process>> {
    with_irqs_disabled(|| {
        PROCESS_TABLE.lock().remove(&pid)
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
