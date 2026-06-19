//! ARM GICv3 driver for the QEMU `virt` machine (`-machine virt,gic-version=3`).
//!
//! This is the default interrupt-controller backend. Compared to GICv2:
//!
//! - The **CPU interface** is a set of EL1 system registers (`ICC_*_EL1`) instead
//!   of an MMIO frame. There is no `0x0801_0000` region under GICv3, which is why
//!   the legacy GICv2 driver faults under QEMU HVF on Apple Silicon (the MMIO
//!   access traps to the hypervisor with `ISV=0` and QEMU asserts).
//! - **SGIs and PPIs** (INTID 0-31) are configured per-PE in the **redistributor**
//!   (GICR) rather than the distributor (GICD).
//!
//! Akuma uses only SGI 0 (scheduler) and PPIs 27/30 (EL1 virtual/physical timer),
//! so the distributor needs only its global control register and no SPI routing
//! (`GICD_IROUTER`) is programmed.
//!
//! Register frames (QEMU `virt`, confirmed from the generated DTB):
//! - GICD at PA `0x0800_0000` (mapped at [`mmu::DEV_GIC_DIST_VA`])
//! - GICR base at PA `0x080A_0000`; CPU0 RD_base frame `0x080A_0000`
//!   ([`mmu::DEV_GICR_RD_VA`]) and SGI_base frame `0x080B_0000`
//!   ([`mmu::DEV_GICR_SGI_VA`]).

use akuma_exec::mmu;

// --- GICD (distributor) MMIO register offsets ---
mod gicd {
    pub const CTLR: usize = 0x0000; // Distributor Control Register
}

// GICD_CTLR bits, with Security disabled (DS=1), as QEMU `virt` presents.
const GICD_CTLR_ARE_NS: u32 = 1 << 4; // Affinity Routing Enable (Non-secure)
const GICD_CTLR_ENABLE_GRP1: u32 = 1 << 1; // Enable Non-secure Group 1
const GICD_CTLR_RWP: u32 = 1 << 31; // Register Write Pending

// --- GICR RD_base frame MMIO register offsets ---
mod gicr_rd {
    pub const WAKER: usize = 0x0014; // Redistributor Wake Register
}

// GICR_WAKER bits.
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;

// --- GICR SGI_base frame MMIO register offsets (SGI/PPI config) ---
mod gicr_sgi {
    pub const IGROUPR0: usize = 0x0080; // Interrupt Group Register 0
    pub const ISENABLER0: usize = 0x0100; // Interrupt Set-Enable Register 0
    pub const ICENABLER0: usize = 0x0180; // Interrupt Clear-Enable Register 0
    pub const IPRIORITYR: usize = 0x0400; // Interrupt Priority (1 byte per INTID)
}

#[inline]
fn gicd(off: usize) -> usize {
    mmu::DEV_GIC_DIST_VA + off
}
#[inline]
fn gicr_rd(off: usize) -> usize {
    mmu::DEV_GICR_RD_VA + off
}
#[inline]
fn gicr_sgi(off: usize) -> usize {
    mmu::DEV_GICR_SGI_VA + off
}

/// 32-bit MMIO read/write via explicit single-register `ldr`/`str` with plain
/// base-register addressing.
///
/// We deliberately do NOT use `read_volatile`/`write_volatile` here: the
/// optimizer is free to lower a `write_volatile` loop to a post-indexed
/// (writeback) store, e.g. `str w, [x], #4`. Writeback and pair/SIMD forms set
/// ESR ISV=0, and QEMU's HVF backend asserts (`hvf.c: assert(isv)`) on a data
/// abort it cannot decode — so a GICR write would crash QEMU under HVF on the
/// `extreme` profile (which chose that addressing mode) while working on
/// `release` (which happened to emit `str w, [x, #off]`). Forcing the
/// instruction form here makes GICv3 MMIO ISV-safe on every build profile.
#[inline]
fn mmio_w32(addr: usize, val: u32) {
    // SAFETY: `addr` is a device-mapped GIC MMIO register.
    unsafe {
        core::arch::asm!("str {v:w}, [{a}]", v = in(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags));
    }
}
#[inline]
fn mmio_r32(addr: usize) -> u32 {
    let val: u32;
    // SAFETY: `addr` is a device-mapped GIC MMIO register.
    unsafe {
        core::arch::asm!("ldr {v:w}, [{a}]", v = out(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags, readonly));
    }
    val
}

// ============================================================================
// CPU interface — EL1 system registers (ICC_*_EL1)
//
// Registers are addressed by their architectural S<op0>_<op1>_C<n>_C<m>_<op2>
// encoding rather than mnemonic names, so the inline asm assembles on any
// AArch64 toolchain regardless of GICv3 mnemonic support.
// ============================================================================

macro_rules! read_sysreg {
    ($enc:literal) => {{
        let v: u64;
        // SAFETY: reading a GICv3 CPU-interface system register.
        unsafe {
            core::arch::asm!(concat!("mrs {0}, ", $enc), out(reg) v, options(nomem, nostack));
        }
        v
    }};
}

macro_rules! write_sysreg {
    ($enc:literal, $val:expr) => {{
        let v: u64 = $val;
        // SAFETY: writing a GICv3 CPU-interface system register.
        unsafe {
            core::arch::asm!(concat!("msr ", $enc, ", {0}"), in(reg) v, options(nomem, nostack));
        }
    }};
}

const ICC_SRE_EL1: &str = "S3_0_C12_C12_5";
const ICC_PMR_EL1: &str = "S3_0_C4_C6_0";
const ICC_BPR1_EL1: &str = "S3_0_C12_C12_3";
const ICC_IGRPEN1_EL1: &str = "S3_0_C12_C12_7";
const ICC_IAR1_EL1: &str = "S3_0_C12_C12_0";
const ICC_EOIR1_EL1: &str = "S3_0_C12_C12_1";
const ICC_SGI1R_EL1: &str = "S3_0_C12_C11_5";

#[inline]
fn isb() {
    // SAFETY: instruction synchronization barrier, no memory effects.
    unsafe { core::arch::asm!("isb", options(nomem, nostack)) }
}
#[inline]
fn dsb_ish() {
    // SAFETY: data synchronization barrier (inner shareable).
    unsafe { core::arch::asm!("dsb ish", options(nomem, nostack)) }
}

/// Initialize the GICv3: distributor, this PE's redistributor, and the
/// system-register CPU interface.
pub fn init() {
    // 1. Enable the system-register CPU interface (ICC_SRE_EL1.SRE = 1).
    let sre = read_sysreg!("S3_0_C12_C12_5");
    write_sysreg!("S3_0_C12_C12_5", sre | 1);
    let _ = ICC_SRE_EL1; // documented name; encoding used above
    isb();

    // 2. Wake this PE's redistributor: clear ProcessorSleep, wait ChildrenAsleep.
    let waker = gicr_rd(gicr_rd::WAKER);
    mmio_w32(waker, mmio_r32(waker) & !GICR_WAKER_PROCESSOR_SLEEP);
    while mmio_r32(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }

    // 3. Configure SGIs/PPIs (INTID 0-31) in the redistributor SGI frame.
    // All Group 1 (Non-secure).
    mmio_w32(gicr_sgi(gicr_sgi::IGROUPR0), 0xFFFF_FFFF);
    // Mid priority for every SGI/PPI (8 INTIDs per 32-bit IPRIORITYR word).
    for i in 0..8 {
        mmio_w32(gicr_sgi(gicr_sgi::IPRIORITYR + i * 4), 0xA0A0_A0A0);
    }
    // Start with all SGIs/PPIs disabled; enable_irq() turns on what we use.
    mmio_w32(gicr_sgi(gicr_sgi::ICENABLER0), 0xFFFF_FFFF);

    // 4. Enable the distributor: affinity routing + Non-secure Group 1.
    mmio_w32(gicd(gicd::CTLR), GICD_CTLR_ARE_NS | GICD_CTLR_ENABLE_GRP1);
    while mmio_r32(gicd(gicd::CTLR)) & GICD_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }

    // 5. Configure the CPU interface and enable Group 1 interrupts.
    write_sysreg!("S3_0_C4_C6_0", 0xFF); // ICC_PMR_EL1: unmask all priorities
    let _ = ICC_PMR_EL1;
    write_sysreg!("S3_0_C12_C12_3", 0); // ICC_BPR1_EL1: no sub-priority grouping
    let _ = ICC_BPR1_EL1;
    write_sysreg!("S3_0_C12_C12_7", 1); // ICC_IGRPEN1_EL1: enable Group 1
    let _ = ICC_IGRPEN1_EL1;
    isb();
}

/// Enable a specific IRQ. SGIs/PPIs (INTID < 32) live in this PE's
/// redistributor; SPIs (>= 32) would use the distributor (unused by Akuma).
pub fn enable_irq(irq: u32) {
    if irq >= 1020 {
        return; // Invalid / special INTID
    }
    if irq < 32 {
        // GICR SGI_base frame, device-mapped for CPU0.
        mmio_w32(gicr_sgi(gicr_sgi::ISENABLER0), 1u32 << irq);
    } else {
        // SPI: GICD_ISENABLER<n> at 0x100 + (irq/32)*4 (best effort; Akuma uses
        // no SPIs, and affinity routing via GICD_IROUTER is not programmed).
        const GICD_ISENABLER: usize = 0x0100;
        let off = GICD_ISENABLER + ((irq / 32) as usize) * 4;
        let bit = 1u32 << (irq % 32);
        mmio_w32(gicd(off), bit);
    }
    dsb_ish();
}

/// Acknowledge an interrupt and return its INTID, or `None` if spurious.
pub fn acknowledge_irq() -> Option<u32> {
    let iar = read_sysreg!("S3_0_C12_C12_0"); // ICC_IAR1_EL1
    let _ = ICC_IAR1_EL1;
    let irq = (iar & 0xFF_FFFF) as u32; // 24-bit INTID
    if irq >= 1020 {
        None // 1020-1023 are special / spurious
    } else {
        Some(irq)
    }
}

/// Signal end of interrupt handling for `irq`.
pub fn end_of_interrupt(irq: u32) {
    write_sysreg!("S3_0_C12_C12_1", u64::from(irq)); // ICC_EOIR1_EL1
    let _ = ICC_EOIR1_EL1;
}

/// Trigger a Software Generated Interrupt to this CPU (affinity 0.0.0.0).
pub fn trigger_sgi(sgi_id: u32) {
    if sgi_id > 15 {
        return;
    }
    // ICC_SGI1R_EL1: IRM=0 (use target list), Aff3/2/1 = 0, INTID at [27:24],
    // TargetList bit 0 selects affinity-0 PE 0 (this CPU).
    let val = (u64::from(sgi_id) << 24) | 1;
    write_sysreg!("S3_0_C12_C11_5", val);
    let _ = ICC_SGI1R_EL1;
    dsb_ish();
    isb();
}

/// Set interrupt priority (0 = highest, 255 = lowest). Only SGI/PPI (< 32) is
/// supported here, which covers Akuma's usage.
#[allow(dead_code)]
pub fn set_priority(irq: u32, priority: u8) {
    if irq >= 32 {
        return;
    }
    // GICR SGI_base IPRIORITYR is a byte-addressable array, device-mapped.
    // Single-register `strb` (no writeback) keeps ISV=1 under HVF.
    let addr = mmu::DEV_GICR_SGI_VA + gicr_sgi::IPRIORITYR + irq as usize;
    // SAFETY: `addr` is a device-mapped GIC MMIO register.
    unsafe {
        core::arch::asm!("strb {v:w}, [{a}]", v = in(reg) u32::from(priority), a = in(reg) addr,
            options(nostack, preserves_flags));
    }
}
