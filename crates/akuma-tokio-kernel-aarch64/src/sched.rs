//! The hooked scheduler's machine half on aarch64: `akuma-threading`'s
//! `X86ArchHooks` (the name is the hooked path's, not the ISA's), the thread
//! runtime and process hooks, and the idle thread.
//!
//! What a switch must install here is small. An EL0 trap saves the whole user
//! register file (FP included) in a frame at the top of the thread's own kernel
//! stack, and the crossing back restores it from there, so `TPIDR_EL0`, `SP_EL0`
//! and the vector registers need no per-switch work; the hooked switch saves the
//! callee-saved kernel state (`x19`-`x30`, `sp`, `DAIF`, `d8`-`d15`). What is left
//! is the page-table root (`TTBR0_EL1`, through the checked
//! `akuma_boot_aarch64::user::switch_to`), the thread's name (`TPIDRRO_EL0`) and
//! the task record the syscall path acts on.
//!
//! Slot space, as on amd64: `0..NSLOTS` are the spawn ring (slot 0 the boot
//! thread), `NSLOTS + cpu` is core `cpu`'s idle thread.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

use akuma_exec::process::UserContext;
use akuma_primitives::cell::BorrowCell;
use akuma_primitives::safe_print;
use akuma_threading::{ProcessHooks, ThreadConfig, ThreadRuntime, X86ArchHooks};

use crate::mm::{MAX_CPUS, NSLOTS};

/// One thread's kernel stack. Its top holds the EL0 trap frame.
pub const KSTACK_BYTES: usize = 64 * 1024;
const MAX_TASKS: usize = NSLOTS + MAX_CPUS;

/// Each slot's kernel-stack top (`0`: none yet). Leaked once per slot on first
/// use and reused when the slot is recycled.
static KSTACK_TOP: [AtomicUsize; MAX_TASKS] = [const { AtomicUsize::new(0) }; MAX_TASKS];
/// Core-pinned idle threads, which no picker may offer.
static IDLE: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];
/// The EL0 register file `fork_process`/`clone_thread` wrote for a new slot,
/// or the caller's capture at its last syscall.
static UCTX: BorrowCell<[Option<UserContext>; MAX_TASKS]> = BorrowCell::new([const { None }; MAX_TASKS]);
/// The task record a ring slot acts on (`0`: the boot record).
static TASK_OF: [AtomicUsize; NSLOTS] = [const { AtomicUsize::new(0) }; NSLOTS];
/// The pid a ring slot runs (`0`: none).
static PID_OF: [AtomicU32; NSLOTS] = [const { AtomicU32::new(0) }; NSLOTS];

/// The slot this core runs.
#[must_use]
pub fn current_slot() -> usize {
    akuma_primitives::preempt::current_tid()
}

/// This core runs `slot` from now on.
pub fn set_current_slot(slot: usize) {
    akuma_boot_aarch64::user::set_current_thread(slot);
}

/// Core `cpu`'s idle thread.
#[must_use]
pub const fn idle_tid(cpu: usize) -> usize {
    NSLOTS + cpu
}

/// The record ring slot `slot` acts on.
#[must_use]
pub fn task_of(slot: usize) -> usize {
    TASK_OF.get(slot).map_or(0, |t| t.load(Relaxed))
}

/// The pid ring slot `slot` runs.
#[must_use]
pub fn pid_of(slot: usize) -> Option<u32> {
    PID_OF.get(slot).map(|p| p.load(Relaxed)).filter(|&p| p != 0)
}

/// Bind ring slot `slot` to `pid` on task record `record`.
pub fn bind(slot: usize, pid: u32, record: usize) {
    if slot < NSLOTS {
        TASK_OF[slot].store(record, Relaxed);
        PID_OF[slot].store(pid, Relaxed);
    }
}

/// The top of `slot`'s kernel stack, once it has one.
#[must_use]
pub fn kstack_top(slot: usize) -> Option<u64> {
    KSTACK_TOP.get(slot).map(|t| t.load(Relaxed) as u64).filter(|&t| t != 0)
}

fn switch_to(_from: usize, to: usize) {
    let record = if to < NSLOTS { task_of(to) } else { 0 };
    let root = crate::mm::record_root(record);
    if !akuma_boot_aarch64::user::switch_to(root) {
        safe_print!(96, "[sched] slot {} has root {:#x} without the kernel's part\n", to, root);
        crate::halt();
    }
    if to < NSLOTS {
        crate::mm::set_task_slot(record);
    }
}

fn can_run(slot: usize) -> bool {
    IDLE.get(slot).is_some_and(|i| !i.load(Relaxed))
}

/// A slot's kernel stack, leaked on first use (fallibly) and kept for its
/// next occupant; the slot's per-thread state reset.
pub fn prepare(slot: usize) -> Option<usize> {
    let top = KSTACK_TOP.get(slot)?;
    if top.load(Relaxed) == 0 {
        let mut v: alloc::vec::Vec<u128> = alloc::vec::Vec::new();
        v.try_reserve_exact(KSTACK_BYTES / 16).ok()?;
        v.resize(KSTACK_BYTES / 16, 0);
        let stack = v.leak();
        top.store(stack.as_ptr() as usize + KSTACK_BYTES, Relaxed);
    }
    IDLE[slot].store(false, Relaxed);
    UCTX.with_mut(|u| u[slot] = None);
    Some(top.load(Relaxed))
}

fn write_user_context(slot: usize, ctx: &UserContext) {
    if slot < MAX_TASKS {
        UCTX.with_mut(|u| u[slot] = Some(*ctx));
    }
}

fn read_user_context(slot: usize) -> Option<UserContext> {
    UCTX.with(|u| u.get(slot).copied().flatten())
}

fn hooks() -> X86ArchHooks {
    X86ArchHooks {
        switch_to,
        current_slot,
        set_current_slot,
        idle_slot: || idle_tid(crate::mm::cpu_index()),
        can_run,
        // No kernel lock yet: one core, IRQs masked in the kernel.
        transfer_lock_depth: |_, _| {},
        allow_tick: akuma_cpu::park::irq_window,
        write_user_context,
        read_user_context,
        prepare_task_slot: prepare,
    }
}

/// Core 0's idle thread: let the tick in, run whatever became ready, else wait
/// for an interrupt. `wfi` wakes on a pending interrupt even while masked, so a
/// tick between the check and the `wfi` is not lost.
extern "C" fn idle_main() -> ! {
    loop {
        akuma_cpu::park::irq_window();
        let due = akuma_threading::wake_is_due(akuma_threading::x86_earliest_wake_time(), crate::fs::uptime_us());
        if due || akuma_threading::x86_runnable_exists() {
            akuma_threading::x86_yield();
        } else {
            akuma_cpu::park::wfi();
        }
    }
}

/// Register the scheduler's tables and seed core 0's idle thread. After the
/// exec runtime (`exec::init`), before any thread but the boot thread exists.
pub fn init() {
    akuma_threading::register(
        ThreadRuntime {
            uptime_us: crate::fs::uptime_us,
            trigger_sgi: |_| {},
            wake_core: |_| {},
            wake_remote_idle: || false,
            end_of_interrupt: akuma_gic::end_of_interrupt,
            print_str: akuma_primitives::console::print_str,
        },
        ThreadConfig {
            reserved_threads: 0,
            kernel_stack_size: KSTACK_BYTES,
            system_thread_stack_size: KSTACK_BYTES,
            user_thread_stack_size: KSTACK_BYTES,
            boot_stack_base: 0,
            boot_stack_top: 0,
            enable_stack_canaries: false,
            stack_canary: 0,
            canary_words: 0,
            network_thread_ratio: 0,
            prioritize_never_scheduled: false,
            deferred_thread_cleanup: false,
            thread_cleanup_cooldown_us: 0,
            syscall_debug_info_enabled: false,
            enable_sgi_debug_prints: false,
        },
    );
    akuma_threading::register_process_hooks(ProcessHooks {
        clear_draining: |_| {},
        drain_in_flight: |_| false,
        lifecycle_trace_on: || false,
        pid_for_thread: pid_of,
        find_pid_by_thread: pid_of,
        // Signals arrive with A5.8.
        is_current_interrupted: || false,
        proc_dump_info: |_| None,
        dump_orphan_processes: || {},
    });
    akuma_threading::register_x86_arch_hooks(hooks());
    set_current_slot(0);
    let idle = idle_tid(0);
    let Some(top) = prepare(idle) else {
        safe_print!(64, "  [FATAL] no stack for the idle thread\n");
        crate::halt();
    };
    IDLE[idle].store(true, Relaxed);
    akuma_threading::x86_seed_entry(idle, top, idle_main);
    akuma_threading::x86_publish(idle);
    akuma_user_access::set_prefault_hook(akuma_tokio_kernel_core::lazy::prefault::<crate::mm::Aarch64>);
}
