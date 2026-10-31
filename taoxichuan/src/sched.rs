//! This kernel as `akuma-sched-amd64` sees it: the scheduler's machine half, shared with the metal kernel
//! (`amd64/src/sched.rs`), over Taoxichuan's own state.
//!
//! The crate performs a switch - page-table root, ring-3 trap stack, FS/GS bases, FP/SIMD state - and decides
//! `can_run`; the [`Tx`] platform answers what is this kernel's: roots come from the task record (so an `exec`
//! that swaps one needs no second write), the trap stack from the per-slot table in `main.rs`, the FS base from
//! `child::SLOT_FS`, and the syscall band and the dispatcher's task record are installed in [`Platform::after_install`].
//! The user-context record is `()`: the syscall stub saves registers on the slot's own band, not in a table.
//!
//! Slot space: `0..NSLOTS` are the ring (and the pool's tids), `NSLOTS + cpu` is core `cpu`'s idle thread
//! (`exec_twin::idle_tid`). Every idle machine is marked `idle` and pinned to its core, so the crate's `can_run`
//! never offers it to a picker and `idle_slot` is the only way onto it.

use core::sync::atomic::Ordering;

use akuma_sched_amd64::{Machine, Platform};

/// Ring slots plus one idle thread per core.
pub const MAX_TASKS: usize = crate::child::NSLOTS + crate::smp::MAX_CPUS;

/// The machine table, indexed by crate thread slot. Every writer is kernel code under the kernel lock.
static MACHINE: [Machine<()>; MAX_TASKS] = [const { Machine::new(()) }; MAX_TASKS];

/// This kernel.
pub struct Tx;

impl Platform for Tx {
    type User = ();
    const STACK_SIZE: usize = akuma_ring3_amd64::SYSCALL_STACK_SIZE;
    /// No freed-root ring here: a root frame can be freed and reissued, so the write is also the flush.
    const RELOAD_SAME_ROOT: bool = true;

    fn machines() -> &'static [Machine<()>] {
        &MACHINE
    }
    fn new_user() {}
    fn cpu_index() -> usize {
        akuma_ring3_amd64::cpu_index()
    }
    fn current_task() -> usize {
        crate::exec_twin::current_slot()
    }
    fn kernel_root() -> u64 {
        crate::smp::kernel_root()
    }
    fn active_root() -> u64 {
        akuma_boot_amd64::paging::active_root()
    }
    unsafe fn activate(root: u64) {
        // SAFETY: the trait's contract - a live PML4 that maps the kernel half.
        unsafe { core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags)) };
    }
    fn set_kernel_stack(top: u64) {
        crate::set_tss_rsp0(top);
    }
    fn set_current_uctx(_user: *mut ()) {}
    fn user_bases(slot: usize, _user: &()) -> (u64, u64) {
        (crate::child::slot_fs_of(slot), 0)
    }
    fn space_root_of(slot: usize, _recorded: u64) -> u64 {
        crate::child::sched_root_of(slot)
    }
    fn trap_top_of(slot: usize, _recorded: u64) -> u64 {
        crate::trap_top_for(slot)
    }
    fn lock_held() -> bool {
        crate::smp::lock_held()
    }
    fn after_install(_from: usize, to: usize) {
        // The incoming task's syscall band, and the record the dispatcher's `task()` reads: a task
        // resumed after another ran on this core must not find the resumer's (the exec'd shell's
        // writev loop reading zeros out of the wrong address space, boot pg-2). The idle threads
        // (`to >= NSLOTS`) run kernel code only and leave the previous task's record armed.
        akuma_ring3_amd64::set_syscall_stack_top(crate::child::sched_kstack_top(to));
        if to < crate::child::NSLOTS {
            crate::set_task_slot(crate::child::sched_task_slot_of(to));
        }
    }
}

/// Mark core `cpu`'s idle thread: never picked by a scan, pinned to its core.
pub fn mark_idle(tid: usize, cpu: usize) {
    if let Some(m) = MACHINE.get(tid) {
        m.idle.store(true, Ordering::Relaxed);
        m.daemon.store(true, Ordering::Relaxed);
        m.pinned.store(cpu as u32, Ordering::Relaxed);
    }
}

/// Reset a claimed slot's FP state to the power-on image (`prepare_task_slot`).
pub fn reset_fx(slot: usize) {
    if let Some(m) = MACHINE.get(slot) {
        m.fx.set(akuma_sched_amd64::FxArea::initial());
    }
}
