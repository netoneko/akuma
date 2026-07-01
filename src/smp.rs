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
    ENF_FAULTED, ENF_LEAKED, ENF_TESTING, FWD_BOUNCE_CAP, MAGIC, MAX_CORES, MSG_CORE_INIT,
    MSG_FWD_ECHO_REPLY, MSG_FWD_ECHO_REQ, MSG_FWD_SYSCALL_REQ, MSG_PRESSURE,
    MSG_REPAID, STATE_BOOTING, STATE_OFFLINE, STATE_ONLINE, STATE_PARKED,
};
use spinning_top::Spinlock;
use crate::console;
use crate::pmm;

/// Per-core boot stack size as a power-of-two shift (1 << 14 = 16 KiB). Only the
/// trampoline + `secondary_rust_start` run on it before the core switches to its
/// private isolated stack, so 16 KiB is ample. (Kernel/asm-only — not in the crate.)
const SECONDARY_STACK_SHIFT: usize = 14;

/// PSCI `CPU_ON` (SMC64) function id.
const PSCI_CPU_ON: u64 = 0xC400_0003;

/// PSCI `CPU_OFF` (SMC32) function id — powers off the CALLING core; does not return
/// on success. A parked secondary that is never initialized within the watchdog window
/// calls this to stop spinning (a later `core_init` re-`CPU_ON`s it).
const PSCI_CPU_OFF: u64 = 0x8400_0002;

/// How long a freshly-online secondary parks awaiting `MSG_CORE_INIT` before it logs an
/// error and `CPU_OFF`s itself (docs/MULTIKERNEL.md R4b lifecycle). Not a hard race: a
/// later `core_init` syscall re-`CPU_ON`s a core that shut down, so this only bounds how
/// long an un-activated core stays powered, it does not bound when userspace must act.
///
/// Sized to comfortably exceed boot-to-herd time: the core parks right after bringup, but
/// the BSP then runs its full self-test suite + rump + SSH before `AUTO_START_HERD` spawns
/// herd, which is what calls `core_init`. A parked core just `WFI`s (near-idle, not
/// spinning), so a generous window costs nothing and lets herd activate the core cleanly
/// in the common case instead of forcing a `CPU_OFF`→re-`CPU_ON` re-bringup every boot.
/// The re-wake path still covers a genuinely-forgotten core (no init system ever acts).
const CORE_INIT_WATCHDOG_US: u64 = 120_000_000;

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

/// THIS core's private partition base/len (replicated `.bss` statics — each secondary
/// sets only its own). Read by the `prepare_user_address_space` runtime hook, which is a
/// bare `fn` pointer and so cannot capture them. Lets the SAME spawn path build a correct
/// user table on a secondary (overlay its replicated kernel window) without a bespoke entry.
static SECONDARY_PART_BASE: AtomicUsize = AtomicUsize::new(0);
static SECONDARY_PART_LEN: AtomicUsize = AtomicUsize::new(0);

/// Runtime hook installed in a secondary's `ExecRuntime` (docs/MULTIKERNEL.md §4.2/R4b.3a):
/// `UserAddressSpace::new()` calls it after the default identity kernel mappings, so every
/// user address space the normal spawn path builds on this core also carries the core's
/// REPLICATED kernel writable window (`.data`/`.bss` → its OWN private pages). Without it a
/// pinned process's syscall would resolve kernel statics (PMM/POOL/process table) to the
/// BSP's copies. `None`/no-op on the BSP (set only on secondaries).
fn secondary_prepare_user_as(uas: &mut akuma_exec::mmu::UserAddressSpace) -> Result<(), &'static str> {
    let pbase = SECONDARY_PART_BASE.load(Ordering::Acquire);
    let plen = SECONDARY_PART_LEN.load(Ordering::Acquire);
    if pbase == 0 || plen == 0 {
        return Err("secondary partition base/len not set");
    }
    build_secondary_user_kernel_view(uas, pbase, plen)
}

/// Whether THIS core routes console output to a per-core ring — i.e. it is an isolated
/// secondary, not the BSP (the UART owner). The BSP never sets `CONSOLE_RING_PA`. Used by
/// the `write` syscall to send a pinned process's tty output to the §8.2 console ring.
pub fn is_on_secondary() -> bool {
    CONSOLE_RING_PA.load(Ordering::Acquire) != 0
}

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

/// Number of contiguous pages for a secondary's isolated boot stack (256 KiB).
///
/// This is the stack the secondary's boot/idle thread runs on, INCLUDING when it spawns
/// its pinned init program (`spawn_init_program` → `Process::from_elf` →
/// `load_elf_with_stack`). The ELF loader has deep, large frames (the BSP runs it on a
/// 1 MiB stack), so the original 16 KiB overflowed into the replicated `.data` below the
/// stack and corrupted kernel statics (CONFIG/RUNTIME read back unregistered). 256 KiB is
/// carved from the core's own 1 GiB partition, so the cost is negligible.
const STACK_PAGES: usize = 64;

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
/// The secondary core that owns a dedicated NIC for a LOCAL network stack (rump), instead of
/// forwarding sockets to core 0 (docs/MULTIKERNEL_NETWORKING_EXPERIMENT.md Stage 0/1). Its
/// virtio-mmio window is mapped into its isolated table (`build_isolated_table`), and it binds
/// `rump_tap` to the dedicated NIC (`secondary_init_local_nic`). Boot with `SMP>=3` (so it's a
/// real secondary and clear of herd's core-1 services) and QEMU `CORE2_NIC=1` (adds the NIC on
/// `virtio-mmio-bus.5`). Assigning this via herd core-init is the follow-up (see the memory note).
#[cfg(feature = "rump")]
const RUMP_NIC_CORE: usize = 2;

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

    // Rump-on-secondary (docs/MULTIKERNEL_NETWORKING_EXPERIMENT.md §7, Stage 0/1): the core
    // that owns a dedicated NIC gets DIRECT access to the virtio-mmio window so it can drive
    // that NIC locally (a local rump stack) instead of forwarding sockets to core 0. All 8
    // mapped virtio slots live in ONE 4 KiB device page at VIRTIO_MMIO_PHYS, mapped at the
    // same DEV_VIRTIO_VA the BSP uses — so `rump_tap`'s VirtIONetRaw resolves its registers.
    // This is a deliberate, single-device relaxation of the RAM-isolation invariant, scoped to
    // one page and one core (its DMA buffers come from its own identity-mapped partition).
    #[cfg(feature = "rump")]
    if idx == RUMP_NIC_CORE {
        const VIRTIO_MMIO_PHYS: usize = 0x0A00_0000;
        if map_4k(l0_pa, akuma_exec::mmu::DEV_VIRTIO_VA, VIRTIO_MMIO_PHYS, pte_device(), &mut bump).is_none() {
            return false;
        }
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

    // R4b lifecycle: each secondary boots → soundness self-tests → PARKS awaiting
    // MSG_CORE_INIT (docs/MULTIKERNEL.md §6/R4b, acceptance/12).
    //
    // Who activates a parked core depends on the boot mode:
    //   - **herd-managed** (`AUTO_START_HERD && MULTIKERNEL_INIT_HERD`, the default): the
    //     cores stay PARKED here. herd, the userspace init system, reads `/proc/cores` and
    //     calls the `core_init` syscall to activate the cores it wants — handing each its
    //     init-program path in the activation message (the program it should run). So the
    //     scheduler/role + workload come up LATER, when herd acts.
    //   - **bare SMP / non-herd**: no userspace init drives activation, so the BSP
    //     auto-activates each core here (with no init program — it just stands up its
    //     scheduler/role for the boot self-tests). The message is delivered to the inbox
    //     (persisted); a parked core drains it on entry, so the doorbell is a best-effort
    //     wake (harmless if the core's GIC receive path isn't up yet).
    let herd_managed = crate::config::AUTO_START_HERD && crate::config::MULTIKERNEL_INIT_HERD;
    if !herd_managed {
        for idx in 0..num_cores {
            if idx == bsp_idx {
                continue;
            }
            cfg.inboxes[idx].push(MSG_CORE_INIT, bsp_idx as u32, 0, 0);
            crate::gic::trigger_sgi_core(idx as u32, DOORBELL_SGI);
        }
    }

    // Wait (bounded ~2s) for each secondary to reach its expected lifecycle state, then
    // report. In herd-managed mode that is PARKED (the cores haven't been activated yet),
    // so only the PRE-park soundness checks (isolated-run marker, `.data`/`.bss`
    // replication, enforcement, per-core PMM/heap — all recorded before parking) are
    // reported; the role probes (R3a/R3b/R4a/user-table) run later, post-activation, and
    // report via the secondary's own console ring. In bare-SMP mode the cores were just
    // auto-activated, so we wait for ONLINE and report the full self-test matrix.
    let want_state = if herd_managed { STATE_PARKED } else { STATE_ONLINE };
    let deadline = crate::timer::uptime_us() + 2_000_000;
    for idx in 0..num_cores {
        if idx == bsp_idx {
            continue;
        }
        let mut reached = false;
        while crate::timer::uptime_us() < deadline {
            if cfg.cores[idx].state.load(Ordering::Acquire) == want_state {
                reached = true;
                break;
            }
            // Service any secondary's forward request (R4a) while we wait — in bare-SMP
            // mode a secondary blocks on its reply before announcing Online, so without
            // this the BSP (spinning on Online) and the secondary (spinning on the reply)
            // deadlock. Harmless in herd-managed mode (nothing to service pre-park).
            service_fwd_requests(cfg, bsp_idx);
            core::hint::spin_loop();
        }
        let reached_label = if herd_managed { " PARKED" } else { " ONLINE" };
        crate::safe_print!(
            80,
            "[SMP] core {}{}",
            idx,
            if reached { reached_label } else { " TIMEOUT (never reached expected state)" }
        );
        if !reached {
            // Pinpoint a hang: in bare-SMP mode report the last R3a stage the secondary
            // reached (R3a hasn't run yet in herd-managed mode — the core just never parked).
            if !herd_managed {
                let percpu = cfg.cores[idx].percpu_phys as usize;
                if percpu != 0 {
                    let st = unsafe {
                        core::ptr::read_volatile((percpu + PERCPU_R3_STAGE) as *const u64)
                    };
                    let yl = unsafe {
                        core::ptr::read_volatile((percpu + PERCPU_R3_YIELDS) as *const u64)
                    };
                    crate::safe_print!(64, " [last R3a stage={} yields={}]", st as usize, yl as usize);
                }
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
        // (Post-activation probes: only valid in bare-SMP mode, where we waited for
        // ONLINE; in herd-managed mode the core is still PARKED, so they run later and
        // report via its console ring.)
        if percpu != 0 && !herd_managed {
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
        if percpu != 0 && !herd_managed {
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
        if percpu != 0 && !herd_managed {
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
        if percpu != 0 && !herd_managed {
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

    // R4b.4: each secondary fetches /bin/hello's ELF header from core 0's filesystem via
    // forwarded openat/read/close (the generic syscall forwarder) — "exec is recursive
    // forwarding." It runs post-bringup (once the secondary's forward-server is up) and
    // reports through its own console ring, so there is no BSP-side reader here.
    crate::safe_print!(48, "[SMP] bringup complete\n");

    // The liveness + debt-reclaim demos assume ONLINE cores running their heartbeat/debt
    // loop. In herd-managed mode the cores are still PARKED (heartbeat not advancing, debt
    // protocol not running), so the demos would misreport "stalled" / block awaiting
    // repayments that never come. Run them only in bare-SMP mode, where bringup brought
    // the cores fully online.
    if !herd_managed {
        monitor_liveness(cfg, num_cores, bsp_idx);
        run_memory_demo(cfg, num_cores, bsp_idx);
    }
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

    // R4b lifecycle: the cheap soundness self-tests (isolation, replication, per-core
    // PMM/heap) are done — now PARK and await activation. We stand up the scheduler/role
    // below ONLY after a `MSG_CORE_INIT` arrives (sent by the initiator: the BSP during
    // bringup today; herd via the `core_init` syscall once wired). No init within the
    // watchdog window ⇒ log + `CPU_OFF` (a later `core_init` re-`CPU_ON`s us). This is the
    // userspace-driven core-activation seam (docs/MULTIKERNEL.md §6/R4b).
    if !secondary_park_and_await_init(cfg, core_idx, percpu) {
        secondary_shutdown(cfg, core_idx); // never returns
    }

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

    // acceptance/12 Milestone-1 line: the activation handshake completed — this core
    // stood up its scheduler/role (R3a) and is about to announce Online. Printed only
    // when the scheduler actually came up (so a too-small partition that skipped R3a
    // doesn't claim "scheduler up").
    if sched_up {
        crate::safe_print!(64, "[core {}] init: scheduler + role up — ONLINE\n", core_idx);
    }

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
    // Use the ACTUAL carved isolated-stack size (`STACK_PAGES`), not the trampoline's
    // `secondary_boot_stacks` shift — `entry_sp` is the top of the partition-carved stack
    // that `build_isolated_table` allocated, which is `STACK_PAGES` pages.
    let sec_base = sec_top - STACK_PAGES * PAGE;
    let user_stack = crate::config::compute_user_stack_size(cc.ram_len as usize);
    let (mut rt, mut cfg) = crate::build_exec_runtime(sec_base, sec_top, user_stack, false);
    rt.trigger_sgi = trigger_sched_sgi_self;
    // Publish our partition for the user-AS overlay hook (a bare fn ptr can't capture it),
    // then install the hook so the normal spawn path builds a correct user table on this
    // core (overlays our replicated kernel window) — the seam that lets a pinned EL0
    // process run here (docs/MULTIKERNEL.md §4.2/R4b.3a).
    SECONDARY_PART_BASE.store(cc.ram_base as usize, Ordering::Release);
    SECONDARY_PART_LEN.store(cc.ram_len as usize, Ordering::Release);
    rt.prepare_user_address_space = Some(secondary_prepare_user_as);
    // Forward a `close` for any RemoteFd (a Proxy'd file/socket) still open when a process
    // exits, so the owner frees its handle (R4b.5, §8.1).
    rt.remote_fd_close = Some(fwd_close_remote);
    // This core has no local VFS: forward exec's file reads to the owner, and force the
    // whole-file load path (one forwarded fetch) instead of demand-paging (R4b.5 Phase 2).
    rt.read_file = secondary_forward_read_file;
    rt.read_at = secondary_forward_read_at;
    rt.file_size = secondary_forward_file_size;
    cfg.prefer_whole_file_load = true;
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
    // A forwarder thread is parked awaiting its reply and the owner just published it (that's
    // why this doorbell rang). Preempt the idle WFI thread back to the waiter NOW instead of
    // on the next timer tick. The idle thread is COOPERATIVE, so an involuntary scheduler SGI
    // won't switch away from it (schedule_indices honors its timeout) — mark the reschedule
    // VOLUNTARY so it switches unconditionally, then ring our own scheduler SGI. This is what
    // takes a forwarded syscall's reply-observe latency from ~1 timer tick to ~tens of µs.
    if FWD_AWAITING_REPLY.load(Ordering::Acquire) {
        akuma_exec::threading::request_voluntary_reschedule();
        trigger_sched_sgi_self(SCHED_SGI);
    }
}

/// The secondary's permanent steady state (R4b.1). Preconditions: R3a stood the
/// scheduler up (runtime registered, TTBR0 override set, `threading::init` done) and
/// TPIDRRO_EL0 holds the boot thread's id. Never returns.
/// PARK after the soundness self-tests and await activation (docs/MULTIKERNEL.md R4b
/// lifecycle). Announces `STATE_PARKED`, brings up this PE's receive path (doorbell SGI +
/// a periodic virtual-timer tick), then sleeps (`WFI`) draining its inbox until either a
/// `MSG_CORE_INIT` arrives (→ returns `true`; the caller stands up the scheduler/role) or
/// the watchdog window elapses (→ returns `false`; the caller `CPU_OFF`s). The receive
/// path is torn down before returning so the role sequence starts from the clean post-R2
/// state. The activation message is delivered to the inbox (persisted) and the loop drains
/// it before each sleep, so a doorbell that races the park-entry can't be lost.
fn secondary_park_and_await_init(cfg: &MachineConfig, core_idx: usize, percpu: usize) -> bool {
    if let Some(cc) = cfg.cores.get(core_idx) {
        cc.state.store(STATE_PARKED, Ordering::Release);
    }
    crate::safe_print!(
        96,
        "[core {}] parked: awaiting MSG_CORE_INIT (watchdog {} ms)\n",
        core_idx,
        CORE_INIT_WATCHDOG_US / 1000
    );

    // Receive path: doorbell SGI (initiator ringing us) + virtual-timer tick (periodic
    // wakeups so the watchdog deadline is checked even with no doorbell). Both land in
    // `smp_irq_handler` (VBAR is `smp_vectors` from the enforcement self-test), which finds
    // PerCpu via TPIDRRO_EL0 — free on a secondary, so stash it.
    if percpu != 0 {
        // SAFETY: TPIDRRO_EL0 is unused on a secondary until the scheduler claims it.
        unsafe { core::arch::asm!("msr tpidrro_el0, {0}", in(reg) percpu, options(nomem, nostack)) };
    }
    secondary_gic_init(core_idx);
    // SAFETY: CNTV* are EL1-accessible; arm a periodic tick (re-armed by smp_irq_handler).
    unsafe {
        let now: u64;
        core::arch::asm!("mrs {0}, cntvct_el0", out(reg) now, options(nomem, nostack));
        core::arch::asm!("msr cntv_cval_el0, {0}", in(reg) now + TIMER_INTERVAL_TICKS, options(nomem, nostack));
        core::arch::asm!("msr cntv_ctl_el0, {0}", in(reg) 1u64, options(nomem, nostack));
    }
    crate::irq::enable_irqs();

    let deadline = crate::timer::uptime_us() + CORE_INIT_WATCHDOG_US;
    let mut activated = false;
    loop {
        if let Some(inbox) = cfg.inboxes.get(core_idx) {
            while let Some(m) = inbox.pop() {
                if m.kind == MSG_CORE_INIT {
                    activated = true; // drain the rest, but we're going online
                }
                // Other kinds (debt protocol) shouldn't arrive before we're online; ignore.
            }
        }
        if activated || crate::timer::uptime_us() >= deadline {
            break;
        }
        // Race-free sleep: mask, re-check the inbox, WFI (wakes on the timer tick or a
        // doorbell even while masked), unmask to take the wake interrupt.
        crate::irq::disable_irqs();
        if cfg.inboxes.get(core_idx).is_some_and(|r| !r.is_empty()) {
            crate::irq::enable_irqs();
            continue;
        }
        // SAFETY: WFI wakes on a pending IRQ despite the mask.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        crate::irq::enable_irqs();
    }

    // Tear down the park receive path: leave IRQs masked + the timer off so the role
    // sequence (R3a stands up its own scheduler vectors/timer) starts from a clean slate.
    crate::irq::disable_irqs();
    disable_cntv_timer();
    activated
}

/// A parked secondary that was never activated within the watchdog window: log it, mark
/// the core `STATE_OFFLINE` (so a later `core_init` knows to re-`CPU_ON` it), and PSCI
/// `CPU_OFF` this PE. Does not return on success (the core powers down).
fn secondary_shutdown(cfg: &MachineConfig, core_idx: usize) -> ! {
    crate::safe_print!(
        112,
        "[core {}] no MSG_CORE_INIT within {} ms — shutting down (CPU_OFF); core_init re-wakes it\n",
        core_idx,
        CORE_INIT_WATCHDOG_US / 1000
    );
    if let Some(cc) = cfg.cores.get(core_idx) {
        cc.state.store(STATE_OFFLINE, Ordering::Release);
    }
    dsb_sy();
    let use_hvc = USE_HVC.load(Ordering::Relaxed);
    psci_call(use_hvc, PSCI_CPU_OFF, 0, 0, 0);
    // CPU_OFF only returns on error — park forever as a fallback.
    loop {
        wfe();
    }
}

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

    // Once the BSP's persistent forward-server thread is live (`fwd_server_ready`), spawn
    // this core's INIT PROGRAM (named by the initiator in MSG_CORE_INIT) as a real EL0
    // process: fetch its ELF via forwarded open/read/close ("exec is recursive forwarding")
    // and spawn it locally on this core's scheduler. Done once.
    let mut init_spawned = false;
    let mut bench_spawned = false;

    // Idle/driver loop on the boot thread: advance the heartbeat, drain the inbox (debt
    // state machine — shedding `Repay` to creditors over the shared rings; we only ever
    // touch shared rings, never a peer's private state), run the one-shot exec-fetch probe
    // when the forward-server is up, then SLEEP (`WFI`) until the next interrupt. The
    // per-core timer still preempts to any OTHER thread that becomes runnable (R4b.3b
    // spawns the first pinned EL0 process here); until then this core stays near-idle
    // instead of pegging a host CPU with scheduler-SGI churn.
    loop {
        if let Some(hb) = cfg.heartbeat.get(idx) {
            hb.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(inbox) = cfg.inboxes.get(idx) {
            while let Some(m) = inbox.pop() {
                match m.kind {
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

        // Spawn the init program ONCE, after the forward-server thread reports ready (so the
        // forwarded ELF-fetch syscalls can only be serviced by the thread, never the exited
        // bringup loop). Blocking on the fetch (many round-trips); fine here — the boot
        // thread is the idle thread, and once the process is spawned the timer preempts to it.
        if !init_spawned && cfg.fwd_server_ready.load(Ordering::Acquire) == 1 {
            spawn_init_program(cfg, idx);
            init_spawned = true;
        }

        // Forward-transport self-test (deterministic; no disk/network needed). Runs after the
        // forward-server is ready, on its own thread so it yields to this idle thread between
        // round-trips — the steady-state doorbell-wake path. Gated OFF by default (RUN_FWD_BENCH).
        if RUN_FWD_BENCH && !bench_spawned && cfg.fwd_server_ready.load(Ordering::Acquire) == 1 {
            spawn_forward_bench();
            bench_spawned = true;
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
            || (!init_spawned && cfg.fwd_server_ready.load(Ordering::Acquire) == 1);
        if pending {
            crate::irq::enable_irqs();
            continue;
        }
        // SAFETY: WFI wakes on a pending IRQ despite the mask; then unmask to take it.
        unsafe { core::arch::asm!("wfi", options(nomem, nostack)) };
        crate::irq::enable_irqs();
    }
}

/// OS capability a syscall touches, for the dispatch decision (docs/MULTIKERNEL.md
/// §10.1). Only `Vfs`/`Net` are *forwardable* — they're routed by the caps map (Own =>
/// local, Proxy(owner) => forward to owner). Everything else (threads, memory, futexes,
/// time, signals, getpid…) is `Local`: it falls through to THIS core's own kernel,
/// resolving against its replicated state. **Console is deliberately NOT here** — tty
/// output is fire-and-forget over the §8.2 per-core append ring, not a synchronous
/// forwarded syscall, so `write(1/2,…)` never takes this path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Capability {
    Vfs,
    Net,
    Local,
}

/// Classify a syscall number into the capability that decides its dispatch, using the
/// kernel's canonical `syscall::nr` constants (no magic numbers). A first cut covering the
/// exec-fetch + socket sets; extend as more syscalls forward.
fn capability_of(nr: u64) -> Capability {
    use crate::syscall::nr;
    match nr {
        // VFS: path/inode/fd I/O against a filesystem.
        nr::OPENAT | nr::CLOSE | nr::READ | nr::WRITE | nr::LSEEK | nr::FSTAT
        | nr::GETDENTS64 | nr::READLINKAT | nr::FCNTL => Capability::Vfs,
        // Net: sockets.
        nr::SOCKET | nr::BIND | nr::CONNECT | nr::SENDTO | nr::RECVFROM | nr::LISTEN
        | nr::ACCEPT | nr::SETSOCKOPT | nr::GETSOCKOPT | nr::SHUTDOWN => Capability::Net,
        _ => Capability::Local,
    }
}

/// Where a syscall on core `self_idx` must run, per the caps map. `None` = handle locally
/// (the syscall's capability is `Local`, or this core OWNS it); `Some(owner)` = forward to
/// `owner`. Phase-0 split (docs §10): core 0 owns VFS + Net; every other core proxies them
/// to core 0. (The caps map is hardcoded to Phase-0 here; §5 makes it a descriptor field.)
fn capability_owner(self_idx: usize, nr: u64) -> Option<usize> {
    const OWNER: usize = 0; // Phase-0: core 0 owns VFS + Net
    match capability_of(nr) {
        Capability::Vfs | Capability::Net if self_idx != OWNER => Some(OWNER),
        _ => None,
    }
}

/// Per-core (replicated `.bss`, so private to each secondary) cooperative lock that
/// serializes this core's use of its SINGLE shared forward slot
/// (`fwd_call`/`fwd_bounce`/`fwd_reply`). Multiple EL0 threads on one secondary (e.g. a
/// pinned `sshd` plus the `curl` it spawned) may issue forwarded syscalls concurrently;
/// the slot holds exactly one outstanding request, so they must take turns. The lock is
/// **cooperative** — a contender `yield_now`s rather than spins, and the holder also
/// `yield_now`s while awaiting its reply — because a raw spinlock held across the reply
/// wait would deadlock the moment the preemptive scheduler switches to another forwarder.
static FWD_SLOT_BUSY: AtomicBool = AtomicBool::new(false);

/// Acquire this core's forward slot, yielding the CPU while it is held by another thread.
fn fwd_slot_acquire() {
    while FWD_SLOT_BUSY.swap(true, Ordering::Acquire) {
        akuma_exec::threading::yield_now();
    }
}

/// Release this core's forward slot.
fn fwd_slot_release() {
    FWD_SLOT_BUSY.store(false, Ordering::Release);
}

/// Set by `forward_syscall` while a forwarder thread is parked (yielding) awaiting its
/// reply. The doorbell handler reads it: the BSP rings our doorbell the instant it publishes
/// a reply, and if a forward is outstanding we ring our OWN scheduler SGI so the scheduler
/// preempts the idle (WFI) thread back to the waiter at once — instead of the waiter only
/// resuming on the next per-core timer tick (`TIMER_INTERVAL_TICKS` later). Per-core
/// replicated `.bss`; only one forward is outstanding per core at a time (`FWD_SLOT_BUSY`).
static FWD_AWAITING_REPLY: AtomicBool = AtomicBool::new(false);

/// Secondary side of ONE forwarded-syscall round-trip (§8.1): take this core's forward
/// slot, publish `nr` + `args` (and an optional inbound buffer) to `fwd_call`/`fwd_bounce`,
/// ring the owner, and wait (yielding) for the dedicated `fwd_reply` mailbox to change.
/// Returns the syscall's return value (Linux errno-encoded). The reply rides a per-core
/// mailbox, NOT the control inbox, so the idle loop's inbox drain can never swallow it.
///
/// This is the TRANSPORT primitive — one request/one reply. The owner services it
/// non-blockingly (it replies at once, even with `-EAGAIN`), so the round-trip is short;
/// BLOCKING semantics (a `recv` that waits for data) live one level up, in the `fwd_*`
/// helpers that re-issue this on `-EAGAIN`. Outbound bytes (a `read`/`recv` result) are
/// left in `fwd_bounce[idx]` for the caller to copy out after this returns.
fn forward_syscall(cfg: &MachineConfig, idx: usize, owner: usize, nr: u64, args: &[u64; 6], in_buf: Option<&[u8]>) -> u64 {
    let (Some(call), Some(bounce), Some(owner_inbox), Some(reply)) = (
        cfg.fwd_call.get(idx),
        cfg.fwd_bounce.get(idx),
        cfg.inboxes.get(owner),
        cfg.fwd_reply.get(idx),
    ) else {
        return enc_err(38); // ENOSYS — no transport
    };
    fwd_slot_acquire();
    if let Some(b) = in_buf {
        bounce.write(b);
    }
    call.set(nr, args);
    let snapshot = reply.seq();
    if !owner_inbox.push(MSG_FWD_SYSCALL_REQ, idx as u32, 0, 0) {
        fwd_slot_release();
        return enc_err(11); // EAGAIN — owner inbox full
    }
    // The owner replies promptly (non-blocking on its side), so this is a short bound on
    // the transport itself — not on any logical blocking, which the caller handles.
    let deadline = crate::timer::uptime_us() + 5_000_000;
    // Announce we're parked awaiting a reply, so a doorbell from the owner (rung right after
    // it publishes) makes our doorbell handler ring a self-scheduler SGI and preempt the idle
    // WFI thread back to us at once (see FWD_AWAITING_REPLY / secondary_doorbell_handler).
    FWD_AWAITING_REPLY.store(true, Ordering::Release);
    let ret = loop {
        if reply.changed(snapshot) {
            let (r, _nr) = reply.read();
            break r;
        }
        if crate::timer::uptime_us() >= deadline {
            break enc_err(110); // ETIMEDOUT — owner never answered
        }
        akuma_exec::threading::yield_now();
    };
    FWD_AWAITING_REPLY.store(false, Ordering::Release);
    fwd_slot_release();
    ret
}

/// Fetch a whole file from the VFS owner into a heap `Vec` via forwarded
/// `openat` -> `read`* -> `close` (the §8.1 outbound data path; "exec is recursive
/// forwarding"). Each `read` returns up to one bounce slot (`FWD_BOUNCE_CAP`) of bytes,
/// copied out of `fwd_bounce[idx]`; the owner advances its fd offset, so successive reads
/// stream the file. Returns `None` on open failure or a read error. Blocking (one forward
/// round-trip per chunk) — the caller is the secondary's idle/boot thread.
// Staging buffers are `[u8; FWD_BOUNCE_CAP]` (64 KiB): the forwarders run on ≥480 KiB kernel
// stacks, so this is safe; see FWD_BOUNCE_CAP for why 64 KiB is the chosen size.
#[allow(clippy::large_stack_arrays)]
fn fetch_file_forwarded(cfg: &MachineConfig, idx: usize, owner: usize, path: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    use crate::syscall::nr;
    // openat: NUL-terminated path in the bounce. `path` is the bytes WITHOUT a NUL.
    let mut p = [0u8; FWD_BOUNCE_CAP];
    let n = path.len().min(FWD_BOUNCE_CAP - 1);
    p[..n].copy_from_slice(&path[..n]);
    // p[n] stays 0 → NUL terminator.
    let h = forward_syscall(cfg, idx, owner, nr::OPENAT, &[0, 0, 0, 0, 0, 0], Some(&p[..=n]));
    if (h as i64) < 0 {
        return None;
    }
    let mut data = alloc::vec::Vec::new();
    let chunk = FWD_BOUNCE_CAP;
    loop {
        let ret = forward_syscall(cfg, idx, owner, nr::READ, &[h, 0, chunk as u64, 0, 0, 0], None);
        let got = ret as i64;
        if got < 0 {
            forward_syscall(cfg, idx, owner, nr::CLOSE, &[h, 0, 0, 0, 0, 0], None);
            return None;
        }
        if got == 0 {
            break; // EOF
        }
        let got = (got as usize).min(chunk);
        let mut buf = [0u8; FWD_BOUNCE_CAP];
        if let Some(bounce) = cfg.fwd_bounce.get(idx) {
            bounce.read(&mut buf);
        }
        data.extend_from_slice(&buf[..got]);
        if got < chunk {
            break; // short read ⇒ EOF
        }
    }
    forward_syscall(cfg, idx, owner, nr::CLOSE, &[h, 0, 0, 0, 0, 0], None);
    Some(data)
}

/// Whether to run the forward-transport self-test on a dedicated secondary after the
/// forward-server comes up. OFF by default: it activates a spare core (see
/// `autostart_bench_core`), so it needs `SMP>=3` to avoid colliding with a herd-pinned
/// service on core 1, and it prints on every boot. Flip to `true` (with `SMP>=3`) to verify
/// the doorbell-wake path in-kernel; the docs/MULTIKERNEL_NETWORKING_EXPERIMENT.md numbers
/// were captured this way. To A/B the doorbell wake, short-circuit the reschedule in
/// `secondary_doorbell_handler` and compare.
const RUN_FWD_BENCH: bool = false;

/// Round-trips the latency self-test times (after a warm-up).
const FWD_BENCH_ITERS: u64 = 40;

/// Upper bound (µs) for the mean forwarded round-trip in the self-test. The doorbell-wake
/// path lands at ~40–50 µs; the old timer-tick-bound path was ~136 000 µs. 5 ms cleanly
/// separates the two, so the test FAILs if the doorbell reschedule ever regresses.
const FWD_LATENCY_MAX_US: u64 = 5_000;

/// Forward-transport self-test: time `FWD_BENCH_ITERS` forwarded round-trips of a trivial
/// owner-serviced syscall (`clock_gettime` — no payload, no EAGAIN retry) in isolation (no
/// network, no disk), and PASS iff the mean is under `FWD_LATENCY_MAX_US`. Runs on its OWN
/// kernel thread so it yields to the (WFI) idle boot thread between round-trips — exactly like
/// a pinned EL0 process's forwarded syscalls — which is the steady-state path the doorbell
/// wake accelerates (and which bringup-time tests can't exercise, as there's no idle thread
/// then). Reports PASS/FAIL like the other SMP self-tests (§R4).
fn run_forward_latency_bench(cfg: &MachineConfig, idx: usize, owner: usize) {
    use crate::syscall::nr;
    let args = [0u64; 6];
    // Warm-up: fault in the owner-side fd table / first-touch caches so timing is steady.
    let _ = forward_syscall(cfg, idx, owner, nr::CLOCK_GETTIME, &args, None);
    let t0 = crate::timer::uptime_us();
    for _ in 0..FWD_BENCH_ITERS {
        let _ = forward_syscall(cfg, idx, owner, nr::CLOCK_GETTIME, &args, None);
    }
    let dt = crate::timer::uptime_us().saturating_sub(t0);
    let per_rt = dt / FWD_BENCH_ITERS.max(1);
    let verdict = if per_rt < FWD_LATENCY_MAX_US { "PASS" } else { "FAIL" };
    crate::safe_print!(
        144,
        "[core {}] fwd self-test: {} round-trips in {} us = {} us/round-trip (< {} us) {}\n",
        idx,
        FWD_BENCH_ITERS,
        dt,
        per_rt,
        FWD_LATENCY_MAX_US,
        verdict
    );
}

/// Bulk-transfer bench: fetch a known owner file over forwarded open/read/close (one
/// `FWD_BOUNCE_CAP` chunk per round-trip) and report size, round-trips, and throughput. This
/// is the signal for the `FWD_BOUNCE_CAP` buffer-size sweep — a larger bounce = fewer
/// round-trips per byte. Runs with the doorbell reschedule at its production default (ON).
fn run_forward_bulk_bench(cfg: &MachineConfig, idx: usize, owner: usize) {
    let path: &[u8] = b"/bin/curl"; // ~1.5 MB static binary staged on the owner's ext2
    let t0 = crate::timer::uptime_us();
    let fetched = fetch_file_forwarded(cfg, idx, owner, path);
    let dt = crate::timer::uptime_us().saturating_sub(t0).max(1);
    let Some(d) = fetched else {
        crate::safe_print!(
            112,
            "[core {}] bulk-bench: forwarded fetch of {} failed\n",
            idx,
            core::str::from_utf8(path).unwrap_or("?")
        );
        return;
    };
    let chunks = d.len().div_ceil(FWD_BOUNCE_CAP);
    let mbps = d.len() as u64 / dt; // bytes/us == MB/s
    crate::safe_print!(
        176,
        "[core {}] bulk-bench: fetched {} bytes of {} in {} us via {} round-trips (CAP={} B) = {} MB/s\n",
        idx,
        d.len(),
        core::str::from_utf8(path).unwrap_or("?"),
        dt,
        chunks,
        FWD_BOUNCE_CAP,
        mbps
    );
}

/// Spawn the forward-latency bench on a dedicated secondary kernel thread (so it yields to
/// the idle boot thread between round-trips). Reads the descriptor + core index from globals
/// inside the thread, mirroring the BSP forward-server's spawn.
fn spawn_forward_bench() {
    let spawned = akuma_exec::threading::spawn_system_thread_fn(|| {
        // SAFETY: descriptor is initialized and mapped (shared) on every online core.
        let cfg = unsafe { &*MACHINE_CONFIG.0.get() };
        let idx = (read_mpidr() & 0xff) as usize;
        run_forward_latency_bench(cfg, idx, 0);
        run_forward_bulk_bench(cfg, idx, 0);
        akuma_exec::threading::mark_current_terminated();
        loop {
            akuma_exec::threading::yield_now();
        }
    });
    if let Err(e) = spawned {
        crate::safe_print!(80, "[SMP] fwd-bench thread spawn FAILED: {}\n", e);
    }
}

/// If the forward-latency bench is enabled, activate the LAST secondary with NO init program
/// so it enters steady state and runs the bench on a core to itself — herd pins its services
/// (sshd/curl) to core 1, so the top core stays idle and gives an uncontended measurement (no
/// sharing of the per-core forward slot / scheduler). Use `SMP>=2`; `SMP=4` keeps it well
/// clear of herd. Called by the BSP after the forward server is up. Idempotent, so it
/// composes with herd. No-op if the bench is off or single-core.
pub fn autostart_bench_core() {
    if !RUN_FWD_BENCH || !PROBED.load(Ordering::Acquire) {
        return;
    }
    let num = NUM_CORES.load(Ordering::Relaxed);
    if num <= 1 {
        return;
    }
    let bench_core = num - 1; // top core; herd only ever pins to core 1
    let r = core_init(bench_core, b"");
    if (r as i64) != 0 {
        crate::safe_print!(80, "[SMP] fwd-bench: core_init({}) returned {}\n", bench_core, r as i64);
    }
}

/// Rump-on-secondary (docs/MULTIKERNEL_NETWORKING_EXPERIMENT.md Stage 1): on the NIC-owning
/// core, register `akuma_net`'s runtime (closures resolve to THIS core's kernel — smoltcp is
/// NOT started here) and bind `rump_tap` to the dedicated NIC on virtio-mmio-bus.5, so a pinned
/// process (rumphttp) drives a LOCAL rump stack over `/dev/net/tap0` instead of forwarding
/// sockets to core 0. Runs once, before the init program is fetched/spawned.
#[cfg(feature = "rump")]
fn secondary_init_local_nic(idx: usize) {
    if idx != RUMP_NIC_CORE {
        return;
    }
    akuma_net::runtime::register(akuma_net::NetRuntime {
        virt_to_phys: secondary_dma_virt_to_phys,
        phys_to_virt: |pa| akuma_exec::mmu::phys_to_virt(pa),
        uptime_us: crate::timer::uptime_us,
        utc_seconds: crate::timer::utc_seconds,
        yield_now: akuma_exec::threading::yield_now,
        current_box_id: || akuma_exec::process::current_process().map_or(0, |p| p.box_id),
        is_current_interrupted: akuma_exec::process::is_current_interrupted,
        rng_fill: secondary_net_rng_stub,
        current_thread_id: || akuma_exec::threading::current_thread_id() as u32,
    });
    // NIC2 is on virtio-mmio-bus.5: DEV_VIRTIO_VA + 5 * 0x200 (the virtio-mmio slot stride).
    let addr = akuma_exec::mmu::DEV_VIRTIO_VA + 5 * 0x200;
    match akuma_net::rump_tap::init_at(addr) {
        Ok(mac) => crate::safe_print!(
            144,
            "[core {}] local NIC bound at bus.5, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} — /dev/net/tap0 is local\n",
            idx, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        ),
        Err(e) => crate::safe_print!(96, "[core {}] local NIC bind FAILED: {} (run QEMU with CORE2_NIC=1)\n", idx, e),
    }
}

/// `virt_to_phys` for the secondary's net DMA. The default identity mapping (`mmu::virt_to_phys`)
/// is WRONG on a secondary for a DMA buffer that lives in a kernel `static`: the replicated
/// `.data`/`.bss` window is mapped at the kernel VA but backed by PRIVATE physical pages (R1),
/// so VA != PA there. `TapNic`'s `rx_buffer` is exactly such a static — with identity `v2p` the
/// device DMA-writes the frame to the wrong physical address and the CPU reads a stale (zero)
/// buffer (TX works only because its buffers come from the identity-mapped partition heap). Walk
/// THIS core's active table for the true PA; partition/identity ranges translate to themselves.
/// The replicated window is physically contiguous (sequential `PartitionBump` pages), so a
/// buffer spanning pages is safe.
#[cfg(feature = "rump")]
fn secondary_dma_virt_to_phys(vaddr: usize) -> usize {
    let off = vaddr & (PAGE - 1);
    let page = vaddr & !(PAGE - 1);
    match akuma_exec::mmu::translate_user_va(current_ttbr0_l0(), page) {
        Some(pa) => (pa & !(PAGE - 1)) | off,
        None => vaddr, // partition RAM is identity-mapped; fall back to identity
    }
}

/// RNG closure for the secondary's net runtime. The local rump-tap path never needs it (rump
/// carries its own entropy; smoltcp isn't run on this core), and RNG MMIO isn't mapped into the
/// secondary's isolated table — so this is a safe no-op stub rather than a faulting call.
#[cfg(feature = "rump")]
fn secondary_net_rng_stub(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
}

/// Spawn this core's INIT PROGRAM as a real EL0 process (docs/MULTIKERNEL.md §10,
/// acceptance/12 Milestone 2). The initiator named it in the `MSG_CORE_INIT` activation
/// message (`cfg.init_program[idx]`); the program is fetched from the VFS owner via
/// forwarded `open`/`read` (this core has no local VFS) and spawned LOCALLY on this core's
/// scheduler — there is no cross-core spawn (§7). Its tty output drains via the console
/// ring (§8.2) and its exit is reaped by this kernel. Runs once, after the BSP
/// forward-server is up; a no-op if no init program was named.
fn spawn_init_program(cfg: &MachineConfig, idx: usize) {
    use crate::syscall::nr;
    let Some(slot) = cfg.init_program.get(idx) else {
        return;
    };
    let mut path_buf = [0u8; akuma_smp::INIT_PROGRAM_CAP];
    let len = slot.get(&mut path_buf);
    if len == 0 || len > path_buf.len() {
        return; // no init program named (or it didn't fit) — nothing to spawn
    }
    // The slot holds the whole command line (program + space-separated args, written by herd
    // through `core_init`). Split it back into argv; the first token is the program path to
    // fetch, the full list is the process's argv (so `curl -sS https://ifconfig.me` works).
    let cmdline = core::str::from_utf8(&path_buf[..len]).unwrap_or("<non-utf8>");
    let argv: alloc::vec::Vec<alloc::string::String> =
        cmdline.split_ascii_whitespace().map(alloc::string::String::from).collect();
    let Some(prog) = argv.first().cloned() else {
        return; // empty command line
    };

    // Seed this core's wall clock from the owner before the process runs, so its TLS cert
    // date checks (and any time-dependent logic) see real time, not 1970 (the RTC is BSP-only).
    seed_realtime_from_owner();

    // Mount this core's local procfs so a spawned process (and any children — e.g. sshd's
    // login shell) can resolve /proc/<pid>/... against THIS core's process table.
    mount_local_procfs();

    // Rump-on-secondary (Stage 1): if this is the NIC-owning core, register the net runtime and
    // bind its dedicated NIC so the pinned process (rumphttp) drives a LOCAL rump stack over
    // /dev/net/tap0 instead of forwarding sockets to core 0. No-op on other cores / non-rump.
    #[cfg(feature = "rump")]
    secondary_init_local_nic(idx);

    // VFS is Proxy'd to the owner on a secondary; fetch the ELF over the forward transport.
    let Some(owner) = capability_owner(idx, nr::OPENAT) else {
        crate::safe_print!(96, "[core {}] init: VFS not forwardable — cannot fetch {}\n", idx, prog);
        return;
    };
    let Some(elf) = fetch_file_forwarded(cfg, idx, owner, prog.as_bytes()) else {
        crate::safe_print!(112, "[core {}] init: failed to fetch {} over forwarded VFS\n", idx, prog);
        return;
    };
    crate::safe_print!(
        128,
        "[core {}] init: fetched {} ({} bytes) via forwarded openat/read/close; spawning EL0 process\n",
        idx,
        prog,
        elf.len()
    );

    // Spawn from the in-memory image via the NORMAL spawn path, with the parsed argv. The
    // per-core kernel-window overlay rides the `prepare_user_address_space` runtime hook (set
    // in run_r3a_coop_test), so the process's syscalls resolve kernel statics to THIS core's
    // private copies.
    match akuma_exec::process::spawn_process_from_image_with_args(&prog, &argv, &elf) {
        Ok((tid, pid)) => crate::safe_print!(
            128,
            "[core {}] init: spawned {} (pid {}, tid {}) — running on this core\n",
            idx,
            prog,
            pid,
            tid
        ),
        Err(e) => crate::safe_print!(176, "[core {}] init: spawn failed for {}: {}\n", idx, prog, e),
    }
}

// ============================================================================
// EL0 syscall forwarding (R4b.5 / §10 Part B): the per-fd + socket forwarders the
// secondary's syscall layer (src/syscall/{fs,net}.rs) calls when a pinned EL0 process
// touches a `Proxy`'d capability. Each wraps `forward_syscall` (ONE transport round-trip)
// with the right marshaling plus, for the would-block socket ops, a blocking retry on
// `-EAGAIN`. All return a Linux-encoded `u64` (handle / count / 0, or `-errno`). The
// caller (the syscall layer) does the user-memory copyin/copyout; these touch only kernel
// buffers and the shared bounce. Owner = core 0 (Phase-0 caps map). No-op (`-ENOSYS`) if
// somehow called off a secondary.
// ============================================================================

/// `-EAGAIN` as the forwarder encodes it (the owner's "would block").
fn is_eagain(r: u64) -> bool {
    r as i64 == -11
}

/// (descriptor, this core index, capability owner) for a forward from a secondary.
fn fwd_ctx() -> Option<(&'static MachineConfig, usize, usize)> {
    if !is_on_secondary() {
        return None;
    }
    // SAFETY: the descriptor is initialized + mapped (shared) on every online core.
    let cfg = unsafe { &*MACHINE_CONFIG.0.get() };
    let idx = (read_mpidr() & 0xff) as usize;
    Some((cfg, idx, 0)) // Phase-0: core 0 owns VFS + Net
}

/// One round-trip with a blocking retry: re-issue on `-EAGAIN` (the owner is non-blocking)
/// until success/other-error, or — if the fd is non-blocking — return `-EAGAIN` at once.
/// Bounded by `timeout_us` so a wedged peer can't hang the caller forever.
fn forward_retry(
    cfg: &MachineConfig, idx: usize, owner: usize, nr: u64, args: &[u64; 6],
    in_buf: Option<&[u8]>, nonblock: bool, timeout_us: u64,
) -> u64 {
    // saturating_add: callers pass u64::MAX for "no timeout" (e.g. a blocking accept), and a
    // plain `+` would overflow → a tiny deadline → spurious ETIMEDOUT on the first EAGAIN.
    let deadline = crate::timer::uptime_us().saturating_add(timeout_us);
    loop {
        let r = forward_syscall(cfg, idx, owner, nr, args, in_buf);
        if !is_eagain(r) {
            return r;
        }
        if nonblock {
            return r; // propagate EAGAIN to EL0
        }
        if crate::timer::uptime_us() >= deadline {
            return enc_err(110); // ETIMEDOUT
        }
        akuma_exec::threading::yield_now();
    }
}

/// Forward `openat` for a non-local path; returns the owner handle (≥0) or `-errno`.
pub fn fwd_openat(path: &str, flags: u32, _mode: u32) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let bytes = path.as_bytes();
    let mut p = [0u8; FWD_PATH_MAX];
    let n = bytes.len().min(FWD_PATH_MAX - 1);
    p[..n].copy_from_slice(&bytes[..n]); // p[n] stays 0 → NUL terminator
    forward_syscall(cfg, idx, owner, nr::OPENAT, &[0, 0, u64::from(flags), 0, 0, 0], Some(&p[..=n]))
}

/// Forward a VFS `read` into `out` (one bounce-sized chunk; short reads are legal). VFS is
/// synchronous on the owner, so no EAGAIN retry. Returns the byte count or `-errno`.
pub fn fwd_read(handle: u32, out: &mut [u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let want = out.len().min(FWD_BOUNCE_CAP) as u64;
    let r = forward_syscall(cfg, idx, owner, nr::READ, &[u64::from(handle), 0, want, 0, 0, 0], None);
    if (r as i64) > 0 && let Some(b) = cfg.fwd_bounce.get(idx) {
        let n = (r as usize).min(out.len());
        b.read(&mut out[..n]);
    }
    r
}

/// Forward a VFS `write` of one bounce-sized chunk; returns the count written or `-errno`.
pub fn fwd_write(handle: u32, data: &[u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let len = data.len().min(FWD_BOUNCE_CAP);
    forward_syscall(cfg, idx, owner, nr::WRITE, &[u64::from(handle), 0, len as u64, 0, 0, 0], Some(&data[..len]))
}

/// Forward `lseek`; returns the new offset or `-errno`.
pub fn fwd_lseek(handle: u32, offset: i64, whence: i32) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    forward_syscall(cfg, idx, owner, nr::LSEEK, &[u64::from(handle), offset as u64, whence as u64, 0, 0, 0], None)
}

/// Forward `fstat`; returns the file SIZE (≥0) or `-errno`. The caller synthesizes the
/// `Stat` (the owner sends only the size — no struct crosses the bounce).
pub fn fwd_fstat_size(handle: u32) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    forward_syscall(cfg, idx, owner, nr::FSTAT, &[u64::from(handle), 0, 0, 0, 0, 0], None)
}

/// Forward `close` of an owner handle; returns 0 or `-errno`.
pub fn fwd_close(handle: u32) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    forward_syscall(cfg, idx, owner, nr::CLOSE, &[u64::from(handle), 0, 0, 0, 0, 0], None)
}

/// Close hook for the runtime: forward a `close` for a RemoteFd still open at process exit.
/// Signature matches `ExecRuntime::remote_fd_close`.
fn fwd_close_remote(_owner: u16, handle: u32, _kind: akuma_exec::process::RemoteKind) {
    let _ = fwd_close(handle);
}

/// Forward `socket(domain, type, proto)`; returns the owner handle or `-errno`.
pub fn fwd_socket(domain: i32, sock_type: i32, proto: i32) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    forward_syscall(cfg, idx, owner, nr::SOCKET, &[domain as u64, sock_type as u64, proto as u64, 0, 0, 0], None)
}

/// Forward `bind(handle, sockaddr)` (16-byte `sockaddr_in`); returns 0 or `-errno`.
pub fn fwd_bind(handle: u32, sockaddr: &[u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let mut a = [0u8; 16];
    let n = sockaddr.len().min(16);
    a[..n].copy_from_slice(&sockaddr[..n]);
    forward_syscall(cfg, idx, owner, nr::BIND, &[u64::from(handle), 0, 0, 0, 0, 0], Some(&a))
}

/// Forward `listen(handle, backlog)`; returns 0 or `-errno`.
pub fn fwd_listen(handle: u32, backlog: i32) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    forward_syscall(cfg, idx, owner, nr::LISTEN, &[u64::from(handle), backlog as u64, 0, 0, 0, 0], None)
}

/// Forward `accept(handle)`; returns a new owner handle or `-errno`, writing the peer's
/// `sockaddr_in` (16 bytes) into `peer_out` if provided. Blocking unless `nonblock`.
#[allow(clippy::large_stack_arrays)] // FWD_BOUNCE_CAP staging buffer; runs on a ≥480 KiB kernel stack
pub fn fwd_accept(handle: u32, nonblock: bool, peer_out: Option<&mut [u8]>) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    // No accept timeout (a listener may idle indefinitely) — pass u64::MAX.
    let r = forward_retry(cfg, idx, owner, nr::ACCEPT, &[u64::from(handle), 0, 0, 0, 0, 0], None, nonblock, u64::MAX);
    if (r as i64) >= 0 && let Some(po) = peer_out && let Some(b) = cfg.fwd_bounce.get(idx) {
        let mut tmp = [0u8; FWD_BOUNCE_CAP];
        b.read(&mut tmp);
        let m = po.len().min(16);
        po[..m].copy_from_slice(&tmp[FWD_SOCK_ADDR_OFF..FWD_SOCK_ADDR_OFF + m]);
    }
    r
}

/// Forward `connect(handle, sockaddr)`; returns 0 or `-errno` (owner blocks, bounded).
pub fn fwd_connect(handle: u32, sockaddr: &[u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let mut a = [0u8; 16];
    let n = sockaddr.len().min(16);
    a[..n].copy_from_slice(&sockaddr[..n]);
    forward_syscall(cfg, idx, owner, nr::CONNECT, &[u64::from(handle), 0, 0, 0, 0, 0], Some(&a))
}

/// Forward `sendto(handle, data[, dest])` (one bounce-sized chunk). Returns the count or
/// `-errno`. Blocking unless `nonblock`.
#[allow(clippy::large_stack_arrays)] // FWD_BOUNCE_CAP staging buffer; runs on a ≥480 KiB kernel stack
pub fn fwd_sendto(handle: u32, data: &[u8], nonblock: bool, dest: Option<&[u8]>) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let len = data.len().min(FWD_SOCK_DATA);
    let mut buf = [0u8; FWD_BOUNCE_CAP];
    buf[..len].copy_from_slice(&data[..len]);
    let has_dest = match dest {
        Some(d) => {
            let m = d.len().min(16);
            buf[FWD_SOCK_ADDR_OFF..FWD_SOCK_ADDR_OFF + m].copy_from_slice(&d[..m]);
            1
        }
        None => 0,
    };
    forward_retry(cfg, idx, owner, nr::SENDTO, &[u64::from(handle), len as u64, has_dest, 0, 0, 0], Some(&buf), nonblock, 10_000_000)
}

/// Forward `recvfrom(handle, out[, peer])` (one bounce-sized chunk). Returns the count or
/// `-errno`, writing the source `sockaddr_in` into `peer_out` if provided. Blocking unless
/// `nonblock`.
#[allow(clippy::large_stack_arrays)] // FWD_BOUNCE_CAP staging buffer; runs on a ≥480 KiB kernel stack
pub fn fwd_recvfrom(handle: u32, out: &mut [u8], nonblock: bool, peer_out: Option<&mut [u8]>) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let want = out.len().min(FWD_SOCK_DATA) as u64;
    let r = forward_retry(cfg, idx, owner, nr::RECVFROM, &[u64::from(handle), want, 0, 0, 0, 0], None, nonblock, 30_000_000);
    if (r as i64) >= 0 && let Some(b) = cfg.fwd_bounce.get(idx) {
        let mut tmp = [0u8; FWD_BOUNCE_CAP];
        b.read(&mut tmp);
        let cnt = (r as usize).min(out.len()).min(FWD_SOCK_DATA);
        out[..cnt].copy_from_slice(&tmp[..cnt]);
        if let Some(po) = peer_out {
            let m = po.len().min(16);
            po[..m].copy_from_slice(&tmp[FWD_SOCK_ADDR_OFF..FWD_SOCK_ADDR_OFF + m]);
        }
    }
    r
}

/// Forward `setsockopt(handle, level, optname, optval)`; returns 0 or `-errno`.
pub fn fwd_setsockopt(handle: u32, level: i32, optname: i32, optval: &[u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let mut a = [0u8; 16];
    let n = optval.len().min(16);
    a[..n].copy_from_slice(&optval[..n]);
    forward_syscall(cfg, idx, owner, nr::SETSOCKOPT, &[u64::from(handle), level as u64, optname as u64, n as u64, 0, 0], Some(&a))
}

/// Forward `getsockopt(handle, level, optname)`; writes the option value into `optval_out`
/// and returns 0 or `-errno`.
#[allow(clippy::large_stack_arrays)] // FWD_BOUNCE_CAP staging buffer; runs on a ≥480 KiB kernel stack
pub fn fwd_getsockopt(handle: u32, level: i32, optname: i32, optval_out: &mut [u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let r = forward_syscall(cfg, idx, owner, nr::GETSOCKOPT, &[u64::from(handle), level as u64, optname as u64, 0, 0, 0], None);
    if (r as i64) >= 0 && let Some(b) = cfg.fwd_bounce.get(idx) {
        let mut tmp = [0u8; FWD_BOUNCE_CAP];
        b.read(&mut tmp);
        let n = (r as usize).min(optval_out.len());
        optval_out[..n].copy_from_slice(&tmp[..n]);
    }
    r
}

/// Secondary `ExecRuntime::read_file` hook: fetch a whole file by forwarding open/read/close
/// to the VFS owner (R4b.5 Phase 2 — exec on a secondary, e.g. sshd spawning `/bin/sh`, or a
/// shell spawning `curl`). Installed only on a secondary, so it always forwards.
pub fn secondary_forward_read_file(path: &str) -> Result<alloc::vec::Vec<u8>, i32> {
    let Some((cfg, idx, owner)) = fwd_ctx() else { return Err(-38) };
    fetch_file_forwarded(cfg, idx, owner, path.as_bytes()).ok_or(-5)
}

/// Secondary `ExecRuntime::read_at` hook: forwarded positional read (fresh handle = pread
/// semantics). A fallback — `prefer_whole_file_load` routes exec through `read_file` — kept
/// correct for the interp/large-file paths.
fn secondary_forward_read_at(path: &str, off: usize, buf: &mut [u8]) -> Result<usize, i32> {
    let h = fwd_openat(path, 0, 0);
    if (h as i64) < 0 {
        return Err(h as i32);
    }
    let handle = h as u32;
    if off != 0 {
        let _ = fwd_lseek(handle, off as i64, 0);
    }
    let mut total = 0usize;
    while total < buf.len() {
        let n = fwd_read(handle, &mut buf[total..]);
        if (n as i64) < 0 {
            let _ = fwd_close(handle);
            return Err(n as i32);
        }
        if n == 0 {
            break;
        }
        total += n as usize;
    }
    let _ = fwd_close(handle);
    Ok(total)
}

/// Secondary `ExecRuntime::file_size` hook: forwarded `fstat` size.
fn secondary_forward_file_size(path: &str) -> Result<u64, &'static str> {
    let h = fwd_openat(path, 0, 0);
    if (h as i64) < 0 {
        return Err("openat failed");
    }
    let handle = h as u32;
    let sz = fwd_fstat_size(handle);
    let _ = fwd_close(handle);
    if (sz as i64) < 0 {
        Err("fstat failed")
    } else {
        Ok(sz)
    }
}

/// Mount this core's OWN local procfs (R4b.5 Phase 2). The VFS mount table is a replicated
/// static (empty on a secondary); procfs is stateless and resolves `/proc/<pid>/...` against
/// THIS core's process table — exactly what an interactive shell needs (sshd's bridge writes
/// the login shell's stdin via `/proc/<pid>/fd/0`, a core-local pid). ext2 is deliberately NOT
/// mounted — real-file paths forward to the owner (the openat hook skips `/proc` and `/dev`).
fn mount_local_procfs() {
    crate::vfs::init();
    let proc_fs = alloc::sync::Arc::new(crate::vfs::proc::ProcFilesystem::new());
    if let Err(e) = crate::vfs::mount("/proc", proc_fs) {
        crate::safe_print!(64, "[core ?] local procfs mount failed: {:?}\n", e);
        return;
    }
    // Enable the `crate::fs::*` wrappers (they gate on this flag) so the local `/proc`
    // resolves — the sshd shell bridge opens `/proc/<pid>/fd/0`. ext2 is not mounted here;
    // real-file paths forward to the owner before ever hitting these wrappers.
    crate::fs::mark_initialized();
}

/// Seed THIS core's wall clock from the owner ONCE (the RTC is a BSP-only device). Forwards a
/// single `clock_gettime` for the owner's Unix epoch and sets the local UTC offset, so the
/// pinned process's (high-frequency) `clock_gettime` is served locally and TLS cert date
/// checks see real time instead of 1970. No-op off a secondary or if the owner has no clock.
pub fn seed_realtime_from_owner() {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return };
    let epoch_us = forward_syscall(cfg, idx, owner, nr::CLOCK_GETTIME, &[0, 0, 0, 0, 0, 0], None);
    if epoch_us > 0 && (epoch_us as i64) > 0 {
        crate::timer::set_utc_time_us(epoch_us);
    }
}

/// Forward `getrandom` into `out` (one bounce-sized chunk) — entropy comes from the owner's
/// virtio-rng (a BSP-owned device). Returns the byte count or `-errno`.
pub fn fwd_getrandom(out: &mut [u8]) -> u64 {
    use crate::syscall::nr;
    let Some((cfg, idx, owner)) = fwd_ctx() else { return enc_err(38) };
    let want = out.len().min(FWD_BOUNCE_CAP);
    let r = forward_syscall(cfg, idx, owner, nr::GETRANDOM, &[want as u64, 0, 0, 0, 0, 0], None);
    if (r as i64) > 0 && let Some(b) = cfg.fwd_bounce.get(idx) {
        let n = (r as usize).min(out.len());
        b.read(&mut out[..n]);
    }
    r
}

// `shutdown` is a global no-op (see `sys_shutdown`), so a remote socket needs no forward —
// it behaves identically to a local one without a wasted round-trip. The owner-side socket
// is torn down by the subsequent `close`.

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

    // 3. Rump-on-secondary: the NIC-owning core drives its NIC's virtio-mmio registers from a
    //    syscall that runs at EL1 UNDER this user table (Akuma is TTBR0-only, no table switch on
    //    trap), so map that one device page here too. The shared device window that gives the
    //    BSP its 0x80_0000_0000+ device mappings is BSP-only (DEVICE_L1_PA is unset on a
    //    secondary), hence the explicit per-core mapping. Scoped to one page and one core.
    #[cfg(feature = "rump")]
    if (read_mpidr() & 0xff) as usize == RUMP_NIC_CORE {
        const VIRTIO_MMIO_PHYS: usize = 0x0A00_0000;
        uas.map_device_page(akuma_exec::mmu::DEV_VIRTIO_VA, VIRTIO_MMIO_PHYS)?;
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

/// BSP side of forward serving: drain its inbox and service forward requests on behalf of
/// compute cores that don't own a capability (§8.1/§10.1). Two kinds — `MSG_FWD_ECHO_REQ`
/// (the R4a transport probe, byte + 1 transform) and `MSG_FWD_SYSCALL_REQ` (a GENERIC
/// forwarded syscall: read `fwd_call[from]` nr + args and `fwd_bounce[from]` pointer-arg
/// bytes, run the REAL syscall on the owner's own resources, copy any outbound bytes back,
/// reply with the return value — one path for open/read/write/sockets, so `curl` rides the
/// same dispatch with more nr arms). Called both from the bringup online-wait loop (ECHO
/// only — real targets aren't up yet) and, post-bringup, from the persistent forward-server
/// thread (where the VFS/net live).
fn service_fwd_requests(cfg: &MachineConfig, bsp_idx: usize) -> usize {
    let Some(bsp_inbox) = cfg.inboxes.get(bsp_idx) else {
        return 0;
    };
    let mut served = 0usize;
    while let Some(m) = bsp_inbox.pop() {
        let from = m.from as usize;
        match m.kind {
            MSG_FWD_ECHO_REQ => {
                let len = m.v0 as usize;
                if let Some(bounce) = cfg.fwd_bounce.get(from) {
                    bounce.map_in_place(len, |b| b.wrapping_add(1));
                }
                if let Some(reply) = cfg.inboxes.get(from) {
                    reply.push(MSG_FWD_ECHO_REPLY, bsp_idx as u32, m.v0, m.v1);
                }
                served += 1;
            }
            MSG_FWD_SYSCALL_REQ => {
                let (nr, ret) = service_forwarded_syscall(cfg, from);
                // Reply on the requester's DEDICATED mailbox (not its inbox), so its idle
                // loop's inbox drain can't swallow the reply (§8.1). publish() bumps the
                // sequence last, ordering the bounce bytes we wrote before the waiter reads.
                if let Some(reply) = cfg.fwd_reply.get(from) {
                    reply.publish(ret, nr);
                }
                // Wake the requester NOW: ring its doorbell so its scheduler preempts the idle
                // (WFI) thread back to the parked forwarder immediately, instead of on that
                // core's next timer tick. publish() above ordered the bounce+seq before this,
                // so by the time the waiter is scheduled it observes the completed reply. The
                // requester's doorbell handler does the actual (voluntary) reschedule.
                crate::gic::trigger_sgi_core(from as u32, DOORBELL_SGI);
                served += 1;
            }
            _ => {} // debt-protocol traffic is handled elsewhere
        }
    }
    served
}

/// Encode a negative result as the Linux ABI does, so the forwarding core sees `-errno`.
fn enc_err(errno: i64) -> u64 {
    (-errno) as u64
}

/// Owner-side (BSP) forwarded fd — the real file/socket a `Proxy`'d capability stands up on
/// the owner core (the fd-affinity invariant, docs §16); the forwarding core holds only this
/// slot's index as an opaque handle. A forwarded `openat` records a `File` (path + offset —
/// `crate::fs` is path+offset, so no kernel fd object is held); a forwarded `socket` records
/// a `Socket` holding the real smoltcp socket index.
// The `File` variant embeds a fixed path buffer (paths are short and this lives in a small
// fixed `static` array, so a boxed indirection would add a heap alloc per forwarded open for
// no real benefit).
#[allow(clippy::large_enum_variant)]
enum ForwardedFd {
    File { path: [u8; FWD_PATH_MAX], path_len: usize, offset: usize },
    Socket { idx: usize },
}
/// Stored owner-side path cap (independent of the bounce: paths are short, the bounce is now
/// 4 KiB and would bloat this static).
const FWD_PATH_MAX: usize = 256;
const FWD_FD_MAX: usize = 64;
static FORWARDED_FDS: Spinlock<[Option<ForwardedFd>; FWD_FD_MAX]> =
    Spinlock::new([const { None }; FWD_FD_MAX]);

/// Socket recv/send data region in the bounce: the trailing 16 bytes are reserved for a
/// `sockaddr_in` (recvfrom's source / sendto's dest) so a single round-trip carries both
/// the payload and the address (§8.1 — the bounce is the sole shared byte buffer).
const FWD_SOCK_DATA: usize = FWD_BOUNCE_CAP - 16;
/// Byte offset of the reserved `sockaddr_in` tail.
const FWD_SOCK_ADDR_OFF: usize = FWD_SOCK_DATA;

/// Reserve an owner-side fd slot for `fd`, returning its handle (slot index) or `-EMFILE`.
fn fwd_fd_alloc(fd: ForwardedFd) -> u64 {
    let mut fds = FORWARDED_FDS.lock();
    match fds.iter().position(Option::is_none) {
        Some(h) => {
            fds[h] = Some(fd);
            h as u64
        }
        None => enc_err(24), // EMFILE
    }
}

/// Look up the real socket index behind an owner-side handle, if it is a `Socket`.
fn fwd_fd_socket(h: usize) -> Option<usize> {
    let fds = FORWARDED_FDS.lock();
    match fds.get(h).and_then(Option::as_ref) {
        Some(ForwardedFd::Socket { idx }) => Some(*idx),
        _ => None,
    }
}

/// Read a `sockaddr_in` (16 bytes) from a bounce region into a `SockAddrIn`.
fn read_sockaddr(buf16: &[u8]) -> akuma_net::socket::SockAddrIn {
    let mut sa = akuma_net::socket::SockAddrIn::default();
    // SAFETY: SockAddrIn is repr(C), 16 bytes, all-bytes-valid (no padding invariants).
    let dst = unsafe {
        core::slice::from_raw_parts_mut((&raw mut sa).cast::<u8>(), core::mem::size_of::<akuma_net::socket::SockAddrIn>())
    };
    let n = dst.len().min(buf16.len());
    dst[..n].copy_from_slice(&buf16[..n]);
    sa
}

/// Serialize a `SocketAddrV4` peer into 16 `sockaddr_in` bytes.
fn write_sockaddr(addr: &akuma_net::socket::SocketAddrV4) -> [u8; 16] {
    let sa = akuma_net::socket::SockAddrIn::from_addr(addr);
    let mut out = [0u8; 16];
    // SAFETY: as above — repr(C) 16-byte POD.
    let src = unsafe {
        core::slice::from_raw_parts((&raw const sa).cast::<u8>(), 16)
    };
    out.copy_from_slice(src);
    out
}

/// Generic owner-side syscall dispatch (§8.1/§10.1): read `fwd_call[from]` (nr + args) and
/// `fwd_bounce[from]` (any pointer buffer), run the REAL operation on the owner's own VFS /
/// network stack, copy outbound bytes back to the bounce, and return `(nr, ret)`. One path
/// for files (openat/read/write/lseek/fstat/close) and sockets (socket/bind/listen/accept/
/// connect/sendto/recvfrom/setsockopt/getsockopt/shutdown) — no per-syscall message type, so
/// `curl` rides the same dispatch as the exec-fetch. Socket ops are NON-BLOCKING here (the
/// secondary re-issues on `-EAGAIN`) so the single forward-server thread never stalls; only
/// `connect` blocks (bounded, one-shot). Unknown syscalls return `-ENOSYS`.
#[allow(clippy::large_stack_arrays)] // FWD_BOUNCE_CAP staging buffers; runs on the fwd-server's ≥480 KiB kernel stack
fn service_forwarded_syscall(cfg: &MachineConfig, from: usize) -> (u64, u64) {
    use crate::syscall::nr;
    use akuma_net::socket;
    let (Some(call), Some(bounce)) = (cfg.fwd_call.get(from), cfg.fwd_bounce.get(from)) else {
        return (0, enc_err(38)); // ENOSYS — no slot for this core
    };
    let (sysno, args) = call.get();
    let to_enc = |r: Result<usize, i32>| match r {
        Ok(n) => n as u64,
        Err(e) => enc_err(i64::from(e)),
    };
    let ret = match sysno {
        // ── VFS ────────────────────────────────────────────────────────────────────────
        // openat(dirfd, path, flags, mode): NUL-terminated path in the bounce; flags in
        // args[2]. Reads (curl/sshd config, CA bundle, exec ELFs) are the common case;
        // O_CREAT/O_TRUNC are honored too so sshd can generate its host key on the owner's
        // ext2 (the subsequent forwarded `write`s go to the same File handle).
        nr::OPENAT => {
            let flags = args[2] as u32;
            const O_CREAT: u32 = 0o100;
            const O_TRUNC: u32 = 0o1000;
            let mut raw = [0u8; FWD_PATH_MAX];
            bounce.read(&mut raw);
            let plen = raw.iter().position(|&b| b == 0).unwrap_or(FWD_PATH_MAX);
            match core::str::from_utf8(&raw[..plen]) {
                Ok(path) => {
                    let exists = crate::fs::file_size(path).is_ok();
                    if !exists && (flags & O_CREAT) != 0 {
                        if crate::fs::write_file(path, &[]).is_err() {
                            return (sysno, enc_err(5)); // EIO — couldn't create
                        }
                    } else if !exists {
                        return (sysno, enc_err(2)); // ENOENT
                    } else if (flags & O_TRUNC) != 0 {
                        let _ = crate::fs::write_file(path, &[]);
                    }
                    let mut fd = ForwardedFd::File { path: [0; FWD_PATH_MAX], path_len: plen, offset: 0 };
                    if let ForwardedFd::File { path: p, .. } = &mut fd {
                        p[..plen].copy_from_slice(&raw[..plen]);
                    }
                    fwd_fd_alloc(fd)
                }
                Err(_) => enc_err(22), // EINVAL (bad path bytes)
            }
        }
        // read(handle, _, len): File → VFS read at offset; Socket → recv (read() on a socket).
        nr::READ => {
            let h = args[0] as usize;
            let want = (args[2] as usize).min(FWD_BOUNCE_CAP);
            // Snapshot path+offset under the lock, read WITHOUT holding it (the VFS may yield).
            let snap = {
                let fds = FORWARDED_FDS.lock();
                match fds.get(h).and_then(Option::as_ref) {
                    Some(ForwardedFd::File { path, path_len, offset }) => {
                        let mut p = [0u8; FWD_PATH_MAX];
                        p[..*path_len].copy_from_slice(&path[..*path_len]);
                        Some((p, *path_len, *offset))
                    }
                    _ => None,
                }
            };
            match snap {
                Some((p, plen, off)) => {
                    let path = core::str::from_utf8(&p[..plen]).unwrap_or("");
                    let mut buf = [0u8; FWD_BOUNCE_CAP];
                    match crate::fs::read_at(path, off, &mut buf[..want]) {
                        Ok(n) => {
                            bounce.write(&buf[..n]);
                            if let Some(ForwardedFd::File { offset, .. }) =
                                FORWARDED_FDS.lock().get_mut(h).and_then(Option::as_mut)
                            {
                                *offset += n;
                            }
                            n as u64
                        }
                        Err(_) => enc_err(5), // EIO
                    }
                }
                // Not a file handle → maybe a socket opened with read(); recv it.
                None => match fwd_fd_socket(h) {
                    Some(idx) => {
                        let mut buf = [0u8; FWD_BOUNCE_CAP];
                        let r = socket::socket_recv(idx, &mut buf[..want.min(FWD_SOCK_DATA)], true);
                        if let Ok(n) = r { bounce.write(&buf[..n]); }
                        to_enc(r)
                    }
                    None => enc_err(9), // EBADF
                },
            }
        }
        // write(handle, _, len): File → VFS write at offset; Socket → send.
        nr::WRITE => {
            let h = args[0] as usize;
            let len = (args[2] as usize).min(FWD_BOUNCE_CAP);
            let mut buf = [0u8; FWD_BOUNCE_CAP];
            bounce.read(&mut buf);
            let snap = {
                let fds = FORWARDED_FDS.lock();
                match fds.get(h).and_then(Option::as_ref) {
                    Some(ForwardedFd::File { path, path_len, offset }) => {
                        let mut p = [0u8; FWD_PATH_MAX];
                        p[..*path_len].copy_from_slice(&path[..*path_len]);
                        Some((p, *path_len, *offset))
                    }
                    _ => None,
                }
            };
            match snap {
                Some((p, plen, off)) => {
                    let path = core::str::from_utf8(&p[..plen]).unwrap_or("");
                    match crate::fs::write_at(path, off, &buf[..len]) {
                        Ok(n) => {
                            if let Some(ForwardedFd::File { offset, .. }) =
                                FORWARDED_FDS.lock().get_mut(h).and_then(Option::as_mut)
                            {
                                *offset += n;
                            }
                            n as u64
                        }
                        Err(_) => enc_err(5), // EIO
                    }
                }
                None => match fwd_fd_socket(h) {
                    Some(idx) => to_enc(socket::socket_send(idx, &buf[..len.min(FWD_SOCK_DATA)], true)),
                    None => enc_err(9), // EBADF
                },
            }
        }
        // lseek(handle, off, whence): File only. whence: 0=SET, 1=CUR, 2=END.
        nr::LSEEK => {
            let h = args[0] as usize;
            let off = args[1] as i64;
            let whence = args[2] as i32;
            let mut fds = FORWARDED_FDS.lock();
            match fds.get_mut(h).and_then(Option::as_mut) {
                Some(ForwardedFd::File { path, path_len, offset }) => {
                    let path_str = core::str::from_utf8(&path[..*path_len]).unwrap_or("");
                    let base = match whence {
                        0 => 0i64,
                        1 => *offset as i64,
                        2 => crate::fs::file_size(path_str).map(|s| s as i64).unwrap_or(0),
                        _ => return (sysno, enc_err(22)), // EINVAL
                    };
                    let np = base + off;
                    if np < 0 { enc_err(22) } else { *offset = np as usize; np as u64 }
                }
                _ => enc_err(9), // EBADF
            }
        }
        // fstat(handle): File only — return the file SIZE as the result; the secondary
        // synthesizes the `Stat` (regular file) locally, so no struct crosses the bounce.
        nr::FSTAT => {
            let h = args[0] as usize;
            let fds = FORWARDED_FDS.lock();
            match fds.get(h).and_then(Option::as_ref) {
                Some(ForwardedFd::File { path, path_len, .. }) => {
                    let p = core::str::from_utf8(&path[..*path_len]).unwrap_or("");
                    match crate::fs::file_size(p) {
                        Ok(sz) => sz,
                        Err(_) => enc_err(2), // ENOENT
                    }
                }
                _ => enc_err(9), // EBADF (socket fstat handled locally on the secondary)
            }
        }
        // close(handle): release the owner-side slot (and the real socket, if any).
        nr::CLOSE => {
            let h = args[0] as usize;
            let removed = { FORWARDED_FDS.lock().get_mut(h).and_then(Option::take) };
            match removed {
                Some(ForwardedFd::Socket { idx }) => { socket::remove_socket(idx); 0 }
                Some(ForwardedFd::File { .. }) => 0,
                None => enc_err(9), // EBADF
            }
        }
        // ── Net ────────────────────────────────────────────────────────────────────────
        // socket(domain, type, proto): create a real socket on the owner; return its handle.
        nr::SOCKET => {
            let base_type = (args[1] as i32) & 0xFF;
            match socket::alloc_socket(base_type) {
                Some(idx) => fwd_fd_alloc(ForwardedFd::Socket { idx }),
                None => enc_err(24), // EMFILE
            }
        }
        // bind(handle, sockaddr): 16-byte sockaddr_in in the bounce.
        nr::BIND => {
            let h = args[0] as usize;
            let mut raw = [0u8; 16];
            bounce.read(&mut raw);
            match fwd_fd_socket(h) {
                Some(idx) => match socket::socket_bind(idx, read_sockaddr(&raw).to_addr()) {
                    Ok(()) => 0,
                    Err(e) => enc_err(i64::from(e)),
                },
                None => enc_err(9),
            }
        }
        // listen(handle, backlog).
        nr::LISTEN => {
            let h = args[0] as usize;
            match fwd_fd_socket(h) {
                Some(idx) => match socket::socket_listen(idx, args[1] as usize) {
                    Ok(()) => 0,
                    Err(e) => enc_err(i64::from(e)),
                },
                None => enc_err(9),
            }
        }
        // accept(handle): non-blocking; peer sockaddr → bounce tail; new socket → new handle.
        nr::ACCEPT => {
            let h = args[0] as usize;
            match fwd_fd_socket(h) {
                Some(idx) => match socket::socket_accept(idx, true) {
                    Ok((new_idx, peer)) => {
                        let mut tail = [0u8; FWD_BOUNCE_CAP];
                        bounce.read(&mut tail);
                        tail[FWD_SOCK_ADDR_OFF..].copy_from_slice(&write_sockaddr(&peer));
                        bounce.write(&tail);
                        fwd_fd_alloc(ForwardedFd::Socket { idx: new_idx })
                    }
                    Err(e) => enc_err(i64::from(e)),
                },
                None => enc_err(9),
            }
        }
        // connect(handle, sockaddr): BLOCKING (bounded) — one-shot, so a brief fwd-server
        // stall is acceptable; keeps the secondary side simple (no EINPROGRESS dance).
        nr::CONNECT => {
            let h = args[0] as usize;
            let mut raw = [0u8; 16];
            bounce.read(&mut raw);
            match fwd_fd_socket(h) {
                Some(idx) => match socket::socket_connect(idx, read_sockaddr(&raw).to_addr(), false) {
                    Ok(()) => 0,
                    Err(e) => enc_err(i64::from(e)),
                },
                None => enc_err(9),
            }
        }
        // sendto(handle, len, has_dest): data in bounce[0..len]; dest sockaddr in tail.
        nr::SENDTO => {
            let h = args[0] as usize;
            let len = (args[1] as usize).min(FWD_SOCK_DATA);
            let has_dest = args[2] != 0;
            let mut buf = [0u8; FWD_BOUNCE_CAP];
            bounce.read(&mut buf);
            match fwd_fd_socket(h) {
                Some(idx) => {
                    if socket::is_udp_socket(idx) {
                        let dest = if has_dest {
                            read_sockaddr(&buf[FWD_SOCK_ADDR_OFF..]).to_addr()
                        } else if let Some(p) = socket::udp_default_peer(idx) {
                            p
                        } else {
                            return (sysno, enc_err(89)); // EDESTADDRREQ
                        };
                        to_enc(socket::socket_send_udp(idx, &buf[..len], dest))
                    } else {
                        to_enc(socket::socket_send(idx, &buf[..len], true))
                    }
                }
                None => enc_err(9),
            }
        }
        // recvfrom(handle, want): non-blocking; data → bounce[0..n]; source sockaddr → tail.
        nr::RECVFROM => {
            let h = args[0] as usize;
            let want = (args[1] as usize).min(FWD_SOCK_DATA);
            match fwd_fd_socket(h) {
                Some(idx) => {
                    let mut buf = [0u8; FWD_BOUNCE_CAP];
                    if socket::is_udp_socket(idx) {
                        match socket::socket_recv_udp(idx, &mut buf[..want], true) {
                            Ok((n, src)) => {
                                buf[FWD_SOCK_ADDR_OFF..].copy_from_slice(&write_sockaddr(&src));
                                bounce.write(&buf);
                                n as u64
                            }
                            Err(e) => enc_err(i64::from(e)),
                        }
                    } else {
                        let r = socket::socket_recv(idx, &mut buf[..want], true);
                        if let Ok(n) = r { bounce.write(&buf[..n]); }
                        to_enc(r)
                    }
                }
                None => enc_err(9),
            }
        }
        // setsockopt(handle, level, optname, optlen): apply the few opts we model; succeed
        // (return 0) for the rest so curl/sshd aren't tripped up by a harmless tunable.
        nr::SETSOCKOPT => {
            let h = args[0] as usize;
            let level = args[1] as i32;
            let optname = args[2] as i32;
            let mut raw = [0u8; 16];
            bounce.read(&mut raw);
            let enabled = raw.first().is_some_and(|&b| b != 0);
            match fwd_fd_socket(h) {
                Some(idx) => {
                    // SOL_TCP(6)/TCP_NODELAY(1); SOL_SOCKET(1)/SO_KEEPALIVE(9).
                    if level == 6 && optname == 1 { socket::set_tcp_nodelay(idx, enabled); }
                    else if level == 1 && optname == 9 { socket::set_socket_keepalive(idx, enabled); }
                    0
                }
                None => enc_err(9),
            }
        }
        // getsockopt(handle, level, optname): we model only SO_ERROR (→ 0, no pending error,
        // since connect is synchronous here); return a 4-byte zero in the bounce.
        nr::GETSOCKOPT => {
            let h = args[0] as usize;
            match fwd_fd_socket(h) {
                Some(_) => { bounce.write(&[0u8; 4]); 4 }
                None => enc_err(9),
            }
        }
        // ── Entropy ────────────────────────────────────────────────────────────────────
        // getrandom(want): the hardware RNG (virtio-rng) is a BSP-owned device, unmapped on a
        // secondary, so a pinned process's entropy reads (`/dev/urandom`, `getrandom`) forward
        // here. Fill the bounce with `want` random bytes from the owner's RNG.
        nr::GETRANDOM => {
            let want = (args[0] as usize).min(FWD_BOUNCE_CAP);
            let mut buf = [0u8; FWD_BOUNCE_CAP];
            match crate::rng::fill_bytes(&mut buf[..want]) {
                Ok(()) => {
                    bounce.write(&buf[..want]);
                    want as u64
                }
                Err(_) => enc_err(5), // EIO
            }
        }
        // ── Wall clock ─────────────────────────────────────────────────────────────────
        // clock_gettime(CLOCK_REALTIME): the RTC (PL031) is BSP-only, so a secondary seeds
        // its wall clock ONCE from here (then serves clock_gettime locally). Returns the
        // owner's current Unix epoch in MICROSECONDS (0 if the owner hasn't set its clock).
        nr::CLOCK_GETTIME => crate::timer::utc_time_us().unwrap_or(0),
        _ => enc_err(38), // ENOSYS — arm not implemented
    };
    (sysno, ret)
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

/// BSP-served core activation — the kernel side of the `core_init` syscall
/// (docs/MULTIKERNEL.md R4b lifecycle). An init system (herd) calls this from userspace
/// to bring a parked secondary online. Steps: validate `idx` is a real secondary; if it
/// had self-`CPU_OFF`'d (Offline) re-`CPU_ON` it to re-arm its park window; then push
/// `MSG_CORE_INIT` to its inbox and ring its doorbell so the parked core wakes, sees the
/// message, and stands up its scheduler/role. Targeted (one core) and idempotent at the
/// receiver — a core that is already Online logs + drops a late/duplicate. Returns 0 on
/// success, or a negative errno (Linux-encoded): `-ENOSYS` if SMP isn't up, `-ENODEV` for
/// a bad/non-secondary index, `-EPERM` if this core isn't the descriptor's `initiator`.
pub fn core_init(idx: usize, init_program: &[u8]) -> u64 {
    if !PROBED.load(Ordering::Acquire) {
        return enc_err(38); // ENOSYS — single-core / no SMP
    }
    let num = NUM_CORES.load(Ordering::Relaxed);
    let bsp = (read_mpidr() & 0xff) as usize;
    if idx == 0 || idx >= num || idx == bsp {
        return enc_err(19); // ENODEV — not a real secondary
    }
    // SAFETY: the descriptor is initialized once at boot and identity-mapped on the BSP;
    // here we only touch atomics (`state`) and the lock-free inbox, both shared-safe.
    let cfg = unsafe { &*MACHINE_CONFIG.0.get() };
    // Only the current initiator (BSP today, an elected leader later) drives PSCI and
    // sends MSG_CORE_INIT (§12). Re-pointable via `cfg.initiator`.
    if bsp as u32 != cfg.initiator.load(Ordering::Relaxed) {
        return enc_err(1); // EPERM — not the initiator
    }

    // Already online ⇒ a late/duplicate activation; log + drop (idempotent at the
    // initiator, mirroring the receiver-side guard).
    if cfg.cores[idx].state.load(Ordering::Acquire) == STATE_ONLINE {
        crate::safe_print!(72, "[SMP] core_init({}): already online — ignored (duplicate)\n", idx);
        return 0;
    }

    // Publish the init-program path (the program this core should run as its first
    // process) BEFORE pushing MSG_CORE_INIT, so the ring push/pop orders it for the
    // secondary. Empty ⇒ the core comes online with no init process. Never a cross-core
    // spawn — the secondary spawns it LOCALLY (docs/MULTIKERNEL.md §7/§10, acceptance/12).
    if let Some(slot) = cfg.init_program.get(idx) {
        slot.set(init_program);
    }

    // Re-CPU_ON a core that shut itself down (the watchdog expired earlier). It re-runs
    // secondary bringup → soundness checks → parks again, and the MSG_CORE_INIT we push
    // below is waiting in its inbox by the time it gets there.
    if cfg.cores[idx].state.load(Ordering::Acquire) == STATE_OFFLINE {
        let target = cfg.cores[idx].mpidr;
        let entry_pa = secondary_entry as *const () as usize as u64;
        let cfg_pa = core::ptr::from_ref::<MachineConfig>(cfg) as u64;
        cfg.cores[idx].state.store(STATE_BOOTING, Ordering::Release);
        dsb_sy();
        let _ = psci_call(USE_HVC.load(Ordering::Relaxed), PSCI_CPU_ON, target, entry_pa, cfg_pa);
    }

    // Push the activation message + ring the doorbell to wake the parked core.
    if let Some(inbox) = cfg.inboxes.get(idx)
        && !inbox.push(MSG_CORE_INIT, bsp as u32, 0, 0)
    {
        return enc_err(11); // EAGAIN — inbox full
    }
    crate::gic::trigger_sgi_core(idx as u32, DOORBELL_SGI);
    if init_program.is_empty() {
        crate::safe_print!(64, "[SMP] core_init({}): activating (MSG_CORE_INIT sent)\n", idx);
    } else {
        crate::safe_print!(
            128,
            "[SMP] core_init({}): activating (MSG_CORE_INIT sent), init program: {}\n",
            idx,
            core::str::from_utf8(init_program).unwrap_or("<non-utf8>")
        );
    }
    0
}

// --- /proc/cores accessors (heap-free; the formatting + the unavoidable Vec<u8>
// allocation live in src/vfs/proc.rs, where the FS trait already requires them). These
// let an init system (herd) enumerate cores + their lifecycle state and activate PARKED
// ones via the core_init syscall. Only ever read on the BSP (it owns the VFS), so the
// view is machine-global regardless of who forwarded the read. ---

/// Number of CPUs to enumerate for `/proc/cores` (1 if SMP wasn't brought up).
pub fn core_count() -> usize {
    if PROBED.load(Ordering::Acquire) {
        NUM_CORES.load(Ordering::Relaxed)
    } else {
        1
    }
}

/// Whether core `idx` is the boot processor (the `/proc/cores` `role` column).
pub fn is_bsp(idx: usize) -> bool {
    if !PROBED.load(Ordering::Acquire) {
        return idx == 0;
    }
    idx == (read_mpidr() & 0xff) as usize
}

/// Lifecycle state of core `idx` as a static string for `/proc/cores`. The BSP is always
/// `online` (it is executing this); peers reflect the shared descriptor's `state` atomic.
pub fn core_state_str(idx: usize) -> &'static str {
    if is_bsp(idx) {
        return "online"; // the boot core is, by definition, online
    }
    // SAFETY: the descriptor is initialized once at boot and identity-mapped on the BSP;
    // we read only the per-core `state` atomic.
    let cfg = unsafe { &*MACHINE_CONFIG.0.get() };
    match cfg.cores.get(idx).map(|c| c.state.load(Ordering::Acquire)) {
        Some(STATE_OFFLINE) => "offline",
        Some(STATE_BOOTING) => "booting",
        Some(STATE_PARKED) => "parked",
        Some(STATE_ONLINE) => "online",
        _ => "unknown",
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
