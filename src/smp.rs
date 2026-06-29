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
    partition, Command, ConsoleRing, CoreConfig, CoreStateMachine, Event, MachineConfig, Range,
    ENF_FAULTED, ENF_LEAKED, ENF_TESTING, MAGIC, MAX_CORES, MSG_FWD_ECHO_REPLY, MSG_FWD_ECHO_REQ,
    MSG_PRESSURE, MSG_REPAID, STATE_BOOTING, STATE_OFFLINE, STATE_ONLINE,
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

// ============================================================================
// Per-core console output (docs/MULTIKERNEL.md §8.2)
//
// A secondary's restricted table does NOT map the UART (it is the BSP-owned
// console device), so the secondary can't `console::print` directly. Instead each
// secondary routes its console output to its per-core `ConsoleRing` in the shared
// descriptor; a BSP drainer thread reads those rings and writes the UART. This is
// the async, fire-and-forget counterpart to the synchronous control inbox — and
// the natural seam to later move the console to a userspace server.
// ============================================================================

/// PA of THIS core's console output ring (a `ConsoleRing` in the shared descriptor),
/// or 0 to write the UART directly. A `.bss` static → replicated per core by R1, so
/// each secondary sets only ITS OWN copy at bringup; the BSP's stays 0 (UART owner).
static CONSOLE_RING_PA: AtomicUsize = AtomicUsize::new(0);

/// PA of THIS core's PerCpu page, for IRQ handlers that run on the real
/// `exception_vector_table` in steady state (R4b.1). The minimal `smp_vectors` path
/// stashes the PerCpu PA in TPIDRRO_EL0, but the per-core scheduler claims that
/// register for the current-thread id — so a steady-state handler finds PerCpu here
/// instead. A `.bss` static → replicated per core; each secondary sets only its own.
static SECONDARY_PERCPU: AtomicUsize = AtomicUsize::new(0);

/// Route this core's `console::print` output to its per-core console ring. Called
/// once by a secondary during bringup, after its restricted table maps the shared
/// descriptor and before it prints anything.
fn set_console_ring(pa: usize) {
    CONSOLE_RING_PA.store(pa, Ordering::Release);
}

/// Console output hook, called from `console::emit`. If this core has a console ring
/// (a secondary), append `bytes` to it and return `true`; otherwise return `false`
/// so the caller writes the UART directly (the BSP / pre-bringup path). Never blocks:
/// a full ring drops the overflow.
pub fn console_emit(bytes: &[u8]) -> bool {
    let pa = CONSOLE_RING_PA.load(Ordering::Acquire);
    if pa == 0 {
        return false;
    }
    // SAFETY: `pa` is a `ConsoleRing` inside the shared descriptor, mapped RW in this
    // core's restricted table. IRQs off so this core is the sole producer (SPSC).
    let ring = unsafe { &*(pa as *const ConsoleRing) };
    crate::irq::with_irqs_disabled(|| {
        ring.write(bytes);
    });
    true
}

/// Drain every secondary core's console ring to the UART (console-owner = BSP).
/// Reads up to a page at a time and writes the raw bytes (the producer frames its
/// own lines), looping per core until its ring is empty.
fn drain_console_rings() {
    // SAFETY: descriptor is initialized and identity-mapped on the BSP.
    let cfg = unsafe { &*MACHINE_CONFIG.0.get() };
    let num_cores = cfg.num_cores as usize;
    let bsp_idx = (read_mpidr() & 0xff) as usize;
    let mut buf = [0u8; 4096];
    for idx in 0..num_cores.min(MAX_CORES) {
        if idx == bsp_idx {
            continue;
        }
        loop {
            let n = cfg.console_rings[idx].read(&mut buf);
            if n == 0 {
                break;
            }
            console::print_bytes(&buf[..n]);
        }
    }
}

/// Spawn the BSP console drainer (no-op single-core). It forwards secondaries' console
/// output to the UART: drains each ring to empty, then yields — so output appears
/// within a scheduler quantum and the 4 KiB rings never overflow under normal logging.
/// (Throttling to a coarser cadence + a coalesced doorbell wake is a future
/// optimization, §8.2.)
///
/// A SYSTEM thread (kernel stack, like the SSH server), spawned from `run_async_main`
/// AFTER preemption is live — spawning a kernel loop into a *user* slot before the
/// scheduler runs dispatches it with a poison PC. The producer side is each
/// secondary's `console::print`, routed to its ring by `console_emit`.
pub fn start_console_drainer() {
    if !PROBED.load(Ordering::Acquire) || NUM_CORES.load(Ordering::Relaxed) <= 1 {
        return;
    }
    let spawned = akuma_exec::threading::spawn_system_thread_fn(|| loop {
        drain_console_rings();
        akuma_exec::threading::yield_now();
    });
    match spawned {
        Ok(tid) => {
            crate::safe_print!(64, "[SMP] console drainer thread spawned (tid {})\n", tid);
        }
        Err(e) => {
            crate::safe_print!(96, "[SMP] console drainer spawn FAILED: {}\n", e);
        }
    }
}

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
/// PerCpu byte offsets where a secondary records its R3a (per-core cooperative
/// scheduler) self-test result for the BSP to verify.
const PERCPU_R3_YIELDS: usize = 72; // total yields the two coop threads performed
const PERCPU_R3_DONE: usize = 80; // # coop threads that ran to completion (== spawned)
/// Progress marker bumped through `run_r3a_coop_test` (1=entry … 7=recorded) and
/// `run_r3b_preempt_test` (10…16). The BSP reads it EVEN on timeout, so a hang inside
/// R3a/R3b is pinpointed (the secondary's own console ring isn't reliably drained while
/// it is still mid-bringup).
const PERCPU_R3_STAGE: usize = 88;
/// R3b (per-core PREEMPTIVE scheduler) self-test result offsets.
const PERCPU_R3B_COUNT: usize = 96; // spinner iterations observed (>0 ⇒ timer preempted)
const PERCPU_R3B_DONE: usize = 104; // 1 if the spinner ran to completion (R3_SKIPPED if skipped)
/// R4a (cross-core syscall-forwarding transport) self-test result: 1 = round-trip
/// verified (BSP transformed our bounce payload + echoed our nonce), 0 = failed/no reply.
const PERCPU_R4A_OK: usize = 112;

/// Secondary user-table kernel-window check (R4b.3a): 1 = a freshly-built user address
/// space resolves a kernel `.data` static to THIS core's PRIVATE page (matching its
/// restricted table), not the BSP's identity copy. 0 = failed.
const PERCPU_USERTAB_PRIV_OK: usize = 120;
/// Diagnostics: the PA the user table / restricted table resolve the probe VA to.
const PERCPU_USERTAB_USER_PA: usize = 128;
const PERCPU_USERTAB_REST_PA: usize = 136;

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
        crate::safe_print!(32, "[SMP] probe: no DTB\n");
        return;
    }
    // SAFETY: `actual_dtb` carries a verified FDT magic.
    let Ok(fdt) = (unsafe { fdt::Fdt::from_ptr(actual_dtb as *const u8) }) else {
        crate::safe_print!(32, "[SMP] probe: invalid DTB\n");
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
    crate::safe_print!(
        64,
        "[SMP] probe: {} core(s), conduit={}\n",
        num_cores,
        if use_hvc { "hvc" } else { "smc" }
    );
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
    crate::safe_print!(64, "[SMP] reserved {} MB of secondary partitions from the BSP PMM\n", reserved_mb);
}

/// BSP entry point: wake every secondary PE and wait for it to report `Online`.
/// No-op (single-core) when the DTB enumerated only one CPU. Uses the stash from
/// [`probe_dtb`] — the DTB itself may already be heap-clobbered by now.
pub fn bringup_secondaries(ram_base: usize, ram_size: usize) {
    if !PROBED.load(Ordering::Acquire) {
        crate::safe_print!(48, "[SMP] not probed; staying single-core\n");
        return;
    }
    let num_cores = NUM_CORES.load(Ordering::Relaxed);
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    let mut mpidrs = [0u64; MAX_CORES];
    for (i, m) in PROBED_MPIDRS.iter().enumerate() {
        mpidrs[i] = m.load(Ordering::Relaxed);
    }
    let bsp_idx = (read_mpidr() & 0xff) as usize;

    crate::safe_print!(64, "[SMP] {} core(s); BSP is core {}\n", num_cores, bsp_idx);

    if num_cores <= 1 {
        crate::safe_print!(64, "[SMP] single core; no secondaries to bring up\n");
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
        crate::safe_print!(
            96,
            "[SMP] core {} partition: base=0x{:x} len={} MB\n",
            idx,
            pbase,
            (plen / (1024 * 1024)) as usize
        );
    }

    // Build each secondary's RESTRICTED, isolated page table (shared code RO +
    // descriptor RW + own stack/PerCpu RW; peers unmapped). On OOM the core's
    // ttbr0_phys stays 0 and it falls back to a parked spin on the boot tables.
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let ok = build_isolated_table(cfg, idx);
        if ok {
            crate::safe_print!(
                112,
                "[SMP] core {} isolated table: ttbr0=0x{:x} sp=0x{:x}\n",
                idx,
                cfg.cores[idx].ttbr0_phys,
                cfg.cores[idx].entry_sp
            );
        } else {
            crate::safe_print!(80, "[SMP] core {} isolated table: OOM (falling back to shared park)\n", idx);
        }
    }

    let cfg_pa = core::ptr::from_ref::<MachineConfig>(cfg) as u64;
    let entry_pa = secondary_entry as *const () as usize as u64;

    // Publish the descriptor + freshly-built page tables to RAM before any
    // secondary's MMU-on read / table walk.
    dsb_sy();

    crate::safe_print!(
        96,
        "[SMP] conduit={}, entry=0x{:x}, descriptor=0x{:x}\n",
        if use_hvc { "hvc" } else { "smc" },
        entry_pa,
        cfg_pa
    );

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
        if r == 0 {
            crate::safe_print!(80, "[SMP] CPU_ON core {} (mpidr=0x{:x}) -> PSCI_SUCCESS\n", idx, target);
        } else {
            crate::safe_print!(80, "[SMP] CPU_ON core {} (mpidr=0x{:x}) -> ERROR {}\n", idx, target, (-r) as usize);
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
            // Service any secondary's forward request (R4a) while we wait — a secondary
            // blocks on its reply before announcing Online, so without this the BSP
            // (spinning on Online) and the secondary (spinning on the reply) deadlock.
            service_fwd_requests(cfg, bsp_idx);
            core::hint::spin_loop();
        }
        crate::safe_print!(
            80,
            "[SMP] core {}{}",
            idx,
            if online { " ONLINE" } else { " TIMEOUT (never reported online)" }
        );
        if !online {
            // Pinpoint a hang: report the last R3a stage the secondary reached.
            let percpu = cfg.cores[idx].percpu_phys as usize;
            if percpu != 0 {
                let st =
                    unsafe { core::ptr::read_volatile((percpu + PERCPU_R3_STAGE) as *const u64) };
                let yl =
                    unsafe { core::ptr::read_volatile((percpu + PERCPU_R3_YIELDS) as *const u64) };
                crate::safe_print!(64, " [last R3a stage={} yields={}]", st as usize, yl as usize);
            }
            crate::safe_print!(8, "\n");
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
                crate::safe_print!(48, " (isolated-run confirmed)");
            } else {
                crate::safe_print!(80, " (WARNING: PerCpu marker missing - ran on shared tables?)");
            }
            // R1: per-core .data/.bss replication. The secondary mutated the shared
            // static into its OWN copy (→ INIT+1); the BSP's copy must be untouched.
            let repl = unsafe { core::ptr::read_volatile((percpu + PERCPU_REPL_TEST) as *const u64) };
            let bsp_copy = SMP_REPLICATION_TEST.load(Ordering::SeqCst);
            if repl == REPL_TEST_INIT + 1 && bsp_copy == REPL_TEST_INIT {
                crate::safe_print!(64, " [replication: private .data/.bss PASS]");
            } else {
                crate::safe_print!(96, " [replication: FAILED secondary=0x{:x} bsp=0x{:x} FAIL]", repl, bsp_copy);
            }
        }
        // Stage 2: report the cross-core enforcement self-test outcome.
        match cfg.enforcement_results[idx].load(Ordering::Acquire) {
            ENF_FAULTED => crate::safe_print!(64, " [enforcement: cross-core access FAULTED PASS]\n"),
            ENF_LEAKED => crate::safe_print!(64, " [enforcement: LEAKED - isolation breach! FAIL]\n"),
            _ => crate::safe_print!(48, " [enforcement: inconclusive]\n"),
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
            if pages > 0 && in_partition && heap_ok == 1 && bsp_untouched {
                crate::safe_print!(
                    176,
                    "[SMP] core {} R2: per-core pmm+heap PASS (alloc'd {} pages from 0x{:x}, heap ok, free={} pages; BSP pool untouched PASS)\n",
                    idx,
                    pages as usize,
                    first_pa,
                    sec_free as usize
                );
            } else {
                crate::safe_print!(
                    176,
                    "[SMP] core {} R2: FAILED (pages={} first=0x{:x} in_part={} heap={} bsp_untouched={} FAIL)\n",
                    idx,
                    pages as usize,
                    first_pa,
                    if in_partition { "1" } else { "0" },
                    heap_ok as usize,
                    if bsp_untouched { "1" } else { "0" }
                );
            }
        }
        // R3a: per-core cooperative scheduler. The secondary registered akuma-exec's
        // runtime locally, stood up its scheduler, and ran two cooperative kernel
        // threads that yielded to each other. Verify both ran to completion and the
        // total yield count is exactly NUM_THREADS * YIELDS_PER_THREAD. `done==0 &&
        // yields==0` means it was skipped (partition too small) — reported distinctly.
        if percpu != 0 {
            let r3_yields =
                unsafe { core::ptr::read_volatile((percpu + PERCPU_R3_YIELDS) as *const u64) };
            let r3_done =
                unsafe { core::ptr::read_volatile((percpu + PERCPU_R3_DONE) as *const u64) };
            let expect_yields = R3_NUM_THREADS * R3_YIELDS_PER_THREAD;
            if r3_done == R3_SKIPPED {
                crate::safe_print!(96, "[SMP] core {} R3a: skipped (partition too small for thread pool)\n", idx);
            } else if r3_done == R3_NUM_THREADS && r3_yields == expect_yields {
                crate::safe_print!(
                    112,
                    "[SMP] core {} R3a: per-core cooperative scheduler PASS ({} threads, {} yields)\n",
                    idx,
                    r3_done as usize,
                    r3_yields as usize
                );
            } else {
                crate::safe_print!(
                    128,
                    "[SMP] core {} R3a: FAILED (done={}/{} yields={}/{} FAIL)\n",
                    idx,
                    r3_done as usize,
                    R3_NUM_THREADS as usize,
                    r3_yields as usize,
                    expect_yields as usize
                );
            }
        }
        // R3b: per-core PREEMPTIVE scheduler. A spinner thread that never yields ran only
        // because the per-core timer preempted the (also non-yielding) boot thread — so a
        // nonzero spin count + the spinner completing proves timer-driven preemption.
        if percpu != 0 {
            let r3b_count =
                unsafe { core::ptr::read_volatile((percpu + PERCPU_R3B_COUNT) as *const u64) };
            let r3b_done =
                unsafe { core::ptr::read_volatile((percpu + PERCPU_R3B_DONE) as *const u64) };
            if r3b_done == R3_SKIPPED {
                crate::safe_print!(64, "[SMP] core {} R3b: skipped\n", idx);
            } else if r3b_done == 1 && r3b_count > 0 {
                crate::safe_print!(
                    144,
                    "[SMP] core {} R3b: per-core preemptive scheduler PASS (timer preempted; spinner ran {} iters)\n",
                    idx,
                    r3b_count as usize
                );
            } else {
                crate::safe_print!(
                    144,
                    "[SMP] core {} R3b: FAILED (done={} spin_count={} - timer did not preempt? FAIL)\n",
                    idx,
                    r3b_done as usize,
                    r3b_count as usize
                );
            }
        }
        // R4a: cross-core syscall-forwarding transport. The secondary shipped a payload
        // through its bounce region to the BSP, which transformed it + replied; a
        // verified round-trip proves the §8.1 data path (the keystone for R4 exec).
        if percpu != 0 {
            let r4a_ok =
                unsafe { core::ptr::read_volatile((percpu + PERCPU_R4A_OK) as *const u64) };
            if r4a_ok == 1 {
                crate::safe_print!(112, "[SMP] core {} R4a: cross-core forward round-trip PASS (bounce + reply verified)\n", idx);
            } else {
                crate::safe_print!(96, "[SMP] core {} R4a: FAILED (no/garbled reply - ok={} FAIL)\n", idx, r4a_ok as usize);
            }
        }
        // R4b.3a: a user (EL0) address space built on this core has the full secondary
        // kernel view — code identity (handler runs), `.data`/`.bss` window to its OWN
        // private pages (not the BSP's), partition identity (phys_to_virt at EL1) — the
        // prerequisite for running an EL0 process here.
        if percpu != 0 {
            let ok = unsafe { core::ptr::read_volatile((percpu + PERCPU_USERTAB_PRIV_OK) as *const u64) };
            let upa = unsafe { core::ptr::read_volatile((percpu + PERCPU_USERTAB_USER_PA) as *const u64) };
            let rpa = unsafe { core::ptr::read_volatile((percpu + PERCPU_USERTAB_REST_PA) as *const u64) };
            if ok == 1 {
                crate::safe_print!(128, "[SMP] core {} user-table: secondary kernel view OK (window -> private 0x{:x}, code+partition identity) PASS\n", idx, upa);
            } else {
                crate::safe_print!(128, "[SMP] core {} user-table: FAILED (win_user=0x{:x} win_rest=0x{:x} FAIL)\n", idx, upa, rpa);
            }
        }
    }

    crate::safe_print!(48, "[SMP] bringup complete\n");

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
    crate::safe_print!(
        96,
        "[SMP] core {} broadcast pressure signal ({}% used) to {} peer(s)\n",
        bsp_idx,
        FAKED_PRESSURE_PCT as usize,
        sent
    );

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
                crate::safe_print!(
                    112,
                    "[SMP]   core {} repaid {} MB at 0x{:x} (accepted + zeroed)\n",
                    from as usize,
                    range.len as usize,
                    range.base
                );
            }
        });
    }
    crate::safe_print!(
        96,
        "[SMP] reclaimed {} repayment(s); BSP pool now {} (faked units)\n",
        repaid,
        bsp_sm.free_pages() as usize
    );

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
        crate::safe_print!(
            96,
            "[SMP] core {} doorbell SGIs serviced: {}{}",
            idx,
            count as usize,
            if count > 0 { " (delivered PASS)\n" } else { " (NOT delivered FAIL)\n" }
        );
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
        crate::safe_print!(
            96,
            "[SMP] heartbeat core {}: {} -> {}{}",
            idx,
            before as usize,
            now as usize,
            if now > before { " (alive)\n" } else { " (STALLED - offline?)\n" }
        );
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

    // §8.2 — route our console output to our per-core ring in the shared descriptor
    // (the UART is unmapped here; the BSP drains the ring to it). Set BEFORE any
    // `console::print` below, so a secondary can finally report for itself.
    set_console_ring(core::ptr::from_ref(&cfg.console_rings[core_idx]) as usize);

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

    // R3a/R3b — per-core scheduler self-tests (record to PerCpu for the BSP). Run after
    // R2 (need the per-core heap+PMM) and BEFORE we re-stamp TPIDRRO_EL0 with the PerCpu
    // PA for the heartbeat loop (the scheduler uses TPIDRRO_EL0 as the current-thread id).
    // Each returns to this isolated context with IRQs masked + `smp_vectors` restored, so
    // the M2 doorbell/heartbeat path below is unaffected. R3b reuses the scheduler R3a
    // stood up, so it only runs if R3a did.
    let sched_up = run_r3a_coop_test(cc, core_idx, percpu);
    if sched_up {
        run_r3b_preempt_test(core_idx, percpu);
    } else if percpu != 0 {
        // R3a skipped (e.g. partition too small) → R3b can't run either.
        // SAFETY: PerCpu page is mapped RW in this core's restricted table.
        unsafe { core::ptr::write_volatile((percpu + PERCPU_R3B_DONE) as *mut u64, R3_SKIPPED) };
    }

    // R4a — cross-core syscall-forwarding transport round-trip. Independent of the
    // scheduler (ring + bounce + spin), so it runs regardless of R3a. The BSP services
    // the request from its bringup wait loop; we spin on the reply, then announce Online.
    run_r4a_fwd_test(cfg, core_idx, percpu);

    // R4b.3a — build a user address space on this core, overlay the replicated kernel
    // window, and verify its kernel statics resolve to OUR private pages (the mapping
    // fix that lets an EL0 process run here without leaking into the BSP's `.data`/`.bss`).
    // Needs the akuma-exec runtime R3a stood up; no-op (records 0) if R3a was skipped.
    if sched_up {
        verify_user_table_kernel_window(cc.ram_base as usize, cc.ram_len as usize, percpu);
    }

    // First words a secondary ever speaks for itself — proving the §8.2 console path
    // end to end: this `print` lands in our console ring; the BSP drainer thread (not
    // yet spawned — these bytes wait in the ring) later forwards them to the UART.
    crate::safe_print!(80, "[core {}] per-core runtime online: pmm + heap + console (via ring)\n", core_idx);

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

    // R4b.1 — if the per-core scheduler is up (R3a ran), this core's STEADY STATE is
    // the real scheduler with timer preemption + doorbell IRQ running PERMANENTLY (no
    // teardown), with the heartbeat/debt work as the boot thread's idle body. Never
    // returns. The M2 `WFI` loop below is the fallback only when R3a was skipped (a
    // partition too small to stand a scheduler up).
    if sched_up {
        secondary_steady_state(cfg, core_idx, percpu, sm);
    }

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
// R3a — per-core COOPERATIVE scheduler (docs/MULTIKERNEL.md §15). Proves a secondary
// can run akuma_exec's process table + scheduler over its OWN replicated statics +
// partition: it registers the runtime locally, stands up the scheduler, installs the
// real exception vectors, and runs two cooperative kernel threads that yield to each
// other. No preemption/timer/syscalls yet (those are R3b/R4).
// ============================================================================

/// Scheduler SGI INTID. Must match `gic::SGI_SCHEDULER` and `yield_now`'s
/// `trigger_sgi(0)`. Distinct from the M2 `DOORBELL_SGI` (= 1).
const SCHED_SGI: u32 = 0;
/// Yields each cooperative worker performs before terminating.
const R3_YIELDS_PER_THREAD: u64 = 8;
/// Number of cooperative workers the boot thread spawns.
const R3_NUM_THREADS: u64 = 2;
/// Upper bound on boot-thread driver iterations (defensive; the test finishes in
/// `R3_NUM_THREADS * R3_YIELDS_PER_THREAD` round-robin hops in practice).
const R3_MAX_SPINS: u64 = 200_000;
/// Skip R3a unless free PMM comfortably covers the release-profile thread-stack pool
/// (~15 MB) — keeps `threading::init` from panicking on a tiny partition (a panic on a
/// secondary would fault, since its console is a ring, not the UART).
const R3_MIN_FREE_PAGES: usize = 8192; // 32 MiB

/// Shared counters for the R3a workers. Replicated `.bss` statics → each core mutates
/// its OWN copy (no cross-core contention); the boot thread re-zeroes them per run.
static R3_YIELD_COUNTER: AtomicU64 = AtomicU64::new(0);
static R3_THREADS_DONE: AtomicU64 = AtomicU64::new(0);

/// Worker body for the R3a test: bump the shared yield counter and `yield_now`,
/// `R3_YIELDS_PER_THREAD` times, then mark done and terminate. Spawned as a CLOSURE via
/// `spawn_system_thread_fn` — that path builds a real `setup_fake_irq_frame`, which the
/// live stack-based scheduler restores. (The bare `spawn(extern fn)` path is
/// register-based — `ctx.sp` = empty stack top — and incompatible with this scheduler:
/// the switch would pop a garbage IRQ frame.) Preemptive, but with no timer armed
/// during the test it only switches on the voluntary yield SGI — i.e. cooperatively.
fn r3_worker_body() -> ! {
    for _ in 0..R3_YIELDS_PER_THREAD {
        R3_YIELD_COUNTER.fetch_add(1, Ordering::SeqCst);
        akuma_exec::threading::yield_now();
    }
    R3_THREADS_DONE.fetch_add(1, Ordering::SeqCst);
    akuma_exec::threading::mark_current_terminated();
    // Terminated: yield so the scheduler reschedules someone else and never us.
    loop {
        akuma_exec::threading::yield_now();
    }
}

/// Set `VBAR_EL1` to a named `.global` vector-table symbol (2 KiB-aligned, in mapped
/// `.text`). Used to swap between the kernel's real `exception_vector_table` (for the
/// scheduler SGI path) and the secondary's minimal `smp_vectors`.
macro_rules! set_vbar {
    ($sym:literal) => {{
        // SAFETY: `$sym` is a 2 KiB-aligned vector table mapped RO in our table.
        unsafe {
            core::arch::asm!(
                concat!("adrp {t}, ", $sym),
                concat!("add  {t}, {t}, :lo12:", $sym),
                "msr vbar_el1, {t}",
                "isb",
                t = out(reg) _,
                options(nomem, nostack),
            );
        }
    }};
}

/// Bring up this PE's GICv3 receive path for the SCHEDULER SGI (INTID 0): enable the
/// system-register CPU interface, wake the redistributor, and enable SGI 0. A subset of
/// [`secondary_gic_init`] (which later also enables the doorbell + timer); the CPU
/// interface state persists, so running both in sequence is fine.
fn scheduler_sgi_enable(idx: usize) {
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
    // SGIs to Group 1, mid priority; enable the scheduler SGI (INTID 0).
    mmio_w32(sgi + GICR_SGI_IGROUPR0, 0xFFFF_FFFF);
    for i in 0..8 {
        mmio_w32(sgi + GICR_SGI_IPRIORITYR + i * 4, 0xA0A0_A0A0);
    }
    mmio_w32(sgi + GICR_SGI_ISENABLER0, 1u32 << SCHED_SGI);
    // SAFETY: ensure the redistributor writes complete before IRQs are unmasked.
    unsafe { core::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
}

/// Self-targeting scheduler SGI for a secondary. The kernel's `gic::trigger_sgi`
/// (which `yield_now` calls via the runtime) hardcodes TargetList bit 0 — i.e. it
/// rings PE 0 (the BSP). On a secondary that would deliver the scheduler SGI to the
/// WRONG core and the yield would never switch. Re-target it at THIS PE (aff0 = our
/// core index) so `yield_now` rings ourselves. Installed in the secondary's runtime.
fn trigger_sched_sgi_self(sgi_id: u32) {
    let aff0 = (read_mpidr() & 0xff) as u32;
    crate::gic::trigger_sgi_core(aff0, sgi_id);
}

/// Sentinel written to `PERCPU_R3_DONE` when R3a is skipped (vs. a genuine 0-progress
/// failure, which leaves it 0). Lets the BSP report "skipped" distinctly.
const R3_SKIPPED: u64 = u64::MAX;

/// Run the R3a cooperative-scheduler self-test on this secondary, recording the
/// outcome to PerCpu for the BSP. Preconditions: per-core heap+PMM are up (R2 ran) and
/// TPIDRRO_EL0 has NOT yet been set to the PerCpu PA (the scheduler uses it as the
/// current-thread id). On return: IRQs masked, `smp_vectors` reinstalled.
///
/// Returns `true` if the scheduler was stood up and the test ran (so R3b can reuse the
/// initialized scheduler), `false` if skipped (no PerCpu / partition too small).
fn run_r3a_coop_test(cc: &CoreConfig, idx: usize, percpu: usize) -> bool {
    if percpu == 0 {
        return false;
    }
    // Guard: a partition too small for the thread-stack pool would panic in
    // threading::init. Mark skipped (sentinel) so the BSP can distinguish it.
    if crate::pmm::free_count() < R3_MIN_FREE_PAGES {
        // SAFETY: PerCpu page is mapped RW in this core's restricted table.
        unsafe { core::ptr::write_volatile((percpu + PERCPU_R3_DONE) as *mut u64, R3_SKIPPED) };
        return false;
    }

    // 1. Make every kernel thread on this core use OUR restricted table, not the BSP's.
    //    `get_boot_ttbr0`'s asm cell (`boot_ttbr0_addr`) is in `.data.boot`, OUTSIDE the
    //    replicated writable window (shared RO here — a write would fault), so we use the
    //    replicated `BOOT_TTBR0_OVERRIDE` static instead. Without this the scheduler would
    //    `msr ttbr0_el1, <BSP table>` on the first switch and destroy isolation.
    akuma_exec::mmu::set_boot_ttbr0_override(cc.ttbr0_phys);

    // 2. Register akuma-exec in our OWN replicated runtime/config cells (the BSP's are
    //    private to it). Canaries OFF; our isolated per-core boot stack as the bounds.
    //    Re-target the scheduler SGI at THIS PE (yield_now must ring ourselves, not the
    //    BSP — see trigger_sched_sgi_self).
    let sec_top = cc.entry_sp as usize;
    let sec_base = sec_top - (1usize << SECONDARY_STACK_SHIFT);
    let user_stack = crate::config::compute_user_stack_size(cc.ram_len as usize);
    let (mut rt, cfg) = crate::build_exec_runtime(sec_base, sec_top, user_stack, false);
    rt.trigger_sgi = trigger_sched_sgi_self;
    akuma_exec::init(rt, cfg);

    // Stage marker helper: record progress to PerCpu so the BSP can pinpoint a hang.
    let stage = |s: u64| {
        // SAFETY: PerCpu page is mapped RW in this core's restricted table.
        unsafe { core::ptr::write_volatile((percpu + PERCPU_R3_STAGE) as *mut u64, s) };
    };
    stage(1); // entered, runtime built + registered

    // 3. Stand up this core's scheduler (allocates thread stacks from OUR PMM).
    akuma_exec::threading::init();
    stage(3); // threading::init done

    // 4. Switch to the real exception vectors (scheduler-SGI path) and enable ONLY
    //    SGI 0, so the sole IRQ that can fire during the test is yield_now's SGI.
    set_vbar!("exception_vector_table");
    scheduler_sgi_enable(idx);
    crate::irq::enable_irqs();
    stage(4); // vectors + gic up, IRQs on

    // 5. Spawn the cooperative workers.
    R3_YIELD_COUNTER.store(0, Ordering::SeqCst);
    R3_THREADS_DONE.store(0, Ordering::SeqCst);
    let mut spawned = 0u64;
    for _ in 0..R3_NUM_THREADS {
        if akuma_exec::threading::spawn_system_thread_fn(r3_worker_body).is_ok() {
            spawned += 1;
        }
    }
    stage(5); // spawned; about to drive the scheduler

    // 6. Drive the scheduler from the boot thread (thread 0) until both workers finish.
    //    Each yield_now switches to a ready worker; control returns here once they all
    //    terminate (or the defensive spin cap is hit).
    let mut spins = 0u64;
    while R3_THREADS_DONE.load(Ordering::Acquire) < spawned && spins < R3_MAX_SPINS {
        akuma_exec::threading::yield_now();
        spins += 1;
    }
    stage(6); // driver loop done

    // 7. Quiesce before returning to the M2 heartbeat path: IRQs off, restore the
    //    minimal smp vectors (whose IRQ handler reads TPIDRRO_EL0 as the PerCpu PA).
    crate::irq::disable_irqs();
    set_vbar!("smp_vectors");

    // 8. Record the proof for the BSP.
    let yields = R3_YIELD_COUNTER.load(Ordering::Acquire);
    let done = R3_THREADS_DONE.load(Ordering::Acquire);
    // SAFETY: PerCpu page is mapped RW in this core's restricted table.
    unsafe {
        core::ptr::write_volatile((percpu + PERCPU_R3_YIELDS) as *mut u64, yields);
        core::ptr::write_volatile((percpu + PERCPU_R3_DONE) as *mut u64, done);
    }
    stage(7); // recorded
    true
}

// ============================================================================
// R3b — per-core PREEMPTIVE scheduler (docs/MULTIKERNEL.md §15). Proves the per-core
// CNTV timer drives the scheduler on a secondary: a spinner thread that NEVER yields
// runs only because the timer preempts the (also non-yielding) boot thread. Reuses the
// scheduler/runtime stood up by R3a. The BSP's preemption path is the model — its
// `timer_irq_handler` re-arms CNTV then `trigger_sgi(SGI_SCHEDULER)`; we do the same but
// re-target the SGI at THIS PE.
// ============================================================================

/// How long the boot thread spin-waits while the timer must preempt it. Must exceed
/// `COOPERATIVE_TIMEOUT_US` (100 ms — thread 0 is cooperative, only preempted past its
/// timeout) by several timer ticks so at least one preemption is guaranteed.
const R3B_RUN_US: u64 = 300_000; // 300 ms

/// R3b worker/handshake state (replicated `.bss` — each core mutates its own copy).
static R3B_STOP: AtomicBool = AtomicBool::new(false);
static R3B_SPIN_COUNT: AtomicU64 = AtomicU64::new(0);
static R3B_DONE: AtomicU64 = AtomicU64::new(0);

/// R3b spinner: a PREEMPTIVE thread that never yields — it just bumps a counter until
/// told to stop. Because neither it nor the boot thread yields, any progress here proves
/// the timer preempted the boot thread to run it.
fn r3b_spinner_body() -> ! {
    while !R3B_STOP.load(Ordering::Relaxed) {
        R3B_SPIN_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    R3B_DONE.store(1, Ordering::SeqCst);
    akuma_exec::threading::mark_current_terminated();
    loop {
        akuma_exec::threading::yield_now();
    }
}

/// Arm this PE's CNTV virtual timer: `CVAL = now + interval`, enabled+unmasked. Re-arming
/// also deasserts the level-triggered PPI. (Mirrors the inline arm in `secondary_main`.)
fn arm_cntv_timer() {
    // SAFETY: CNTV* are EL1-accessible system registers.
    unsafe {
        let now: u64;
        core::arch::asm!("mrs {0}, cntvct_el0", out(reg) now, options(nomem, nostack));
        core::arch::asm!("msr cntv_cval_el0, {0}", in(reg) now + TIMER_INTERVAL_TICKS, options(nomem, nostack));
        core::arch::asm!("msr cntv_ctl_el0, {0}", in(reg) 1u64, options(nomem, nostack));
    }
}

/// Disable this PE's CNTV virtual timer (CTL.ENABLE = 0).
fn disable_cntv_timer() {
    // SAFETY: CNTV_CTL_EL0 is EL1-accessible.
    unsafe { core::arch::asm!("msr cntv_ctl_el0, {0}", in(reg) 0u64, options(nomem, nostack)) };
}

/// Enable the virtual-timer PPI (INTID 27) in THIS core's redistributor. The CPU
/// interface + redistributor were already woken by `scheduler_sgi_enable` (R3a).
fn enable_timer_ppi(idx: usize) {
    let sgi = GICR_BASE + idx * GICR_STRIDE + GICR_SGI_OFFSET;
    mmio_w32(sgi + GICR_SGI_ISENABLER0, 1u32 << TIMER_PPI);
    // SAFETY: ensure the redistributor write completes before IRQs are unmasked.
    unsafe { core::arch::asm!("dsb ish", options(nostack, preserves_flags)) };
}

/// Per-core timer IRQ handler for R3b preemption: re-arm CNTV (deassert the level IRQ)
/// then ring the scheduler SGI to OURSELVES. Registered for `TIMER_PPI` in this core's
/// (replicated) dispatch table; invoked by the real `exception_vector_table` IRQ path.
fn secondary_timer_preempt_handler(_irq: u32) {
    arm_cntv_timer();
    trigger_sched_sgi_self(SCHED_SGI);
}

/// Run the R3b preemptive-scheduler self-test. Reuses the scheduler stood up by
/// `run_r3a_coop_test` (runtime registered, TTBR0 override set, `threading::init` done).
/// On return: IRQs masked, CNTV disabled, `smp_vectors` reinstalled.
fn run_r3b_preempt_test(idx: usize, percpu: usize) {
    if percpu == 0 {
        return;
    }
    let stage = |s: u64| {
        // SAFETY: PerCpu page is mapped RW in this core's restricted table.
        unsafe { core::ptr::write_volatile((percpu + PERCPU_R3_STAGE) as *mut u64, s) };
    };
    stage(10);

    // 1. Route the timer PPI to the scheduler: register a handler that re-arms CNTV and
    //    rings the scheduler SGI at ourselves. No GIC poke (that would hit core 0).
    crate::irq::register_handler_no_gic(TIMER_PPI, secondary_timer_preempt_handler);

    // 2. Real vectors (scheduler SGI + timer dispatch); enable PPI 27 + arm CNTV. SGI 0
    //    is still enabled from R3a's scheduler_sgi_enable.
    set_vbar!("exception_vector_table");
    enable_timer_ppi(idx);
    R3B_STOP.store(false, Ordering::SeqCst);
    R3B_SPIN_COUNT.store(0, Ordering::SeqCst);
    R3B_DONE.store(0, Ordering::SeqCst);
    arm_cntv_timer();
    crate::irq::enable_irqs();
    stage(11);

    // 3. Spawn ONE preemptive spinner (never yields).
    let spawned = akuma_exec::threading::spawn_system_thread_fn(r3b_spinner_body).is_ok();
    stage(12);

    // 4. Boot thread spin-waits WITHOUT yielding for R3B_RUN_US. The only way the spinner
    //    can run (and bump its counter) is the timer preempting us — that IS the proof.
    //    `uptime_us` reads CNTVCT each iteration, so the loop can't be optimized away.
    let deadline = crate::timer::uptime_us() + R3B_RUN_US;
    while crate::timer::uptime_us() < deadline {
        core::hint::spin_loop();
    }
    let count = R3B_SPIN_COUNT.load(Ordering::Acquire);
    stage(13);

    // 5. Stop the spinner and let it terminate (now we may yield).
    R3B_STOP.store(true, Ordering::SeqCst);
    let mut spins = 0u64;
    while spawned && R3B_DONE.load(Ordering::Acquire) == 0 && spins < R3_MAX_SPINS {
        akuma_exec::threading::yield_now();
        spins += 1;
    }
    stage(14);

    // 6. Quiesce: disable the timer, mask IRQs, restore the minimal smp vectors (the M2
    //    heartbeat/doorbell path below re-arms CNTV + uses smp_irq_handler).
    disable_cntv_timer();
    crate::irq::disable_irqs();
    set_vbar!("smp_vectors");

    // 7. Record the proof: spin_count>0 ⇒ the timer preempted the never-yielding boot
    //    thread to run the never-yielding spinner.
    // SAFETY: PerCpu page is mapped RW in this core's restricted table.
    unsafe {
        core::ptr::write_volatile((percpu + PERCPU_R3B_COUNT) as *mut u64, count);
        core::ptr::write_volatile(
            (percpu + PERCPU_R3B_DONE) as *mut u64,
            R3B_DONE.load(Ordering::Acquire),
        );
    }
    stage(16);
}

// ============================================================================
// R4b.1 — per-core scheduler as STEADY STATE (docs/MULTIKERNEL.md §15/§16). R3a/R3b
// proved cooperative + preemptive scheduling on a secondary, then tore down to the M2
// `WFI` heartbeat loop. R4b.1 makes the scheduler PERMANENT: the secondary runs on the
// real `exception_vector_table` with its per-core timer wired to the scheduler and the
// cross-core doorbell as an ordinary IRQ, and the heartbeat/debt-protocol work becomes
// the boot thread's idle body. This is the foundation R4b.2+ build on — a pinned EL0
// process needs a live per-core scheduler to be switched in.
// ============================================================================

/// Steady-state doorbell-SGI handler (a peer rang us). Bump THIS core's PerCpu doorbell
/// counter — the same counter the bringup-path `smp_irq_handler` maintains — so the M2
/// verification still observes a serviced doorbell; the boot thread's idle loop drains
/// the inbox. Runs on the real `exception_vector_table` IRQ path (`dispatch_irq`), so it
/// finds PerCpu via [`SECONDARY_PERCPU`], NOT TPIDRRO_EL0 (now the scheduler thread id).
fn secondary_doorbell_handler(_irq: u32) {
    let percpu = SECONDARY_PERCPU.load(Ordering::Acquire);
    if percpu != 0 {
        // SAFETY: PerCpu page is mapped RW in this core's restricted table; this core
        // is the sole writer of its own counter (IRQs masked during the handler).
        unsafe {
            let p = (percpu + PERCPU_DOORBELL_COUNT) as *mut u64;
            p.write_volatile(p.read_volatile() + 1);
        }
    }
}

/// The secondary's permanent steady state (R4b.1). Preconditions: R3a stood the
/// scheduler up (runtime registered, TTBR0 override set, `threading::init` done) and
/// TPIDRRO_EL0 holds the boot thread's id. Never returns.
fn secondary_steady_state(cfg: &MachineConfig, idx: usize, percpu: usize, mut sm: CoreStateMachine) -> ! {
    // Handlers run on the real vectors, where TPIDRRO_EL0 is the thread id — publish
    // PerCpu via a replicated static for `secondary_doorbell_handler` to read.
    SECONDARY_PERCPU.store(percpu, Ordering::Release);

    // Wire the per-core IRQ sources into this core's (replicated) dispatch table without
    // poking the GIC (`register_handler` would write core 0's redistributor — §irq.rs).
    // The timer handler re-arms CNTV + self-rings the scheduler SGI (R3b's mechanism,
    // now permanent); the doorbell handler counts a peer's ring. Re-registering the
    // timer handler is idempotent (R3b may have set it).
    crate::irq::register_handler_no_gic(TIMER_PPI, secondary_timer_preempt_handler);
    crate::irq::register_handler_no_gic(DOORBELL_SGI, secondary_doorbell_handler);

    // Bring up this PE's GIC receive path for ALL three per-core sources: doorbell SGI +
    // timer PPI (`secondary_gic_init`) and the scheduler SGI 0 (`scheduler_sgi_enable`).
    // Both wake the CPU interface + redistributor; the CPU-interface state persists, so
    // running them in sequence is fine (the doc note on `scheduler_sgi_enable`).
    secondary_gic_init(idx);
    scheduler_sgi_enable(idx);

    // Real vectors permanently; arm the preemption timer; unmask IRQs. The scheduler now
    // preempts us on each tick, `yield_now` switches threads via the self-rung SGI, and a
    // peer's doorbell is delivered to `secondary_doorbell_handler`.
    set_vbar!("exception_vector_table");
    enable_timer_ppi(idx);
    arm_cntv_timer();
    crate::irq::enable_irqs();

    // R4b.2 post-bringup forward probe: once the BSP's persistent forward-server thread
    // is live (`fwd_server_ready`), send ONE echo round-trip from this idle loop and
    // verify the reply — proving the long-running THREAD (not the transient bringup wait
    // loop, long exited) services cross-core forwards. State: 0=not sent, 1=awaiting
    // reply, 2=done. A distinct nonce keeps it unambiguous vs the R4a bringup probe.
    let probe_nonce = 0xB2B2_0000_0000_0000u64 | idx as u64;
    let mut probe = 0u8;
    let mut probe_payload = [0u8; R4A_LEN];
    for (i, b) in probe_payload.iter_mut().enumerate() {
        *b = i as u8;
    }

    // Idle/driver loop on the boot thread: advance the heartbeat, drain the inbox (debt
    // state machine — shedding `Repay` to creditors over the shared rings, plus the R4b.2
    // probe reply — we only ever touch shared rings, never a peer's private state), fire
    // the probe when ready, then SLEEP (`WFI`) until the next interrupt. The per-core timer
    // still preempts to any OTHER thread that becomes runnable (R4b.3 spawns the first
    // pinned EL0 process here); until then this core stays near-idle instead of pegging a
    // host CPU with scheduler-SGI churn.
    loop {
        if let Some(hb) = cfg.heartbeat.get(idx) {
            hb.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(inbox) = cfg.inboxes.get(idx) {
            while let Some(m) = inbox.pop() {
                match m.kind {
                    MSG_FWD_ECHO_REPLY if probe == 1 && m.v1 == probe_nonce => {
                        // copyout + verify the BSP transform (byte + 1), then report via
                        // our console ring (drained to the UART by the BSP drainer).
                        let ok = cfg.fwd_bounce.get(idx).is_some_and(|b| {
                            let mut got = [0u8; R4A_LEN];
                            b.read(&mut got);
                            m.v0 as usize == R4A_LEN
                                && got.iter().enumerate().all(|(i, &x)| x == (i as u8).wrapping_add(1))
                        });
                        crate::safe_print!(
                            80,
                            "[core {}] post-bringup forward round-trip {}\n",
                            idx,
                            if ok { "PASS" } else { "FAIL" }
                        );
                        probe = 2;
                    }
                    MSG_PRESSURE | MSG_REPAID => {
                        let ev = if m.kind == MSG_PRESSURE {
                            Event::Pressure { from: m.from }
                        } else {
                            Event::Repaid { from: m.from, range: Range { base: m.v0, len: m.v1 } }
                        };
                        sm.step(ev, &mut |cmd| {
                            if let Command::Repay { creditor, range } = cmd
                                && let Some(to) = cfg.inboxes.get(creditor as usize)
                            {
                                to.push(MSG_REPAID, idx as u32, range.base, range.len);
                            }
                        });
                    }
                    _ => {}
                }
            }
        }

        // Fire the probe once, after the forward-server thread reports ready (so the
        // request can only be serviced by the thread, never the exited bringup loop).
        let want_probe = probe == 0 && cfg.fwd_server_ready.load(Ordering::Acquire) == 1;
        if want_probe
            && let (Some(bounce), Some(bsp_inbox)) = (cfg.fwd_bounce.get(idx), cfg.inboxes.first())
        {
            bounce.write(&probe_payload);
            if bsp_inbox.push(MSG_FWD_ECHO_REQ, idx as u32, R4A_LEN as u64, probe_nonce) {
                probe = 1;
            }
        }

        // Idle: SLEEP until the next interrupt rather than busy-yielding. A tight
        // `yield_now` loop (scheduler-SGI per pass) pegs this core at 100% and, on a
        // virtualized GIC, floods the hypervisor with VM exits — starving the BSP. With
        // no other thread runnable, `WFI` instead: the per-core timer still fires (waking
        // us to re-drain + keeping liveness advancing) and preempts us to any thread that
        // DOES become runnable (R4b.3+), and a peer's doorbell wakes us immediately. Mask
        // IRQs first, then re-check for pending work, so a message racing in after the
        // drain can't be lost (its doorbell SGI stays pending and WFI returns at once).
        crate::irq::disable_irqs();
        let pending = cfg.inboxes.get(idx).is_some_and(|r| !r.is_empty())
            || (probe == 0 && cfg.fwd_server_ready.load(Ordering::Acquire) == 1);
        if pending {
            crate::irq::enable_irqs();
            continue;
        }
        // SAFETY: WFI wakes on a pending IRQ despite the mask; then unmask to take it.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        crate::irq::enable_irqs();
    }
}

// ============================================================================
// R4a — cross-core syscall-forwarding TRANSPORT (docs/MULTIKERNEL.md §8.1/§10). The
// keystone for the R4 demo ("exec is recursive forwarding"): before a process can run
// on a secondary, the data-movement path — request/reply over the ring + a shared
// bounce region — must work. This proves exactly that round-trip, independent of the
// scheduler (ring + bounce + bounded spin), so it runs even when R3a was skipped.
//
// A secondary `copyin`s a known payload into its `fwd_bounce` slot, sends a request to
// the BSP, and spins for the reply; the BSP (servicing in its bringup wait loop) reads
// the slot, applies a transform (stand-in for the real owner-side syscall), writes the
// result back, and replies. The secondary `copyout`s + verifies. Neither core ever
// touches the other's partition — only the shared bounce region.
// ============================================================================

/// Bytes the R4a probe ships through the bounce region.
const R4A_LEN: usize = 32;

/// Per-core nonce so a reply is unambiguously matched to THIS core's request (and a
/// stale/foreign message can't spoof success).
fn r4a_nonce(idx: usize) -> u64 {
    0xD00D_0000_0000_0000 | idx as u64
}

/// Secondary side of the R4a forward round-trip. Records 1/0 (verified/failed) to
/// PerCpu. BSP = core 0 (the debt protocol's `const BSP` convention).
fn run_r4a_fwd_test(cfg: &MachineConfig, idx: usize, percpu: usize) {
    if percpu == 0 {
        return;
    }
    let (Some(bounce), Some(bsp_inbox), Some(my_inbox)) =
        (cfg.fwd_bounce.get(idx), cfg.inboxes.first(), cfg.inboxes.get(idx))
    else {
        return;
    };

    // copyin: write a known pattern (byte i = i) into OUR bounce slot.
    let mut payload = [0u8; R4A_LEN];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = i as u8;
    }
    bounce.write(&payload);

    // Send the forward request to the BSP, then spin (time-bounded) for the reply.
    let nonce = r4a_nonce(idx);
    let mut ok = 0u64;
    if bsp_inbox.push(MSG_FWD_ECHO_REQ, idx as u32, R4A_LEN as u64, nonce) {
        let deadline = crate::timer::uptime_us() + 1_000_000; // 1 s safety bound
        while crate::timer::uptime_us() < deadline {
            // Accept only OUR reply (matched by nonce); ignore any other message.
            if let Some(m) = my_inbox.pop()
                && m.kind == MSG_FWD_ECHO_REPLY
                && m.v1 == nonce
            {
                // copyout: read back + verify the BSP's transform (byte + 1).
                let mut got = [0u8; R4A_LEN];
                bounce.read(&mut got);
                let len_ok = m.v0 as usize == R4A_LEN;
                let bytes_ok = got
                    .iter()
                    .enumerate()
                    .all(|(i, &b)| b == (i as u8).wrapping_add(1));
                ok = u64::from(len_ok && bytes_ok);
                break;
            }
            core::hint::spin_loop();
        }
    }
    // SAFETY: PerCpu page is mapped RW in this core's restricted table.
    unsafe { core::ptr::write_volatile((percpu + PERCPU_R4A_OK) as *mut u64, ok) };
}

// ============================================================================
// R4b.3 — EL0 on the secondary (docs/MULTIKERNEL.md §10, §15). A user (EL0) process's
// page table must carry the SECONDARY's replicated kernel writable window, NOT the plain
// identity map `add_kernel_mappings` builds. Akuma is TTBR0-only: an EL0->EL1 syscall
// trap runs the kernel under the PROCESS's TTBR0 (no switch), so if that table identity-
// maps RAM, the kernel's `.data`/`.bss` statics (PMM/POOL/process table) resolve to the
// BSP's physical copies — an isolation breach + corruption. R4b.3a (here) builds the
// overlay that fixes this and verifies it on a freshly-created user address space; R4b.3b
// then runs a real pinned EL0 process on such a table.
// ============================================================================

/// Overlay the secondary's replicated kernel writable window onto `uas`. Walks the
/// current (restricted) TTBR0 per page across `[_data_start, _kernel_phys_end)` and
/// remaps each VA in `uas` to the SAME private PA, **EL1-only RW** (`PXN|UXN`, AP defaults
/// to EL1 — so EL0 cannot touch kernel statics; note the restricted table itself uses
/// `AP_RW_ALL`, harmless there because no EL0 runs on it, but unsafe in a user table).
/// RO code is shared-identity (same PA both ways → no overlay needed); the shared
/// descriptor page maps to `cfg_pa` in the restricted table, so the walk carries it
/// across unchanged. `map_page` shatters the covering 2 MiB identity block as needed.
fn build_secondary_user_kernel_view(
    uas: &mut akuma_exec::mmu::UserAddressSpace,
    pbase: usize,
    plen: usize,
) -> Result<(), &'static str> {
    let restricted_l0 = current_ttbr0_l0();
    let code_start = KERNEL_PHYS_BASE;
    let data_start = &raw const _data_start as usize;
    let win_end = (&raw const _kernel_phys_end as usize).next_multiple_of(PAGE);

    // 1. Kernel code + replicated writable window, in one walk over the restricted table
    //    (the source of truth for this core's view). Per page, copy the PA the restricted
    //    table uses and the matching access: code [KERNEL_PHYS_BASE, _data_start) is
    //    shared-identity RO+X at EL1 (no PXN → executable, so the syscall handler runs);
    //    the window [_data_start, _kernel_phys_end) is this core's PRIVATE `.data`/`.bss`
    //    (the shared descriptor page rides across as its `cfg_pa` mapping), EL1-RW no-exec.
    //    `map_page` shatters any covering block as needed.
    let mut va = code_start;
    while va < win_end {
        if let Some(pa) = akuma_exec::mmu::translate_user_va(restricted_l0, va) {
            let f = if va < data_start {
                flags::AP_RO_EL1 | flags::UXN // RO + executable at EL1, no EL0
            } else {
                flags::PXN | flags::UXN // RW at EL1 (AP default), no execute, no EL0
            };
            uas.map_page(va & !(PAGE - 1), pa & !(PAGE - 1), f)?;
        }
        va += PAGE;
    }

    // 2. This core's partition as identity 2 MiB EL1 blocks, so `phys_to_virt` resolves
    //    for any PMM page / heap / kernel stack the kernel touches while servicing a
    //    syscall under this table. The BSP's RAM is deliberately NOT mapped (isolation).
    let len_2mb = plen & !(TWO_MB - 1);
    let mut off = 0;
    while off < len_2mb {
        uas.map_kernel_block_2mb(pbase + off, pbase + off)?;
        off += TWO_MB;
    }
    Ok(())
}

/// Read this core's active TTBR0 L0 table as a `*const u64` (identity on a secondary —
/// the partition 2 MiB blocks map its own page-table pages).
fn current_ttbr0_l0() -> *const u64 {
    let ttbr0: u64;
    // SAFETY: reading TTBR0_EL1, the current address-space root.
    unsafe { core::arch::asm!("mrs {0}, ttbr0_el1", out(reg) ttbr0, options(nomem, nostack)) };
    phys_to_virt((ttbr0 & 0x0000_FFFF_FFFF_F000) as usize) as *const u64
}

/// Build a user address space on the secondary with the full secondary kernel view and
/// prove three regions resolve correctly — the R4b.3a mapping fix that lets an EL0
/// process run here. Records an aggregate 1/0 to PerCpu (+ the window PAs for
/// diagnostics). Needs the akuma-exec runtime, which R3a stood up; no-op (records 0) if
/// R3a was skipped. Probes:
///   - **code** (`KERNEL_PHYS_BASE`): identity (user_pa == VA), so the syscall handler runs;
///   - **window** (a kernel `.data` static): PRIVATE — user_pa == restricted_pa, != VA;
///   - **partition** (`pbase`): identity (user_pa == VA), so `phys_to_virt` works at EL1.
fn verify_user_table_kernel_window(pbase: usize, plen: usize, percpu: usize) {
    if percpu == 0 {
        return;
    }
    let record = |ok: u64, win_user_pa: u64, win_rest_pa: u64| {
        // SAFETY: PerCpu page is mapped RW in this core's restricted table.
        unsafe {
            core::ptr::write_volatile((percpu + PERCPU_USERTAB_USER_PA) as *mut u64, win_user_pa);
            core::ptr::write_volatile((percpu + PERCPU_USERTAB_REST_PA) as *mut u64, win_rest_pa);
            core::ptr::write_volatile((percpu + PERCPU_USERTAB_PRIV_OK) as *mut u64, ok);
        }
    };

    let Some(mut uas) = akuma_exec::mmu::UserAddressSpace::new() else {
        record(0, 0, 0);
        return;
    };
    if build_secondary_user_kernel_view(&mut uas, pbase, plen).is_err() {
        record(0, 0, 0);
        return;
    }

    let user_l0 = phys_to_virt(uas.l0_phys()) as *const u64;
    let rest_l0 = current_ttbr0_l0();
    let xlate = |l0, va: usize| akuma_exec::mmu::translate_user_va(l0, va);

    // code: identity in both tables.
    let code_va = KERNEL_PHYS_BASE;
    let code_ok = xlate(user_l0, code_va) == Some(code_va);
    // window: PRIVATE — matches the restricted table, not identity (not the BSP's copy).
    let win_va = (&raw const SMP_REPLICATION_TEST as usize) & !(PAGE - 1);
    let win_user = xlate(user_l0, win_va);
    let win_rest = xlate(rest_l0, win_va);
    let win_ok = matches!((win_user, win_rest), (Some(u), Some(r)) if u == r && r != win_va);
    // partition: identity (so phys_to_virt resolves for kernel RAM at EL1).
    let part_ok = xlate(user_l0, pbase) == Some(pbase);

    record(
        u64::from(code_ok && win_ok && part_ok),
        win_user.unwrap_or(0) as u64,
        win_rest.unwrap_or(0) as u64,
    );
    // `uas` drops here: its Drop frees the user table's OWN page-table pages + ASID. The
    // kernel PAs it pointed at were never tracked as user frames, so they are untouched.
}

/// BSP side of forward serving: drain its inbox and service `MSG_FWD_ECHO_REQ`s. Called
/// repeatedly from the bringup online-wait loop so a secondary spinning on its reply
/// can't deadlock against the BSP spinning on that secondary's `Online`. The transform
/// (byte + 1) stands in for the real owner-side syscall (VFS `read`, socket op) of
/// R4b/R4c; the data path it exercises — bounce region + reply ring — is the real one.
fn service_fwd_requests(cfg: &MachineConfig, bsp_idx: usize) -> usize {
    let Some(bsp_inbox) = cfg.inboxes.get(bsp_idx) else {
        return 0;
    };
    let mut served = 0usize;
    while let Some(m) = bsp_inbox.pop() {
        if m.kind != MSG_FWD_ECHO_REQ {
            continue; // debt-protocol traffic is post-bringup; ignore anything else here
        }
        let from = m.from as usize;
        let len = m.v0 as usize;
        if let Some(bounce) = cfg.fwd_bounce.get(from) {
            bounce.map_in_place(len, |b| b.wrapping_add(1));
        }
        if let Some(reply) = cfg.inboxes.get(from) {
            reply.push(MSG_FWD_ECHO_REPLY, bsp_idx as u32, m.v0, m.v1);
        }
        served += 1;
    }
    served
}

/// Spawn the BSP's persistent forward-server (R4b.2). A SYSTEM thread (kernel stack,
/// like the console drainer), spawned from `run_async_main` AFTER preemption is live.
/// It drains `inboxes[bsp]` and services `MSG_FWD_ECHO_REQ`s for the lifetime of the
/// system — the steady-state replacement for the transient bringup wait loop, which
/// only serviced forwards while waiting for cores to report Online. R4b.4+ point this
/// at the real owner-side syscalls (VFS `read`, sockets); for now it runs the same
/// `service_fwd_requests` echo transform. Sets `fwd_server_ready` so secondaries know
/// the long-running servicer is live before they send a post-bringup forward.
pub fn start_fwd_server() {
    if !PROBED.load(Ordering::Acquire) || NUM_CORES.load(Ordering::Relaxed) <= 1 {
        return;
    }
    let spawned = akuma_exec::threading::spawn_system_thread_fn(|| {
        // SAFETY: descriptor is initialized and identity-mapped on the BSP.
        let cfg = unsafe { &*MACHINE_CONFIG.0.get() };
        let bsp_idx = (read_mpidr() & 0xff) as usize;
        cfg.fwd_server_ready.store(1, Ordering::Release);
        let mut total = 0usize;
        loop {
            let n = service_fwd_requests(cfg, bsp_idx);
            if n > 0 && total == 0 {
                // First forward serviced post-bringup ⇒ this is the THREAD, not the
                // bringup loop (long exited) — the R4b.2 proof.
                crate::safe_print!(80, "[SMP] fwd-server thread: serviced post-bringup forward(s) PASS\n");
            }
            total += n;
            akuma_exec::threading::yield_now();
        }
    });
    match spawned {
        Ok(tid) => {
            crate::safe_print!(64, "[SMP] fwd-server thread spawned (tid {})\n", tid);
        }
        Err(e) => {
            crate::safe_print!(80, "[SMP] fwd-server thread spawn FAILED: {}\n", e);
        }
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
