//! Multikernel secondary-core bringup (docs/MULTIKERNEL.md).
//!
//! **M0 — second core spins.** The BSP (core 0) parses the DTB once to learn the
//! set of physical PEs, fills a shared [`MachineConfig`] descriptor, then wakes
//! each secondary with a PSCI `CPU_ON` whose `context_id` is the descriptor's
//! physical address. Each secondary enters [`secondary_entry`] (asm) with the MMU
//! off, brings the MMU up against the BSP's *existing* boot page tables
//! ("isolation by convention", §4.2 — every core maps all RAM for now; real
//! per-core TTBR1 isolation is M1), sets up a private boot stack, and calls
//! [`secondary_rust_start`], which marks its [`CoreConfig::state`] `Online` and
//! parks in a low-power `wfe` loop. The BSP polls the `state` atomics and reports.
//!
//! Why this is coherent with no cache maintenance: the descriptor and the state
//! atomics live in Normal, Inner-Shareable, Write-Back RAM (the boot tables map
//! `0x4000_0000`–`0x7FFF_FFFF` that way). The BSP writes the descriptor with its
//! MMU on, issues a `DSB SY` before `CPU_ON`, and the secondary only reads it
//! *after* enabling its own MMU — so the read goes through the inner-shareable
//! coherency domain, not an MMU-off (Device-nGnRnE) bypass. The only values the
//! secondary touches MMU-off are `boot_ttbr0_addr`/`boot_ttbr1_addr`, which the
//! BSP wrote MMU-off in early boot (straight to RAM, no dirty cache lines).
//!
//! Everything here is behind `cfg(kernel_smp)` (the `smp` feature); the default
//! single-core build does not compile a line of it.

use core::arch::global_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::console;

/// Maximum physical PEs the descriptor describes. QEMU `virt` packs CPU affinity
/// as `aff0 = cpu_index` for the first 16 cores (single cluster), so a core's
/// index into [`MachineConfig::cores`] is `MPIDR_EL1 & 0xff`.
pub const MAX_CORES: usize = 8;

/// Per-core boot stack size as a power-of-two shift (1 << 14 = 16 KiB). Only the
/// trampoline + `secondary_rust_start` run on it before M1 hands the core a real
/// per-core stack, so 16 KiB is ample.
const SECONDARY_STACK_SHIFT: usize = 14;

/// Sanity magic so a secondary can confirm it read a real descriptor.
const MAGIC: u64 = 0x414b_554d_414d_4b31; // "AKUMAMK1"

// Core lifecycle states (CoreConfig::state). The BSP watches Offline -> Online.
const STATE_OFFLINE: u32 = 0;
const STATE_BOOTING: u32 = 1;
const STATE_ONLINE: u32 = 2;

/// PSCI `CPU_ON` (SMC64) function id.
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// Per-core slot in the shared descriptor. `#[repr(C)]` + a fixed layout so the
/// (future) asm trampoline and Rust agree byte-for-byte.
#[repr(C)]
struct CoreConfig {
    /// MPIDR_EL1 affinity of this PE (PSCI `CPU_ON` target).
    mpidr: u64,
    /// PRIVATE physical partition for this core. Reserved at M0 (0); the per-core
    /// PMM in M1 reads these at runtime (never a compile-time const) so memory
    /// renegotiation (§9) stays a message-protocol addition, not a format change.
    ram_base: u64,
    ram_len: u64,
    kernel_end: u64,
    /// Per-core boot-stack top. Reserved at M0 (the trampoline uses the static
    /// `secondary_boot_stacks` pool); M1 points this at a partition-private stack.
    entry_sp: u64,
    /// Offline -> Booting -> Online. Cross-core via inner-shareable coherency.
    state: AtomicU32,
    _pad: u32,
}

impl CoreConfig {
    const fn new() -> Self {
        Self {
            mpidr: 0,
            ram_base: 0,
            ram_len: 0,
            kernel_end: 0,
            entry_sp: 0,
            state: AtomicU32::new(STATE_OFFLINE),
            _pad: 0,
        }
    }
}

/// Read-only-after-init machine descriptor. The BSP fills it before any `CPU_ON`;
/// secondaries only read it (their `state` atomic is the sole field they write).
#[repr(C)]
struct MachineConfig {
    magic: u64,
    version: u32,
    num_cores: u32,
    /// Self physical address (lets a secondary re-find the page; sanity only here).
    config_phys_addr: u64,
    cores: [CoreConfig; MAX_CORES],
}

impl MachineConfig {
    const fn new() -> Self {
        Self {
            magic: 0,
            version: 0,
            num_cores: 0,
            config_phys_addr: 0,
            cores: [const { CoreConfig::new() }; MAX_CORES],
        }
    }
}

/// `Sync` wrapper: the BSP writes the inner config exactly once (single-threaded,
/// before any secondary runs); afterwards every access is either a read or a
/// cross-core atomic on a `state` field. The kernel is identity-mapped, so the
/// static's VA equals its PA — exactly the `context_id` we hand PSCI.
struct SyncConfig(UnsafeCell<MachineConfig>);
// SAFETY: see the type doc — initialization is single-threaded and ordered before
// any reader by the DSB-SY + CPU_ON handshake; live mutation is atomic-only.
unsafe impl Sync for SyncConfig {}

static MACHINE_CONFIG: SyncConfig = SyncConfig(UnsafeCell::new(MachineConfig::new()));

unsafe extern "C" {
    /// Secondary trampoline (asm below). Its link address equals its physical
    /// address under the identity map, so `secondary_entry as usize` is the
    /// PSCI entry point.
    fn secondary_entry();
}

#[inline]
fn read_mpidr() -> u64 {
    let v: u64;
    // SAFETY: reading the affinity register has no side effects.
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)) }
    v
}

#[inline]
fn dsb_sy() {
    // SAFETY: full-system data synchronization barrier, no memory operands.
    unsafe { core::arch::asm!("dsb sy", options(nostack, preserves_flags)) }
}

#[inline]
fn wfe() {
    // SAFETY: wait-for-event idles the PE until an event/interrupt.
    unsafe { core::arch::asm!("wfe", options(nomem, nostack)) }
}

/// Issue a PSCI SMC64 call over the conduit the platform declared (`hvc`/`smc`).
/// Returns the PSCI status in x0 (0 = `SUCCESS`). x1–x17 are clobbered per SMCCC.
fn psci_call(use_hvc: bool, func: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    // SAFETY: a standard SMCCC call. We clobber the full caller-saved GPR range
    // (x1–x17) the SMC Calling Convention permits the callee to use.
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

/// Resolve the DTB pointer the way `detect_memory` does (QEMU does not set x0 for
/// flat kernels; the DTB sits 2 MiB-aligned above the image at 0x4020_0000).
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

/// `true` if the platform's PSCI conduit is `hvc`, per the DTB `/psci` node's
/// `method` property. QEMU `virt` uses `hvc`; default to it when absent.
fn psci_is_hvc(fdt: &fdt::Fdt) -> bool {
    fdt.find_node("/psci")
        .and_then(|n| n.property("method"))
        .is_none_or(|p| p.value.starts_with(b"hvc"))
}

/// Collect MPIDRs from the DTB `/cpus` nodes, indexed by `aff0 = mpidr & 0xff`
/// (matches the trampoline's `secondary_boot_stacks` / `cores[]` indexing).
/// Returns `(mpidrs, count)`.
fn collect_mpidrs(fdt: &fdt::Fdt) -> ([u64; MAX_CORES], usize) {
    let mut mpidrs = [0u64; MAX_CORES];
    let mut count = 0usize;
    for cpu in fdt.cpus() {
        let mpidr = cpu.ids().first() as u64;
        let idx = (mpidr & 0xff) as usize;
        if idx < MAX_CORES {
            mpidrs[idx] = mpidr;
            count = count.max(idx + 1);
        }
    }
    (mpidrs, count)
}

/// BSP entry point: wake every secondary PE and wait for it to report `Online`.
/// No-op (single-core) when the DTB enumerates only one CPU.
pub fn bringup_secondaries(dtb_ptr: usize) {
    let actual_dtb = resolve_dtb(dtb_ptr);
    if actual_dtb == 0 {
        console::print("[SMP] no DTB; staying single-core\n");
        return;
    }
    // SAFETY: `actual_dtb` carries a verified FDT magic.
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr(actual_dtb as *const u8) }) else {
        console::print("[SMP] invalid DTB; staying single-core\n");
        return;
    };

    let (mpidrs, num_cores) = collect_mpidrs(&fdt);
    let bsp_idx = (read_mpidr() & 0xff) as usize;

    console::print("[SMP] DTB enumerates ");
    console::print_dec(num_cores);
    console::print(" core(s); BSP is core ");
    console::print_dec(bsp_idx);
    console::print("\n");

    if num_cores <= 1 {
        console::print("[SMP] single core; no secondaries to bring up\n");
        return;
    }

    let use_hvc = psci_is_hvc(&fdt);

    // Fill the descriptor (single-threaded; before any CPU_ON).
    // SAFETY: no secondary is running yet, so this exclusive &mut is sound.
    let cfg = unsafe { &mut *MACHINE_CONFIG.0.get() };
    cfg.magic = MAGIC;
    cfg.version = 1;
    cfg.num_cores = num_cores as u32;
    cfg.config_phys_addr = core::ptr::from_mut(cfg) as u64;
    for (idx, &mpidr) in mpidrs.iter().enumerate().take(num_cores) {
        cfg.cores[idx].mpidr = mpidr;
        cfg.cores[idx].state.store(STATE_OFFLINE, Ordering::Relaxed);
    }

    let cfg_pa = core::ptr::from_ref::<MachineConfig>(cfg) as u64;
    let entry_pa = secondary_entry as *const () as usize as u64;

    // Publish the descriptor to RAM before any secondary's MMU-on read.
    dsb_sy();

    console::print("[SMP] conduit=");
    console::print(if use_hvc { "hvc" } else { "smc" });
    console::print(", entry=0x");
    console::print_hex(entry_pa);
    console::print(", descriptor=0x");
    console::print_hex(cfg_pa);
    console::print("\n");

    // Wake each secondary.
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let target = cfg.cores[idx].mpidr;
        cfg.cores[idx].state.store(STATE_BOOTING, Ordering::Relaxed);
        dsb_sy();
        let r = psci_call(use_hvc, PSCI_CPU_ON, target, entry_pa, cfg_pa);
        console::print("[SMP] CPU_ON core ");
        console::print_dec(idx);
        console::print(" (mpidr=0x");
        console::print_hex(target);
        console::print(") -> ");
        if r == 0 {
            console::print("PSCI_SUCCESS\n");
        } else {
            console::print("ERROR ");
            console::print_dec((-r) as usize);
            console::print("\n");
        }
    }

    // Wait for secondaries to report Online (bounded by uptime, ~2s).
    let deadline = crate::timer::uptime_us() + 2_000_000;
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let mut online = false;
        while crate::timer::uptime_us() < deadline {
            if cfg.cores[idx].state.load(Ordering::Acquire) == STATE_ONLINE {
                online = true;
                break;
            }
            core::hint::spin_loop();
        }
        console::print("[SMP] core ");
        console::print_dec(idx);
        console::print(if online { " ONLINE\n" } else { " TIMEOUT (never reported online)\n" });
    }

    console::print("[SMP] bringup complete\n");
}

/// Secondary Rust entry, called from [`secondary_entry`] with the MMU already on.
///
/// M0 contract: announce liveness via the descriptor `state` atomic and park.
/// It must NOT touch the console — the UART/console spinlock is BSP-owned, and
/// contending it here could deadlock (§8.2 makes console output a BSP-drained
/// ring in M3). Liveness is observable to the BSP purely through `state`.
#[unsafe(no_mangle)]
pub extern "C" fn secondary_rust_start(cfg_pa: usize, core_idx: usize) -> ! {
    // SAFETY: `cfg_pa` is the descriptor PA handed via PSCI context_id; identity
    // map makes it a valid VA. Validate magic before trusting any field.
    let cfg = unsafe { &*(cfg_pa as *const MachineConfig) };
    if cfg.magic == MAGIC && core_idx < MAX_CORES {
        cfg.cores[core_idx].state.store(STATE_ONLINE, Ordering::Release);
    }
    loop {
        wfe();
    }
}

// ============================================================================
// Secondary trampoline (asm). Entered with the MMU OFF at EL1, interrupts masked
// (PSCI brings a core up in the architectural masked state). x0 = context_id =
// descriptor PA. We:
//   1. enable FPU/SIMD,
//   2. derive core idx = MPIDR aff0 and set SP to this core's private boot stack,
//   3. install the BSP's existing boot page tables + MMU regs and enable the MMU
//      (isolation-by-convention; per-core TTBR1 is M1),
//   4. call secondary_rust_start(context_id, core_idx).
// Only `.global` symbols (boot_ttbr0_addr/boot_ttbr1_addr from boot.rs,
// secondary_rust_start from Rust) cross the global_asm unit boundary; the MMU
// register values are inlined (mirroring boot.rs `configure_mmu_regs`).
// ============================================================================
global_asm!(
    r#"
.section .text.boot
.global secondary_entry
secondary_entry:
    mov     x19, x0                 // x19 = context_id (descriptor PA)

    // 1. Enable FPU/SIMD (FPEN = 0b11).
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb

    // 2. core idx = MPIDR aff0; bail to park if it exceeds MAX_CORES.
    mrs     x20, mpidr_el1
    and     x20, x20, #0xff
    cmp     x20, #{max_cores}
    b.ge    .Lsec_park
    // SP = &secondary_boot_stacks[idx] + STACK_SIZE  (top of this core's stack)
    adrp    x0, secondary_boot_stacks
    add     x0, x0, :lo12:secondary_boot_stacks
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
    // TTBR0_EL1 <- *boot_ttbr0_addr  (read MMU-off: BSP wrote it MMU-off -> RAM)
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

    // 4. secondary_rust_start(context_id, core_idx)
    mov     x0, x19
    mov     x1, x20
    bl      secondary_rust_start

.Lsec_park:
    wfe
    b       .Lsec_park

.section .bss.smp
.balign 16
secondary_boot_stacks:
    .space  {stacks_bytes}
"#,
    max_cores = const MAX_CORES,
    stack_shift = const SECONDARY_STACK_SHIFT,
    stacks_bytes = const (MAX_CORES << SECONDARY_STACK_SHIFT),
);
