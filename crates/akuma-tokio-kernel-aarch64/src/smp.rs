//! The secondary cores and the kernel lock on aarch64.
//!
//! The lock is `akuma_tokio_kernel_core::bkl`, the one the amd64 half runs
//! under: every trap into the kernel takes it, user mode and a halted core run
//! without it, and a switch hands the depth to the next thread.
//!
//! Secondaries start through PSCI `CPU_ON` at `akuma_boot_aarch64::secondary`'s
//! trampoline, which enters [`secondary_start`] on the core's own 64 KiB stack
//! with the boot tables live. That context becomes the core's idle thread
//! (adopted, `ON_CPU` latched, before the core starts, so no peer can pick a
//! stack that is in use). Each core installs the vectors, wakes its GIC
//! redistributor, arms its own CNTV tick and runs the idle loop.
//!
//! What amd64 needs IPIs for, this machine mostly does not: TLB maintenance is
//! broadcast to the inner-shareable domain (`tlbi ...is`), so there is no
//! shootdown handshake, and nothing a waiting core must service. The one IPI is
//! the wake: a thread readied on one core rings the scheduler SGI at a halted
//! peer, whose `wfi` ends.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use akuma_tokio_kernel_core::bkl::{self, BklCpu};

use crate::mm::{MAX_CPUS, cpu_index};

/// This machine as the core's lock sees it.
pub struct Aarch64Cpu;

impl BklCpu for Aarch64Cpu {
    fn index() -> usize {
        cpu_index()
    }
}

/// Take the kernel lock (or nest).
pub fn enter() {
    bkl::enter::<Aarch64Cpu>();
}

/// Leave one level of the kernel lock.
pub fn leave() {
    bkl::leave::<Aarch64Cpu>();
}

/// Release the kernel lock completely (user mode is next).
pub fn leave_all() {
    let _ = bkl::leave_all::<Aarch64Cpu>();
}

/// Run `f` with the kernel lock released.
pub fn unlocked<R>(f: impl FnOnce() -> R) -> R {
    bkl::unlocked::<Aarch64Cpu, R>(f)
}

/// The scheduler's `transfer_lock_depth`.
pub fn transfer_depth(from: usize, to: usize) {
    bkl::transfer_depth::<Aarch64Cpu>(from, to);
}

/// Cores running the kernel: the boot core plus every secondary that reported in.
static ONLINE: AtomicUsize = AtomicUsize::new(1);
/// Which secondaries reached the idle loop.
static UP: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// Which cores are in `wfi` with nothing to run.
static HALTED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Cores running the kernel.
#[must_use]
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire)
}

fn is_up(idx: usize) -> bool {
    idx == 0 || UP.get(idx).is_some_and(|u| u.load(Ordering::Acquire))
}

/// Halt this core until an interrupt is pending - the tick, a device, or a wake
/// SGI - with the kernel lock released. Does not halt if work is already
/// visible. The interrupt is taken by the caller's next IRQ window.
pub fn park_halt() {
    let me = cpu_index();
    unlocked(|| {
        HALTED[me].store(true, Ordering::SeqCst);
        if !akuma_threading::x86_runnable_exists() {
            akuma_cpu::park::wfi();
        }
        HALTED[me].store(false, Ordering::SeqCst);
    });
}

/// The `trigger_sgi` hook: a wake just made a thread READY, so ring one halted
/// peer. A no-op on one core or when no peer is halted.
pub fn kick_halted_core() {
    if online_cpus() <= 1 {
        return;
    }
    let me = cpu_index();
    for (idx, halted) in HALTED.iter().enumerate() {
        if idx != me && is_up(idx) && halted.load(Ordering::SeqCst) {
            #[cfg(feature = "smp")]
            akuma_gic::trigger_sgi_core(idx as u32, akuma_gic::SGI_SCHEDULER);
            return;
        }
    }
}

#[cfg(feature = "smp")]
mod start {
    use super::{AtomicUsize, MAX_CPUS, ONLINE, Ordering, UP};
    use akuma_primitives::safe_print;
    use akuma_boot_aarch64::secondary;

    /// The redistributor frames' base (PA) and the stride between cores' frames
    /// (`RD_base` + `SGI_base`, 64 KiB each).
    static GICR_BASE: AtomicUsize = AtomicUsize::new(0);
    const GICR_STRIDE: usize = 0x2_0000;
    const GICR_SGI_OFFSET: usize = 0x1_0000;
    /// How long a started core gets to reach its idle loop.
    const ONLINE_BUDGET_US: u64 = 500_000;

    /// Start cores `1..n` where `n = min(cap, cpus)`: each one's idle thread
    /// adopted first, then `CPU_ON` at the trampoline, then a bounded wait for it
    /// to report in. Core `i` is `MPIDR_EL1.Aff0 = i` (QEMU virt and Firecracker
    /// both number a flat cluster). Returns how many started.
    pub fn start_secondaries(cap: usize, cpus: usize, gicr_pa: usize) -> usize {
        GICR_BASE.store(gicr_pa, Ordering::Relaxed);
        let n = cap.min(cpus).min(MAX_CPUS).min(secondary::MAX_CORES);
        if n <= 1 {
            return 0;
        }
        // Everything a secondary reads before it takes the lock, published
        // before any of them runs.
        akuma_cpu::barrier::dsb_sy();
        let mut started = 0;
        for (idx, up) in UP.iter().enumerate().take(n).skip(1) {
            crate::sched::adopt_idle(idx);
            let r = akuma_psci::call(akuma_psci::CPU_ON, idx as u64, secondary::entry_pa(), idx as u64);
            if r != 0 {
                safe_print!(64, "  smp: CPU_ON core {} refused: {}\n", idx, r);
                continue;
            }
            let t0 = crate::fs::uptime_us();
            // The secondary takes the kernel lock to report in: wait without it.
            let up = super::unlocked(|| {
                while !up.load(Ordering::Acquire) {
                    if crate::fs::uptime_us() - t0 > ONLINE_BUDGET_US {
                        return false;
                    }
                    core::hint::spin_loop();
                }
                true
            });
            if up {
                started += 1;
            } else {
                safe_print!(64, "  smp: core {} did not come online\n", idx);
            }
        }
        ONLINE.store(1 + started, Ordering::Release);
        started
    }

    /// A secondary's first Rust, on its trampoline stack: name the core's idle
    /// thread, take the vectors, its redistributor and its tick, then idle.
    pub fn secondary_start(core: u64) -> ! {
        let idx = core as usize;
        crate::sched::set_current_slot(crate::sched::idle_tid(idx));
        akuma_vectors_aarch64::install();
        akuma_gic::secondary_init(
            idx,
            akuma_gic::RedistributorLayout {
                base_pa: GICR_BASE.load(Ordering::Relaxed),
                stride: GICR_STRIDE,
                sgi_offset: GICR_SGI_OFFSET,
            },
        );
        akuma_timer::arm_periodic_tick();
        super::enter();
        safe_print!(64, "  smp: core {} online\n", idx);
        UP[idx].store(true, Ordering::Release);
        super::leave();
        crate::sched::idle_main()
    }
}

#[cfg(feature = "smp")]
pub use start::{secondary_start, start_secondaries};

/// Without `smp` no core is ever started; a stray one parks.
#[cfg(not(feature = "smp"))]
pub fn secondary_start(_core: u64) -> ! {
    loop {
        akuma_cpu::park::wfe();
    }
}
