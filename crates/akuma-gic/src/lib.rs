//! ARM GICv3 driver for the QEMU `virt` machine (`-machine virt,gic-version=3`)
//! and Firecracker.
//!
//! Extracted from `src/gic.rs` + `src/gic_v3.rs` + the redistributor half of
//! `src/smp_shared.rs` on 2026-09-01
//! (`docs/archive/AKUMA_GIC_CONSOLIDATION.md`). Before that the same controller
//! was driven from three files, with `mmio_w32`/`mmio_r32` and the `GICR_WAKER_*`
//! bits written out twice.
//!
//! # Why this crate cannot forbid `unsafe`
//!
//! Every operation here is a device MMIO access or a CPU-interface system
//! register. There is no pure half to separate out — which is why this was never
//! extracted under the old "extract the logic worth host-testing" criterion, and
//! why it is extracted now under the newer one: getting `unsafe` **out of
//! `src/`**. The blocks did not disappear, they moved somewhere that owns them.
//!
//! The `ICC_*_EL1` writes deliberately do NOT move into `akuma-cpu`. That crate
//! is "AArch64 instructions that are **safe to execute**"; enabling a CPU
//! interface or writing `ICC_EOIR1_EL1` changes interrupt delivery for the whole
//! core, so it stays `unsafe` at a site that knows the controller's state.
//!
//! # The one contract
//!
//! Every address passed to [`mmio_w32`]/[`mmio_r32`] is a **device-mapped GIC
//! register** — either inside the L0[1] device window (`addr::DEV_GIC*`, how the
//! BSP reaches it) or inside the low 1 GiB identity block (how a secondary
//! reaches its redistributor during bring-up, before the full map is installed).
//! Nothing else may be passed. That single sentence discharges every MMIO
//! `unsafe` below, the same way `akuma-net-nic` discharges its DMA blocks.
//!
//! # ISV-safety is not a style preference
//!
//! `read_volatile`/`write_volatile` are deliberately avoided: the optimizer may
//! lower a volatile loop to a post-indexed (writeback) store, `str w, [x], #4`.
//! Writeback and pair/SIMD forms set `ESR.ISV=0`, and QEMU's HVF backend asserts
//! (`hvf.c: assert(isv)`) on a data abort it cannot decode. That crashed QEMU
//! under HVF on the `extreme` profile while working on `release`, purely because
//! the two picked different addressing modes. Forcing the instruction form makes
//! GICv3 MMIO ISV-safe on every profile — do not "simplify" these to
//! `write_volatile`.
//!
//! # GICv2 is gone
//!
//! A legacy GICv2 MMIO backend lived behind `feature = "gic-v2"` until
//! 2026-09-01. It was never enabled by any build script, profile or acceptance
//! playbook, and it *could not* work under HVF: QEMU presents GICv3 there with no
//! `0x0801_0000` CPU-interface frame at all, so its first distributor write
//! faulted with `ISV=0` and HVF asserted (`archive/QEMU_HVF_ISV_BUG.md` root
//! cause 1). It carried four `unsafe` blocks for a path that could not run, so it
//! was deleted rather than moved.
//!
//! Register frames (QEMU `virt`, confirmed from the generated DTB):
//! - GICD at PA `0x0800_0000` (mapped at [`addr::DEV_GIC_DIST_VA`])
//! - GICR base at PA `0x080A_0000`; CPU0 RD_base frame `0x080A_0000`
//!   ([`addr::DEV_GICR_RD_VA`]) and SGI_base frame `0x080B_0000`
//!   ([`addr::DEV_GICR_SGI_VA`]).

#![no_std]

use akuma_primitives::addr;

/// SGI number Akuma rings for scheduling. INTID 0.
///
/// Lived in `src/gic.rs` (the deleted backend dispatcher) and is the most-named
/// GIC item in the tree, so it comes along rather than being re-declared at the
/// six call sites.
pub const SGI_SCHEDULER: u32 = 0;

/// Where a secondary core's redistributor frames are, so
/// [`secondary_init`] can find them without naming `src/platform.rs`.
///
/// The geometry is machine-specific — and on Firecracker it also depends on the
/// configured vCPU count — so it is supplied by the caller, which reads it from
/// the installed device map. A redistributor discovered from the FDT therefore
/// wins over any compile-time literal. Getting this wrong points a core at
/// another core's frames, which silently costs it its timer interrupt.
#[derive(Clone, Copy)]
pub struct RedistributorLayout {
    /// PA of redistributor 0. Core `n`'s RD_base is `base_pa + n * stride`.
    pub base_pa: usize,
    /// Bytes between consecutive cores' redistributor frames.
    pub stride: usize,
    /// Offset from a core's RD_base to its SGI_base frame.
    pub sgi_offset: usize,
}

// --- GICD (distributor) MMIO register offsets ---
mod gicd {
    pub const CTLR: usize = 0x0000; // Distributor Control Register
    // The SPI configuration banks. Until 2026-08-19 only `CTLR` was here,
    // because nothing registered an SPI — the sole device IRQ was the virtual
    // timer (PPI 27), which lives in the redistributor. Enabling the
    // virtio-net interrupt needs all four: an SPI left at its reset state is
    // in Group 0, which `ICC_IGRPEN1_EL1` does not deliver, and has no route.
    pub const IGROUPR: usize = 0x0080; // 1 bit per INTID
    pub const ISENABLER: usize = 0x0100; // 1 bit per INTID
    pub const IPRIORITYR: usize = 0x0400; // 1 byte per INTID
    pub const IROUTER: usize = 0x6000; // 8 bytes per INTID, from INTID 32
}

/// Interrupt priority for everything Akuma enables. Below `ICC_PMR_EL1` (0xFF),
/// so it is deliverable, and identical to the value `init` writes for SGIs/PPIs
/// — nothing here wants a priority hierarchy.
const IRQ_PRIORITY: u8 = 0xA0;

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
    addr::DEV_GIC_DIST_VA + off
}
#[inline]
fn gicr_rd(off: usize) -> usize {
    addr::DEV_GICR_RD_VA + off
}
#[inline]
fn gicr_sgi(off: usize) -> usize {
    addr::DEV_GICR_SGI_VA + off
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
pub fn mmio_w32(addr: usize, val: u32) {
    // SAFETY: `addr` is a device-mapped GIC MMIO register.
    unsafe {
        core::arch::asm!("str {v:w}, [{a}]", v = in(reg) val, a = in(reg) addr,
            options(nostack, preserves_flags));
    }
}
#[inline]
#[must_use]
pub fn mmio_r32(addr: usize) -> u32 {
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
    akuma_cpu::barrier::isb();
}
#[inline]
fn dsb_ish() {
    akuma_cpu::barrier::dsb_ish();
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

/// Enable a specific IRQ.
///
/// SGIs/PPIs (INTID < 32) live in this PE's redistributor, already configured
/// as Group 1 at [`IRQ_PRIORITY`] by [`init`] — enabling one is a single
/// `ISENABLER0` bit.
///
/// SPIs (INTID >= 32) are **not** pre-configured, because `init` only walks the
/// redistributor. Before 2026-08-19 this arm wrote `ISENABLER` alone and was
/// commented "best effort", which was accurate: an SPI enabled that way never
/// reaches the CPU, since its reset state is Group 0 and `ICC_IGRPEN1_EL1`
/// delivers Group 1 only. That was fine while the virtual timer (PPI 27) was
/// the only device interrupt this kernel registered; it stopped being fine when
/// the network stack needed a virtio-net RX interrupt to avoid waiting a whole
/// scheduler tick for every packet
/// (`docs/archive/AKUMA_NET_ISSUES.md` §3.1).
///
/// The SPI path now programs all four banks, in the order the architecture
/// requires — group, priority and route **before** enable, so the interrupt
/// cannot be delivered under a half-written configuration:
///
/// 1. `GICD_IGROUPR` — Group 1 Non-secure, matching `ICC_IGRPEN1_EL1`.
/// 2. `GICD_IPRIORITYR` — [`IRQ_PRIORITY`], below `ICC_PMR_EL1`'s 0xFF.
/// 3. `GICD_IROUTER` — affinity 0.0.0.0 (core 0), written explicitly rather
///    than relying on a reset value the architecture leaves UNKNOWN. Akuma
///    routes every device IRQ to the boot core; under `smp-shared` the handler
///    runs there and peers are reached through the scheduler SGI as before.
/// 4. `GICD_ISENABLER` — the enable bit, last.
///
/// `GICD_ICFGR` is deliberately untouched: virtio-mmio is level-triggered and
/// level is the reset state for SPIs, so writing it would only risk flipping a
/// correct setting.
pub fn enable_irq(irq: u32) {
    if irq >= 1020 {
        return; // Invalid / special INTID
    }
    if irq < 32 {
        // GICR SGI_base frame, device-mapped for CPU0.
        mmio_w32(gicr_sgi(gicr_sgi::ISENABLER0), 1u32 << irq);
        dsb_ish();
        return;
    }

    let idx = irq as usize;
    let word = (idx / 32) * 4;
    let bit = 1u32 << (irq % 32);

    // 1. Group 1 Non-secure (read-modify-write: the word holds 32 INTIDs).
    let grp_off = gicd::IGROUPR + word;
    mmio_w32(gicd(grp_off), mmio_r32(gicd(grp_off)) | bit);

    // 2. Priority. One byte per INTID, four per 32-bit word.
    let prio_off = gicd::IPRIORITYR + (idx / 4) * 4;
    let shift = (idx % 4) * 8;
    let prio = (mmio_r32(gicd(prio_off)) & !(0xFFu32 << shift))
        | (u32::from(IRQ_PRIORITY) << shift);
    mmio_w32(gicd(prio_off), prio);

    // 3. Route to core 0. IROUTER is 64-bit per INTID and indexed from 32;
    //    write it as two 32-bit stores because that is all `mmio_w32` offers,
    //    and the register is not required to be accessed atomically.
    let route_off = gicd::IROUTER + idx * 8;
    mmio_w32(gicd(route_off), 0); // Aff0/Aff1
    mmio_w32(gicd(route_off + 4), 0); // Aff2/Aff3, IRM=0 (targeted)

    // Configuration must land before the enable bit.
    dsb_ish();

    // 4. Enable.
    mmio_w32(gicd(gicd::ISENABLER + word), bit);
    dsb_ish();
    while mmio_r32(gicd(gicd::CTLR)) & GICD_CTLR_RWP != 0 {
        core::hint::spin_loop();
    }
}

/// Acknowledge an interrupt and return its INTID, or `None` if spurious.
#[must_use]
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
// Unused under `kernel_smp_shared` (self-targets via `trigger_sgi_core`); kept for the
// default/single-core path.
#[allow(dead_code)]
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

/// Trigger an SGI on a SPECIFIC core, identified by its affinity-0 (`MPIDR & 0xff`).
///
/// Unlike [`trigger_sgi`] (which hardcodes the target list to PE0), this targets one
/// peer in cluster Aff1=0 by setting that PE's bit in the 16-bit TargetList. Valid for
/// `aff0 < 16` (QEMU `virt` single cluster); larger affinities would need Aff1 routing.
/// Used by real shared-kernel SMP for each core to ring its OWN scheduler SGI (the
/// timer handler is shared, so it must self-target rather than hit PE0).
#[cfg(kernel_smp_shared)]
pub fn trigger_sgi_core(target_aff0: u32, sgi_id: u32) {
    if sgi_id > 15 || target_aff0 >= 16 {
        return;
    }
    // IRM=0, Aff3/2/1 = 0, INTID at [27:24], TargetList bit `target_aff0`.
    let val = (u64::from(sgi_id) << 24) | (1u64 << target_aff0);
    write_sysreg!("S3_0_C12_C11_5", val);
    let _ = ICC_SGI1R_EL1;
    dsb_ish();
    isb();
}

/// Send an SGI to every PE in affinity cluster 0.0.0 (aff0 0..15), self included.
///
/// `ICC_SGI1R_EL1`'s TargetList is a 16-bit mask over aff0 within one cluster,
/// so a single register write reaches every core Akuma runs on — the machine is
/// a flat `-smp N` with Aff1/2/3 = 0. Bits for PEs that do not exist are
/// ignored by the GIC, so no core count is needed here.
#[cfg(kernel_smp_shared)]
pub fn broadcast_sgi(sgi_id: u32) {
    if sgi_id > 15 {
        return;
    }
    // IRM=0 (use the target list), Aff3/2/1 = 0, INTID at [27:24],
    // TargetList = all 16 aff0 slots.
    let val = (u64::from(sgi_id) << 24) | 0xFFFF;
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
    let addr = addr::DEV_GICR_SGI_VA + gicr_sgi::IPRIORITYR + irq as usize;
    // SAFETY: `addr` is a device-mapped GIC MMIO register.
    unsafe {
        core::arch::asm!("strb {v:w}, [{a}]", v = in(reg) u32::from(priority), a = in(reg) addr,
            options(nostack, preserves_flags));
    }
}

/// Bring up THIS secondary's GICv3 receive path.
///
/// Enables the system-register CPU interface, wakes its redistributor, and
/// enables the scheduler SGI (INTID 0) plus the EL1 virtual-timer PPI (27). The
/// distributor's global config was already done once by the BSP in [`init`].
///
/// `idx` is the core's affinity-0 index; `layout` says where its frames are.
///
/// Secondaries reach the redistributor through the **low identity mapping**
/// (`boot.rs` L1[0] maps 0..1 GiB as a device block, and the GIC is below 1 GiB
/// on both supported machines) rather than through the L0[1] device window,
/// because this runs during bring-up on the boot page table. That is why this
/// takes a PA-derived layout while [`init`] uses the `addr::DEV_*` VAs — the two
/// are reaching the same hardware through different mappings, deliberately.
///
/// This lived in `src/smp_shared.rs` until 2026-09-01, where it carried its own
/// copies of `mmio_w32`, `mmio_r32`, the `GICR_WAKER_*` bits and four raw `msr`
/// instructions. All of them already existed here, so folding it in removed
/// three `unsafe` blocks outright rather than relocating them.
#[cfg(kernel_smp_shared)]
pub fn secondary_init(idx: usize, layout: RedistributorLayout) {
    // EL1 virtual-timer PPI (the shared 10 ms scheduler tick) and the scheduler
    // SGI, which each core rings at itself from the shared timer handler.
    const TIMER_PPI: u32 = 27;

    // CPU interface: system registers on, priority mask open, no sub-priority
    // grouping, Group 1 delivery enabled. Same sequence as `init` steps 1 and 5.
    let sre = read_sysreg!("S3_0_C12_C12_5");
    write_sysreg!("S3_0_C12_C12_5", sre | 1); // ICC_SRE_EL1.SRE
    isb();
    write_sysreg!("S3_0_C4_C6_0", 0xFF); // ICC_PMR_EL1
    write_sysreg!("S3_0_C12_C12_3", 0); // ICC_BPR1_EL1
    write_sysreg!("S3_0_C12_C12_7", 1); // ICC_IGRPEN1_EL1
    isb();

    let rd = layout.base_pa + idx * layout.stride;
    let sgi = rd + layout.sgi_offset;

    // Wake this PE's redistributor: clear ProcessorSleep, wait ChildrenAsleep.
    let waker = rd + gicr_rd::WAKER;
    mmio_w32(waker, mmio_r32(waker) & !GICR_WAKER_PROCESSOR_SLEEP);
    while mmio_r32(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }

    // All SGIs/PPIs Group 1, mid priority (8 INTIDs per 32-bit IPRIORITYR word).
    mmio_w32(sgi + gicr_sgi::IGROUPR0, 0xFFFF_FFFF);
    for i in 0..8 {
        mmio_w32(sgi + gicr_sgi::IPRIORITYR + i * 4, 0xA0A0_A0A0);
    }
    mmio_w32(sgi + gicr_sgi::ISENABLER0, (1u32 << SGI_SCHEDULER) | (1u32 << TIMER_PPI));

    // Redistributor writes must land before IRQs are unmasked.
    dsb_ish();
}

/// Ring `sgi_id` at the core executing this call, found from `MPIDR_EL1`'s
/// affinity-0 field.
///
/// The shared timer handler runs on whichever core took the tick, so it must
/// self-target rather than hit PE0 the way [`trigger_sgi`] does. Lived in
/// `src/gic.rs` (the deleted backend dispatcher) — it is the one entry point
/// there that was more than a `#[cfg]` forward.
#[cfg(kernel_smp_shared)]
pub fn trigger_sgi_self(sgi_id: u32) {
    let mpidr = akuma_cpu::sysreg::mpidr_el1();
    trigger_sgi_core((mpidr & 0xff) as u32, sgi_id);
}
