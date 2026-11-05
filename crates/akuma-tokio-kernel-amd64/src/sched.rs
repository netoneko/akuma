//! This kernel as `akuma-sched-amd64` sees it: the scheduler's machine half, shared with the metal kernel
//! (`amd64/src/sched.rs`), over Taoxichuan's own state.
//!
//! The crate performs a switch - page-table root, ring-3 trap stack, FS/GS bases, FP/SIMD state - and decides
//! `can_run`; the [`Tx`] platform answers what is this kernel's: roots come from the task record (so an `exec`
//! that swaps one needs no second write), the trap stack from the per-slot table in `main.rs`, the FS base from
//! `child::SLOT_FS`, and the syscall band and the dispatcher's task record are installed in [`Platform::after_install`].
//! The user-context record is [`TxUser`]: the ring-3 capture `fork_process` and `clone_thread` read, and the slot's FS
//! base. The syscall stub saves registers on the slot's own band, not in a table. Since 2026-11-01 the whole
//! `X86ArchHooks` table is `akuma_sched_amd64::hooks::<Tx>()`: stacks, user context and FS base live in the crate's
//! `Machine` table, as on amd64, where this kernel used to keep them in parallel arrays.
//!
//! Slot space: `0..NSLOTS` are the ring (and the pool's tids), `NSLOTS + cpu` is core `cpu`'s idle thread
//! (`exec_twin::idle_tid`). Every idle machine is marked `idle` and pinned to its core, so the crate's `can_run`
//! never offers it to a picker and `idle_slot` is the only way onto it.


use akuma_sched_amd64::{Machine, Platform};

/// Ring slots plus one idle thread per core.
pub const MAX_TASKS: usize = crate::child::NSLOTS + crate::smp::MAX_CPUS;

/// The machine table, indexed by crate thread slot. Every writer is kernel code under the kernel lock.
static MACHINE: [Machine<TxUser>; MAX_TASKS] = [const { Machine::new(TxUser::new()) }; MAX_TASKS];

/// A slot's user-context record.
#[derive(Clone, Copy)]
pub struct TxUser {
    /// The ring-3 register capture (`capture_user_context` on every syscall, or the one
    /// `fork_process`/`clone_thread` wrote for a new slot); `None` until one exists.
    ctx: Option<akuma_exec::process::UserContext>,
    /// `IA32_FS_BASE` for this thread: the MSR is per core, so `switch_to` installs this.
    fs: u64,
}

impl TxUser {
    const fn new() -> Self {
        Self { ctx: None, fs: 0 }
    }
}

/// Record slot `slot`'s FS base (`arch_prctl(ARCH_SET_FS)`, an exec, a new thread's TLS).
pub fn set_slot_fs(slot: usize, fs: u64) {
    if let Some(m) = MACHINE.get(slot) {
        m.uctx.with_mut(|u| u.fs = fs);
    }
}

/// The top of slot `slot`'s kernel stack, once `prepare_task_slot` gave it one.
pub fn kstack_top(slot: usize) -> Option<u64> {
    let s = MACHINE.get(slot)?.stacks.get()?;
    Some((s.kernel_base() + akuma_ring3_amd64::SYSCALL_STACK_SIZE) as u64)
}

/// This kernel.
pub struct Tx;

impl Platform for Tx {
    type User = TxUser;
    const STACK_SIZE: usize = akuma_ring3_amd64::SYSCALL_STACK_SIZE;
    /// No freed-root ring here: a root frame can be freed and reissued, so the write is also the flush.
    const RELOAD_SAME_ROOT: bool = true;

    fn machines() -> &'static [Machine<TxUser>] {
        &MACHINE
    }
    fn new_user() -> TxUser {
        TxUser::new()
    }
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
        // SAFETY: the trait's contract - a live PML4 that maps the kernel half - is `load_cr3`'s.
        unsafe { akuma_pagetable::amd64::load_cr3(root) };
    }
    fn set_kernel_stack(top: u64) {
        crate::set_tss_rsp0(top);
    }
    fn set_current_uctx(_user: *mut TxUser) {}
    fn user_bases(_slot: usize, user: &TxUser) -> (u64, u64) {
        (user.fs, 0)
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
    fn slot_prepared(slot: usize, _stacks: &akuma_sched_amd64::Stacks) {
        // A slot is born owning the kernel lock it was created under (the switch
        // hands the depth over); see `smp::born_locked`.
        crate::smp::born_locked(slot);
    }
    fn set_current_task(slot: usize) {
        crate::exec_twin::set_current_slot(slot);
    }
    fn idle_task() -> usize {
        crate::exec_twin::idle_tid(akuma_ring3_amd64::cpu_index())
    }
    fn transfer_lock_depth(from: usize, to: usize) {
        crate::smp::transfer_depth(from, to);
    }
    fn allow_tick() {
        // One instruction wide, and the tick handler (`timer_trap`) is bounded and
        // allocation-free, so the worst a tick in the window does is count.
        akuma_cpu::park::irq_window();
    }
    fn write_user_context(slot: usize, ctx: &akuma_exec::process::UserContext) {
        // `slot` is not running (the shared spawn path's INITIALIZING discipline).
        if let Some(m) = MACHINE.get(slot) {
            m.uctx.with_mut(|u| u.ctx = Some(*ctx));
        }
    }
    fn read_user_context(slot: usize) -> Option<akuma_exec::process::UserContext> {
        // `None` when the slot never had a capture: a child built from it would
        // launch at whatever the registers happened to hold.
        MACHINE.get(slot)?.uctx.with(|u| u.ctx)
    }
}

/// Mark core `cpu`'s idle thread (the crate's rule; see `akuma_sched_amd64::mark_idle`).
pub fn mark_idle(tid: usize, cpu: usize) {
    if let Some(m) = MACHINE.get(tid) {
        akuma_sched_amd64::mark_idle(m, cpu);
    }
}

#[cfg(feature = "smp")]
/// Core `cpu`'s idle thread is the context it is about to run: mark and adopt it.
pub fn adopt_idle(tid: usize, cpu: usize) {
    mark_idle(tid, cpu);
    akuma_threading::x86_adopt_running_thread(tid);
}
