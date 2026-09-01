//! # Zero `unsafe` blocks — but it cannot carry `forbid`
//!
//! This module reached **0 `unsafe {}` blocks** on 2026-09-01. It held 8 in
//! August: `akuma-fdt` took it to 4 (`archive/AKUMA_FDT_EXTRACTION.md`), the GIC
//! consolidation took 3 more (`archive/AKUMA_GIC_CONSOLIDATION.md`), and the
//! last — the PSCI SMC/HVC conduit — became `akuma-psci`.
//!
//! It still cannot take `#![forbid(unsafe_code)]` the way `src/syscall/` does,
//! and that is worth knowing before anyone tries: the lint also rejects
//! `unsafe extern` blocks, `core::arch::global_asm!` and
//! `#[unsafe(no_mangle)]`, and this module needs all three for the secondary
//! trampoline that PSCI `CPU_ON` jumps to. **Zero `unsafe` operations is not the
//! same as being able to forbid** — the remaining constructs are asm and
//! linkage, not operations. Any file with a vector table or a trampoline is in
//! the same position, which is why `src/exceptions.rs` has to *move to a crate*
//! rather than be cleaned in place.
//!
//! Keep it at zero anyway: if this module needs a privileged operation, put it
//! behind a named function in the crate that owns the hardware it pokes —
//! `akuma-gic` for the interrupt controller, `akuma-psci` for the conduit,
//! `akuma-cpu` for instructions that are safe to execute.

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
#![cfg_attr(not(kernel_tests), allow(dead_code))]

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
// The syscall number table (crate 1). The BKL opt-out seed below names its
// entries rather than spelling their numbers — see the note above the seed.

/// Maximum cores we bring up. Comfortably under the `aff0 < 16` single-cluster limit
/// on QEMU `virt`.
const MAX_CORES: usize = 8;

/// Per-core boot/idle stack size shift (64 KiB). This stack backs the secondary's
/// idle thread, so it must hold the full IRQ excursion — the 832-byte trap frame plus
/// the shared `timer_irq_handler` (kernel-timer alarm processing, preemption watchdog,
/// logging) and the scheduler — which is far deeper than M0's WFE park. 16 KiB
/// overflowed here (fault-in-fault while holding the BKL); 64 KiB is ample.
const STACK_SHIFT: usize = 16;

// PSCI (matches crate::smp — QEMU `virt` exposes PSCI over the DTB-declared conduit).
// SYSTEM_OFF/SYSTEM_RESET function IDs live in akuma_boot alongside the rest of
// the reboot ABI (`sc-reboot` only) — nothing hardware-specific about a constant.

// --- DTB-probed topology, stashed before the heap can overwrite the DTB ------------
static PROBED: AtomicBool = AtomicBool::new(false);
static NUM_CORES: AtomicUsize = AtomicUsize::new(1);
static PROBED_MPIDRS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

/// Count of secondaries that have reached their online barrier. Genuinely shared
/// (not replicated) — a plain kernel static IS cross-core state in this build.
static ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Bitmask of cores currently halted in their idle loop (bit `aff0` set = idle). Used
/// by [`wake_remote_idle`] to nudge an idle core to pick up a just-woken thread
/// promptly (M4 cross-core wakeup) instead of waiting for its ~10 ms timer tick.
static CORE_IDLE_MASK: AtomicU32 = AtomicU32::new(0);

// --- BKL policy toggles: moved to `akuma-bkl` --------------------------------
//
// The seven runtime BKL-drop toggles and the per-syscall opt-out bitmap moved to
// `akuma_bkl::policy` on 2026-09-01. They are pure policy state — relaxed atomic
// loads with paired A/B setters, no `unsafe` anywhere — and the crate that owns
// the BKL protocol is where the decision to take or skip it belongs. Re-exported
// here so every `smp_shared::*_bkl_drop_enabled` call site is spelled as it was.
//
// `process_bkl_drop_enabled` below is deliberately NOT among them: its atomic
// lives in `akuma_exec::process::bkl_guard`, and `akuma-exec` depends on
// `akuma-bkl`, so moving it would invert that edge.
pub use akuma_bkl::policy::{
    drivers_bkl_drop_enabled, exec_bkl_drop_enabled, mm_bkl_drop_enabled,
    vfs_bkl_drop_enabled,
};
// The A/B setters, and the three getters whose last production reader was the
// exception path (which reads `akuma_bkl::policy::` directly since 2026-09-01),
// are now touched only by the boot self-tests — `process_tests.rs`, which
// `no-tests` builds (`devbox-smoltcp`, extreme-size) compile out while denying
// unused imports. Same shape as `pmm.rs`'s tests-only re-exports.
#[allow(unused_imports)]
pub use akuma_bkl::policy::{
    irq_bkl_drop_enabled, sched_bklfree_el0_enabled, set_drivers_bkl_drop_enabled,
    set_exec_bkl_drop_enabled, set_fault_bkl_drop_enabled, set_irq_bkl_drop_enabled,
    set_mm_bkl_drop_enabled, set_sched_bklfree_el0_enabled, set_syscall_bkl_optout,
    set_vfs_bkl_drop_enabled, syscall_bkl_optout,
};

/// Runtime toggle (default **on**) for `no-bkl-process` (Phase 3 of
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md) — drop the BKL for `fork_process`'s
/// CoW share/demote pass, relying on the address space's own `as_lock` (the same lock
/// the CoW fault handler takes BKL-free) held in bounded chunks around every
/// parent-page-table access, plus the existing `COW_REFCOUNTS` / PMM / allocator
/// spinlocks. `ProcessBklGuard` latches this at construction so an ON→OFF flip
/// mid-fork cannot unbalance the ticket FIFO.
///
/// Unlike the toggles above, the atomic itself lives in `akuma_exec::process::bkl_guard`
/// — the guard is constructed inside `fork_process`, which cannot name bin-crate items.
/// These are thin re-exports so every BKL toggle stays reachable from one module.
#[inline]
pub fn process_bkl_drop_enabled() -> bool {
    akuma_exec::process::process_bkl_drop_enabled()
}

/// Enable/disable the fork page-copy BKL-drop at runtime. Used by the boot self-test
/// `test_fork_bkl_drop` for its A/B phase; also the kill switch and the handle for an
/// interactive same-binary A/B under `bkl-profile`.
pub fn set_process_bkl_drop_enabled(on: bool) {
    akuma_exec::process::set_process_bkl_drop_enabled(on);
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
/// (a race that finds no idle core just falls back to the timer). Reached via the
/// `wake_remote_idle` runtime hook — the scheduler's displacement bypass calls it to
/// route READY work to idle capacity instead of preempting a RUNNING thread.
/// Returns `true` iff an idle peer was found and rung.
pub fn wake_remote_idle() -> bool {
    let self_aff0 = read_mpidr() & 0xff;
    let mask = CORE_IDLE_MASK.load(Ordering::Acquire) & !(1u32 << self_aff0);
    if mask == 0 {
        return false; // no idle peer
    }
    let target = mask.trailing_zeros(); // lowest idle peer core
    akuma_gic::trigger_sgi_core(target, akuma_gic::SGI_SCHEDULER);
    true
}

/// Send a scheduler SGI to a specific core so its scheduler picks up a just-woken
/// READY thread without waiting for the ~10 ms timer tick. Called from
/// `ThreadWaker::wake` via the `wake_core` runtime hook. Best-effort: a race that
/// finds the core already processing an SGI just falls back to the timer tick.
pub fn wake_core(core_id: u8) {
    let self_aff0 = (read_mpidr() & 0xff) as u8;
    if core_id == self_aff0 || core_id == 0xFF {
        return; // self-SGI already fired by trigger_sgi; or no last-known core
    }
    akuma_gic::trigger_sgi_core(u32::from(core_id), akuma_gic::SGI_SCHEDULER);
}

unsafe extern "C" {
    /// The secondary trampoline (asm, `.text.boot`, defined below). Taking its
    /// address gives the identity-mapped PA to hand PSCI `CPU_ON` as the entry point.
    fn secondary_entry_shared();

    /// The per-core boot/idle stacks (`.bss.smp_shared`, defined in the same
    /// `global_asm!` block below). Declared with its true type so the size the
    /// trampoline's `add x0, x0, x20, lsl #STACK_SHIFT` walks is stated to the
    /// compiler rather than only to the reader; see [`secondary_stack_base`].
    static secondary_boot_stacks_shared: [u8; MAX_CORES << STACK_SHIFT];
}

#[inline]
fn read_mpidr() -> u64 {
    akuma_cpu::sysreg::mpidr_el1()
}

/// `true` if the PSCI conduit is `hvc` (QEMU `virt`); default to it when absent.
fn psci_is_hvc(fdt: &akuma_fdt::Fdt<'_>) -> bool {
    fdt.find_node("/psci")
        .and_then(|n| n.property("method"))
        .is_none_or(|p| p.value.starts_with(b"hvc"))
}

/// Parse `/cpus` + `/psci` from the DTB and stash the topology.
///
/// Called from `kernel_main` before heap init — the heap can land on the DTB on
/// large-RAM configs, which is the entire reason this snapshots into statics
/// rather than re-reading the tree when `bringup_secondaries` needs it. That
/// ordering used to be a comment; taking a borrowed `Fdt` makes the compiler
/// hold it, because the borrow cannot outlive `kernel_main`'s DTB block.
///
/// This resolved and parsed the blob itself until 2026-09-01 — a duplicate of
/// what `detect_memory` and `install_fdt_device_map` each did on the same
/// bytes, and one of the six `unsafe` operations `akuma-fdt` replaced with one.
pub fn probe_dtb(fdt: Option<&akuma_fdt::Fdt<'_>>) {
    let Some(fdt) = fdt else {
        crate::safe_print!(64, "[SMP-shared] no DTB; staying single-core\n");
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
    akuma_psci::set_conduit(if psci_is_hvc(fdt) {
        akuma_psci::Conduit::Hvc
    } else {
        akuma_psci::Conduit::Smc
    });
    PROBED.store(true, Ordering::Release);
    crate::safe_print!(64, "[SMP-shared] probed {} core(s)\n", count.max(1));
}

/// M0 bringup: PSCI `CPU_ON` every secondary onto the shared boot tables, then wait
/// (bounded) for each to report online. Called from `kernel_main` after `akuma_gic::init`.
pub fn bringup_secondaries() {
    if !PROBED.load(Ordering::Acquire) {
        crate::safe_print!(56, "[SMP-shared] not probed; staying single-core\n");
        return;
    }
    let num_cores = NUM_CORES.load(Ordering::Relaxed);
    let bsp_idx = (read_mpidr() & 0xff) as usize;
    crate::safe_print!(64, "[SMP-shared] {} core(s); BSP is core {}\n", num_cores, bsp_idx);

    if num_cores <= 1 {
        crate::safe_print!(56, "[SMP-shared] single core; no secondaries\n");
        return;
    }

    // Serialize console output from here on, BEFORE the first `CPU_ON`. Doing this
    // on the BSP rather than in `secondary_entry_shared` is what closes the window
    // entirely: a secondary that flipped the flag itself would already be racing
    // the BSP's own bringup prints, which is measurably enough to corrupt one line
    // ("CPU_ON core 1 (mpi[dSrM=P0x1) -->s ok"). Past this point at least two
    // cores can reach `emit()`, which is exactly the condition the lock is for.
    // See `console::set_multicore`.
    crate::console::set_multicore();

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
        let r = akuma_psci::call(akuma_psci::CPU_ON, target, entry_pa, idx as u64);
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
    akuma_cpu::barrier::dsb_sy();
}

/// Where this machine's redistributor frames are, for `akuma_gic::secondary_init`.
///
/// The geometry is machine-specific — and on Firecracker it also depends on the
/// configured vCPU count — so it is read from the **installed device map**
/// (`crate::platform`), letting a redistributor discovered from the FDT win over
/// the compile-time bootstrap literal. Getting this wrong points a core at
/// another core's frames, which silently costs it its timer interrupt.
///
/// The driver itself moved to `akuma-gic` on 2026-09-01. What was here — an
/// `mmio_w32`/`mmio_r32` pair copied verbatim from `gic_v3.rs`, a second copy of
/// the `GICR_WAKER_*` bits, and four raw `msr` instructions duplicating
/// `gic_v3::init`'s CPU-interface sequence — was **three `unsafe` blocks that
/// already existed elsewhere**, so consolidating removed them rather than
/// relocating them (`docs/archive/AKUMA_GIC_CONSOLIDATION.md`).
#[cfg(kernel_smp_shared)]
fn redistributor_layout() -> akuma_gic::RedistributorLayout {
    akuma_gic::RedistributorLayout {
        base_pa: crate::platform::gicr_base_pa(),
        stride: crate::platform::GICR_STRIDE,
        sgi_offset: crate::platform::GICR_SGI_OFFSET,
    }
}

/// Base VA of this core's 64 KiB (`1 << STACK_SHIFT`) boot/idle stack in
/// `secondary_boot_stacks_shared`.
///
/// This was an `adrp`/`add` pair in an `unsafe` block until 2026-09-01. Taking
/// the address of an `extern` static is safe in edition 2024 (`&raw const`
/// resolves the symbol without reading through it), so the whole operation is
/// expressible without `asm!` — and the array type carries the bound the asm
/// version could only state in a comment.
pub fn secondary_stack_base(core: usize) -> usize {
    (&raw const secondary_boot_stacks_shared) as usize + (core << STACK_SHIFT)
}

/// Set `VBAR_EL1` to the shared exception vector table (the BSP's) so this core takes
/// syscalls/IRQs/faults through the same handlers.
///
/// This had its own `adrp`/`add`/`msr` sequence until 2026-09-01. It is the same
/// install `exceptions::init` does on the BSP — and "the same" is the whole
/// point of shared-kernel SMP — so it now calls the one implementation instead
/// of carrying a second copy that could drift from it.
fn set_shared_vbar() {
    crate::exceptions::install_vbar();
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
    // Spawning is best-effort (slot-limited), so report what was actually created,
    // not what was attempted: `cores + 1` was printed unconditionally, which turned
    // a partial spawn into a confident false count in the boot log — and
    // `cores_that_ran_workers()` below feeds a boot self-test that is read against
    // exactly this line.
    let mut spawned = 0u32;
    for _ in 0..=cores {
        if akuma_exec::threading::spawn_system_thread_fn(smp_worker).is_ok() {
            spawned += 1;
        }
    }
    crate::safe_print!(64, "[SMP-shared] spawned {} demo workers\n", spawned);
}

/// Self-test waiter that parks in a pure `blocking_relax()` loop — exactly what a thread
/// blocked in a socket recv (`wait_until`) or a DNS resolve does. Each time it is scheduled
/// it holds the Big Kernel Lock and must DROP it across the relax so peer cores can enter
/// the kernel; if `blocking_relax` regressed to not drop the BKL, a waiter that lands on a
/// peer core would hold it forever and freeze every other core (the meow->LLM wedge). Stops
/// on `DEMO_STOP`.
fn blocking_relax_waiter() -> ! {
    while !DEMO_STOP.load(Ordering::Acquire) {
        akuma_exec::threading::blocking_relax();
    }
    demo_exit()
}

/// Spawn one [`blocking_relax_waiter`] per core (BSP self-test). They occupy every core so
/// the test can prove the BSP still makes BKL-requiring forward progress while they are all
/// parked in a blocking wait. Best-effort (slot-limited); stop via [`stop_and_reclaim_demos`].
pub fn spawn_blocking_relax_waiters() {
    let cores = NUM_CORES.load(Ordering::Relaxed);
    if cores <= 1 {
        return;
    }
    for _ in 0..cores {
        let _ = akuma_exec::threading::spawn_system_thread_fn(blocking_relax_waiter);
    }
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
            akuma_cpu::park::wfe();
        }
    };
    crate::safe_print!(64, "[SMP-shared] core {} online (idle tid {})\n", core, slot);

    // Bring up this PE's interrupt receive path, install shared vectors, arm the tick.
    akuma_gic::secondary_init(core, redistributor_layout());
    set_shared_vbar();
    // Arm this core's periodic tick from the shared choice (BSP's probe /
    // override result, published via akuma-timer's registry).
    crate::timer::enable_timer_interrupts(crate::timer::current_tick_us());

    // Unmask IRQs: from here the timer tick drives this core's scheduler.
    // The `isb` matters here specifically: this is the first time this core takes
    // interrupts, so a tick already pending must be taken before the idle loop is
    // entered rather than at some later synchronization point.
    akuma_primitives::irq::unmask_irqs_sync();

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
        akuma_cpu::park::wfi();
        // Re-take the BKL for our brief kernel work before the next halt (idempotent
        // if the waking IRQ's reconcile already re-acquired it for us).
        akuma_exec::bkl::enter_kernel();
        // Profiler only: this bootstrap idle loop has no syscall/fault entry point, so
        // name its hold rather than leaving it "unknown" (BKL_VFS_CARVE_OUT.md §18).
        akuma_exec::sync::set_holder_tag(
            akuma_exec::bkl::current_core_id(),
            akuma_exec::sync::HOLD_TAG_IDLE,
        );
        set_core_idle(core, false);
        // Per-core half of `process::reclaim`'s idle drain site (the BSP's lives in
        // thread 0's idle loop): a secondary that has nothing to run is a collector
        // this box would otherwise never use. We hold the BKL here, which sits ABOVE
        // every drop-path lock in the order (`BKL > as_lock > {PMM, ...}`) — the same
        // context netpoll_maint's reclaim already runs in.
        akuma_exec::process::reclaim::drain_retired_if_requested();
    }
}

// Secondary trampoline. Mirrors boot.rs's MMU setup, but loads the SHARED boot
// TTBR0/TTBR1 (never a restricted per-core table) and tail-calls
// `secondary_shared_start`. Lives in `.text.boot` so it is identity-reachable with
// the MMU still off at entry. `boot_ttbr0_addr`/`boot_ttbr1_addr` are the BSP's
// boot-table roots, published MMU-off into `.data.boot` by boot.rs.
core::arch::global_asm!(
    r#"
.section .text.boot
.global secondary_entry_shared
secondary_entry_shared:
    mov     x19, x0                 // x19 = context_id (a3 from PSCI CPU_ON; unused M0)

    // 0. Zero TPIDRRO_EL0 before anything can call `current_tid()`.
    //
    // Same reason as the BSP path in `boot.rs`: the register's reset value is
    // architecturally UNKNOWN, and KVM stamps UNKNOWN-reset registers with
    // 0x1de7ec7edbadc0de, which `current_tid()` treats as fatal. Each PSCI-woken
    // secondary gets its own freshly-reset register, so zeroing it on the BSP is
    // not enough.
    msr     tpidrro_el0, xzr

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
    // CNTKCTL_EL1.EL0VCTEN — EL0 reads CNTVCT/CNTFRQ directly (see boot.rs).
    mov     x0, #0x2
    msr     cntkctl_el1, x0
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
    //
    // Including the two `bic`s: each PSCI-woken core gets its own SCTLR_EL1 with
    // its own reset value, and KVM's has SA/SA0 set while QEMU's does not. Clearing
    // them on the BSP alone would leave every secondary enforcing EL0 SP-alignment
    // checks that this kernel's userspace has never satisfied. See boot.rs.
    mrs     x0, sctlr_el1
    orr     x0, x0, #1              // M  = MMU enable
    orr     x0, x0, #(1 << 2)      // C  = data cache
    orr     x0, x0, #(1 << 12)     // I  = instruction cache
    orr     x0, x0, #(1 << 14)     // DZE
    orr     x0, x0, #(1 << 15)     // UCT
    orr     x0, x0, #(1 << 26)     // UCI
    bic     x0, x0, #(1 << 3)      // SA  = 0: no SP alignment check at EL1
    bic     x0, x0, #(1 << 4)      // SA0 = 0: no SP alignment check at EL0
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
// `.global` because `secondary_stack_base` is pub and gets inlined into callers in
// other codegen units (the boot self-test). Without it the symbol is local to this
// global_asm! block and any out-of-CGU `adrp` reference fails to link.
.global secondary_boot_stacks_shared
secondary_boot_stacks_shared:
    .space  {stacks_bytes}
"#,
    max_cores = const MAX_CORES,
    stack_shift = const STACK_SHIFT,
    stacks_bytes = const (MAX_CORES << STACK_SHIFT),
);
