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

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Maximum cores we bring up. Matches the multikernel's `akuma_smp::MAX_CORES` and is
/// comfortably under the `aff0 < 16` single-cluster limit on QEMU `virt`.
const MAX_CORES: usize = 8;

/// Per-core boot/park stack size shift (16 KiB). Ample for the M0 park path; a
/// secondary switches to a real thread stack when it joins the scheduler (M2).
const STACK_SHIFT: usize = 14;

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

/// Secondary-core Rust entry (M0). Runs on the SHARED boot page tables and the single
/// shared PMM/heap. For now: announce online, then park in `WFE`. Later milestones
/// replace the park with GIC/timer setup and joining the shared scheduler.
///
/// # Safety
/// Called only from the `secondary_entry_shared` trampoline, once, per core, with the
/// MMU on and this core's private boot stack installed.
#[unsafe(no_mangle)]
pub extern "C" fn secondary_shared_start(_context_id: u64, core_idx: u64) -> ! {
    // A plain kernel static write here is visible to the BSP — shared, not replicated.
    ONLINE_COUNT.fetch_add(1, Ordering::AcqRel);
    // safe_print! is heap-free and reaches the shared UART via the boot device map.
    crate::safe_print!(48, "[SMP-shared] core {} online\n", core_idx);

    loop {
        // SAFETY: idle until an event/interrupt; M0 secondaries do no further work.
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
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
