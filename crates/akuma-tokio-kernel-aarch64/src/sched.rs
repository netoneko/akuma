//! The hooked scheduler's machine half on aarch64: `akuma-threading`'s
//! `ArchHooks` (the name is the hooked path's, not the ISA's), the thread
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

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use akuma_exec::process::UserContext;
use akuma_primitives::cell::BorrowCell;
use akuma_primitives::safe_print;
use akuma_threading::{ProcessHooks, ThreadConfig, ThreadRuntime, ArchHooks};
use akuma_tokio_kernel_core::proc;
use akuma_vectors_aarch64::Frame;

use crate::mm::MAX_CPUS;
use akuma_tokio_kernel_core::proc::NSLOTS;

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
/// A slot's FP/SIMD state as of its last `clone`. `UserContext` carries the
/// general registers only, and musl's `svc` declares no vector clobbers, so a
/// fork child must start with its parent's vector registers.
#[derive(Clone, Copy)]
struct Fp {
    q: [u128; 32],
    fpcr: u64,
    fpsr: u64,
}

impl Fp {
    const ZERO: Self = Self { q: [0; 32], fpcr: 0, fpsr: 0 };
}

static FP: BorrowCell<[Fp; MAX_TASKS]> = BorrowCell::new([Fp::ZERO; MAX_TASKS]);

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

/// The top of `slot`'s kernel stack, once it has one.
#[must_use]
pub fn kstack_top(slot: usize) -> Option<u64> {
    KSTACK_TOP.get(slot).map(|t| t.load(Relaxed) as u64).filter(|&t| t != 0)
}

fn switch_to(_from: usize, to: usize) {
    let root = proc::sched_root_of::<crate::mm::Aarch64>(to);
    if !akuma_boot_aarch64::user::switch_to(root) {
        safe_print!(96, "[sched] slot {} has root {:#x} without the kernel's part\n", to, root);
        crate::halt();
    }
    // The idle threads run kernel code only and leave the record armed.
    if to < NSLOTS {
        crate::mm::set_task_slot(proc::sched_task_slot_of(to));
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
    FP.with_mut(|f| f[slot] = Fp::ZERO);
    Some(top.load(Relaxed))
}

/// `fork_process`/`clone_thread` writing a new slot's register file, from the
/// creator's context: the creator's FP state goes with it.
fn write_user_context(slot: usize, ctx: &UserContext) {
    if slot < MAX_TASKS {
        UCTX.with_mut(|u| u[slot] = Some(*ctx));
        let from = current_slot();
        if from < MAX_TASKS {
            FP.with_mut(|f| f[slot] = f[from]);
        }
    }
}

/// Record the running slot's EL0 register file from its trap frame: what
/// `fork_process` reads (`read_user_context`) as the child's.
pub fn capture(f: &Frame) {
    let slot = current_slot();
    if slot >= MAX_TASKS {
        return;
    }
    let x = &f.x;
    let ctx = UserContext {
        x0: x[0], x1: x[1], x2: x[2], x3: x[3], x4: x[4], x5: x[5], x6: x[6], x7: x[7],
        x8: x[8], x9: x[9], x10: x[10], x11: x[11], x12: x[12], x13: x[13], x14: x[14], x15: x[15],
        x16: x[16], x17: x[17], x18: x[18], x19: x[19], x20: x[20], x21: x[21], x22: x[22], x23: x[23],
        x24: x[24], x25: x[25], x26: x[26], x27: x[27], x28: x[28], x29: x[29], x30: x[30],
        sp: f.sp,
        pc: f.elr,
        spsr: f.spsr,
        tpidr: f.tpidr,
        ttbr0: 0,
    };
    UCTX.with_mut(|u| u[slot] = Some(ctx));
    FP.with_mut(|s| s[slot] = Fp { q: f.q, fpcr: f.fpcr, fpsr: f.fpsr });
}

/// The EL0 frame a slot crosses into from `ctx`, with the slot's FP state.
#[must_use]
pub fn frame_from(slot: usize, ctx: &UserContext) -> Frame {
    let mut f = Frame::zeroed();
    f.x = [
        ctx.x0, ctx.x1, ctx.x2, ctx.x3, ctx.x4, ctx.x5, ctx.x6, ctx.x7, ctx.x8, ctx.x9, ctx.x10, ctx.x11, ctx.x12,
        ctx.x13, ctx.x14, ctx.x15, ctx.x16, ctx.x17, ctx.x18, ctx.x19, ctx.x20, ctx.x21, ctx.x22, ctx.x23, ctx.x24,
        ctx.x25, ctx.x26, ctx.x27, ctx.x28, ctx.x29, ctx.x30,
    ];
    f.sp = ctx.sp;
    f.elr = ctx.pc;
    f.spsr = ctx.spsr;
    f.tpidr = ctx.tpidr;
    if slot < MAX_TASKS {
        FP.with(|s| {
            f.q = s[slot].q;
            f.fpcr = s[slot].fpcr;
            f.fpsr = s[slot].fpsr;
        });
    }
    f
}

fn read_user_context(slot: usize) -> Option<UserContext> {
    UCTX.with(|u| u.get(slot).copied().flatten())
}

fn hooks() -> ArchHooks {
    ArchHooks {
        switch_to,
        current_slot,
        set_current_slot,
        idle_slot: || idle_tid(crate::mm::cpu_index()),
        can_run,
        transfer_lock_depth: crate::smp::transfer_depth,
        allow_tick: akuma_cpu::park::irq_window,
        write_user_context,
        read_user_context,
        prepare_task_slot: prepare,
    }
}

/// A core's idle thread (core 0's seeded, a secondary's adopted): pump the
/// NIC, let the tick in, run whatever became ready, else halt with the kernel
/// lock released. It runs at lock depth 0 and takes the lock for each piece of
/// kernel work. `wfi` wakes on a pending interrupt even while masked, so a tick
/// between the check and the `wfi` is not lost.
pub extern "C" fn idle_main() -> ! {
    loop {
        crate::smp::enter();
        crate::net::netpoll();
        crate::smp::leave();
        akuma_cpu::park::irq_window();
        let due = akuma_threading::wake_is_due(akuma_threading::earliest_wake_time(), crate::fs::uptime_us());
        if due || akuma_threading::runnable_exists() {
            // The pick and the switch read the shared slot and task tables.
            crate::smp::enter();
            akuma_threading::try_yield();
            crate::smp::leave();
        } else {
            crate::smp::park_halt();
        }
    }
}

/// Core `cpu`'s idle thread is the context it is about to run (a secondary's
/// trampoline stack): mark it idle and adopt it, before the core starts.
#[cfg(feature = "smp")]
pub fn adopt_idle(cpu: usize) {
    let idle = idle_tid(cpu);
    IDLE[idle].store(true, Relaxed);
    akuma_threading::adopt_running_thread(idle);
}

/// Register the scheduler's tables and seed core 0's idle thread. After the
/// exec runtime (`exec::init`), before any thread but the boot thread exists.
pub fn init() {
    akuma_threading::register(
        ThreadRuntime {
            uptime_us: crate::fs::uptime_us,
            trigger_sgi: |_| crate::smp::kick_halted_core(),
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
        pid_for_thread: proc::pid_for_slot,
        find_pid_by_thread: proc::pid_for_slot,
        // Signals arrive with A5.8.
        is_current_interrupted: || false,
        proc_dump_info: |_| None,
        dump_orphan_processes: || {},
    });
    akuma_threading::register_arch_hooks(hooks());
    set_current_slot(0);
    let idle = idle_tid(0);
    let Some(top) = prepare(idle) else {
        safe_print!(64, "  [FATAL] no stack for the idle thread\n");
        crate::halt();
    };
    IDLE[idle].store(true, Relaxed);
    akuma_threading::seed_entry(idle, top, idle_main);
    akuma_threading::publish_slot(idle);
    akuma_user_access::set_prefault_hook(akuma_tokio_kernel_core::lazy::prefault::<crate::mm::Aarch64>);
}
