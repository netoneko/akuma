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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use akuma_exec::mmu::{attr_index, flags, phys_to_virt, MAIR_DEVICE_NGNRNE, MAIR_NORMAL_WB};
// Pure data plane + protocol lives in the host-testable `akuma-smp` crate; this
// module is the kernel glue (asm, PSCI, page tables, the pump).
use akuma_smp::{
    partition, Command, CoreStateMachine, Event, MachineConfig, Range, ENF_FAULTED, ENF_LEAKED,
    ENF_TESTING, MAGIC, MAX_CORES, MSG_PRESSURE, MSG_REPAID, STATE_BOOTING, STATE_OFFLINE,
    STATE_ONLINE,
};
use crate::console;
use crate::pmm;

/// Per-core boot stack size as a power-of-two shift (1 << 14 = 16 KiB). Only the
/// trampoline + `secondary_rust_start` run on it before the core switches to its
/// private isolated stack, so 16 KiB is ample. (Kernel/asm-only — not in the crate.)
const SECONDARY_STACK_SHIFT: usize = 14;

/// PSCI `CPU_ON` (SMC64) function id.
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// `Sync` wrapper: the BSP writes the inner config exactly once (single-threaded,
/// before any secondary runs); afterwards every access is either a read or a
/// cross-core atomic on a `state` field. The kernel is identity-mapped, so the
/// static's VA equals its PA — exactly the `context_id` we hand PSCI.
struct SyncConfig(UnsafeCell<MachineConfig>);
// SAFETY: see the type doc — initialization is single-threaded and ordered before
// any reader by the DSB-SY + CPU_ON handshake; live mutation is atomic-only.
unsafe impl Sync for SyncConfig {}

static MACHINE_CONFIG: SyncConfig = SyncConfig(UnsafeCell::new(MachineConfig::new()));

// DTB snapshot, taken in `probe_dtb` BEFORE the heap allocator can overwrite the
// DTB (on large-RAM configs QEMU places it exactly at heap_start). `bringup_
// secondaries` reads this stash, never the (possibly-clobbered) DTB.
static PROBED: AtomicBool = AtomicBool::new(false);
static NUM_CORES: AtomicUsize = AtomicUsize::new(0);
static USE_HVC: AtomicBool = AtomicBool::new(true);
static PROBED_MPIDRS: [AtomicU64; MAX_CORES] = [const { AtomicU64::new(0) }; MAX_CORES];

unsafe extern "C" {
    /// Secondary trampoline (asm below). Its link address equals its physical
    /// address under the identity map, so `secondary_entry as usize` is the
    /// PSCI entry point.
    fn secondary_entry();

    /// Switch this PE onto its restricted table and private stack, then jump to
    /// `secondary_main(cfg_pa, core_idx)` (never returns). Implemented in asm
    /// because the TTBR0+SP switch must be done in pure register ops — the old
    /// (shared) stack becomes unmapped the instant TTBR0 changes.
    fn secondary_enter_isolated(ttbr0_phys: u64, sp: u64, cfg_pa: u64, core_idx: u64) -> !;

    /// Per-core minimal exception vector table (asm below), 2 KiB-aligned. Its
    /// "Current EL SPx, Synchronous" slot points at `smp_sync_handler`, which
    /// records a cross-core fault and skips the offending instruction. Set into
    /// `VBAR_EL1` by the secondary for the Stage-2 enforcement self-test.
    static smp_vectors: u8;
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

const PAGE: usize = 4096;
/// 2 MiB block size — the granule the secondary's partition is identity-mapped at
/// (one L2 block descriptor per 2 MiB; 512 of them fill one L2 table = 1 GiB).
const TWO_MB: usize = 2 * 1024 * 1024;

/// Number of contiguous pages for a secondary's isolated boot stack (16 KiB).
const STACK_PAGES: usize = 4;

/// R2 — initial per-core kernel heap, carved from the secondary's partition just
/// above the BSP-built kernel image (page tables + replicated `.data`/`.bss` +
/// stack + PerCpu). 2 MiB is ample to seed the secondary's `talc` and hold its PMM
/// bitmap (a 1 GiB partition's bitmap is only 32 KiB); the heap then grows on
/// demand from the secondary's own PMM via the OOM handler, exactly like the BSP.
const SECONDARY_HEAP_BYTES: usize = 2 * 1024 * 1024;

/// Bump allocator over a single core's RAM partition. The BSP uses it to carve a
/// secondary's ENTIRE bringup working set — page-table pages, the replicated
/// `.data`/`.bss`, the boot stack, the PerCpu page — from that core's OWN
/// partition, never from the BSP `pmm`. Two payoffs (docs/MULTIKERNEL.md §15, R2):
/// the BSP physical pool stays untouched by secondary setup, and the consumed
/// prefix becomes the secondary's `kernel_end` — the exact cut its per-core PMM
/// marks used before managing the rest of the partition as free pages.
///
/// Pages are zeroed through the BSP's identity map (`phys_to_virt`), which spans
/// all detected RAM during bringup, so the partition (even a high one the restricted
/// table will later own) is writable here.
struct PartitionBump {
    cursor: usize,
    end: usize,
}

impl PartitionBump {
    fn new(base: usize, len: usize) -> Self {
        Self { cursor: base, end: base + len }
    }

    /// Carve `n` contiguous, zeroed 4 KiB pages; returns the base PA, or `None` if
    /// the partition is exhausted.
    fn alloc_pages(&mut self, n: usize) -> Option<usize> {
        let bytes = n * PAGE;
        if bytes > self.end - self.cursor {
            return None;
        }
        let pa = self.cursor;
        self.cursor += bytes;
        // SAFETY: `pa` is a partition PA, identity-mapped in the BSP boot tables
        // (which cover all RAM); zero the freshly-carved pages before use.
        unsafe { core::ptr::write_bytes(phys_to_virt(pa), 0, bytes) };
        Some(pa)
    }

    #[inline]
    fn alloc_page(&mut self) -> Option<usize> {
        self.alloc_pages(1)
    }

    #[inline]
    fn cursor(&self) -> usize {
        self.cursor
    }
}

unsafe extern "C" {
    /// Linker symbol: first byte of `.data` (page-aligned). Everything below it
    /// (`.text`/`.rodata`, from KERNEL_PHYS_BASE) is read-only shareable code; at
    /// and above it is the kernel's WRITABLE window (`.data` then `.bss`).
    static _data_start: u8;
    /// First byte of `.bss` (= end of `.data`). `[_data_start, __bss_start)` is the
    /// initialized `.data` we snapshot pristine for per-core replication.
    static __bss_start: u8;
    /// End of `.bss` (page-rounded `_kernel_phys_end`). `[_data_start,
    /// _kernel_phys_end)` is the full writable window replicated per core.
    static _kernel_phys_end: u8;
}

/// Pristine snapshot of `.data`, taken by [`snapshot_pristine_data`] at the very
/// start of boot before anything mutates it. The BSP copies this into each
/// secondary's private `.data` so its replicated statics start from correct
/// initial values (not the BSP's mutated runtime state). `.bss` needs no snapshot
/// (it's zero). Sized to an upper bound of `.data`; a boot guard checks the fit.
/// In `.bss` itself (so it's NOLOAD — no `.bin` bloat); `smp`-only.
const DATA_SNAPSHOT_CAP: usize = 512 * 1024;
struct DataSnapshot(UnsafeCell<[u8; DATA_SNAPSHOT_CAP]>);
// SAFETY: written once at boot (single-threaded) before any secondary; read-only after.
unsafe impl Sync for DataSnapshot {}
static DATA_SNAPSHOT: DataSnapshot = DataSnapshot(UnsafeCell::new([0; DATA_SNAPSHOT_CAP]));

/// Replication self-test static (R1). A `.data` static (non-zero init) so it also
/// exercises the snapshot path. Each secondary does `fetch_add(1)`; if replication
/// works, every secondary sees `INIT` then `INIT+1` in ITS OWN copy while the BSP's
/// copy stays `INIT`. If the static were shared, the BSP's copy would change.
const REPL_TEST_INIT: u64 = 0xAA00;
static SMP_REPLICATION_TEST: AtomicU64 = AtomicU64::new(REPL_TEST_INIT);
/// PerCpu byte offset where a secondary records its replication-test read-back.
const PERCPU_REPL_TEST: usize = 32;
/// PerCpu byte offsets where a secondary records its R2 (per-core PMM + heap)
/// self-test result for the BSP to verify (all distinct from the offsets above).
const PERCPU_R2_PAGES: usize = 40; // # pages its private PMM handed out
const PERCPU_R2_FIRST_PA: usize = 48; // PA of the first such page (BSP checks in-partition)
const PERCPU_R2_HEAP_OK: usize = 56; // 1 if a private-heap alloc round-tripped
const PERCPU_R2_FREE: usize = 64; // its private PMM free-page count after the test

/// Snapshot pristine `.data` — MUST be the first thing `rust_start` does, before any
/// code mutates a `.data`/`.bss` static. Copies `[_data_start, __bss_start)` into
/// `DATA_SNAPSHOT`. Boots-halts if `.data` exceeds the snapshot capacity (so the
/// cap can't silently truncate). `smp`-only; the default build never calls it.
pub fn snapshot_pristine_data() {
    let data_start = &raw const _data_start as usize;
    let data_end = &raw const __bss_start as usize;
    let len = data_end - data_start;
    if len > DATA_SNAPSHOT_CAP {
        // Can't print safely this early on all paths; just refuse to proceed.
        loop {
            core::hint::spin_loop();
        }
    }
    // SAFETY: single-threaded at the very start of boot; copying the live `.data`
    // image into the snapshot buffer before anything mutates it.
    unsafe {
        core::ptr::copy_nonoverlapping(
            data_start as *const u8,
            DATA_SNAPSHOT.0.get().cast::<u8>(),
            len,
        );
    }
}

/// Physical/virtual base of the kernel image (identity-mapped; see boot.rs).
const KERNEL_PHYS_BASE: usize = 0x4010_0000;

/// L3 leaf flags. AArch64 quirk: at L3 a *page* descriptor sets bit[1] (the same
/// bit named `TABLE` at upper levels), so valid L3 leaves carry `VALID | TABLE`.
/// Code: RO at all ELs, EL1-executable (no PXN), EL0 non-exec (UXN).
fn pte_code() -> u64 {
    flags::VALID | flags::TABLE | flags::AF | flags::SH_INNER
        | attr_index(MAIR_NORMAL_WB) | flags::AP_RO_ALL | flags::UXN
}
/// Data: RW at all ELs, non-executable everywhere (PXN|UXN).
fn pte_rw() -> u64 {
    flags::VALID | flags::TABLE | flags::AF | flags::SH_INNER
        | attr_index(MAIR_NORMAL_WB) | flags::AP_RW_ALL | flags::PXN | flags::UXN
}
/// Device-nGnRnE (MAIR index 0, set by the trampoline), RW, non-executable. For
/// mapping a secondary's own GIC redistributor frames into its restricted table.
fn pte_device() -> u64 {
    flags::VALID | flags::TABLE | flags::AF | flags::SH_OUTER
        | attr_index(MAIR_DEVICE_NGNRNE) | flags::PXN | flags::UXN
}
/// L2 *block* descriptor (2 MiB), Normal Write-Back, RW at all ELs, non-executable.
/// Note `flags::BLOCK` (bit[1] = 0): at L2 a cleared TABLE bit selects a block, not
/// a table pointer. Used to identity-map a secondary's whole partition cheaply.
fn pte_block_rw() -> u64 {
    flags::VALID | flags::BLOCK | flags::AF | flags::SH_INNER
        | attr_index(MAIR_NORMAL_WB) | flags::AP_RW_ALL | flags::PXN | flags::UXN
}

// --- GICv3 redistributor (per-PE MMIO) for the cross-core SGI doorbell (§7) ---
/// GICR base PA on QEMU `virt`; CPU `i`'s frames are at `base + i*GICR_STRIDE`.
const GICR_BASE: usize = 0x080A_0000;
/// Per-PE stride: an RD frame (64 KiB) + an SGI frame (64 KiB).
const GICR_STRIDE: usize = 0x2_0000;
/// SGI frame offset within a PE's redistributor.
const GICR_SGI_OFFSET: usize = 0x1_0000;
const GICR_WAKER: usize = 0x0014; // in the RD frame
const GICR_WAKER_PROCESSOR_SLEEP: u32 = 1 << 1;
const GICR_WAKER_CHILDREN_ASLEEP: u32 = 1 << 2;
const GICR_SGI_IGROUPR0: usize = 0x0080; // in the SGI frame
const GICR_SGI_ISENABLER0: usize = 0x0100;
const GICR_SGI_IPRIORITYR: usize = 0x0400;
/// SGI INTID used as the multikernel doorbell (distinct from the BSP scheduler's
/// SGI 0; redistributor config is per-PE, so the choice is independent anyway).
const DOORBELL_SGI: u32 = 1;
/// PerCpu byte offset where the IRQ handler counts doorbell SGIs it serviced.
const PERCPU_DOORBELL_COUNT: usize = 24;

/// EL1 virtual timer PPI (INTID 27) — the per-core heartbeat tick. Per-PE, so it
/// doesn't conflict with the BSP's own scheduler timer on the same INTID.
const TIMER_PPI: u32 = 27;
/// Heartbeat tick interval in virtual-counter ticks. `0x10_0000` ≈ 17–44 ms across
/// QEMU's typical 24–62.5 MHz `CNTFRQ`. Loadable as a single `movz #0x10, lsl #16`,
/// so the asm IRQ handler re-arms with the SAME value — keep them in sync.
const TIMER_INTERVAL_TICKS: u64 = 0x10_0000;

/// ISV-safe 32-bit MMIO (single `str`/`ldr`, no writeback) — same reasoning as
/// `gic_v3::mmio_w32` (writeback/pair forms assert under QEMU HVF).
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

/// Bring up THIS secondary's GICv3 receive path so a cross-core doorbell SGI can be
/// delivered: enable the system-register CPU interface (sysregs — no mapping), wake
/// this PE's redistributor and enable the doorbell SGI (MMIO — the RD/SGI frames are
/// mapped device in the restricted table). The distributor's global config (ARE +
/// Group 1) was already done by the BSP and is system-wide.
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
    // Wake this redistributor: clear ProcessorSleep, wait ChildrenAsleep.
    let waker = rd + GICR_WAKER;
    mmio_w32(waker, mmio_r32(waker) & !GICR_WAKER_PROCESSOR_SLEEP);
    while mmio_r32(waker) & GICR_WAKER_CHILDREN_ASLEEP != 0 {
        core::hint::spin_loop();
    }
    // SGIs/PPIs to Group 1, mid priority, then enable the doorbell SGI AND the
    // virtual-timer PPI (INTID 27) — the periodic heartbeat-tick / wfe wakeup.
    mmio_w32(sgi + GICR_SGI_IGROUPR0, 0xFFFF_FFFF);
    for i in 0..8 {
        mmio_w32(sgi + GICR_SGI_IPRIORITYR + i * 4, 0xA0A0_A0A0);
    }
    mmio_w32(sgi + GICR_SGI_ISENABLER0, (1u32 << DOORBELL_SGI) | (1u32 << TIMER_PPI));
    // SAFETY: ensure the redistributor writes complete before IRQs are unmasked.
    unsafe { core::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
}

/// Read (or allocate+link) the next-level table under `table[idx]`, returning its
/// physical address. Intermediate tables come from the PMM (the BSP builds these
/// on its boot tables, where `phys_to_virt` is identity, so writes land in RAM).
fn get_or_create(table: *mut u64, idx: usize, bump: &mut PartitionBump) -> Option<usize> {
    // SAFETY: `table` is an identity-mapped page-table page; `idx < 512`.
    unsafe {
        let e = table.add(idx).read_volatile();
        if e & flags::VALID != 0 {
            return Some((e & 0x0000_FFFF_FFFF_F000) as usize);
        }
        let pa = bump.alloc_page()?;
        table
            .add(idx)
            .write_volatile((pa as u64) | flags::VALID | flags::TABLE);
        Some(pa)
    }
}

/// Map one 4 KiB page `pa -> va` with `leaf_flags` into the table rooted at
/// `l0_pa`, creating intermediate levels as needed (carved from `bump`).
fn map_4k(l0_pa: usize, va: usize, pa: usize, leaf_flags: u64, bump: &mut PartitionBump) -> Option<()> {
    let l0 = phys_to_virt(l0_pa).cast::<u64>();
    let l1 = phys_to_virt(get_or_create(l0, (va >> 39) & 0x1FF, bump)?).cast::<u64>();
    let l2 = phys_to_virt(get_or_create(l1, (va >> 30) & 0x1FF, bump)?).cast::<u64>();
    let l3 = phys_to_virt(get_or_create(l2, (va >> 21) & 0x1FF, bump)?).cast::<u64>();
    // SAFETY: `l3` is an identity-mapped L3 table; index < 512.
    unsafe {
        l3.add((va >> 12) & 0x1FF)
            .write_volatile((pa as u64) | leaf_flags);
    }
    Some(())
}

fn map_range_4k(l0_pa: usize, base: usize, len: usize, leaf_flags: u64, bump: &mut PartitionBump) -> Option<()> {
    let pages = len.div_ceil(PAGE);
    for i in 0..pages {
        let a = base + i * PAGE;
        map_4k(l0_pa, a, a, leaf_flags, bump)?; // identity (va == pa)
    }
    Some(())
}

/// Map one 2 MiB block `pa -> va` with `leaf_flags` (an L2 *block* descriptor)
/// into the table rooted at `l0_pa`. L0/L1 intermediates are carved from `bump`;
/// if an L2 table already exists under this L1 entry (e.g. the kernel image shares
/// the 1 GiB region), the block is written into a free slot of that same L2 —
/// kernel 4 KiB maps sit at low L2 indices, partition blocks at high ones.
fn map_2mb(l0_pa: usize, va: usize, pa: usize, leaf_flags: u64, bump: &mut PartitionBump) -> Option<()> {
    let l0 = phys_to_virt(l0_pa).cast::<u64>();
    let l1 = phys_to_virt(get_or_create(l0, (va >> 39) & 0x1FF, bump)?).cast::<u64>();
    let l2 = phys_to_virt(get_or_create(l1, (va >> 30) & 0x1FF, bump)?).cast::<u64>();
    // SAFETY: `l2` is an identity-mapped L2 table; index < 512.
    unsafe {
        l2.add((va >> 21) & 0x1FF)
            .write_volatile((pa as u64) | leaf_flags);
    }
    Some(())
}

/// Identity-map `[base, base+len)` as 2 MiB RW blocks (`len` must be 2 MiB-aligned).
/// This is the keystone of R2: it gives the secondary direct access to ALL of its
/// partition — the BSP-carved kernel image, the heap slab, and every page its
/// per-core PMM will later hand out — so its `alloc`/page-table walks resolve to
/// real, mapped RAM in its own partition.
fn map_partition_blocks(l0_pa: usize, base: usize, len: usize, bump: &mut PartitionBump) -> Option<()> {
    let blocks = len / TWO_MB;
    for i in 0..blocks {
        let a = base + i * TWO_MB;
        map_2mb(l0_pa, a, a, pte_block_rw(), bump)?;
    }
    Some(())
}

/// Map the kernel's writable window `[_data_start, _kernel_phys_end)` (`.data` +
/// `.bss`) to freshly-allocated PRIVATE pages at the SAME VA in the table rooted at
/// `l0_pa`. `.data` pages are initialized from the pristine [`DATA_SNAPSHOT`]; `.bss`
/// pages stay zero. After this, the shared kernel code running on this table sees
/// its OWN copy of every `static` — the core of per-core isolation (§4.2).
///
/// `[skip_va, skip_va+skip_len)` (the SHARED descriptor) is left UNMAPPED here — the
/// caller maps it shared afterwards. Replicating it would give the secondary a
/// private, zeroed descriptor and break the cross-core comms contract.
fn replicate_writable_window(l0_pa: usize, skip_va: usize, skip_len: usize, bump: &mut PartitionBump) -> Option<()> {
    let data_start = &raw const _data_start as usize;
    let bss_start = &raw const __bss_start as usize;
    let end = (&raw const _kernel_phys_end as usize).next_multiple_of(PAGE);
    let snap = DATA_SNAPSHOT.0.get().cast::<u8>();
    let skip_end = skip_va + skip_len;

    let mut va = data_start; // page-aligned by the linker
    while va < end {
        // Skip the shared descriptor's page(s) (mapped shared by the caller).
        if va < skip_end && va + PAGE > skip_va {
            va += PAGE;
            continue;
        }
        let page = bump.alloc_page()?; // from the partition, zeroed → correct for `.bss`
        if va < bss_start {
            // Copy this page's `.data` bytes from the pristine snapshot.
            let copy_len = core::cmp::min(PAGE, bss_start - va);
            // SAFETY: snapshot covers `[0, data_len)`; `page` is identity-mapped on
            // the BSP, so we can write its initial contents here.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    snap.add(va - data_start),
                    phys_to_virt(page),
                    copy_len,
                );
            }
        }
        map_4k(l0_pa, va, page, pte_rw(), bump)?;
        va += PAGE;
    }
    Some(())
}

/// Build core `idx`'s RESTRICTED page table and private working set. Maps ONLY:
/// shared kernel code RO+X, the descriptor page RW, and this core's own stack +
/// PerCpu RW (all identity). Peer partitions and BSP `.data`/`.bss`/heap are left
/// unmapped, so any cross-core access from the secondary faults. Records
/// `ttbr0_phys`/`entry_sp`/`percpu_phys` in the descriptor. Returns `false` (and
/// leaves `ttbr0_phys == 0`) on OOM, so the secondary falls back to a parked spin.
fn build_isolated_table(cfg: &mut MachineConfig, idx: usize) -> bool {
    // R2: carve EVERYTHING for this secondary from its OWN partition (page tables,
    // replicated window, stack, PerCpu) via a bump allocator — never the BSP `pmm`.
    // The consumed prefix becomes the secondary's `kernel_end`.
    let pbase = cfg.cores[idx].ram_base as usize;
    let plen = cfg.cores[idx].ram_len as usize;
    let mut bump = PartitionBump::new(pbase, plen);

    let Some(l0_pa) = bump.alloc_page() else {
        return false;
    };

    // 1. Shared kernel code (.text/.rodata): [KERNEL_PHYS_BASE, _data_start) RO+X.
    let code_end = &raw const _data_start as usize;
    let code_len = code_end.saturating_sub(KERNEL_PHYS_BASE);
    if map_range_4k(l0_pa, KERNEL_PHYS_BASE, code_len, pte_code(), &mut bump).is_none() {
        return false;
    }

    // 2. R1 — per-core REPLICATED writable window: map [_data_start,
    // _kernel_phys_end) (the kernel's `.data` + `.bss`) to PRIVATE physical pages
    // at the SAME kernel VA, so `static PMM`/allocator/etc. resolve to this core's
    // own instance (docs/MULTIKERNEL.md §4.2) — the same shared code, isolated by
    // the page tables. `.data` is initialized from the pristine snapshot; `.bss` is
    // zeroed. This is what lets a secondary later run its own pmm/heap/exec.
    //
    // EXCEPTION: the descriptor (`MACHINE_CONFIG`) is itself a `.bss` static, but it
    // is the SHARED cross-core comms region — it must NOT be replicated. We skip its
    // page(s) here and map them shared next.
    let cfg_pa = core::ptr::from_ref::<MachineConfig>(cfg) as usize;
    let cfg_len = core::mem::size_of::<MachineConfig>();
    if replicate_writable_window(l0_pa, cfg_pa, cfg_len, &mut bump).is_none() {
        return false;
    }

    // 2b. The SHARED descriptor page(s) — map to the BSP's single copy (identity),
    // overriding any replicated mapping, so every core sees the same rings/state.
    if map_range_4k(l0_pa, cfg_pa, cfg_len, pte_rw(), &mut bump).is_none() {
        return false;
    }

    // 3. This core's private stack (contiguous) + PerCpu page — carved from the
    // partition. No explicit 4 KiB identity map needed: the 2 MiB partition block
    // map in step 5 covers them (they live low in the partition).
    let Some(stack_pa) = bump.alloc_pages(STACK_PAGES) else {
        return false;
    };
    let Some(percpu_pa) = bump.alloc_page() else {
        return false;
    };

    // 4. This core's own GIC redistributor frames (device, OUTSIDE the partition),
    // so it can wake its redistributor and receive the doorbell SGI (§7). Only THIS
    // core's frames — peers' redistributors and all RAM-isolation properties are
    // untouched.
    let rd = GICR_BASE + idx * GICR_STRIDE;
    let sgi = rd + GICR_SGI_OFFSET;
    if map_4k(l0_pa, rd, rd, pte_device(), &mut bump).is_none()
        || map_4k(l0_pa, sgi, sgi, pte_device(), &mut bump).is_none()
    {
        return false;
    }

    // 5. R2 — identity-map this core's ENTIRE partition as 2 MiB RW blocks, so the
    // secondary can address all of it: the bump-carved kernel image (above), the
    // heap slab it seeds, and every page its per-core PMM will hand out. `ram_len`
    // is rounded DOWN to a 2 MiB multiple (the last core may absorb an unaligned
    // remainder); the secondary's PMM is given the same rounded length so it never
    // hands out an unmapped tail page.
    let len_2mb = plen & !(TWO_MB - 1);
    if map_partition_blocks(l0_pa, pbase, len_2mb, &mut bump).is_none() {
        return false;
    }

    cfg.cores[idx].ttbr0_phys = l0_pa as u64;
    cfg.cores[idx].entry_sp = (stack_pa + STACK_PAGES * PAGE) as u64;
    cfg.cores[idx].percpu_phys = percpu_pa as u64;
    // R2: the consumed prefix of the partition is the secondary's `kernel_end` — its
    // per-core PMM marks [pbase, kernel_end) used and manages the rest as free.
    cfg.cores[idx].kernel_end = bump.cursor() as u64;
    true
}

/// Snapshot CPU MPIDRs + PSCI conduit from the DTB into module statics. MUST be
/// called early in boot (before `allocator::init`): on large-RAM configs QEMU
/// places the DTB exactly at heap_start, so the heap clobbers it before the late
/// `bringup_secondaries` runs. `detect_memory` already proved the DTB parses here.
pub fn probe_dtb(dtb_ptr: usize) {
    let actual_dtb = resolve_dtb(dtb_ptr);
    if actual_dtb == 0 {
        console::print("[SMP] probe: no DTB\n");
        return;
    }
    // SAFETY: `actual_dtb` carries a verified FDT magic.
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr(actual_dtb as *const u8) }) else {
        console::print("[SMP] probe: invalid DTB\n");
        return;
    };
    let (mpidrs, num_cores) = collect_mpidrs(&fdt);
    let use_hvc = psci_is_hvc(&fdt);
    for (i, m) in PROBED_MPIDRS.iter().enumerate() {
        m.store(mpidrs[i], Ordering::Relaxed);
    }
    NUM_CORES.store(num_cores, Ordering::Relaxed);
    USE_HVC.store(use_hvc, Ordering::Relaxed);
    PROBED.store(true, Ordering::Release);
    console::print("[SMP] probe: ");
    console::print_dec(num_cores);
    console::print(" core(s), conduit=");
    console::print(if use_hvc { "hvc\n" } else { "smc\n" });
}

/// Hand each secondary core sole ownership of its RAM partition by removing those
/// ranges from the BSP's PMM. MUST run right after `pmm::init`/`mark_pmm_ready` and
/// BEFORE any other BSP allocation (e.g. `mmu::init`), so the BSP can never hand out
/// a page that a secondary's per-core PMM (R2) also owns — the two pools stay
/// strictly disjoint. The BSP keeps only its own partition. No-op single-core.
/// Uses the [`probe_dtb`] stash (the DTB itself may be heap-clobbered by now).
pub fn reserve_secondary_partitions(ram_base: usize, ram_size: usize) {
    if !PROBED.load(Ordering::Acquire) {
        return;
    }
    let num_cores = NUM_CORES.load(Ordering::Relaxed);
    if num_cores <= 1 {
        return;
    }
    let bsp_idx = (read_mpidr() & 0xff) as usize;
    let parts = partition(ram_base, ram_size, num_cores);
    let mut reserved_mb = 0usize;
    for (idx, &(base, len)) in parts.iter().enumerate().take(num_cores) {
        if idx == bsp_idx {
            continue;
        }
        pmm::reserve_range(base as usize, len as usize);
        reserved_mb += (len / (1024 * 1024)) as usize;
    }
    console::print("[SMP] reserved ");
    console::print_dec(reserved_mb);
    console::print(" MB of secondary partitions from the BSP PMM\n");
}

/// BSP entry point: wake every secondary PE and wait for it to report `Online`.
/// No-op (single-core) when the DTB enumerated only one CPU. Uses the stash from
/// [`probe_dtb`] — the DTB itself may already be heap-clobbered by now.
pub fn bringup_secondaries(ram_base: usize, ram_size: usize) {
    if !PROBED.load(Ordering::Acquire) {
        console::print("[SMP] not probed; staying single-core\n");
        return;
    }
    let num_cores = NUM_CORES.load(Ordering::Relaxed);
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    let mut mpidrs = [0u64; MAX_CORES];
    for (i, m) in PROBED_MPIDRS.iter().enumerate() {
        mpidrs[i] = m.load(Ordering::Relaxed);
    }
    let bsp_idx = (read_mpidr() & 0xff) as usize;

    console::print("[SMP] ");
    console::print_dec(num_cores);
    console::print(" core(s); BSP is core ");
    console::print_dec(bsp_idx);
    console::print("\n");

    if num_cores <= 1 {
        console::print("[SMP] single core; no secondaries to bring up\n");
        return;
    }

    // Fill the descriptor (single-threaded; before any CPU_ON).
    // SAFETY: no secondary is running yet, so this exclusive &mut is sound.
    let cfg = unsafe { &mut *MACHINE_CONFIG.0.get() };
    cfg.magic = MAGIC;
    cfg.version = 1;
    cfg.num_cores = num_cores as u32;
    cfg.config_phys_addr = core::ptr::from_mut(cfg) as u64;
    let parts = partition(ram_base, ram_size, num_cores);
    for (idx, &mpidr) in mpidrs.iter().enumerate().take(num_cores) {
        let (pbase, plen) = parts[idx];
        cfg.cores[idx].mpidr = mpidr;
        cfg.cores[idx].ram_base = pbase;
        cfg.cores[idx].ram_len = plen;
        // kernel_end = pmm "used below here" cut. The BSP keeps its real cut; for a
        // secondary it starts at its partition base (the BSP bumps it when it builds
        // the core's private image in the page-table-isolation step).
        cfg.cores[idx].kernel_end = pbase;
        cfg.cores[idx].state.store(STATE_OFFLINE, Ordering::Relaxed);
        console::print("[SMP] core ");
        console::print_dec(idx);
        console::print(" partition: base=0x");
        console::print_hex(pbase);
        console::print(" len=");
        console::print_dec((plen / (1024 * 1024)) as usize);
        console::print(" MB\n");
    }

    // Build each secondary's RESTRICTED, isolated page table (shared code RO +
    // descriptor RW + own stack/PerCpu RW; peers unmapped). On OOM the core's
    // ttbr0_phys stays 0 and it falls back to a parked spin on the boot tables.
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let ok = build_isolated_table(cfg, idx);
        console::print("[SMP] core ");
        console::print_dec(idx);
        if ok {
            console::print(" isolated table: ttbr0=0x");
            console::print_hex(cfg.cores[idx].ttbr0_phys);
            console::print(" sp=0x");
            console::print_hex(cfg.cores[idx].entry_sp);
            console::print("\n");
        } else {
            console::print(" isolated table: OOM (falling back to shared park)\n");
        }
    }

    let cfg_pa = core::ptr::from_ref::<MachineConfig>(cfg) as u64;
    let entry_pa = secondary_entry as *const () as usize as u64;

    // Publish the descriptor + freshly-built page tables to RAM before any
    // secondary's MMU-on read / table walk.
    dsb_sy();

    console::print("[SMP] conduit=");
    console::print(if use_hvc { "hvc" } else { "smc" });
    console::print(", entry=0x");
    console::print_hex(entry_pa);
    console::print(", descriptor=0x");
    console::print_hex(cfg_pa);
    console::print("\n");

    // R2 proof baseline: the BSP's own free-page count BEFORE the secondaries run.
    // Each secondary allocs from its OWN (replicated) PMM over its OWN partition —
    // which the BSP PMM no longer owns (reserved at boot) — so this must be unchanged
    // after they run. The BSP does not allocate in the wake/wait loops below.
    let bsp_free_before = pmm::free_count();

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
        console::print(if online { " ONLINE" } else { " TIMEOUT (never reported online)\n" });
        if !online {
            continue;
        }
        // Bringup-time verification (BSP runs on the all-seeing boot table): confirm
        // the secondary ran secondary_main *in isolation* by checking the marker it
        // wrote to its private PerCpu page. Proves the TTBR0 switch + restricted
        // table are correct, not merely that the core didn't crash.
        let percpu = cfg.cores[idx].percpu_phys as usize;
        if percpu != 0 {
            // SAFETY: percpu PA is identity-mapped in the BSP boot tables.
            let marker = unsafe { core::ptr::read_volatile(percpu as *const u64) };
            if marker == MAGIC ^ idx as u64 {
                console::print(" (isolated-run confirmed)");
            } else {
                console::print(" (WARNING: PerCpu marker missing — ran on shared tables?)");
            }
            // R1: per-core .data/.bss replication. The secondary mutated the shared
            // static into its OWN copy (→ INIT+1); the BSP's copy must be untouched.
            let repl = unsafe { core::ptr::read_volatile((percpu + PERCPU_REPL_TEST) as *const u64) };
            let bsp_copy = SMP_REPLICATION_TEST.load(Ordering::SeqCst);
            if repl == REPL_TEST_INIT + 1 && bsp_copy == REPL_TEST_INIT {
                console::print(" [replication: private .data/.bss ✓]");
            } else {
                console::print(" [replication: FAILED secondary=0x");
                console::print_hex(repl);
                console::print(" bsp=0x");
                console::print_hex(bsp_copy);
                console::print(" ✗]");
            }
        }
        // Stage 2: report the cross-core enforcement self-test outcome.
        match cfg.enforcement_results[idx].load(Ordering::Acquire) {
            ENF_FAULTED => console::print(" [enforcement: cross-core access FAULTED ✓]\n"),
            ENF_LEAKED => console::print(" [enforcement: LEAKED — isolation breach! ✗]\n"),
            _ => console::print(" [enforcement: inconclusive]\n"),
        }
        // R2: per-core PMM + heap. The secondary stood up its own allocator/pmm over
        // its partition and recorded the result. Verify: it handed out pages, the
        // first one is INSIDE this core's partition (not the BSP's), the private-heap
        // round-trip succeeded, and the BSP's own free-page count is UNCHANGED.
        if percpu != 0 {
            let pages = unsafe { core::ptr::read_volatile((percpu + PERCPU_R2_PAGES) as *const u64) };
            let first_pa = unsafe { core::ptr::read_volatile((percpu + PERCPU_R2_FIRST_PA) as *const u64) };
            let heap_ok = unsafe { core::ptr::read_volatile((percpu + PERCPU_R2_HEAP_OK) as *const u64) };
            let sec_free = unsafe { core::ptr::read_volatile((percpu + PERCPU_R2_FREE) as *const u64) };
            let pbase = cfg.cores[idx].ram_base;
            let pend = pbase + cfg.cores[idx].ram_len;
            let in_partition = first_pa >= pbase && first_pa < pend;
            let bsp_untouched = pmm::free_count() == bsp_free_before;
            console::print("[SMP] core ");
            console::print_dec(idx);
            if pages > 0 && in_partition && heap_ok == 1 && bsp_untouched {
                console::print(" R2: per-core pmm+heap ✓ (alloc'd ");
                console::print_dec(pages as usize);
                console::print(" pages from 0x");
                console::print_hex(first_pa);
                console::print(", heap ok, free=");
                console::print_dec(sec_free as usize);
                console::print(" pages; BSP pool untouched ✓)\n");
            } else {
                console::print(" R2: FAILED (pages=");
                console::print_dec(pages as usize);
                console::print(" first=0x");
                console::print_hex(first_pa);
                console::print(" in_part=");
                console::print(if in_partition { "1" } else { "0" });
                console::print(" heap=");
                console::print_dec(heap_ok as usize);
                console::print(" bsp_untouched=");
                console::print(if bsp_untouched { "1" } else { "0" });
                console::print(" ✗)\n");
            }
        }
    }

    console::print("[SMP] bringup complete\n");

    monitor_liveness(cfg, num_cores, bsp_idx);
    run_memory_demo(cfg, num_cores, bsp_idx);
}

/// Step 3 — drive the debt-based reclaim protocol (§9) over real rings on real
/// cores using the **host-tested** `akuma_smp::CoreStateMachine`. The BSP plays the
/// creditor under pressure: at bringup each secondary's machine was pre-loaded with
/// a (faked) debt to the BSP; here the BSP broadcasts the pressure signal, each
/// debtor's machine sheds its debt back (a `Repay` → `MSG_REPAID` on the BSP's
/// ring), and the BSP's own machine `Accept`s each repayment (zero + reclaim). All
/// values are faked (no per-core PMM yet) — logged only — but the PROTOCOL LOGIC is
/// exactly the code the host simulator validates.
fn run_memory_demo(cfg: &MachineConfig, num_cores: usize, bsp_idx: usize) {
    const FAKED_PRESSURE_PCT: u64 = 30;

    let Some(bsp_inbox) = cfg.inboxes.get(bsp_idx) else {
        return;
    };

    // The BSP's own state machine: it lent to each secondary and is now low.
    let mut bsp_sm = CoreStateMachine::new(bsp_idx as u32, num_cores, FAKED_PRESSURE_PCT, 50);

    // Broadcast the pressure signal to every secondary (fire-and-forget).
    let mut sent = 0;
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        if let Some(peer) = cfg.inboxes.get(idx)
            && peer.push(MSG_PRESSURE, bsp_idx as u32, 0, 0)
        {
            sent += 1;
        }
    }
    console::print("[SMP] core ");
    console::print_dec(bsp_idx);
    console::print(" broadcast pressure signal (");
    console::print_dec(FAKED_PRESSURE_PCT as usize);
    console::print("% used) to ");
    console::print_dec(sent);
    console::print(" peer(s)\n");

    // M2 — ring the cross-core doorbell SGI at each secondary. They also poll, so
    // this isn't required for progress; it proves SGI *delivery*: each secondary's
    // IRQ handler bumps its PerCpu doorbell counter, which we read back below.
    for idx in 0..num_cores {
        if idx != bsp_idx {
            crate::gic::trigger_sgi_core(idx as u32, DOORBELL_SGI);
        }
    }

    // Give the spinning secondaries time to drain + repay (non-blocking on our end).
    let until = crate::timer::uptime_us() + 300_000;
    while crate::timer::uptime_us() < until {
        core::hint::spin_loop();
    }

    // Drain repayments; feed each to the BSP's state machine. Its `Accept` command
    // is where the receiver-zeroing happens (faked here = logged).
    let mut repaid = 0;
    while let Some(m) = bsp_inbox.pop() {
        if m.kind != MSG_REPAID {
            continue;
        }
        let ev = Event::Repaid { from: m.from, range: Range { base: m.v0, len: m.v1 } };
        bsp_sm.step(ev, &mut |cmd| {
            if let Command::Accept { from, range } = cmd {
                repaid += 1;
                console::print("[SMP]   core ");
                console::print_dec(from as usize);
                console::print(" repaid ");
                console::print_dec(range.len as usize);
                console::print(" MB at 0x");
                console::print_hex(range.base);
                console::print(" (accepted + zeroed)\n");
            }
        });
    }
    console::print("[SMP] reclaimed ");
    console::print_dec(repaid);
    console::print(" repayment(s); BSP pool now ");
    console::print_dec(bsp_sm.free_pages() as usize);
    console::print(" (faked units)\n");

    // M2 — confirm each secondary actually took + serviced the doorbell SGI.
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let pp = cfg.cores[idx].percpu_phys as usize;
        if pp == 0 {
            continue;
        }
        // SAFETY: PerCpu PA is identity-mapped in the BSP boot tables.
        let count = unsafe { core::ptr::read_volatile((pp + PERCPU_DOORBELL_COUNT) as *const u64) };
        console::print("[SMP] core ");
        console::print_dec(idx);
        console::print(" doorbell SGIs serviced: ");
        console::print_dec(count as usize);
        console::print(if count > 0 { " (delivered ✓)\n" } else { " (NOT delivered ✗)\n" });
    }
}

/// Stage 3a — sample every secondary's heartbeat twice (~0.5 s apart) and report
/// whether it advanced. A non-advancing counter ⇒ that core is wedged/offline.
/// Read-only sampling of the SHARED heartbeat slots (no peer-private access). The
/// BSP then returns to normal boot while the secondaries keep beating.
fn monitor_liveness(cfg: &MachineConfig, num_cores: usize, bsp_idx: usize) {
    let mut first = [0u64; MAX_CORES];
    for (idx, h) in first.iter_mut().enumerate().take(num_cores) {
        *h = cfg.heartbeat[idx].load(Ordering::Relaxed);
    }
    let until = crate::timer::uptime_us() + 500_000;
    while crate::timer::uptime_us() < until {
        core::hint::spin_loop();
    }
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let now = cfg.heartbeat[idx].load(Ordering::Relaxed);
        let before = first.get(idx).copied().unwrap_or(0);
        console::print("[SMP] heartbeat core ");
        console::print_dec(idx);
        console::print(": ");
        console::print_dec(before as usize);
        console::print(" -> ");
        console::print_dec(now as usize);
        console::print(if now > before { " (alive)\n" } else { " (STALLED — offline?)\n" });
    }
}

/// Secondary Rust entry, on the shared BOOT tables + a shared boot stack. Reads
/// its [`CoreConfig`] and, if the BSP built an isolated table, hands off into it
/// via [`secondary_enter_isolated`] (switch TTBR0 + SP, never returns here). With
/// no isolated table (OOM), falls back to a parked spin on the shared tables.
///
/// Must NOT touch the console — the UART/console lock is BSP-owned (§8.2). Once
/// isolated, [`secondary_main`] cannot reach it anyway (it is unmapped).
#[unsafe(no_mangle)]
pub extern "C" fn secondary_rust_start(cfg_pa: usize, core_idx: usize) -> ! {
    // SAFETY: `cfg_pa` is the descriptor PA from PSCI context_id; identity-mapped.
    let cfg = unsafe { &*(cfg_pa as *const MachineConfig) };
    if cfg.magic == MAGIC && core_idx < MAX_CORES {
        let cc = &cfg.cores[core_idx];
        let ttbr0 = cc.ttbr0_phys;
        let sp = cc.entry_sp;
        if ttbr0 != 0 && sp != 0 {
            // SAFETY: BSP built this restricted table; switch into isolation and
            // jump to secondary_main on the private stack (no return path).
            unsafe { secondary_enter_isolated(ttbr0, sp, cfg_pa as u64, core_idx as u64) }
        }
        // Fallback: announce liveness on the shared tables and park.
        cc.state.store(STATE_ONLINE, Ordering::Release);
    }
    loop {
        wfe();
    }
}

/// Isolated secondary main, running on its RESTRICTED page table + private stack.
///
/// Hard constraint: it may touch ONLY memory its table maps — the shared kernel
/// code (it executes), the descriptor page (`cfg`), and its own PerCpu page. Any
/// other access (console, heap, PMM, peer state) is UNMAPPED and would fault. So
/// this deliberately avoids `console`/`alloc`/`pmm`; it talks to the BSP solely
/// through the descriptor's atomics and the shared message rings. It runs the
/// alloc-free `akuma_smp::CoreStateMachine` for the debt-reclaim protocol.
#[unsafe(no_mangle)]
extern "C" fn secondary_main(cfg_pa: usize, core_idx: usize) -> ! {
    // SAFETY: cfg page is mapped RW in this core's table.
    let cfg = unsafe { &*(cfg_pa as *const MachineConfig) };
    // `.get()` (not indexing) so an out-of-range idx can't hit the panic path,
    // whose console output is unmapped here and would fault instead of panic.
    let Some(cc) = cfg.cores.get(core_idx) else {
        loop {
            wfe();
        }
    };

    // Touch our PRIVATE PerCpu page (mapped only by us): write a liveness marker.
    // Proves the isolated mapping is usable for private writable state.
    let percpu = cc.percpu_phys as usize;
    if percpu != 0 {
        // SAFETY: percpu page is mapped RW in this core's table.
        unsafe { core::ptr::write_volatile(percpu as *mut u64, MAGIC ^ core_idx as u64); }
    }

    // R1 — per-core replication proof: mutate a shared-code `.data` static. With the
    // writable window replicated, this hits OUR private copy (which starts from the
    // pristine snapshot value REPL_TEST_INIT), so we read back INIT+1 while the BSP's
    // copy stays INIT. Record the read-back for the BSP to verify.
    let repl = SMP_REPLICATION_TEST.fetch_add(1, Ordering::SeqCst) + 1;
    if percpu != 0 {
        // SAFETY: percpu page is mapped RW in this core's table.
        unsafe { core::ptr::write_volatile((percpu + PERCPU_REPL_TEST) as *mut u64, repl) };
    }

    // Stage 2 — enforcement self-test: prove the MMU FAULTS a cross-core access.
    run_enforcement_test(cfg, cfg_pa, core_idx);

    // R2 — stand up this core's OWN pmm + heap over its partition and prove an
    // isolated alloc works (records the result to PerCpu for the BSP). Done BEFORE
    // announcing Online so the BSP can verify it synchronously once it sees Online.
    run_r2_test(cc.ram_base as usize, cc.ram_len as usize, cc.kernel_end as usize, percpu);

    // Announce Online from the isolated context (Release orders the enforcement
    // result + PerCpu marker before the BSP's Acquire load of `state`).
    cc.state.store(STATE_ONLINE, Ordering::Release);

    let Some(hb) = cfg.heartbeat.get(core_idx) else {
        loop {
            wfe();
        }
    };

    // Step 3 — run the host-tested debt-reclaim state machine in the isolated loop.
    // It is alloc-free, so it lives entirely on this private stack (the kernel heap
    // is unmapped here). Pre-load a FAKED debt to the BSP (core 0): pretend we
    // borrowed `borrowed_mb` from it. All units/values are faked (no per-core PMM
    // yet); the LOGIC is exactly what the host sim validates.
    const BSP: u32 = 0;
    let num_cores = cfg.num_cores as usize;
    let partition_mb = cc.ram_len / (1024 * 1024);
    let borrowed_mb = partition_mb / 10; // 10% of our partition, "borrowed from BSP"
    let mut sm = CoreStateMachine::new(core_idx as u32, num_cores, partition_mb, partition_mb / 4);
    sm.step(
        Event::Borrowed { creditor: BSP, range: Range { base: cc.ram_base, len: borrowed_mb } },
        &mut |_| {},
    );

    // M2 — enable the cross-core doorbell: bring up this PE's GIC receive path and
    // unmask IRQs so a peer's SGI is delivered to `smp_irq_handler` (which finds the
    // PerCpu doorbell counter via TPIDRRO_EL0). VBAR_EL1 was already pointed at
    // `smp_vectors` by the enforcement self-test.
    if percpu != 0 {
        // SAFETY: stash PerCpu PA in TPIDRRO_EL0 (free on secondaries) for the handler.
        unsafe {
            core::arch::asm!("msr tpidrro_el0, {0}", in(reg) percpu, options(nomem, nostack));
        }
    }
    secondary_gic_init(core_idx);
    // Arm this core's virtual timer for periodic heartbeat-tick wakeups (so the
    // loop can `wfe`-sleep yet keep liveness advancing). CNTV_CVAL = now + interval;
    // CNTV_CTL = 1 (enable, unmasked). The IRQ handler re-arms on each tick.
    // SAFETY: CNTV* are EL1-accessible system registers.
    unsafe {
        let now: u64;
        core::arch::asm!("mrs {0}, cntvct_el0", out(reg) now, options(nomem, nostack));
        core::arch::asm!("msr cntv_cval_el0, {0}", in(reg) now + TIMER_INTERVAL_TICKS, options(nomem, nostack));
        core::arch::asm!("msr cntv_ctl_el0, {0}", in(reg) 1u64, options(nomem, nostack));
    }
    // Unmask IRQs now that the vector + GIC + timer are ready. `crate::irq` is
    // pure asm (no statics), so it is safe to call in this isolated context.
    crate::irq::enable_irqs();

    loop {
        hb.fetch_add(1, Ordering::Relaxed);

        // Drain inbox → drive the state machine (non-blocking). Its `Repay` commands
        // are shed back to the creditor's ring; we only ever touch the SHARED rings,
        // never a peer's private state.
        if let Some(inbox) = cfg.inboxes.get(core_idx) {
            while let Some(m) = inbox.pop() {
                let ev = match m.kind {
                    MSG_PRESSURE => Event::Pressure { from: m.from },
                    MSG_REPAID => Event::Repaid { from: m.from, range: Range { base: m.v0, len: m.v1 } },
                    _ => continue,
                };
                sm.step(ev, &mut |cmd| {
                    if let Command::Repay { creditor, range } = cmd
                        && let Some(to) = cfg.inboxes.get(creditor as usize)
                    {
                        to.push(MSG_REPAID, core_idx as u32, range.base, range.len);
                    }
                    // Command::Accept (we'd be a creditor) would zero+reclaim here;
                    // in this demo secondaries are debtors only.
                });
            }
        }

        // Race-free sleep until the next INTERRUPT — a timer tick (heartbeat) or a
        // doorbell SGI (a peer rang us). WFI, not WFE: WFE waits for an *event* and
        // returns spuriously (leaving the core busy-spinning); WFI halts until an
        // interrupt is pending.
        //
        // To avoid a lost wakeup if a message lands after the drain above, MASK IRQs
        // first (`crate::irq` — pure asm, safe here), then re-check the inbox. WFI
        // still wakes on a pending interrupt even while masked, and any producer that
        // pushes also rings a doorbell SGI — so a message racing in after the re-check
        // leaves that SGI pending and WFI returns immediately. Unmasking last takes
        // the interrupt that woke us.
        crate::irq::disable_irqs();
        if cfg.inboxes.get(core_idx).is_some_and(|r| !r.is_empty()) {
            crate::irq::enable_irqs(); // work arrived — don't sleep, go process it
            continue;
        }
        // SAFETY: WFI wakes on a pending IRQ despite the mask; then unmask to take it.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        crate::irq::enable_irqs();
    }
}

/// Deliberately read the first BSP-private page (`_data_start`, just above the
/// shared code window and therefore UNMAPPED in this core's restricted table).
/// If isolation holds, the load takes a translation fault → `smp_sync_handler`
/// records `ENF_FAULTED` and skips the load. If the read instead *succeeds*, the
/// table leaked peer memory and we record `ENF_LEAKED`. Result lands in the shared
/// `enforcement_results[idx]` for the BSP to report.
#[inline(never)]
fn run_enforcement_test(cfg: &MachineConfig, cfg_pa: usize, core_idx: usize) {
    let Some(slot) = cfg.enforcement_results.get(core_idx) else {
        return;
    };
    slot.store(ENF_TESTING, Ordering::Release);

    // SAFETY: install our descriptor base in TPIDR_EL1 (free on secondaries — the
    // akuma scheduler that normally owns it does not run here) so the fault
    // handler can locate `enforcement_results`; point VBAR_EL1 at our vectors.
    unsafe {
        let vbar = &raw const smp_vectors as usize;
        core::arch::asm!(
            "msr tpidr_el1, {cfg}",
            "msr vbar_el1, {vbar}",
            "isb",
            cfg = in(reg) cfg_pa,
            vbar = in(reg) vbar,
            options(nostack, preserves_flags),
        );
    }

    // The deliberate cross-core probe: an address well above the kernel image and
    // outside every region the restricted table maps (code/descriptor/own chunk),
    // so it is BSP-private and must be unmapped here.
    let probe = 0x4080_0000 as *const u8;
    // SAFETY: intentionally faulting read; the handler skips it and we don't use
    // the (garbage) value beyond the optimization barrier.
    let v = unsafe { core::ptr::read_volatile(probe) };
    core::hint::black_box(v);

    // If the handler never fired, the read crossed cores undetected = leak.
    if slot.load(Ordering::Acquire) != ENF_FAULTED {
        slot.store(ENF_LEAKED, Ordering::Release);
    }
}

/// R2 — bring up this secondary's OWN per-core PMM + heap over its partition, then
/// prove an isolated `alloc` (docs/MULTIKERNEL.md §15). Because R1 already gives the
/// core a PRIVATE `.data`/`.bss`, the kernel's `static TALC`/`static PMM`/`PMM_READY`
/// all resolve to THIS core's replicated copies — so we can drive the *unchanged*
/// `allocator::init` / `pmm::init` and they touch nothing the BSP owns:
///
///   1. seed a small private heap, carved by the BSP from this partition just above
///      `kernel_end` (the consumed bringup prefix);
///   2. init the private PMM over `[pbase, pbase+len_2mb)`, marking the heap +
///      kernel prefix used and managing the rest as free pages;
///   3. allocate from both pools and record the result to PerCpu.
///
/// All values land in PerCpu atomics (the secondary still has no console/UART map);
/// the BSP reads them once the core reports Online and confirms the allocations came
/// from this partition with the BSP pool untouched. No `console::print` here.
fn run_r2_test(pbase: usize, plen: usize, kernel_end: usize, percpu: usize) {
    if percpu == 0 {
        return;
    }
    let len_2mb = plen & !(TWO_MB - 1);
    // Heap slab: just above the BSP-carved kernel image, page-aligned, inside the
    // 2 MiB-block-mapped region. Bail if the partition is too small to hold it.
    let heap_base = kernel_end.next_multiple_of(PAGE);
    let heap_len = SECONDARY_HEAP_BYTES;
    let pmm_kernel_end = heap_base + heap_len;
    if heap_base < pbase || pmm_kernel_end + PAGE > pbase + len_2mb {
        return;
    }

    // 1. Seed this core's private `talc` heap.
    if crate::allocator::init(heap_base, heap_len).is_err() {
        return;
    }
    // 2. Init this core's private PMM over its partition; `[pbase, pmm_kernel_end)`
    // (kernel image + heap) is marked used, the rest becomes its free pool.
    crate::pmm::init(pbase, len_2mb, pmm_kernel_end);
    crate::allocator::mark_pmm_ready();

    // 3a. Private-heap proof: allocate, fill, read back (exercises `talc` + the
    // mapped heap). A `Vec` round-trip is enough; it frees on drop.
    let heap_ok = {
        let v = alloc::vec![0xA5u8; PAGE];
        v.iter().all(|&b| b == 0xA5)
    };

    // 3b. Private-PMM proof: hand out several zeroed pages from this partition.
    let mut pages = 0u64;
    let mut first_pa = 0u64;
    for _ in 0..16 {
        match crate::pmm::alloc_page_zeroed() {
            Some(f) => {
                if first_pa == 0 {
                    first_pa = f.addr as u64;
                }
                pages += 1;
            }
            None => break,
        }
    }

    // Record for the BSP (Release so the values are visible with the Online store).
    // SAFETY: PerCpu page is mapped RW in this core's restricted table.
    unsafe {
        core::ptr::write_volatile((percpu + PERCPU_R2_PAGES) as *mut u64, pages);
        core::ptr::write_volatile((percpu + PERCPU_R2_FIRST_PA) as *mut u64, first_pa);
        core::ptr::write_volatile((percpu + PERCPU_R2_HEAP_OK) as *mut u64, u64::from(heap_ok));
        core::ptr::write_volatile(
            (percpu + PERCPU_R2_FREE) as *mut u64,
            crate::pmm::free_count() as u64,
        );
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

// secondary_enter_isolated(x0=ttbr0_phys, x1=sp, x2=cfg_pa, x3=core_idx) -> !
// Switch onto the restricted table + private stack, then tail-call secondary_main.
// CRITICAL: no memory access (no stack use) between the TTBR0 switch and the SP
// switch — the old shared boot stack becomes unmapped the instant TTBR0 changes.
// All-register ops only. The executing code (.text) is mapped in BOTH tables at
// the same VA, so instruction fetch continues across the switch.
.global secondary_enter_isolated
secondary_enter_isolated:
    msr     ttbr0_el1, x0
    isb                             // make the TTBR0 switch take effect first
    dsb     ish
    tlbi    vmalle1                 // drop the GLOBAL boot-table 1GB block + all else
    dsb     ish
    isb
    mov     sp, x1                  // switch to the private stack (now mapped)
    mov     x0, x2                  // cfg_pa
    mov     x1, x3                  // core_idx
    b       secondary_main          // never returns

// ---------------------------------------------------------------------------
// Per-core minimal exception vectors (Stage 2 enforcement self-test). 16 slots,
// 0x80 bytes apart, table 2 KiB-aligned. Only "Current EL SPx, Synchronous"
// (offset 0x200) is meaningful here — the deliberate cross-core load faults to
// it; everything else parks (IRQs are masked and no timer runs on the secondary).
// ---------------------------------------------------------------------------
.balign 0x800
.global smp_vectors
smp_vectors:
.balign 0x80
    b       smp_park_vec            // 0x000 Cur EL SP0 Sync
.balign 0x80
    b       smp_park_vec            // 0x080 Cur EL SP0 IRQ
.balign 0x80
    b       smp_park_vec            // 0x100 Cur EL SP0 FIQ
.balign 0x80
    b       smp_park_vec            // 0x180 Cur EL SP0 SError
.balign 0x80
    b       smp_sync_handler        // 0x200 Cur EL SPx Sync  <-- probe faults here
.balign 0x80
    b       smp_irq_handler         // 0x280 Cur EL SPx IRQ   <-- doorbell SGI lands here
.balign 0x80
    b       smp_park_vec            // 0x300 Cur EL SPx FIQ
.balign 0x80
    b       smp_park_vec            // 0x380 Cur EL SPx SError
.balign 0x80
    b       smp_park_vec            // 0x400 Lower EL a64 Sync
.balign 0x80
    b       smp_park_vec            // 0x480 Lower EL a64 IRQ
.balign 0x80
    b       smp_park_vec            // 0x500 Lower EL a64 FIQ
.balign 0x80
    b       smp_park_vec            // 0x580 Lower EL a64 SError
.balign 0x80
    b       smp_park_vec            // 0x600 Lower EL a32 Sync
.balign 0x80
    b       smp_park_vec            // 0x680 Lower EL a32 IRQ
.balign 0x80
    b       smp_park_vec            // 0x700 Lower EL a32 FIQ
.balign 0x80
    b       smp_park_vec            // 0x780 Lower EL a32 SError

// Records the cross-core fault and resumes past the offending load. TPIDR_EL1
// holds the descriptor base (enforcement_results at offset 0); idx = MPIDR aff0.
smp_sync_handler:
    sub     sp, sp, #32
    stp     x0, x1, [sp]
    str     x2, [sp, #16]
    mrs     x0, tpidr_el1           // descriptor base
    mrs     x1, mpidr_el1
    and     x1, x1, #0xff           // core idx
    mov     w2, #{enf_faulted}
    str     w2, [x0, x1, lsl #2]    // enforcement_results[idx] = ENF_FAULTED
    mrs     x0, elr_el1
    add     x0, x0, #4              // skip the faulting load instruction
    msr     elr_el1, x0
    ldr     x2, [sp, #16]
    ldp     x0, x1, [sp]
    add     sp, sp, #32
    eret

// Secondary IRQ handler. Two sources: the virtual-timer PPI (INTID 27, the
// heartbeat tick — re-arm CVAL so the level IRQ deasserts) and the doorbell SGI
// (bump the PerCpu counter). Either way, taking the IRQ is the point: it pops the
// core out of `wfe`, after which the main loop drains the ring. TPIDRRO_EL0 holds
// the PerCpu PA (set by secondary_main; free on secondaries — no threading here).
smp_irq_handler:
    sub     sp, sp, #32
    stp     x0, x1, [sp]
    str     x2, [sp, #16]
    mrs     x0, S3_0_C12_C12_0          // ICC_IAR1_EL1: acknowledge → INTID in x0
    cmp     x0, #{timer_ppi}            // virtual-timer PPI?
    b.ne    .Lsmp_doorbell
    // Timer: re-arm CVAL = CNTVCT + interval (deasserts the level IRQ).
    mrs     x1, cntvct_el0
    movz    x2, #0x10, lsl #16          // TIMER_INTERVAL_TICKS = 0x10_0000
    add     x1, x1, x2
    msr     cntv_cval_el0, x1
    b       .Lsmp_eoi
.Lsmp_doorbell:
    mrs     x1, tpidrro_el0             // PerCpu PA
    ldr     x2, [x1, #{db_off}]
    add     x2, x2, #1
    str     x2, [x1, #{db_off}]         // PerCpu.doorbell_count += 1
.Lsmp_eoi:
    msr     S3_0_C12_C12_1, x0          // ICC_EOIR1_EL1: end of interrupt
    ldr     x2, [sp, #16]
    ldp     x0, x1, [sp]
    add     sp, sp, #32
    eret

smp_park_vec:
    wfe
    b       smp_park_vec

.section .bss.smp
.balign 16
secondary_boot_stacks:
    .space  {stacks_bytes}
"#,
    max_cores = const MAX_CORES,
    stack_shift = const SECONDARY_STACK_SHIFT,
    stacks_bytes = const (MAX_CORES << SECONDARY_STACK_SHIFT),
    enf_faulted = const ENF_FAULTED,
    db_off = const PERCPU_DOORBELL_COUNT,
    timer_ppi = const TIMER_PPI,
);
