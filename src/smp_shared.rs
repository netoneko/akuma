//! Real (shared-kernel) SMP — classic symmetric multiprocessing.
//!
//! This is the INVERSE of the multikernel in [`crate::smp`]. There, every core runs
//! a private copy of the kernel over a disjoint RAM partition, with `.data`/`.bss`
//! replicated into per-core pages; cores share nothing but a message ring. Here,
//! ALL cores execute ONE shared kernel image over ONE set of page tables, ONE PMM,
//! ONE heap, and (in later milestones) ONE global run queue, coordinated by real
//! cross-core locks. A `static` in this build is genuinely shared across cores — not
//! replicated — which is the whole point.
//!
//! Everything here is behind `cfg(kernel_smp_shared)` (the `smp-shared` feature);
//! the default and multikernel (`smp`) builds compile none of it, and build.rs makes
//! `smp` and `smp-shared` mutually exclusive.
//!
//! **Milestone M0 (this file):** bring N cores up on the shared boot page tables and
//! the single shared PMM/heap — no isolation, no partitions. Each secondary sets up
//! nothing beyond its own stack, reports itself online in a shared counter, prints
//! over the shared UART (identity-mapped as device via the boot L1[0] block), and
//! parks in `WFE`. The shared scheduler, per-core GIC/timer, and the Big Kernel Lock
//! arrive in later milestones. See docs/archive/SMP_SHARED.md for the running log and
//! docs/reference/subsystems/smp-shared.md for the current-state design.

// The M2c/M3/M4 demo workers + self-test accessors are exercised only by the boot
// self-tests in `process_tests` (compiled out under `no-tests` / `size`). In those
// runtime-only builds (e.g. devbox-smoltcp) they are intentionally unused — suppress the
// dead-code lint there. Test builds still lint dead code normally.
#![cfg_attr(any(feature = "no-tests", kernel_profile_size), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Maximum cores we bring up. Matches the multikernel's `akuma_smp::MAX_CORES` and is
/// comfortably under the `aff0 < 16` single-cluster limit on QEMU `virt`.
const MAX_CORES: usize = 8;

/// Per-core boot/idle stack size shift (64 KiB). This stack backs the secondary's
/// idle thread, so it must hold the full IRQ excursion — the 832-byte trap frame plus
/// the shared `timer_irq_handler` (kernel-timer alarm processing, preemption watchdog,
/// logging) and the scheduler — which is far deeper than M0's WFE park. 16 KiB
/// overflowed here (fault-in-fault while holding the BKL); 64 KiB is ample.
const STACK_SHIFT: usize = 16;

// PSCI (matches crate::smp — QEMU `virt` exposes PSCI over the DTB-declared conduit).
const PSCI_CPU_ON: u64 = 0xC400_0003;

// --- DTB-probed topology, stashed before the heap can overwrite the DTB ------------
static PROBED: AtomicBool = AtomicBool::new(false);
static NUM_CORES: AtomicUsize = AtomicUsize::new(1);
static USE_HVC: AtomicBool = AtomicBool::new(true);
static PROBED_MPIDRS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Count of secondaries that have reached their online barrier. Genuinely shared
/// (not replicated) — a plain kernel static IS cross-core state in this build.
static ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Bitmask of cores currently halted in their idle loop (bit `aff0` set = idle). Used
/// by [`wake_remote_idle`] to nudge an idle core to pick up a just-woken thread
/// promptly (M4 cross-core wakeup) instead of waiting for its ~10 ms timer tick.
static CORE_IDLE_MASK: AtomicU32 = AtomicU32::new(0);

/// Runtime toggle (default on) for the M5b Stage 4a optimization: drop the BKL around a
/// file-backed fault's block-I/O fill pass so peer cores can enter the kernel while this
/// core waits on disk. Exposed so a boot self-test can A/B-measure its effect on
/// cross-core BKL contention (see `test_smp_shared_fault_parallelism`).
static FAULT_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the file-fault block-I/O BKL-drop (M5b Stage 4a) is currently enabled.
#[inline]
pub fn fault_bkl_drop_enabled() -> bool {
    FAULT_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the file-fault block-I/O BKL-drop at runtime (A/B measurement only).
pub fn set_fault_bkl_drop_enabled(on: bool) {
    FAULT_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

/// Mark this core idle/busy in [`CORE_IDLE_MASK`] (called around the idle WFI).
#[inline]
fn set_core_idle(core: usize, idle: bool) {
    let bit = 1u32 << (core as u32);
    if idle {
        CORE_IDLE_MASK.fetch_or(bit, Ordering::Release);
    } else {
        CORE_IDLE_MASK.fetch_and(!bit, Ordering::Release);
    }
}

/// Ring one idle peer core's scheduler SGI so it wakes from WFI, reschedules, and can
/// pick up a just-woken READY thread now rather than on its next timer tick. Best-effort
/// (a race that finds no idle core just falls back to the timer). Called from `wake()`
/// via the `wake_remote_idle` runtime hook. No-op unless a peer is idle.
pub fn wake_remote_idle() {
    let self_aff0 = read_mpidr() & 0xff;
    let mask = CORE_IDLE_MASK.load(Ordering::Acquire) & !(1u32 << self_aff0);
    if mask == 0 {
        return; // no idle peer
    }
    let target = mask.trailing_zeros(); // lowest idle peer core
    crate::gic::trigger_sgi_core(target, SCHED_SGI);
}

unsafe extern "C" {
    /// The secondary trampoline (asm, `.text.boot`, defined below). Taking its
    /// address gives the identity-mapped PA to hand PSCI `CPU_ON` as the entry point.
    fn secondary_entry_shared();
}

#[inline]
fn read_mpidr() -> u64 {
    let v: u64;
    // SAFETY: reading the affinity register has no side effects.
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)) }
    v
}

/// Issue a PSCI call over the platform conduit (`hvc`/`smc`). Returns x0 (0 = SUCCESS).
fn psci_call(use_hvc: bool, func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SAFETY: a standard SMCCC call; we clobber the caller-saved GPR range (x1–x17).
    unsafe {
        if use_hvc {
            core::arch::asm!(
                "hvc #0",
                inout("x0") func => ret,
                in("x1") a1, in("x2") a2, in("x3") a3,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        } else {
            core::arch::asm!(
                "smc #0",
                inout("x0") func => ret,
                in("x1") a1, in("x2") a2, in("x3") a3,
                lateout("x4") _, lateout("x5") _, lateout("x6") _, lateout("x7") _,
                lateout("x8") _, lateout("x9") _, lateout("x10") _, lateout("x11") _,
                lateout("x12") _, lateout("x13") _, lateout("x14") _, lateout("x15") _,
                lateout("x16") _, lateout("x17") _,
                options(nostack),
            );
        }
    }
    ret
}

/// Resolve the DTB pointer the way `detect_memory` does (QEMU does not set x0 for flat
/// kernels; the DTB sits 2 MiB-aligned above the image at 0x4020_0000).
fn resolve_dtb(dtb_ptr: usize) -> usize {
    const DTB_LOCATION: usize = 0x4020_0000;
    const FDT_MAGIC_LE: u32 = 0xedfe0dd0;
    if dtb_ptr != 0 {
        return dtb_ptr;
    }
    // SAFETY: speculative read of a u32 at a fixed RAM address; magic-checked.
    let magic = unsafe { core::ptr::read_volatile(DTB_LOCATION as *const u32) };
    if magic == FDT_MAGIC_LE { DTB_LOCATION } else { 0 }
}

/// `true` if the PSCI conduit is `hvc` (QEMU `virt`); default to it when absent.
fn psci_is_hvc(fdt: &fdt::Fdt) -> bool {
    fdt.find_node("/psci")
        .and_then(|n| n.property("method"))
        .is_none_or(|p| p.value.starts_with(b"hvc"))
}

/// Parse `/cpus` + `/psci` from the DTB and stash the topology. Called from
/// `kernel_main` before heap init (the heap can land on the DTB on large-RAM configs).
pub fn probe_dtb(dtb_ptr: usize) {
    let resolved = resolve_dtb(dtb_ptr);
    if resolved == 0 {
        crate::safe_print!(64, "[SMP-shared] no DTB; staying single-core\n");
        return;
    }
    // SAFETY: `resolved` points at a validated FDT blob (magic-checked above / by QEMU).
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr(resolved as *const u8) }) else {
        crate::safe_print!(64, "[SMP-shared] DTB parse failed; single-core\n");
        return;
    };
    let mut count = 0usize;
    for cpu in fdt.cpus() {
        let mpidr = cpu.ids().first() as u64;
        let idx = (mpidr & 0xff) as usize;
        if idx < MAX_CORES {
            PROBED_MPIDRS[idx].store(mpidr, Ordering::Relaxed);
            count = count.max(idx + 1);
        }
    }
    NUM_CORES.store(count.max(1), Ordering::Relaxed);
    USE_HVC.store(psci_is_hvc(&fdt), Ordering::Relaxed);
    PROBED.store(true, Ordering::Release);
    crate::safe_print!(64, "[SMP-shared] probed {} core(s)\n", count.max(1));
}

/// M0 bringup: PSCI `CPU_ON` every secondary onto the shared boot tables, then wait
/// (bounded) for each to report online. Called from `kernel_main` after `gic::init`.
pub fn bringup_secondaries() {
    if !PROBED.load(Ordering::Acquire) {
        crate::safe_print!(56, "[SMP-shared] not probed; staying single-core\n");
        return;
    }
    let num_cores = NUM_CORES.load(Ordering::Relaxed);
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    let bsp_idx = (read_mpidr() & 0xff) as usize;
    crate::safe_print!(64, "[SMP-shared] {} core(s); BSP is core {}\n", num_cores, bsp_idx);

    if num_cores <= 1 {
        crate::safe_print!(56, "[SMP-shared] single core; no secondaries\n");
        return;
    }

    let entry_pa = secondary_entry_shared as *const () as u64;
    // Publish everything the secondaries read (this module's statics, their stacks)
    // before any of them starts executing.
    dsb_sy();

    let mut expected = 0usize;
    for (idx, slot) in PROBED_MPIDRS.iter().enumerate().take(num_cores) {
        if idx == bsp_idx {
            continue;
        }
        let target = slot.load(Ordering::Relaxed);
        // `context_id` (a3) is unused in M0; pass the core index for future use / debug.
        let r = psci_call(use_hvc, PSCI_CPU_ON, target, entry_pa, idx as u64);
        if r == 0 {
            expected += 1;
            crate::safe_print!(80, "[SMP-shared] CPU_ON core {} (mpidr=0x{:x}) -> ok\n", idx, target);
        } else {
            crate::safe_print!(80, "[SMP-shared] CPU_ON core {} failed: {}\n", idx, r);
        }
    }

    // Bounded wait for the secondaries to reach the online barrier. This is a coarse
    // spin (no scheduler yet); ~50M iterations is generous on QEMU.
    let mut spins = 0u64;
    while ONLINE_COUNT.load(Ordering::Acquire) < expected && spins < 50_000_000 {
        core::hint::spin_loop();
        spins += 1;
    }
    let online = ONLINE_COUNT.load(Ordering::Acquire);
    if online == expected {
        crate::safe_print!(72, "[SMP-shared] \u{2713} {} secondary core(s) online (shared kernel)\n", online);
    } else {
        crate::safe_print!(80, "[SMP-shared] only {}/{} secondaries reported online\n", online, expected);
    }
}

/// Number of secondaries that reached the M0 online barrier (excludes the BSP).
/// Consumed by the boot self-test in `process_tests.rs` (compiled out under `no-tests`,
/// e.g. the devbox-smoltcp runtime image — hence `allow(dead_code)`).
#[allow(dead_code)]
pub fn online_secondary_count() -> usize {
    ONLINE_COUNT.load(Ordering::Acquire)
}

/// Number of cores the DTB reported (including the BSP). See `online_secondary_count`.
#[allow(dead_code)]
pub fn probed_core_count() -> usize {
    NUM_CORES.load(Ordering::Relaxed)
}

#[inline]
fn dsb_sy() {
    // SAFETY: full-system data synchronization barrier, no memory operands.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) }
}

// --- Per-PE GICv3 receive path (M2c) --------------------------------------------
// The boot L1[0] block identity-maps device space, so a secondary on the shared boot
// tables reaches its own redistributor at its physical address directly. Constants
// mirror `crate::smp` (kept private here so the multikernel path stays untouched).
const GICR_BASE: usize = 0x080A_0000;
const GICR_STRIDE: usize = 0x2_0000;
const GICR_SGI_OFFSET: usize = 0x1_0000;
const GICR_WAKER: usize = 0x0014;
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_SGI_IGROUPR0: usize = 0x0080;
const GICR_SGI_ISENABLER0: usize = 0x0100;
const GICR_SGI_IPRIORITYR: usize = 0x0400;
/// EL1 virtual-timer PPI (the shared 10 ms scheduler tick) and the scheduler SGI
/// (INTID 0), which each core rings at itself from the shared timer handler.
const TIMER_PPI: u32 = 27;
const SCHED_SGI: u32 = 0;

/// ISV-safe single-register MMIO (no writeback/pair form — those assert under QEMU HVF;
/// same reasoning as `gic_v3::mmio_w32`).
fn mmio_w32(addr: usize, val: u32) {
    // SAFETY: `addr` is a device-mapped GIC redistributor register.
    unsafe {
        core::arch::asm!("str {v:w}, [{a}]", v = in(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags));
    }
}
fn mmio_r32(addr: usize) -> u32 {
    let val: u32;
    // SAFETY: `addr` is a device-mapped GIC redistributor register.
    unsafe {
        core::arch::asm!("ldr {v:w}, [{a}]", v = out(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags, readonly));
    }
    val
}

/// Bring up THIS secondary's GICv3 receive path: enable the system-register CPU
/// interface, wake its redistributor, and enable the scheduler SGI (INTID 0) + the
/// virtual-timer PPI (27). The distributor's global config was done once by the BSP.
fn secondary_gic_init(idx: usize) {
    // SAFETY: GICv3 CPU-interface system registers; values per the architecture.
    unsafe {
        let sre: u64;
        core::arch::asm!("mrs {0}, S3_0_C12_C12_5", out(reg) sre, options(nomem, nostack));
        core::arch::asm!("msr S3_0_C12_C12_5, {0}", in(reg) sre | 1, options(nomem, nostack)); // ICC_SRE_EL1.SRE
        core::arch::asm!("isb", options(nomem, nostack));
        core::arch::asm!("msr S3_0_C4_C6_0, {0}", in(reg) 0xFFu64, options(nomem, nostack)); // ICC_PMR_EL1
        core::arch::asm!("msr S3_0_C12_C12_3, {0}", in(reg) 0u64, options(nomem, nostack)); // ICC_BPR1_EL1
        core::arch::asm!("msr S3_0_C12_C12_7, {0}", in(reg) 1u64, options(nomem, nostack)); // ICC_IGRPEN1_EL1
        core::arch::asm!("isb", options(nomem, nostack));
    }
    let rd = GICR_BASE + idx * GICR_STRIDE;
    let sgi = rd + GICR_SGI_OFFSET;
    let waker = rd + GICR_WAKER;
    mmio_w32(waker, mmio_r32(waker) & !GICR_WAKER_PROCESSOR_SLEEP);
    while mmio_r32(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }
    mmio_w32(sgi + GICR_SGI_IGROUPR0, 0xFFFF_FFFF);
    for i in 0..8 {
        mmio_w32(sgi + GICR_SGI_IPRIORITYR + i * 4, 0xA0A0_A0A0);
    }
    mmio_w32(sgi + GICR_SGI_ISENABLER0, (1u32 << SCHED_SGI) | (1u32 << TIMER_PPI));
    // SAFETY: ensure redistributor writes complete before IRQs are unmasked.
    unsafe { core::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
}

/// Base VA of this core's 16 KiB boot/idle stack in `secondary_boot_stacks_shared`.
fn secondary_stack_base(core: usize) -> usize {
    let addr: usize;
    // SAFETY: resolves the `.bss.smp_shared` symbol's address; no memory access.
    unsafe {
        core::arch::asm!(
            "adrp {t}, secondary_boot_stacks_shared",
            "add {t}, {t}, :lo12:secondary_boot_stacks_shared",
            t = out(reg) addr,
            options(nomem, nostack),
        );
    }
    addr + (core << STACK_SHIFT)
}

/// Set `VBAR_EL1` to the shared exception vector table (the BSP's) so this core takes
/// syscalls/IRQs/faults through the same handlers.
fn set_shared_vbar() {
    // SAFETY: installs the kernel's exception vector base for this PE.
    unsafe {
        core::arch::asm!(
            "adrp {t}, exception_vector_table",
            "add {t}, {t}, :lo12:exception_vector_table",
            "msr vbar_el1, {t}",
            "isb",
            t = out(reg) _,
            options(nomem, nostack),
        );
    }
}

/// Per-core counter of scheduler ticks each core has serviced a worker on — the M2c
/// proof that threads execute across cores. Genuinely shared (not replicated).
static CORES_SEEN: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Per-core count of EL0 traps (syscalls / user faults) serviced — the M3 proof that
/// USERSPACE runs across cores. Bumped from the EL0 trap entry (`rust_sync_el0_handler`).
static CORES_SEEN_USER: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Record that this core just took an EL0 trap (a user syscall or fault). Called from
/// the syscall entry; an EL0 trap only originates from userspace, so a nonzero count on
/// core N means a user process executed on core N.
#[inline]
pub fn record_el0_trap() {
    let aff0 = (read_mpidr() & 0xff) as usize;
    if aff0 < MAX_CORES {
        CORES_SEEN_USER[aff0].fetch_add(1, Ordering::Relaxed);
    }
}

/// Number of cores that have serviced a user (EL0) trap (M3 self-test).
pub fn cores_that_ran_userspace() -> usize {
    CORES_SEEN_USER.iter().filter(|c| c.load(Ordering::Relaxed) > 0).count()
}

/// Per-core user-trap count (diagnostics / self-test).
pub fn user_traps(core: usize) -> u64 {
    if core < MAX_CORES {
        CORES_SEEN_USER[core].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Bitmask of cores a single migration-test thread has observed itself running on
/// (bit `aff0`). `count_ones() >= 2` proves that ONE thread migrated across cores (M4).
static MIGRATION_MASK: AtomicU32 = AtomicU32::new(0);

/// Stop flag for the demo workers/probes so they self-terminate after their test and
/// free their (scarce) system-thread slots for the next test. See [`stop_and_reclaim_demos`].
static DEMO_STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Terminate the current kernel thread and park (the standard kernel-thread exit path:
/// mark terminated so `cleanup_terminated` recycles the slot, then yield forever).
fn demo_exit() -> ! {
    akuma_exec::threading::mark_current_terminated();
    loop {
        akuma_exec::threading::yield_now();
    }
}

/// Signal all demo workers/probes to stop, wait for them to self-terminate, reclaim
/// their slots, then reset the flag for the next test. Keeps the scarce system-thread
/// slots (RESERVED_THREADS) from leaking across the M2c/M3/M4 self-tests.
pub fn stop_and_reclaim_demos() {
    DEMO_STOP.store(true, Ordering::Release);
    // Let workers wake from their short sleep, observe the flag, and self-terminate.
    let start = crate::timer::uptime_us();
    while crate::timer::uptime_us().saturating_sub(start) < 300_000 {
        akuma_exec::threading::yield_now();
        akuma_exec::threading::idle_halt();
    }
    akuma_exec::threading::cleanup_terminated();
    DEMO_STOP.store(false, Ordering::Release);
}

/// M4 migration-proof worker: record this core in `MIGRATION_MASK`, then SLEEP so the
/// scheduler is free to resume us on a *different* core next time. Stops on `DEMO_STOP`.
fn migration_worker() -> ! {
    while !DEMO_STOP.load(Ordering::Acquire) {
        let aff0 = read_mpidr() & 0xff;
        MIGRATION_MASK.fetch_or(1u32 << aff0, Ordering::Relaxed);
        akuma_exec::threading::sleep_us(1500);
    }
    demo_exit()
}

/// Spawn ONE migration-test thread (M4). One thread that lands on >1 core over its
/// lifetime demonstrates cross-core migration (not just different threads on different
/// cores). Call once, from the BSP.
pub fn spawn_migration_probe() {
    if NUM_CORES.load(Ordering::Relaxed) <= 1 {
        return;
    }
    let _ = akuma_exec::threading::spawn_system_thread_fn(migration_worker);
}

/// Number of distinct cores the single migration-probe thread has run on (M4 self-test).
pub fn migration_core_count() -> u32 {
    MIGRATION_MASK.load(Ordering::Relaxed).count_ones()
}

/// Demo worker (real shared-kernel SMP M2c): bump this core's counter, then SLEEP.
/// Sleeping (rather than busy-looping) is essential — a kernel thread holds the Big
/// Kernel Lock while it runs, so a never-sleeping worker would let one core monopolize
/// the lock and starve the others. Sleeping makes the worker WAITING, letting a core
/// drop to idle (releasing the BKL) so peers can pick up work. Runs forever.
fn smp_worker() -> ! {
    while !DEMO_STOP.load(Ordering::Acquire) {
        let aff0 = (read_mpidr() & 0xff) as usize;
        if aff0 < MAX_CORES {
            CORES_SEEN[aff0].fetch_add(1, Ordering::Relaxed);
        }
        akuma_exec::threading::sleep_us(2000);
    }
    demo_exit()
}

/// Spawn the M2c demo workers from the BSP (after secondaries are up). Each is an
/// ordinary shared-pool kernel thread; the per-core schedulers distribute them, so over
/// time both the BSP and the secondaries run them. Call once.
pub fn spawn_worker_demo() {
    let cores = NUM_CORES.load(Ordering::Relaxed);
    if cores <= 1 {
        return;
    }
    // A few workers (cores + 1) so there is usually one runnable when a core wakes.
    for _ in 0..=cores {
        let _ = akuma_exec::threading::spawn_system_thread_fn(smp_worker);
    }
    crate::safe_print!(64, "[SMP-shared] spawned {} demo workers\n", cores + 1);
}

/// Number of cores that have run a demo worker at least once (for the boot self-test).
pub fn cores_that_ran_workers() -> usize {
    CORES_SEEN.iter().filter(|c| c.load(Ordering::Relaxed) > 0).count()
}

/// Per-core worker tick count (diagnostics / self-test).
pub fn worker_ticks(core: usize) -> u64 {
    if core < MAX_CORES {
        CORES_SEEN[core].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Secondary-core Rust entry (M2c). Runs on the SHARED boot page tables + PMM/heap.
/// Adopts its boot context as this core's idle thread, brings up its GIC receive path,
/// installs the shared vectors, arms the shared 10 ms virtual-timer tick, enables IRQs,
/// and enters the idle loop — from which the timer preempts it onto any runnable thread
/// in the shared scheduler. The BKL (held across each IRQ/scheduler excursion)
/// serializes kernel execution; `idle_halt` drops it around WFI so peers can enter.
///
/// # Safety
/// Called only from the `secondary_entry_shared` trampoline, once per core, MMU on,
/// with this core's boot stack installed and IRQs masked.
#[unsafe(no_mangle)]
pub extern "C" fn secondary_shared_start(_context_id: u64, core_idx: u64) -> ! {
    let core = core_idx as usize;
    let stack_base = secondary_stack_base(core);
    let stack_size = 1usize << STACK_SHIFT;
    let exc_top = (stack_base + stack_size) as u64;

    // Adopt the current (boot) context as this core's idle thread so the shared
    // scheduler can switch away from it and back. Sets TPIDRRO_EL0.
    let idle =
        akuma_exec::threading::adopt_current_as_core_idle(core, exc_top, stack_base, stack_size);

    ONLINE_COUNT.fetch_add(1, Ordering::AcqRel);
    let Some(slot) = idle else {
        crate::safe_print!(64, "[SMP-shared] core {} online but NO idle slot; parking\n", core);
        loop {
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
    };
    crate::safe_print!(64, "[SMP-shared] core {} online (idle tid {})\n", core, slot);

    // Bring up this PE's interrupt receive path, install shared vectors, arm the tick.
    secondary_gic_init(core);
    set_shared_vbar();
    crate::timer::enable_timer_interrupts(crate::config::TIMER_INTERVAL_US);

    // Unmask IRQs: from here the timer tick drives this core's scheduler.
    // SAFETY: vectors + GIC + timer are configured; safe to take interrupts now.
    unsafe { core::arch::asm!("msr daifclr, #2", "isb", options(nomem, nostack)) };

    // Idle loop. Two requirements pull against each other: (1) don't hammer the BKL
    // when there's nothing to run (a `yield_now()` every iteration livelocks SMP=4 as
    // idle cores fight over the lock); (2) still switch onto a thread the moment one is
    // runnable. The solution: release the BKL, WFI, re-acquire — WITHOUT disabling
    // preemption (unlike `idle_halt`). With preemption enabled, the timer tick's
    // self-SGI runs the scheduler and preempts this idle thread onto any READY thread;
    // when nothing is runnable we just fall back to WFI. One timer IRQ + one SGI per
    // ~10 ms tick — cheap and contention-free.
    loop {
        // Publish that this core is idle so `wake_remote_idle` can nudge it, then drop
        // the BKL so peers can enter the kernel while we're halted.
        set_core_idle(core, true);
        akuma_exec::bkl::leave_kernel();
        // SAFETY: IRQs are enabled; the timer/device IRQ (or a cross-core wake SGI)
        // wakes us and (via the scheduler SGI) may switch us to a runnable thread.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        // Re-take the BKL for our brief kernel work before the next halt (idempotent
        // if the waking IRQ's reconcile already re-acquired it for us).
        akuma_exec::bkl::enter_kernel();
        set_core_idle(core, false);
    }
}

// Secondary trampoline. Mirrors `crate::smp::secondary_entry` and boot.rs's MMU setup,
// but loads the SHARED boot TTBR0/TTBR1 (never a restricted per-core table) and tail-
// calls `secondary_shared_start`. Lives in `.text.boot` so it is identity-reachable
// with the MMU still off at entry. `boot_ttbr0_addr`/`boot_ttbr1_addr` are the BSP's
// boot-table roots, published MMU-off into `.data.boot` by boot.rs.
core::arch::global_asm!(
    r#"
.section .text.boot
.global secondary_entry_shared
secondary_entry_shared:
    mov     x19, x0                 // x19 = context_id (a3 from PSCI CPU_ON; unused M0)

    // 1. Enable FPU/SIMD (FPEN = 0b11).
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb

    // 2. core idx = MPIDR aff0; bail to park if it exceeds MAX_CORES.
    mrs     x20, mpidr_el1
    and     x20, x20, #0xff
    cmp     x20, #{max_cores}
    b.ge    .Lsh_park
    // SP = &secondary_boot_stacks_shared[idx] + STACK_SIZE (top of this core's stack)
    adrp    x0, secondary_boot_stacks_shared
    add     x0, x0, :lo12:secondary_boot_stacks_shared
    add     x0, x0, x20, lsl #{stack_shift}
    mov     x1, #1
    add     x0, x0, x1, lsl #{stack_shift}
    mov     sp, x0

    // 3. MMU registers (mirror boot.rs configure_mmu_regs).
    // MAIR_EL1 = 0xFFBB4400
    mov     x0, #0x4400
    movk    x0, #0xFFBB, lsl #16
    msr     mair_el1, x0
    // TCR_EL1 = 0x0000_0005_B510_3510
    mov     x0, #0x3510
    movk    x0, #0xB510, lsl #16
    movk    x0, #0x5, lsl #32
    msr     tcr_el1, x0
    // TTBR0_EL1 <- *boot_ttbr0_addr (the BSP's SHARED boot table; read MMU-off)
    adrp    x0, boot_ttbr0_addr
    add     x0, x0, :lo12:boot_ttbr0_addr
    ldr     x0, [x0]
    msr     ttbr0_el1, x0
    // TTBR1_EL1 <- *boot_ttbr1_addr
    adrp    x0, boot_ttbr1_addr
    add     x0, x0, :lo12:boot_ttbr1_addr
    ldr     x0, [x0]
    msr     ttbr1_el1, x0
    tlbi    vmalle1
    dsb     sy
    isb
    // Enable MMU + caches (same SCTLR bits as boot.rs _boot_code).
    mrs     x0, sctlr_el1
    orr     x0, x0, #1              // M  = MMU enable
    orr     x0, x0, #(1 << 2)      // C  = data cache
    orr     x0, x0, #(1 << 12)     // I  = instruction cache
    orr     x0, x0, #(1 << 14)     // DZE
    orr     x0, x0, #(1 << 15)     // UCT
    orr     x0, x0, #(1 << 26)     // UCI
    msr     sctlr_el1, x0
    isb

    // 4. secondary_shared_start(context_id, core_idx)
    mov     x0, x19
    mov     x1, x20
    bl      secondary_shared_start

.Lsh_park:
    wfe
    b       .Lsh_park

.section .bss.smp_shared
.balign 16
secondary_boot_stacks_shared:
    .space  {stacks_bytes}
"#,
    max_cores = const MAX_CORES,
    stack_shift = const STACK_SHIFT,
    stacks_bytes = const (MAX_CORES << STACK_SHIFT),
);
