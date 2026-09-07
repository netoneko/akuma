//! Virtual memory: page tables, address spaces, ASIDs, and the TTBR free gate.
//!
//! AArch64, 4 KB granule, 4-level page tables (L0-L3).
//!
//! # Why this is a crate
//!
//! Extracted from `akuma-exec` on 2026-08-30
//! (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.5). Two numbers made it the
//! highest-value body to isolate: it held **41% of `akuma-exec`'s entire `unsafe`
//! budget** (86 of 209 sites) at the **lowest test coverage in that crate**
//! (7.9%). Concentrating it here is the `akuma-net-nic` move — "irreducible is a
//! property of a body of code, not of a crate" — and it is the state in which
//! `akuma-net` first became reviewable, and then `forbid`-able.
//!
//! `UserAddressSpace`'s `Drop`, the deferred-free path (`free_or_defer_as_frames`,
//! `drain_pending_ttbr_frees`) and the per-core `ACTIVE_L0`/`PREV_L0` gate are the
//! mechanism behind the page-table-UAF BKL storm
//! (`docs/archive/PAGE_TABLE_UAF_BKL_STORM.md`) and the F8 saved-context TTBR0
//! gate. The gate in particular is a decision function over
//! `(l0_phys, per-core published L0s, saved contexts)` — exactly the shape that
//! hosts well, and which previously needed a devbox boot to exercise.
//!
//! # This crate is *virtual* memory. `akuma-pmm` is *physical*.
//!
//! That seam is load-bearing and this crate must not erode it. `akuma-pmm`
//! allocates frames — 1,139 lines, 5 `unsafe` sites, and a stated invariant that
//! it takes no dependency on `akuma-exec`. This crate maps them, and carries 71
//! `unsafe` sites plus a hook into the scheduler. Folding the two together would
//! take the frame allocator from 5 `unsafe` sites to 91 and drag `akuma-mmap`,
//! `akuma-ext2` and `akuma-virtio` downstream of all of them. The memory family is
//! three crates, bottom-up: `akuma-pmm` (frames) -> `akuma-mmap` (region records,
//! `forbid`) -> `akuma-mmu` (page tables).
//!
//! # The one upward dependency
//!
//! [`SchedHooks`]: two questions only the scheduler can answer, both about
//! whether an address space is still live on some core. Everything else this
//! crate needed from `akuma-exec` either moved down to `akuma-primitives`
//! already, moved *up* out of here (`user_access.rs`, whose eight process
//! references were the whole of the old `mmu <-> process` cycle), or became the
//! `debug-info` feature.

#![cfg_attr(not(test), no_std)]
// Inherited verbatim from `akuma-exec`'s crate-root `allow` list. This code did
// not change when it moved out on 2026-08-30, so its lint posture must not
// either — a split that silently turns 20 warnings on is not behaviour-preserving,
// and fixing them in the same commit would hide the move in the diff. Tighten
// these deliberately, later, one lint at a time.
#![allow(
    clippy::too_long_first_doc_paragraph,
    clippy::must_use_candidate,
    clippy::redundant_pub_crate,
    clippy::unnecessary_cast,
    clippy::ptr_as_ptr,
    clippy::verbose_bit_mask,
    clippy::single_match_else,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::new_without_default,
    clippy::manual_div_ceil,
    clippy::cast_lossless,
    clippy::vec_init_then_push,
    clippy::unused_self,
    clippy::semicolon_if_nothing_returned,
    clippy::needless_continue,
    clippy::manual_is_multiple_of,
    clippy::identity_op,
    clippy::collapsible_if,
    clippy::cast_possible_wrap,
    clippy::inline_always,
    clippy::missing_safety_doc,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    clippy::cast_ptr_alignment,
    clippy::declare_interior_mutable_const,
    clippy::missing_const_for_fn,
    clippy::too_many_lines,
    clippy::borrow_as_ptr,
    clippy::ref_as_ptr,
    clippy::items_after_statements,
    clippy::redundant_else,
    clippy::option_if_let_else,
    clippy::needless_range_loop,
    clippy::collapsible_else_if,
    clippy::significant_drop_tightening,
    clippy::unreadable_literal,
    clippy::similar_names,
    clippy::implicit_saturating_sub,
    clippy::manual_let_else,
    clippy::let_and_return,
    clippy::use_self,
    unused_unsafe,
    unused_mut,
    dead_code,
)]

extern crate alloc;

pub mod asid;
pub mod types;

/// The tree's one heap-free print macro, re-exported so this crate's
/// `crate::safe_print!(…)` call sites resolve unchanged.
pub use akuma_primitives::safe_print;

/// Allocation source for debug frame tracking.
///
/// Re-exported from `akuma-pmm`, which owns the tracker. This was a mirrored
/// copy of that enum "so this crate can attribute frames without depending on
/// the execution crate" — but the type it was mirroring was `akuma-exec`'s own
/// copy, and the one that matters lives *below* both of us in `akuma-pmm`,
/// which this crate already depends on. Unified 2026-09-01.
pub use akuma_pmm::FrameSource;

/// Attribute a frame to a source in the PMM's tracker.
pub fn track_frame(frame: PhysFrame, source: FrameSource) {
    akuma_pmm::track_frame(frame.addr, source);
}

/// The scheduler questions this crate cannot answer for itself.
///
/// Both are about whether an address space is still live somewhere. The TTBR free
/// gate must not free an L0 that a core's saved context still points at — that is
/// the F8 defect (`[SGI-S FREED-L0]` must never print) — and only the thread table
/// knows. Registered once by `akuma_exec::init`.
///
/// This is the **whole** of this crate's upward surface. It is a two-item table
/// rather than the "just call `threading::`" it replaced, because the alternative
/// is a dependency cycle: `akuma-exec` maps pages through this crate on every
/// fault.
#[derive(Clone, Copy)]
pub struct SchedHooks {
    /// Is any thread's *saved* context still pointing at this L0 base? Returns
    /// `(tid, state)` of the first one found. The free gate's second arm.
    pub any_saved_ctx_on_l0: fn(u64) -> Option<(usize, u8)>,
    /// Record the TTBR0 the current thread is expected to run under, so a
    /// mismatch at switch-in is detectable.
    pub note_current_expected_l0: fn(u64),
    /// Is the calling thread already TERMINATED? Such a thread can be reaped at
    /// any yield and never resume, so it must not *run* the pending-frame drain
    /// — the drain would be abandoned mid-loop, orphaning the frames and the
    /// `user_frames` map it had taken ownership of
    /// (`docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md`).
    pub current_thread_is_terminated: fn() -> bool,
}

static SCHED: akuma_primitives::Registered<SchedHooks> = akuma_primitives::Registered::new(
    "akuma-mmu: SchedHooks not registered — call akuma_exec::init() first",
);

/// Register the scheduler hooks. Call once, from `akuma_exec::init`.
pub fn register_sched_hooks(h: SchedHooks) {
    SCHED.register(h);
}

/// Whether any thread's saved context still references `l0_base`.
///
/// Degrades to `None` before registration — during early boot there are no saved
/// contexts to conflict with, and a panic here would fire before the console is
/// useful.
#[inline]
fn any_saved_ctx_on_l0(l0_base: u64) -> Option<(usize, u8)> {
    SCHED.get().and_then(|h| (h.any_saved_ctx_on_l0)(l0_base))
}

/// Is the calling thread terminal? `false` before registration — during early
/// boot nothing has terminated, and the gate is a refusal to do work, so the
/// permissive default is also the safe one.
#[inline]
fn thread_is_terminal() -> bool {
    SCHED.get().is_some_and(|h| (h.current_thread_is_terminated)())
}

/// Record the current thread's expected TTBR0. No-op before registration.
#[inline]
fn note_current_expected_l0(ttbr0: u64) {
    if let Some(h) = SCHED.get() {
        (h.note_current_expected_l0)(ttbr0);
    }
}

/// Whether the `[AS-*]` address-space lifecycle trace is compiled in.
///
/// **A compile-time gate, by design.** It replaced
/// `akuma_exec::process::lifecycle_trace_on()`, which was
/// `cfg!(feature = "debug-info") && config().syscall_debug_info_enabled` — the
/// runtime half is dropped, so with the feature compiled in the trace is always
/// on rather than also consulting `syscall_debug_info_enabled`. That is a
/// deliberate, narrow behaviour change on a debug-only path: the feature is off
/// in every shipping profile, so the default build is byte-identical, and having
/// opted in at compile time is opt-in enough. Keeping the runtime half would have
/// meant a `fn() -> bool` hook on the address-space create/exec/free path, which
/// is exactly the indirect call the `cfg!` shape exists to avoid.
#[inline(always)]
const fn lifecycle_trace_on() -> bool {
    cfg!(feature = "debug-info")
}

pub use types::*;

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use akuma_mmap::PhysFrame;
// Both architectures' `UserAddressSpace` carries one — the AArch64 struct since
// `akuma-user-space` was split out, the x86_64 one since B3 widened it
// (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`).
use akuma_user_space::FrameLedger;
use akuma_primitives::irq::{with_irqs_disabled, IrqGuard};

/// MMU initialization state
static MMU_INITIALIZED: AtomicBool = AtomicBool::new(false);

static RAM_BASE: AtomicUsize = AtomicUsize::new(0);
static RAM_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Check if MMU is initialized
pub fn is_initialized() -> bool {
    MMU_INITIALIZED.load(Ordering::Acquire)
}

/// Mark MMU as initialized
pub fn init(ram_base: usize, ram_size: usize) {
    RAM_BASE.store(ram_base, Ordering::Release);
    RAM_SIZE.store(ram_size, Ordering::Release);
    MMU_INITIALIZED.store(true, Ordering::Release);
    #[cfg(all(target_os = "none", target_arch = "aarch64"))]
    extend_boot_ram_identity_map(ram_base, ram_size);
}

/// Top (exclusive) of the RAM range that `src/boot.rs` statically identity-maps:
/// L1[0]=device [0,1GB), L1[1]=[1GB,2GB), L1[2]=[2GB,3GB). Anything above this
/// must be mapped at runtime once the detected RAM size is known.
const BOOT_STATIC_MAP_END: usize = 0xC000_0000; // 3 GB

/// Ensure the boot identity map covers `addr`, mapping its 1 GiB block if not.
///
/// `boot.rs` statically maps `[0, 3 GiB)`. That is enough for QEMU virt, where
/// the DTB is placed immediately after the kernel image at a low address. It is
/// **not** enough for Firecracker, which puts the FDT in the last 2 MiB of guest
/// RAM: a 4 GiB microVM has RAM at `0x8020_0000..0x1_8020_0000` and its FDT at
/// roughly 6 GiB, far outside the static map. Reading `x0` there faults before
/// the kernel has printed a single line about memory.
///
/// [`extend_boot_ram_identity_map`] cannot solve this — it needs the RAM size,
/// which is precisely what the FDT is being read to discover. So this maps one
/// 1 GiB block on demand, before the read.
///
/// Only the block containing `addr` is mapped, deliberately: mapping the whole
/// 512 GiB L1 as Normal memory would invite speculative access to addresses with
/// no backing store.
///
/// # Boot-phase only
/// Editing the boot table is only sound while it is the *only* table: on the
/// boot page table, single-threaded, before any other address space exists.
/// That window closes at [`init`], so the obligation is discharged here by
/// refusing to run once [`is_initialized`] is true rather than being pushed
/// onto the caller as an `unsafe` contract.
///
/// # Returns
/// Whether `addr` is readable through the boot identity map on return — `true`
/// if it was already covered or this call mapped it, `false` if it is outside
/// what the boot table can reach or the window has closed. [`with_boot_identity_fdt`]
/// is what that answer exists for: a raw boot-time read needs a *checked* mapping,
/// not a mapping attempt.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn ensure_boot_identity_covers(addr: usize) -> bool {
    if is_initialized() {
        debug_assert!(false, "ensure_boot_identity_covers after mmu::init");
        return false;
    }
    let idx = addr >> 30;
    if idx == 0 || idx > 511 {
        return false; // block 0 is the device block; anything past 511 is not addressable here
    }
    if addr < BOOT_STATIC_MAP_END {
        return true; // already covered by boot.rs's static blocks
    }

    let l0_phys = (get_boot_ttbr0() & 0x0000_FFFF_FFFF_F000) as usize;
    if l0_phys == 0 {
        return false;
    }
    let l0 = phys_to_virt(l0_phys) as *const u64;
    let l0_0 = unsafe { core::ptr::read_volatile(l0) };
    let l1_phys = (l0_0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l1 = phys_to_virt(l1_phys) as *mut u64;

    // Same attributes boot.rs uses for its RAM blocks.
    let block_flags =
        flags::VALID | flags::BLOCK | flags::AF | flags::SH_INNER | attr_index(MAIR_NORMAL_WB);

    unsafe {
        let existing = core::ptr::read_volatile(l1.add(idx));
        if existing & flags::VALID != 0 {
            return true; // already mapped
        }
        core::ptr::write_volatile(l1.add(idx), ((idx << 30) as u64) | block_flags);
    }
    boot_table_flush_sync();
    true
}

/// Read the flattened device tree at `pa`, for the duration of one closure.
///
/// This is the only supported way to reach [`akuma_fdt::locate`] from the boot
/// path, and it exists so that the "is this pointer mapped?" obligation is
/// *discharged* rather than passed along. `locate` is `unsafe` because it
/// speculatively reads eight bytes at an address nothing has validated; the
/// caller in `kernel_main` could only ever vouch for that by comment. This crate
/// owns the boot identity map, so [`ensure_boot_identity_covers`] maps the block
/// and reports whether `addr` came out covered — an out-of-range pointer, or one
/// arriving after [`init`] has closed the boot-table window, yields `None`
/// instead of a translation fault before the console has said anything about
/// memory.
///
/// # Why a closure
/// The blob is **not** valid for the rest of boot: on large-RAM configs the heap
/// is placed on top of it. `locate` hands back an unbounded lifetime, so nothing
/// in the type system stops a `Dtb` from outliving its bytes — the closure is
/// what bounds it, and every consumer (device map, `detect_memory`, the SMP
/// probe) has to run inside. Returning the `Dtb` from this function instead
/// would hand back exactly the dangling borrow it exists to prevent.
///
/// `pa` is resolved first ([`akuma_fdt::resolve`]), so a zero pointer — the
/// "scan for QEMU virt's fixed location" case — is mapped and checked at the
/// address that will actually be read.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn with_boot_identity_fdt<R>(pa: usize, f: impl FnOnce(Option<&akuma_fdt::Dtb<'_>>) -> R) -> R {
    let base = akuma_fdt::resolve(pa);
    if !ensure_boot_identity_covers(base) {
        return f(None);
    }
    // SAFETY: `base` is confirmed mapped through the boot identity map directly
    // above, and `locate` validates the FDT magic and declared size before
    // trusting either. Boot phase: single-threaded, on the boot page table,
    // before any user address space exists.
    let dtb = unsafe { akuma_fdt::locate(base) };
    f(dtb.as_ref())
}

/// Extend the boot TTBR0 identity map to cover ALL detected RAM.
///
/// `boot.rs` statically maps only `[0, 3GB)`. The PMM, however, hands out frames
/// across the full detected RAM, and the kernel zeroes/accesses any such frame
/// via `phys_to_virt` (VA == PA) while the boot table may be the active TTBR0
/// (e.g. the deactivate→swap window in `replace_image`). On a >2GB machine a
/// frame at PA >= 3GB then hits an unmapped boot-table entry → EL1 translation
/// fault (observed killing clang/ld during exec at MEMORY>=3.5G; see
/// docs/COW_OPTIMIZATIONS.md). Map the remaining RAM as 1GB NORMAL blocks
/// (EL1-only, matching boot.rs's L1[1]/L1[2]) so kernel-context access to any
/// valid frame works regardless of which TTBR0 is active. Per-AS user mappings
/// already cover full RAM via `add_kernel_mappings`; this fixes the *boot* table.
#[cfg(all(target_os = "none", target_arch = "aarch64"))]
fn extend_boot_ram_identity_map(ram_base: usize, ram_size: usize) {
    let _ = ram_base;
    let ram_end = ram_base.saturating_add(ram_size);
    if ram_end <= BOOT_STATIC_MAP_END {
        return; // RAM fits within the static boot map; nothing to do.
    }

    // boot L0[0] -> the 1GB-block identity L1 table. get_boot_ttbr0 carries
    // ASID 0, so its value is the L0 physical address directly.
    let l0_phys = (get_boot_ttbr0() & 0x0000_FFFF_FFFF_F000) as usize;
    if l0_phys == 0 { return; }
    let l0 = phys_to_virt(l0_phys) as *const u64;
    let l0_0 = unsafe { core::ptr::read_volatile(l0) };
    let l1_phys = (l0_0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l1 = phys_to_virt(l1_phys) as *mut u64;

    // 1GB NORMAL block, EL1 RW / EL0 none — same attributes boot.rs uses.
    let block_flags =
        flags::VALID | flags::BLOCK | flags::AF | flags::SH_INNER | attr_index(MAIR_NORMAL_WB);

    let start_idx = BOOT_STATIC_MAP_END >> 30;        // 3
    let end_idx = ((ram_end - 1) >> 30).min(511);     // last 1GB L1 entry for RAM
    for idx in start_idx..=end_idx {
        let pa = (idx << 30) as u64;
        unsafe { core::ptr::write_volatile(l1.add(idx), pa | block_flags); }
    }
    boot_table_flush_sync();
}

/// `dsb ish; tlbi vmalle1; dsb ish; isb` — the core-local full-TLB flush that
/// every *boot*-table edit in this file ends with.
///
/// Core-local (`vmalle1`), not the inner-shareable `flush_tlb_all`: these
/// callers correct the boot table, which no secondary is translating through
/// yet. Kept as one named helper because the four sites must not drift apart —
/// dropping either `dsb` leaves the descriptor write unordered against the
/// invalidate, and dropping the `isb` lets already-fetched instructions use the
/// stale translation.
#[inline(always)]
fn boot_table_flush_sync() {
    akuma_cpu::barrier::dsb_ish();
    akuma_cpu::tlb::vmalle1();
    akuma_cpu::barrier::dsb_ish();
    akuma_cpu::barrier::isb();
}

/// Fallback kernel-RAM window bounds used before `init` (host unit tests, early
/// boot). These match the historical hardcoded 2GB-RAM identity map so behavior
/// is unchanged when RAM bounds aren't known yet.
const FALLBACK_RAM_BASE: usize = 0x4000_0000;
const FALLBACK_RAM_END: usize = 0xC000_0000;

/// Physical base of usable RAM (where the PMM allocates from).
pub fn ram_base() -> usize {
    let b = RAM_BASE.load(Ordering::Acquire);
    if b == 0 { FALLBACK_RAM_BASE } else { b }
}

/// Physical end (exclusive) of usable RAM.
pub fn ram_end() -> usize {
    let size = RAM_SIZE.load(Ordering::Acquire);
    if size == 0 { FALLBACK_RAM_END } else { ram_base() + size }
}

/// Top (exclusive) of the kernel RAM identity-map VA window, rounded up to a 1GB
/// boundary.  `add_kernel_mappings` identity-maps `[ram_base, ram_end)` as
/// EL1-only 2MB blocks in every user address space, so user VA allocation must
/// avoid `[KERNEL_VA_START, kernel_va_end())` — otherwise a user mapping lands on
/// top of those kernel blocks and an EL0 access permission-faults (the MEMORY>2GB
/// SIGSEGV; see docs/COW_OPTIMIZATIONS.md).  Scaling this with detected RAM is
/// what lets `MEMORY` exceed 2GB.  Rounding up to 1GB keeps user VAs out of any
/// L1 entry that holds a kernel RAM L2 table, so user page tables never share an
/// L2 with the kernel identity map.
pub fn kernel_va_end() -> usize {
    let size = RAM_SIZE.load(Ordering::Acquire);
    if size == 0 { return FALLBACK_RAM_END; }
    const GB: usize = 1 << 30;
    (ram_end() + GB - 1) & !(GB - 1)
}

/// Bounds of the kernel image window, used by the exception/scheduler tripwires
/// to classify an `ELR` as kernel or user code.
///
/// Runtime rather than `const` because the kernel's load address is
/// machine-specific: `0x4010_0000` on QEMU virt, `0x8030_0000` under Firecracker.
/// A hardcoded `0x4010_0000..0x6000_0000` here (and in five places in
/// `src/exceptions.rs`) inverted into an empty range on Firecracker, so every
/// legitimate EL1 frame was reported as poisoned — `[SGI-S POISON]` and
/// `[IRQ POISON]` on every scheduler switch and timer tick.
static KERNEL_TEXT_START: AtomicUsize = AtomicUsize::new(0x4010_0000);
static KERNEL_TEXT_END: AtomicUsize = AtomicUsize::new(0x6000_0000);

/// Install the kernel image window. Call once during early init, from the kernel
/// crate that owns `config::KERNEL_PHYS_BASE`.
pub fn set_kernel_text_window(start: usize, end: usize) {
    debug_assert!(start < end, "kernel text window is inverted or empty");
    KERNEL_TEXT_START.store(start, Ordering::Relaxed);
    KERNEL_TEXT_END.store(end, Ordering::Relaxed);
}

/// Is `addr` inside the kernel image window?
///
/// Two relaxed atomic loads. These run on the IRQ and context-switch paths, which
/// already do two `read_volatile`s of the trap frame right beside this call, so
/// the cost is in the noise — and correctness across machines is worth more than
/// a `const` here.
#[inline]
#[must_use]
pub fn is_kernel_text(addr: usize) -> bool {
    addr >= KERNEL_TEXT_START.load(Ordering::Relaxed)
        && addr < KERNEL_TEXT_END.load(Ordering::Relaxed)
}

/// Physical address of the shared device L1 table (under L0[1]).
/// Allocated once by `init_shared_device_tables()`, then referenced
/// by every user address space's `add_kernel_mappings()`.
static SHARED_DEV_L1_PHYS: AtomicUsize = AtomicUsize::new(0);

/// The greatest number of device regions the L0[1] window can describe.
///
/// One more than the current platforms need, so adding a device does not force a
/// constant change. The bound exists because the table is a static array of
/// atomics rather than a heap `Vec`: it is populated before the allocator runs.
pub const MAX_DEV_REGIONS: usize = 16;

/// One device region in the L0[1] window: kernel VA, machine PA, byte length.
///
/// `va` comes from `akuma_primitives::addr` and is our choice, fixed at compile
/// time. `pa` is the *machine's*, and is therefore a runtime value — QEMU virt
/// and Firecracker disagree on every one of them, and Firecracker's GIC
/// redistributor base additionally depends on the configured vCPU count
/// (`docs/archive/FIRECRACKER_PORT.md` §2.1), which no build-time constant can
/// express. That asymmetry — fixed VAs, discovered PAs — is the whole shape of
/// the device-map abstraction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DevRegion {
    pub va: usize,
    pub pa: usize,
    pub size: usize,
}

/// The active device map. Written once by [`set_device_map`] before the first
/// address space exists, read by [`init_shared_device_tables`] and
/// [`rebuild_boot_device_table`].
static DEV_MAP: [(AtomicUsize, AtomicUsize, AtomicUsize); MAX_DEV_REGIONS] =
    [const { (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)) }; MAX_DEV_REGIONS];
static DEV_MAP_LEN: AtomicUsize = AtomicUsize::new(0);

/// Install the device map: which machine PA backs each device VA.
///
/// Must be called before [`init_shared_device_tables`] and before any user
/// address space is created. Regions are validated against the compile-time
/// window layout, so a caller cannot install a region that escapes the L0[1]
/// window or is not page-aligned — the failure mode that
/// `docs/archive/GICD_IROUTER_ALIASING.md` describes is a mapping that is
/// *shorter* than the device it claims to cover, and a short region here means a
/// short mapping there.
///
/// Returns the number of regions installed.
pub fn set_device_map(regions: &[DevRegion]) -> usize {
    // Referencing the const forces its evaluation, which is what actually
    // enforces the no-overlap invariant on the VA layout.
    let () = DEV_WINDOW_NO_OVERLAP;

    let n = regions.len().min(MAX_DEV_REGIONS);
    for (slot, r) in DEV_MAP.iter().zip(regions.iter().take(n)) {
        debug_assert_eq!(r.va % PAGE_SIZE, 0, "device VA not page-aligned");
        debug_assert_eq!(r.pa % PAGE_SIZE, 0, "device PA not page-aligned");
        debug_assert!(r.size > 0 && r.size % PAGE_SIZE == 0, "bad device region size");
        debug_assert!(
            r.va >= DEV_WINDOW_VA && r.va - DEV_WINDOW_VA + r.size <= DEV_WINDOW_SIZE,
            "device region escapes the L0[1] window"
        );
        slot.0.store(r.va, Ordering::Relaxed);
        slot.1.store(r.pa, Ordering::Relaxed);
        slot.2.store(r.size, Ordering::Relaxed);
    }
    DEV_MAP_LEN.store(n, Ordering::Release);
    n
}

/// The installed device map, copied out. Empty until [`set_device_map`] runs.
#[must_use]
pub fn device_map() -> ([DevRegion; MAX_DEV_REGIONS], usize) {
    let n = DEV_MAP_LEN.load(Ordering::Acquire);
    let mut out = [DevRegion { va: 0, pa: 0, size: 0 }; MAX_DEV_REGIONS];
    for (o, slot) in out.iter_mut().zip(DEV_MAP.iter()).take(n) {
        o.va = slot.0.load(Ordering::Relaxed);
        o.pa = slot.1.load(Ordering::Relaxed);
        o.size = slot.2.load(Ordering::Relaxed);
    }
    (out, n)
}

/// Page-descriptor flags for a device page: Device-nGnRnE, EL1-only, never
/// executable.
#[inline]
fn device_page_flags() -> u64 {
    flags::VALID
        | flags::TABLE
        | flags::AF
        | attr_index(MAIR_DEVICE_NGNRNE)
        | flags::PXN
        | flags::UXN
        | flags::SH_OUTER
}

/// Write every page of the installed device map into an L3 table covering
/// `DEV_WINDOW_VA`.
///
/// # Safety
/// `l3_ptr` must point at a writable, 512-entry L3 table that L0[1]→L1[0]→L2[0]
/// resolves to.
unsafe fn write_device_l3(l3_ptr: *mut u64) {
    let flags = device_page_flags();
    let (map, n) = device_map();
    for r in map.iter().take(n) {
        let first = (r.va - DEV_WINDOW_VA) / PAGE_SIZE;
        for page in 0..(r.size / PAGE_SIZE) {
            let pa = (r.pa + page * PAGE_SIZE) as u64;
            unsafe { l3_ptr.add(first + page).write_volatile(pa | flags) };
        }
    }
}

/// Rewrite the **boot** table's device L3 from the installed device map.
///
/// `boot.rs` fills that L3 in pre-MMU assembly from compile-time literals, which
/// is only enough to reach the console. Once the FDT has been parsed the real
/// machine addresses are known and may differ — on Firecracker the GIC
/// redistributor moves with vCPU count — so the boot table has to be corrected
/// before any GIC or virtio MMIO access. Everything else in the boot table is
/// left alone.
///
/// The L3 is **found, not supplied**: this walks the live boot TTBR0 for
/// L0[1] -> L1[0] -> L2[0], which is exactly [`write_device_l3`]'s stated
/// precondition, so there is no wrong table a caller can hand in. It used to
/// take the address as an argument, derived independently in `src/boot.rs` from
/// the `boot_page_tables` linker symbol ("the device L3 is page 5") — two
/// descriptions of one table, and the `unsafe` on this function existed only to
/// make the caller promise they still agreed.
///
/// # Boot-phase only
/// Same window as [`ensure_boot_identity_covers`]: the boot table is only safe
/// to edit in place while it is the only table and nothing else is running.
/// Enforced here rather than asserted by the caller.
pub fn rebuild_boot_device_table() {
    if is_initialized() {
        debug_assert!(false, "rebuild_boot_device_table after mmu::init");
        return;
    }
    let Some(l3_phys) = boot_device_l3_phys() else {
        return;
    };
    // SAFETY: `l3_phys` came out of the boot table's own L0[1]->L1[0]->L2[0]
    // chain, so it is by construction the writable 512-entry L3 that chain
    // resolves to, and `phys_to_virt` is the identity on it.
    unsafe {
        write_device_l3(phys_to_virt(l3_phys) as *mut u64);
    }
    boot_table_flush_sync();
}

/// Walk the boot TTBR0 for the device L3 under `L0[1] -> L1[0] -> L2[0]`.
///
/// `None` if any level of that chain is absent — which is the case on the host
/// (`get_boot_ttbr0` is a `0` stub there) and would be the case if the boot
/// assembly ever stopped installing the device window.
fn boot_device_l3_phys() -> Option<usize> {
    const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

    // SAFETY: each address is the physical base of a page-table level reached
    // from the boot TTBR0, identity-mapped by the boot table, and read as a
    // single aligned u64. Checked valid before descending.
    unsafe fn descend(table_phys: usize, index: usize) -> Option<usize> {
        if table_phys == 0 {
            return None;
        }
        let entry = unsafe { core::ptr::read_volatile((phys_to_virt(table_phys) as *const u64).add(index)) };
        if entry & (flags::VALID | flags::TABLE) != (flags::VALID | flags::TABLE) {
            return None;
        }
        Some((entry & ADDR_MASK) as usize)
    }

    let l0_phys = (get_boot_ttbr0() & ADDR_MASK) as usize;
    unsafe {
        let l1 = descend(l0_phys, 1)?;
        let l2 = descend(l1, 0)?;
        descend(l2, 0)
    }
}

/// Allocate the shared L1/L2/L3 device page tables that every user address
/// space will reference via L0[1].  Must be called once during kernel init,
/// after the PMM is ready and after [`set_device_map`].
pub fn init_shared_device_tables() {
    debug_assert!(
        DEV_MAP_LEN.load(Ordering::Acquire) > 0,
        "init_shared_device_tables before set_device_map: no devices would be mapped"
    );

    let l1 = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).expect("shared dev L1");
    let l2 = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).expect("shared dev L2");
    let l3 = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).expect("shared dev L3");

    unsafe {
        let l1_ptr = phys_to_virt(l1.addr) as *mut u64;
        let l2_ptr = phys_to_virt(l2.addr) as *mut u64;
        let l3_ptr = phys_to_virt(l3.addr) as *mut u64;

        // L1[0] -> L2
        l1_ptr.write_volatile((l2.addr as u64) | flags::VALID | flags::TABLE);
        // L2[0] -> L3
        l2_ptr.write_volatile((l3.addr as u64) | flags::VALID | flags::TABLE);

        write_device_l3(l3_ptr);
    }

    SHARED_DEV_L1_PHYS.store(l1.addr, Ordering::Release);
}

// =============================================================================
// Physical/Virtual Address Translation
// =============================================================================

/// Kernel virtual/physical translation, re-exported from `akuma_primitives::addr`.
///
/// Both are the identity, and both are `#[inline(always)]` — moving them is what
/// let `akuma-virtio` (and through it `akuma-net`) stop depending on this crate.
/// Read that module's header before changing either: they must stay free
/// functions, because Phase 3 deleted the runtime-hook version for costing a
/// spinlocked read on the per-packet DMA path.
pub use akuma_primitives::addr::{phys_to_virt, virt_to_phys};

/// Make freshly-written instruction bytes in `[kva, kva + len)` visible to the
/// instruction-fetch path. `kva` is a *kernel* (identity) virtual address that
/// aliases the physical bytes.
///
/// **Both halves are required.** `ic ivau` only invalidates the I-cache; the
/// refill then reads from the Point of Unification. If the bytes were written
/// through the D-cache (dynamic relocations, a W^X `RW`→`RX` flip, demand-paged
/// text) the dirty line may not have reached the PoU yet, so without the
/// preceding `dc cvau` the CPU can fetch **stale** instructions. Under
/// multi-threaded loads this surfaced as a fixed user call site issuing a
/// syscall with a corrupted `x8` — a stale `mov x8, #imm` — i.e. the "x8 race"
/// (see `docs/AKUMA_SELF_HOSTING.md` §7j). Sequence:
/// `dc cvau` (clean to PoU) → `dsb ish` → `ic ivau` (invalidate I-cache) →
/// `dsb ish` → `isb`.
///
/// `len == 0` is a no-op. The range is widened down to the 64-byte cache-line
/// containing `kva` so a sub-line `len` still cleans/invalidates the whole line.
pub fn sync_icache_range(kva: usize, len: usize) {
    if len == 0 {
        return;
    }
    // Cache Writeback Granule is 64 bytes on every core Akuma targets; align
    // the start down so a partial first line is still maintained.
    let start = kva & !63;
    let end = kva + len;
    let mut p = start;
    while p < end {
        akuma_cpu::cache::dc_cvau(p);
        p += 64;
    }
    akuma_cpu::barrier::dsb_ish();
    let mut p = start;
    while p < end {
        akuma_cpu::cache::ic_ivau(p);
        p += 64;
    }
    akuma_cpu::barrier::dsb_ish();
    akuma_cpu::barrier::isb();
}

/// Which cores a TLB invalidation must reach.
///
/// Every `flush_tlb_*` call in this tree today invalidates a *user address
/// space's* translation, and a shared-kernel process can run on any core, so
/// every current call site needs [`AllCores`](TlbTarget::AllCores). The
/// variant exists anyway — see
/// `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §3 — because the previous
/// API could not *say* which cores an invalidation covered, and four of this
/// project's most expensive investigations
/// (`fork_cow_tlb_asid_flush`, `page_table_uaf_ttbr_gate_fix`,
/// `oncpu_gate_scheduler_race`, `cowstale_stale_write_fault_fixed`) turned on
/// exactly that question. `ThisCore` is for a translation provably private to
/// the calling core (e.g. a scratch mapping nothing else could have cached);
/// picking it for anything else under `kernel_smp_shared` reintroduces the
/// stale-peer-translation bug those investigations closed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TlbTarget {
    /// Only the calling core may have cached this translation.
    ThisCore,
    /// Any core running this kernel image may have cached this translation
    /// (the address space is, or could become, live on more than one core).
    AllCores,
}

/// A pending TLB-invalidation completion.
///
/// `#[must_use]` so a `flush_tlb_*` call cannot be issued and silently
/// forgotten about — the instruction that invalidates a translation and the
/// barrier that makes the invalidation *visible* are two different steps, and
/// dropping this token is what performs the second one. On AArch64 that is
/// the `dsb ish` + `isb` pair this crate always emitted right after the
/// `tlbi`; moving it into `Drop` costs nothing (the token is a drop-in
/// replacement for a call that used to be inline in the same statement) and
/// gives a future non-broadcast target — an IPI-based shootdown, should one
/// ever be built — one place to wait for acknowledgement instead of a second
/// vocabulary bolted on beside this one. See `akuma-psci`'s call functions for
/// the same reasoning applied to a different silently-droppable return.
#[must_use = "a TLB invalidation is not complete until this token is dropped"]
pub struct TlbFlush(bool);

impl TlbFlush {
    /// A real invalidation was issued; drop this at the point completion is needed.
    #[inline(always)]
    fn pending() -> Self {
        Self(true)
    }

    /// Nothing was invalidated (e.g. a zero-length range) — `Drop` is a no-op.
    /// Exists so the early-return shapes below can still hand back a token
    /// without buying a barrier pair they don't need.
    #[inline(always)]
    fn done() -> Self {
        Self(false)
    }
}

impl Drop for TlbFlush {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(all(target_os = "none", target_arch = "aarch64"))]
        if self.0 {
            akuma_cpu::barrier::dsb_ish();
            akuma_cpu::barrier::isb();
        }
        #[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
        let _ = self.0;
    }
}

// Real shared-kernel SMP: TLB maintenance that affects a shared/user address space must
// reach EVERY core (a page-table edit on one core while another runs that space in EL0
// would otherwise leave the peer with stale entries). The inner-shareable `...is`
// variants broadcast the invalidation across the inner-shareable domain (all cores on
// QEMU `virt`). Other builds keep the cheaper core-local form. The context-switch flush
// (threading) stays local — it only needs to clear the switching core's own TLB.
#[cfg(all(target_os = "none", target_arch = "aarch64", kernel_smp_shared))]
pub fn flush_tlb_all(target: TlbTarget) -> TlbFlush {
    akuma_cpu::barrier::dsb_ishst();
    match target {
        TlbTarget::AllCores => akuma_cpu::tlb::vmalle1is(),
        TlbTarget::ThisCore => akuma_cpu::tlb::vmalle1(),
    }
    TlbFlush::pending()
}

#[cfg(all(target_os = "none", target_arch = "aarch64", not(kernel_smp_shared)))]
pub fn flush_tlb_all(_target: TlbTarget) -> TlbFlush {
    akuma_cpu::barrier::dsb_ishst();
    akuma_cpu::tlb::vmalle1();
    TlbFlush::pending()
}

// x86_64 has no broadcast invalidation instruction (`REDUCING_PLATFORM_DEPENDENCY.md`
// §3), so there is no `kernel_smp_shared` split to make here — a full `CR3` reload
// is the only "flush everywhere this core knows about" this target has, and it is
// what `invlpg`-per-page would degrade to past a threshold anyway (see
// `flush_tlb_range_all_asid`'s `FULL_FLUSH_THRESHOLD`).
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn flush_tlb_all(_target: TlbTarget) -> TlbFlush {
    // SAFETY: reloading the CR3 already active changes no mapping — it only
    // forces every non-global TLB entry to be re-walked.
    unsafe { x86_write_cr3(x86_read_cr3()) };
    TlbFlush::pending()
}

#[cfg(not(any(
    all(target_os = "none", target_arch = "aarch64"),
    all(target_os = "none", target_arch = "x86_64"),
)))]
pub fn flush_tlb_all(_target: TlbTarget) -> TlbFlush { TlbFlush::done() }

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn get_boot_ttbr0() -> u64 {
    unsafe {
        let addr: u64;
        core::arch::asm!(
            "adrp {tmp}, boot_ttbr0_addr",
            "add {tmp}, {tmp}, :lo12:boot_ttbr0_addr",
            "ldr {out}, [{tmp}]",
            tmp = out(reg) _,
            out = out(reg) addr,
        );
        addr
    }
}

#[cfg(not(all(target_os = "none", target_arch = "aarch64")))]
pub fn get_boot_ttbr0() -> u64 { 0 }

// Inner-shareable under shared SMP (see `flush_tlb_all`).
#[cfg(all(target_os = "none", target_arch = "aarch64", kernel_smp_shared))]
pub fn flush_tlb_asid(asid: u16, target: TlbTarget) -> TlbFlush {
    akuma_cpu::barrier::dsb_ishst();
    match target {
        TlbTarget::AllCores => akuma_cpu::tlb::aside1is(asid),
        TlbTarget::ThisCore => akuma_cpu::tlb::aside1(asid),
    }
    TlbFlush::pending()
}

#[cfg(all(target_os = "none", target_arch = "aarch64", not(kernel_smp_shared)))]
pub fn flush_tlb_asid(asid: u16, _target: TlbTarget) -> TlbFlush {
    akuma_cpu::barrier::dsb_ishst();
    akuma_cpu::tlb::aside1(asid);
    TlbFlush::pending()
}

// No PCID support on this target (out of scope, see
// `proposals/AKUMA_MMU_ARCH_PORTABILITY.md` Phase 4), so an ASID-scoped flush
// has no cheaper x86_64 form than a full flush — a correct, more aggressive
// superset, same trade-off `flush_tlb_all`'s own x86_64 arm makes.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn flush_tlb_asid(_asid: u16, target: TlbTarget) -> TlbFlush {
    flush_tlb_all(target)
}

#[cfg(not(any(
    all(target_os = "none", target_arch = "aarch64"),
    all(target_os = "none", target_arch = "x86_64"),
)))]
pub fn flush_tlb_asid(_asid: u16, _target: TlbTarget) -> TlbFlush { TlbFlush::done() }

// Inner-shareable under shared SMP (see `flush_tlb_all`).
#[cfg(all(target_os = "none", target_arch = "aarch64", kernel_smp_shared))]
pub fn flush_tlb_page(va: usize, target: TlbTarget) -> TlbFlush {
    akuma_cpu::barrier::dsb_ishst();
    match target {
        TlbTarget::AllCores => akuma_cpu::tlb::vaae1is((va >> 12) as u64),
        TlbTarget::ThisCore => akuma_cpu::tlb::vaae1((va >> 12) as u64),
    }
    TlbFlush::pending()
}

#[cfg(all(target_os = "none", target_arch = "aarch64", not(kernel_smp_shared)))]
pub fn flush_tlb_page(va: usize, _target: TlbTarget) -> TlbFlush {
    akuma_cpu::barrier::dsb_ishst();
    akuma_cpu::tlb::vaae1((va >> 12) as u64);
    TlbFlush::pending()
}

#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn flush_tlb_page(va: usize, _target: TlbTarget) -> TlbFlush {
    akuma_cpu::tlb::invlpg(va);
    TlbFlush::pending()
}

#[cfg(not(any(
    all(target_os = "none", target_arch = "aarch64"),
    all(target_os = "none", target_arch = "x86_64"),
)))]
pub fn flush_tlb_page(_va: usize, _target: TlbTarget) -> TlbFlush { TlbFlush::done() }

// ============================================================================
// User Address Space Management
// ============================================================================

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use spinning_top::Spinlock;


use asid::AsidAllocator;

static ASID_ALLOCATOR: Spinlock<AsidAllocator> = Spinlock::new(AsidAllocator::new());

/// Rate-limited report that the ASID space is full. Non-zero means either genuine
/// concurrency above MAX_ASID or — far more likely — leaked ASIDs from address spaces
/// whose `Drop` never ran.
fn asid_exhausted_warn() {
    use core::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 5 || n.is_multiple_of(100) {
        // `safe_print!`, not an `ExecRuntime` hook: this is a fixed `&'static str`
        // on the path that reports the ASID space is exhausted, and the console is
        // what survives when allocation is what broke
        // (docs/reference/subsystems/console.md § "Printing rules"). It was the
        // last `runtime()` reference in this crate.
        crate::safe_print!(160,
            "[asid] EXHAUSTED: no free ASID (MAX_ASID=256) — address-space creation failing; \
             suspect leaked ASIDs from address spaces whose Drop never ran\n");
    }
}


/// Heap-free formatted print for address-space lifecycle tracing. The `[AS-*]`
/// lines are the correlation record for the page-table-UAF storm hunt
/// (docs/archive/PAGE_TABLE_UAF_BKL_STORM.md): a lockprobe capture gives the
/// stuck core's TTBR0 (ASID + L0 physical), and these lines are what link that
/// L0 back to a pid and a teardown path in the console log.
/// `#[inline]` is load-bearing across the crate boundary: callers build the
/// `Arguments` with `format_args!` at the call site, and it is only when this
/// body inlines to nothing (feature off) that LLVM can delete that construction
/// too. Inside one crate that happened for free.
#[inline]
pub fn as_trace(args: core::fmt::Arguments) {
    // Gated as a LIFECYCLE trace (2026-08-29). It was unconditional, and it is the
    // single largest console producer in the tree: `[AS-NEW]`/`[AS-EXEC]`/
    // `[AS-FREE]`/`[AS-DEFER]` fire on every address-space create, exec and free,
    // which is once or more per process. A plain boot-suite run emitted **1,342**
    // of these lines; an in-VM `-j4` build is orders more, and each costs ~160
    // bytes at ~2.4 us/byte (docs/archive/CONSOLE_LOG_COST.md).
    //
    // Address-space create/exec/free is exactly what `syscall_debug_info_enabled`
    // already covers — its own doc is about `[FORK-DBG]`/`[TRAMP]` costing ~20
    // serial lines per `fork()`. `lifecycle_trace_on()` folds to a compile-time
    // `false` without the `debug-info` feature, so with it off this call and its
    // `Arguments` construction disappear entirely.
    if crate::lifecycle_trace_on() {
        // Was a fifth function-local copy of the stack writer. `print_args` is the
        // `Arguments`-shaped entry point to the shared one — this helper takes
        // pre-built `Arguments` rather than being a macro, so it can't use
        // `safe_print!` directly.
        akuma_primitives::console::print_args::<160>(args);
    }
}

// ===== Per-core live-TTBR0 registry =====
//
// The page-table-UAF storm (docs/archive/PAGE_TABLE_UAF_BKL_STORM.md): an
// address space's page-table frames were freed and PMM-poisoned while some
// core's TTBR0_EL1 was still loaded with that L0 — the next TLB miss on that
// core (including the fetch of its own exception vector, which also goes
// through TTBR0) then walks poison and the core stops retiring instructions
// forever, taking the BKL with it. The software refcount over
// `UserAddressSpace` objects (SHARED_L0_TABLE) cannot see the *hardware*
// register, so every teardown path that can outrun a core's switch-out is a
// trigger (killer-side hard-terminate in `kill_thread_group`, the parent's
// wait4-reap → 10ms `reclaim_retired_processes` racing the exiting core's
// final switch, ...).
//
// This registry closes the gap at the free side. Every TTBR0_EL1 write
// publishes the new L0 base here, and `UserAddressSpace::drop` refuses to
// free page-table frames while any core still shows the dying L0 — the
// frames park in `PENDING_TTBR_FREES` and are freed by a later drain, once
// the holder has demonstrably moved off (every switch path reprograms TTBR0
// and republishes).
//
// Publish protocol (no unsafe instant): on a transition A→B the writer
// stores PREV=A, then ACTIVE=B, then executes the `msr`, then clears PREV.
// At every instant the live TTBR0 base is contained in {ACTIVE, PREV}, so a
// scanner that sees neither slot equal to X knows TTBR0 != X — and since a
// dying L0 can never be *newly* installed (installing requires a live
// `UserAddressSpace`, whose existence forbids the drop that runs this scan),
// "not held now" is stable for the dying table, not merely a snapshot.

/// Per-core slots sized for the largest supported SMP configuration.
pub const TTBR_TRACK_CORES: usize = 8;

const L0_BASE_MASK: u64 = 0x0000_FFFF_FFFF_F000;

#[allow(clippy::declare_interior_mutable_const)]
const ATOMIC_U64_ZERO: AtomicU64 = AtomicU64::new(0);
/// L0 base currently programmed in each core's TTBR0_EL1 (0 = boot/kernel
/// table or never published — the boot table is never freed, so 0 is safe).
static ACTIVE_L0: [AtomicU64; TTBR_TRACK_CORES] = [ATOMIC_U64_ZERO; TTBR_TRACK_CORES];
/// L0 base a core is transitioning *away from* (non-zero only inside the
/// few-instruction msr window of a TTBR0 write).
static PREV_L0: [AtomicU64; TTBR_TRACK_CORES] = [ATOMIC_U64_ZERO; TTBR_TRACK_CORES];

/// Begin a TTBR0 transition on this core: publish the new L0 while keeping
/// the old one visible. Call immediately BEFORE the `msr ttbr0_el1` (IRQs
/// must already be masked so the pair can't migrate cores). Returns the core
/// index to pass to [`publish_l0_end`].
#[inline]
pub fn publish_l0_begin(new_ttbr0: u64) -> usize {
    let core = (akuma_bkl::bkl::current_core_id() as usize) % TTBR_TRACK_CORES;
    let old = ACTIVE_L0[core].load(Ordering::Relaxed);
    PREV_L0[core].store(old, Ordering::SeqCst);
    ACTIVE_L0[core].store(new_ttbr0 & L0_BASE_MASK, Ordering::SeqCst);
    core
}

/// Finish a TTBR0 transition: the `msr` has retired, only ACTIVE covers us now.
#[inline]
pub fn publish_l0_end(core: usize) {
    PREV_L0[core].store(0, Ordering::SeqCst);
}

/// Is any core's live TTBR0 (or in-flight transition) on this L0 base?
/// Returns the first core index found, for diagnostics.
pub fn any_core_on_l0(l0_phys: usize) -> Option<usize> {
    let l0 = l0_phys as u64 & L0_BASE_MASK;
    if l0 == 0 {
        return None;
    }
    for (c, (active, prev)) in ACTIVE_L0.iter().zip(PREV_L0.iter()).enumerate() {
        if active.load(Ordering::SeqCst) == l0 || prev.load(Ordering::SeqCst) == l0 {
            return Some(c);
        }
    }
    None
}

/// Test hook: fake a core's published live L0 (boot-suite self-tests only —
/// they can't get a peer core to genuinely park its TTBR0 on a test table).
#[doc(hidden)]
pub fn test_publish_core_l0(core: usize, l0_phys: usize) {
    ACTIVE_L0[core % TTBR_TRACK_CORES].store(l0_phys as u64 & L0_BASE_MASK, Ordering::SeqCst);
}

/// Ring of the most recently freed L0 bases (F8 tripwire). The scheduler checks
/// an incoming context's TTBR0 against this before installing it: a hit means a
/// saved context still references a torn-down address space, which is the freed-
/// L0-into-TTBR0 wedge (`COW_PILE_AUDIT.md` §10) about to happen. Entries are
/// cleared when the PMM hands the frame back out as a NEW L0 (`UserAddressSpace::
/// new`), so reuse doesn't false-positive; reuse as a non-L0 page keeps the entry,
/// which is exactly when installing it is most destructive.
const FREED_L0_RING: usize = 16;
static RECENT_FREED_L0: [AtomicU64; FREED_L0_RING] = [ATOMIC_U64_ZERO; FREED_L0_RING];
static RECENT_FREED_L0_NEXT: AtomicUsize = AtomicUsize::new(0);

fn note_freed_l0(l0_base: u64) {
    let i = RECENT_FREED_L0_NEXT.fetch_add(1, Ordering::Relaxed) % FREED_L0_RING;
    RECENT_FREED_L0[i].store(l0_base, Ordering::SeqCst);
}

fn unnote_freed_l0(l0_base: u64) {
    for slot in &RECENT_FREED_L0 {
        let _ = slot.compare_exchange(l0_base, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

/// Was `l0_base` (a masked L0 base) freed recently and not since re-issued as an
/// L0? Scheduler-side arm of the F8 tripwire; lock-free, bounded.
pub fn l0_recently_freed(l0_base: u64) -> bool {
    l0_base != 0
        && RECENT_FREED_L0
            .iter()
            .any(|s| s.load(Ordering::SeqCst) == l0_base)
}

/// Temporary address-space lifecycle accounting, behind `leak-instr` (off by
/// default). Root-caused the self-host heap leak; kept because
/// `free_now_enter == free_now_exit` and `[ASSTUCK] in_flight=0` are the
/// regression gate for it (`docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md`).
#[cfg(feature = "leak-instr")]
#[allow(clippy::pub_underscore_fields, clippy::redundant_pub_crate)]
mod instr {
    use super::*;

    /// ── Temporary address-space lifecycle accounting (heap-leak hunt) ──
    ///
    /// `user_frames` (`BTreeMap<usize, u32>`) is where the self-host heap leak
    /// lives — its 144-byte leaf and 240-byte internal nodes are the leaked class
    /// (`docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md`). These say whether the maps
    /// are stranded because address spaces are not dropped, or because entries are
    /// never removed from maps that are.
    pub(super) static AS_NEW: AtomicUsize = AtomicUsize::new(0);
    pub(super) static AS_NEW_SHARED: AtomicUsize = AtomicUsize::new(0);
    pub(super) static AS_DROP: AtomicUsize = AtomicUsize::new(0);
    pub(super) static AS_DROP_SHARED: AtomicUsize = AtomicUsize::new(0);
    // The `user_frames` counters moved to `akuma-user-space` with the map they
    // count; the hooks below forward. Only the address-space *lifecycle*
    // counters stay here, because they count this crate's own events.
    /// Drop-completion bracket. This kernel does not unwind — a killed or abandoned
    /// teardown thread skips destructors — so a `Drop` that is entered and never
    /// left leaks the `user_frames` map it is holding in a local, with every other
    /// counter reading clean. Enter is `AS_DROP + AS_DROP_SHARED`.
    pub(super) static AS_DROP_EXIT: AtomicUsize = AtomicUsize::new(0);
    /// The same bracket around the inner window: `free_as_frames_now` subtracts the
    /// map's length on entry and then frees pages one at a time.
    pub(super) static FREE_NOW_ENTER: AtomicUsize = AtomicUsize::new(0);
    pub(super) static FREE_NOW_EXIT: AtomicUsize = AtomicUsize::new(0);

    /// In-flight `Drop` ledger: who entered and has not left.
    ///
    /// The bracket counters say *how many* teardowns went missing; this says
    /// *which*. A slot is claimed on `Drop` entry and released on exit, so anything
    /// still occupied at dump time is a teardown that was abandoned mid-flight,
    /// named by the address space it was tearing down and the thread doing it.
    /// Fixed-size and lock-free — this runs on the teardown path.
    pub(super) const DROP_SLOTS: usize = 512;
    /// Occupancy + owner: 0 = free, otherwise `tid + 1`.
    pub(super) static DROP_TID: [AtomicUsize; DROP_SLOTS] = [const { AtomicUsize::new(0) }; DROP_SLOTS];
    pub(super) static DROP_L0: [AtomicUsize; DROP_SLOTS] = [const { AtomicUsize::new(0) }; DROP_SLOTS];
    pub(super) static DROP_ASID: [AtomicUsize; DROP_SLOTS] = [const { AtomicUsize::new(0) }; DROP_SLOTS];
    pub(super) static DROP_UF_LEN: [AtomicUsize; DROP_SLOTS] = [const { AtomicUsize::new(0) }; DROP_SLOTS];
    /// What the slot is bracketing: 0 = `UserAddressSpace::drop`, 1 =
    /// `free_as_frames_now` reached from a drop, 2 = reached from a pending drain.
    pub(super) static DROP_KIND: [AtomicUsize; DROP_SLOTS] = [const { AtomicUsize::new(0) }; DROP_SLOTS];
    /// Drops that found every slot occupied — i.e. already-stuck entries crowding
    /// the ledger out. Non-zero means the ledger is saturated, not that nothing leaked.
    pub(super) static DROP_LEDGER_OVERFLOW: AtomicUsize = AtomicUsize::new(0);

    /// Claim a ledger slot for this teardown. `None` when the ledger is full.
    pub(super) fn drop_ledger_enter(l0: usize, asid: u16, uf_len: usize, kind: usize) -> Option<usize> {
        let tid = akuma_primitives::preempt::current_tid() + 1;
        for i in 0..DROP_SLOTS {
            if DROP_TID[i]
                .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                DROP_L0[i].store(l0, Ordering::Relaxed);
                DROP_ASID[i].store(asid as usize, Ordering::Relaxed);
                DROP_UF_LEN[i].store(uf_len, Ordering::Relaxed);
                DROP_KIND[i].store(kind, Ordering::Relaxed);
                return Some(i);
            }
        }
        DROP_LEDGER_OVERFLOW.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub(super) fn drop_ledger_exit(slot: Option<usize>) {
        if let Some(i) = slot {
            DROP_TID[i].store(0, Ordering::Relaxed);
        }
    }

    /// Print every teardown still in flight, and the total `user_frames` entries
    /// they are holding. At idle that total should be 0.
    pub fn dump_stuck_drops() {
        let mut stuck = 0usize;
        let mut held = 0usize;
        for i in 0..DROP_SLOTS {
            let tid = DROP_TID[i].load(Ordering::Relaxed);
            if tid == 0 {
                continue;
            }
            stuck += 1;
            let uf = DROP_UF_LEN[i].load(Ordering::Relaxed);
            held += uf;
            let kind = match DROP_KIND[i].load(Ordering::Relaxed) {
                0 => "as_drop",
                1 => "free_now/from_drop",
                _ => "free_now/from_drain",
            };
            if stuck <= 12 {
                akuma_primitives::safe_print!(160,
                    "[ASSTUCK] slot={} kind={} tid={} l0=0x{:x} asid=0x{:x} uf_entries={}\n",
                    i, kind, tid - 1, DROP_L0[i].load(Ordering::Relaxed),
                    DROP_ASID[i].load(Ordering::Relaxed), uf);
            }
        }
        akuma_primitives::safe_print!(128,
            "[ASSTUCK] in_flight={} holding_uf_entries={} ledger_overflow={}\n",
            stuck, held, DROP_LEDGER_OVERFLOW.load(Ordering::Relaxed));
    }

    /// (drop exits, free_as_frames_now entries, free_as_frames_now exits).
    pub fn as_drop_bracket_stats() -> (usize, usize, usize) {
        (
            AS_DROP_EXIT.load(Ordering::Relaxed),
            FREE_NOW_ENTER.load(Ordering::Relaxed),
            FREE_NOW_EXIT.load(Ordering::Relaxed),
        )
    }

    /// (inserts, removals, freed at teardown, dropped with the struct, silently dropped).
    ///
    /// Forwarded: the counters live with the map, in `akuma-user-space`.
    pub fn uf_flow_stats() -> (usize, usize, usize, usize, usize) {
        akuma_user_space::instr::uf_flow_stats()
    }

    /// (new, new_shared, dropped, dropped_shared, live user_frames entries).
    pub fn as_lifecycle_stats() -> (usize, usize, usize, usize, usize) {
        (
            AS_NEW.load(Ordering::Relaxed),
            AS_NEW_SHARED.load(Ordering::Relaxed),
            AS_DROP.load(Ordering::Relaxed),
            AS_DROP_SHARED.load(Ordering::Relaxed),
            akuma_user_space::instr::uf_live_entries(),
        )
    }

    /// Ledger handle: a slot index while instrumented.
    pub(super) type Ledger = Option<usize>;

    pub(super) fn as_new() { AS_NEW.fetch_add(1, Ordering::Relaxed); }
    pub(super) fn as_new_shared() { AS_NEW_SHARED.fetch_add(1, Ordering::Relaxed); }
    pub(super) fn as_drop_enter(shared: bool, l0: usize, asid: u16, uf_len: usize) -> Ledger {
        if shared { AS_DROP_SHARED.fetch_add(1, Ordering::Relaxed); }
        else { AS_DROP.fetch_add(1, Ordering::Relaxed); }
        drop_ledger_enter(l0, asid, uf_len, 0)
    }
    pub(super) fn as_drop_exit(l: Ledger) {
        AS_DROP_EXIT.fetch_add(1, Ordering::Relaxed);
        drop_ledger_exit(l);
    }
    pub(super) fn free_now_enter(l0: usize, uf_len: usize, kind: usize) -> Ledger {
        FREE_NOW_ENTER.fetch_add(1, Ordering::Relaxed);
        akuma_user_space::instr::uf_freed_now(uf_len);
        drop_ledger_enter(l0, 0, uf_len, kind)
    }
    pub(super) fn free_now_exit(l: Ledger) {
        FREE_NOW_EXIT.fetch_add(1, Ordering::Relaxed);
        drop_ledger_exit(l);
    }
    pub(super) fn uf_drop_remainder(n: usize) {
        akuma_user_space::instr::uf_drop_remainder(n);
    }
    pub(super) fn uf_silent(n: usize) {
        akuma_user_space::instr::uf_silent(n);
    }
}

/// `leak-instr` off: every hook compiles to nothing.
#[cfg(not(feature = "leak-instr"))]
#[allow(clippy::let_unit_value)]
mod instr {
    pub(super) type Ledger = ();
    #[inline(always)] pub(super) fn as_new() {}
    #[inline(always)] pub(super) fn as_new_shared() {}
    #[inline(always)] pub(super) fn as_drop_enter(_s: bool, _l0: usize, _a: u16, _n: usize) -> Ledger {}
    #[inline(always)] pub(super) fn as_drop_exit(_l: Ledger) {}
    #[inline(always)] pub(super) fn free_now_enter(_l0: usize, _n: usize, _k: usize) -> Ledger {}
    #[inline(always)] pub(super) fn free_now_exit(_l: Ledger) {}
    #[inline(always)] pub(super) fn uf_drop_remainder(_n: usize) {}
    #[inline(always)] pub(super) fn uf_silent(_n: usize) {}
}

/// Instrumentation accessors. Zeros / no-ops unless `leak-instr` is enabled.
#[cfg(not(feature = "leak-instr"))]
pub fn as_lifecycle_stats() -> (usize, usize, usize, usize, usize) { (0, 0, 0, 0, 0) }
#[cfg(not(feature = "leak-instr"))]
pub fn uf_flow_stats() -> (usize, usize, usize, usize, usize) { (0, 0, 0, 0, 0) }
#[cfg(not(feature = "leak-instr"))]
pub fn as_drop_bracket_stats() -> (usize, usize, usize) { (0, 0, 0) }
#[cfg(not(feature = "leak-instr"))]
pub fn dump_stuck_drops() {}
#[cfg(feature = "leak-instr")]
pub use instr::{as_drop_bracket_stats, as_lifecycle_stats, dump_stuck_drops, uf_flow_stats};

/// Frames of a torn-down address space whose free was deferred because some
/// core's TTBR0 was still resident on the L0 (see module comment above).
struct PendingAsFree {
    l0_frame: PhysFrame,
    user_frames: BTreeMap<usize, u32>,
    pt_frames: Vec<PhysFrame>,
    asid: u16,
}

static PENDING_TTBR_FREES: Spinlock<Vec<PendingAsFree>> = Spinlock::new(Vec::new());
/// Lock-free mirror of `PENDING_TTBR_FREES.len()` so hot paths can skip the
/// drain without taking the lock.
static PENDING_TTBR_FREE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// (entries, user frames, page-table frames) parked awaiting a TTBR-clear
/// drain. Diagnostics + boot-suite PMM-conservation accounting.
pub fn pending_ttbr_free_stats() -> (usize, usize, usize) {
    with_irqs_disabled(|| {
        let pending = PENDING_TTBR_FREES.lock();
        let mut uf = 0;
        let mut pf = 0;
        for e in pending.iter() {
            uf += e.user_frames.len();
            pf += e.pt_frames.len() + 1; // + the L0 itself
        }
        (pending.len(), uf, pf)
    })
}

/// Free the frames of a dead address space, or park them if some core's
/// TTBR0 is still resident on `l0_addr`. The sole exit for page-table frames
/// from both `UserAddressSpace::drop` branches (owner-immediate and
/// last-shared-view), so the liveness gate cannot be bypassed.
fn free_or_defer_as_frames(
    l0_addr: usize,
    asid: u16,
    l0_frame: PhysFrame,
    user_frames: BTreeMap<usize, u32>,
    pt_frames: Vec<PhysFrame>,
    path: &str,
) {
    if let Some(core) = any_core_on_l0(l0_addr) {
        as_trace(format_args!(
            "[AS-FREE-DEFER] l0=0x{:x} asid=0x{:x} path={} held_by_core={}\n",
            l0_addr, asid, path, core));
        with_irqs_disabled(|| {
            let mut pending = PENDING_TTBR_FREES.lock();
            pending.push(PendingAsFree { l0_frame, user_frames, pt_frames, asid });
            PENDING_TTBR_FREE_COUNT.store(pending.len(), Ordering::Release);
        });
        return;
    }
    // Second gate, saved contexts (the F8 fix, `COW_PILE_AUDIT.md` §10): a thread
    // preempted before its exit path's `deactivate()` — or killed without ever
    // running it — parks with the dying L0 in its SAVED `ctx.ttbr0`, where the
    // per-core scan above cannot see it and the scheduler SGI will install it
    // verbatim on switch-in. Freeing under such a reference wedges the machine
    // (recursive fetch abort at vector+0x200) the moment that slot is scheduled.
    // Park the frames instead; the reference dissolves at the slot's next
    // context save (post-`deactivate()` switch-out), recycle (context zeroed) or
    // spawn re-seed, and the next drain frees them.
    if let Some((tid, state)) = crate::any_saved_ctx_on_l0(l0_addr as u64 & L0_BASE_MASK) {
        as_trace(format_args!(
            "[AS-FREE-DEFER] l0=0x{:x} asid=0x{:x} path={} held_by_ctx tid={} state={}\n",
            l0_addr, asid, path, tid, state));
        with_irqs_disabled(|| {
            let mut pending = PENDING_TTBR_FREES.lock();
            pending.push(PendingAsFree { l0_frame, user_frames, pt_frames, asid });
            PENDING_TTBR_FREE_COUNT.store(pending.len(), Ordering::Release);
        });
        return;
    }
    as_trace(format_args!("[AS-FREE] l0=0x{:x} asid=0x{:x} path={} core={}\n",
        l0_addr, asid, path, akuma_bkl::bkl::current_core_id()));
    free_as_frames_now(l0_frame, &user_frames, &pt_frames, 1);
}

/// Unconditional frame release — only reachable through the liveness gate.
fn free_as_frames_now(l0_frame: PhysFrame, user_frames: &BTreeMap<usize, u32>, pt_frames: &[PhysFrame], kind: usize) {
    #[allow(clippy::let_unit_value)]
    let _ledger = instr::free_now_enter(l0_frame.addr, user_frames.len(), kind);
    note_freed_l0(l0_frame.addr as u64 & L0_BASE_MASK);
    let tid = akuma_primitives::preempt::current_tid() as u32;
    {
        let _irq = IrqGuard::new();
        // Free each distinct physical page exactly ONCE. `user_frames` counts
        // how many VAs map a PA, not how many times it was allocated.
        for (&addr, &_count) in user_frames {
            akuma_pmm::free_page_at(addr, tid, akuma_pmm::FreeSite::AsTeardown);
        }
    }
    {
        let _irq = IrqGuard::new();
        for frame in pt_frames { akuma_pmm::free_page_at(frame.addr, tid, akuma_pmm::FreeSite::AsTeardown); }
    }
    akuma_pmm::free_page_at(l0_frame.addr, tid, akuma_pmm::FreeSite::AsTeardown);
    instr::free_now_exit(_ledger);
}

/// Re-check parked address-space frames and free the ones whose L0 no core
/// holds any more. Returns how many entries were released. Called
/// opportunistically from `UserAddressSpace::drop` and from the periodic
/// retired-process reclaim; boot-suite tests call it directly.
pub fn drain_pending_ttbr_frees() -> usize {
    if PENDING_TTBR_FREE_COUNT.load(Ordering::Acquire) == 0 {
        return 0;
    }
    // A TERMINATED thread can be reaped at any yield and will never resume, so a
    // multi-thousand-page free started here is abandoned mid-loop — and the entry
    // it owns has already left the list, so nothing can ever find it again. That
    // is the self-host heap leak: 82 abandoned drains per clean build holding
    // 1.47 M `user_frames` entries (`docs/archive/SELFHOST_KERNEL_HEAP_LEAK.md`).
    //
    // The split this restores is `process::reclaim`'s own: terminal sites
    // *request* reclamation, they do not *perform* it. Four non-terminal
    // collectors remain — both idle loops, `netpoll_maint`, and the allocator's
    // pressure path (which runs on the allocating thread, non-terminal by
    // construction) — plus every address-space drop that is not itself on a
    // terminal thread.
    if thread_is_terminal() {
        return 0;
    }
    let mut n = 0;
    // ONE entry per iteration, claimed under the lock and freed outside it.
    // Draining the whole ready set into a local `Vec` first meant an abandoned
    // sweep lost every entry it was carrying, not just the one in flight.
    while let Some(e) = take_one_ready_ttbr_free() {
        as_trace(format_args!("[AS-FREE] l0=0x{:x} asid=0x{:x} path=drained core={}\n",
            e.l0_frame.addr, e.asid, akuma_bkl::bkl::current_core_id()));
        free_as_frames_now(e.l0_frame, &e.user_frames, &e.pt_frames, 2);
        n += 1;
    }
    n
}

/// Remove and return one parked address space whose L0 no core's TTBR0 and no
/// thread's saved context still names, or `None` when none is ready.
///
/// Both gates are the same pair `free_or_defer_as_frames` applies, and both must
/// stay: releasing on the core check alone re-opens the F8 window this list exists
/// to close.
fn take_one_ready_ttbr_free() -> Option<PendingAsFree> {
    with_irqs_disabled(|| {
        let mut pending = PENDING_TTBR_FREES.lock();
        let mut i = 0;
        while i < pending.len() {
            let l0 = pending[i].l0_frame.addr;
            if any_core_on_l0(l0).is_none()
                && crate::any_saved_ctx_on_l0(l0 as u64 & L0_BASE_MASK).is_none()
            {
                let e = pending.swap_remove(i);
                PENDING_TTBR_FREE_COUNT.store(pending.len(), Ordering::Release);
                return Some(e);
            }
            i += 1;
        }
        PENDING_TTBR_FREE_COUNT.store(pending.len(), Ordering::Release);
        None
    })
}

/// Tracks shared L0 page table reference counts and deferred frame lists.
///
/// When CLONE_THREAD creates shared views of an address space, we need to
/// ensure the page tables aren't freed until the last thread exits.
/// If the owner (shared=false) drops first, its frames are stored here
/// for the last shared view to free.
struct SharedL0Entry {
    ref_count: usize,
    deferred_user_frames: Option<BTreeMap<usize, u32>>,
    deferred_pt_frames: Option<Vec<PhysFrame>>,
    deferred_l0: Option<PhysFrame>,
}

/// Temporary: an entry removed from `SHARED_L0_TABLE` while it still carries a
/// deferred map drops that map here, silently. Without this the leak-hunt
/// residual counts those entries as still live. Remove with the counters.
impl Drop for SharedL0Entry {
    fn drop(&mut self) {
        if let Some(uf) = &self.deferred_user_frames {
            instr::uf_silent(uf.len());
        }
    }
}

static SHARED_L0_TABLE: Spinlock<BTreeMap<usize, SharedL0Entry>> =
    Spinlock::new(BTreeMap::new());

/// Diagnostics: (entries, deferred user frames, deferred page-table frames)
/// currently parked in `SHARED_L0_TABLE`. A non-zero deferred count with no
/// live threads means an owner died while a shared view leaked — those frames
/// are stranded until the view drops. Boot-suite PMM-conservation tests use
/// this to tell "frames parked here" apart from a genuine PMM leak.
pub fn shared_l0_stats() -> (usize, usize, usize) {
    with_irqs_disabled(|| {
        let table = SHARED_L0_TABLE.lock();
        let mut deferred_user = 0usize;
        let mut deferred_pt = 0usize;
        for e in table.values() {
            if let Some(uf) = &e.deferred_user_frames {
                deferred_user += uf.len();
            }
            if let Some(pf) = &e.deferred_pt_frames {
                deferred_pt += pf.len();
            }
        }
        (table.len(), deferred_user, deferred_pt)
    })
}

/// Copy `dst.len()` bytes **out of** the physical frame at `pa`.
///
/// Safe because the range is checked against the RAM the PMM manages
/// ([`akuma_pmm::contains`]) before anything is dereferenced: an arbitrary
/// `usize` either lands in real RAM or is refused with `false`. That check is
/// what the `phys_to_virt` + `slice::from_raw_parts` idiom at the call sites
/// never had.
///
/// # What this does not promise
///
/// A *snapshot*, not a stable view. If the frame is also mapped writable into a
/// live address space — which is exactly the case for `MAP_SHARED` write-back —
/// userspace may be storing into it while this runs, so individual bytes may be
/// pre- or post-store. That is inherent to writing a shared mapping back to disk
/// and is what Linux's own write-back does; the improvement over the old code is
/// that this no longer hands the compiler a `&[u8]` it is entitled to assume is
/// unchanging.
#[must_use]
pub fn copy_from_phys(pa: usize, dst: &mut [u8]) -> bool {
    if dst.is_empty() {
        return true;
    }
    if !akuma_pmm::contains(pa, dst.len()) {
        return false;
    }
    // SAFETY: the whole range is inside PMM-managed RAM, which is identity-mapped
    // and readable; `dst` is a live exclusive slice and cannot overlap it (it is
    // kernel heap or stack, never a raw frame view).
    unsafe {
        core::ptr::copy_nonoverlapping(phys_to_virt(pa).cast_const(), dst.as_mut_ptr(), dst.len());
    }
    true
}

/// Copy `src` **into** the physical frame at `pa`. The mirror of
/// [`copy_from_phys`], with the same bounds check.
///
/// Intended for a frame the caller has just allocated and not yet mapped, where
/// there is no other reference to race with. Writing into a frame that *is*
/// mapped elsewhere will be seen by whoever has it, which is occasionally the
/// point and otherwise a bug.
#[must_use]
pub fn copy_to_phys(pa: usize, src: &[u8]) -> bool {
    if src.is_empty() {
        return true;
    }
    if !akuma_pmm::contains(pa, src.len()) {
        return false;
    }
    // SAFETY: as `copy_from_phys`, with the roles reversed.
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), phys_to_virt(pa), src.len());
    }
    true
}

/// Copy one `PAGE_SIZE` frame from `src_pa` to `dst_pa`.
///
/// The fork path's page-by-page copy: parent frame -> freshly allocated child
/// frame. Both must be distinct PMM-managed frames — `false` if either is
/// out of range or they are the same frame (an overlapping `copy_nonoverlapping`
/// is UB, and a self-copy is always a caller bug here).
///
/// Replaces the `phys_to_virt` + `copy_nonoverlapping` idiom that stood at four
/// `unsafe` sites in `akuma-exec/src/process/mod.rs`.
#[must_use]
pub fn copy_phys_page(dst_pa: usize, src_pa: usize) -> bool {
    let dst = dst_pa & !(PAGE_SIZE - 1);
    let src = src_pa & !(PAGE_SIZE - 1);
    if dst == src
        || !akuma_pmm::contains(dst, PAGE_SIZE)
        || !akuma_pmm::contains(src, PAGE_SIZE)
    {
        return false;
    }
    // SAFETY: both ranges are page-aligned, `PAGE_SIZE` long, inside PMM RAM
    // (identity-mapped and readable/writable) and — checked above — disjoint.
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(src).cast_const(),
            phys_to_virt(dst),
            PAGE_SIZE,
        );
    }
    true
}

/// Write a plain `#[repr(C)]` value into the physical frame at `pa`.
///
/// For a struct the kernel drops into a frame it owns and has not mapped yet —
/// `ProcessInfo` at fork/exec. `T` must be `Copy` plain-old-data; `false` if
/// `size_of::<T>()` bytes from `pa` leave PMM RAM.
#[must_use]
pub fn write_phys<T: Copy>(pa: usize, val: &T) -> bool {
    if !akuma_pmm::contains(pa, core::mem::size_of::<T>()) {
        return false;
    }
    // SAFETY: `pa` holds `size_of::<T>()` bytes of PMM RAM (identity-mapped,
    // writable); `val` is a live `&T`. Alignment: `phys_to_virt(pa)` of a frame
    // base is page-aligned, and every call site passes a frame base.
    unsafe {
        core::ptr::write(phys_to_virt(pa).cast::<T>(), *val);
    }
    true
}

/// Run `f` with a `&mut [u8]` view of `[offset, offset + len)` inside the
/// physical frame at `pa`, reached through its kernel identity mapping.
///
/// For a freshly-allocated frame the kernel is filling before it maps it —
/// demand-paging's per-page file read (`process::lazy_prefault`). `None` if `pa`
/// is not a page-aligned PMM frame or the window leaves it.
///
/// Replaces a `phys_to_virt` + `slice::from_raw_parts_mut` at the one prefault
/// site; the frame is unpublished, so `f` is the only accessor for its duration.
#[must_use]
pub fn with_phys_bytes_mut<R>(
    pa: usize,
    offset: usize,
    len: usize,
    f: impl FnOnce(&mut [u8]) -> R,
) -> Option<R> {
    let end = offset.checked_add(len)?;
    if end > PAGE_SIZE || (pa & (PAGE_SIZE - 1)) != 0 || !akuma_pmm::contains(pa, PAGE_SIZE) {
        return None;
    }
    // SAFETY: `pa` is a page-aligned PMM frame (identity-mapped, writable); the
    // window is inside that page; the frame is not published to any page table,
    // so `f` holds the only reference for its duration.
    let buf = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pa).add(offset), len) };
    Some(f(buf))
}

#[cfg(target_arch = "aarch64")]
pub struct UserAddressSpace {
    l0_frame: PhysFrame,
    /// Which physical frames this address space holds, and how many VAs map
    /// each. Arch-neutral and host-tested — `akuma-user-space`. Everything left
    /// in this struct is the AArch64 page-table walker.
    ledger: FrameLedger,
    asid: u16,
}

#[cfg(target_arch = "aarch64")]
impl UserAddressSpace {
    pub fn new() -> Option<Self> {
        let l0_frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new)?;
        // This frame is a live L0 again — drop it from the freed-L0 tripwire ring
        // so the scheduler doesn't flag the new owner's legitimate installs.
        unnote_freed_l0(l0_frame.addr as u64 & L0_BASE_MASK);
        track_frame(l0_frame, FrameSource::UserPageTable);
        let asid = match with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc()) {
            Some(a) => a,
            // Exhaustion used to propagate silently through `?`: address-space creation
            // just started failing, so fork/exec returned an error with nothing in the
            // log to say why. With only MAX_ASID=256 slots and a build that spawns
            // thousands of processes, a single missed `UserAddressSpace::drop` leaks an
            // ASID permanently, and enough of them wedge process creation invisibly.
            None => {
                asid_exhausted_warn();
                return None;
            }
        };
        let mut addr_space = Self {
            l0_frame,
            ledger: FrameLedger::new(false),
            asid,
        };
        addr_space.add_kernel_mappings().ok()?;
        instr::as_new();
        Some(addr_space)
    }

    /// Create a shared view of an existing address space (for CLONE_THREAD).
    /// Uses the same L0 page table; Drop will NOT free the pages.
    pub fn new_shared(parent_l0_phys: usize) -> Option<Self> {
        let asid = match with_irqs_disabled(|| ASID_ALLOCATOR.lock().alloc()) {
            Some(a) => a,
            // Exhaustion used to propagate silently through `?`: address-space creation
            // just started failing, so fork/exec returned an error with nothing in the
            // log to say why. With only MAX_ASID=256 slots and a build that spawns
            // thousands of processes, a single missed `UserAddressSpace::drop` leaks an
            // ASID permanently, and enough of them wedge process creation invisibly.
            None => {
                asid_exhausted_warn();
                return None;
            }
        };
        with_irqs_disabled(|| {
            let mut table = SHARED_L0_TABLE.lock();
            table.entry(parent_l0_phys)
                .and_modify(|e| e.ref_count += 1)
                .or_insert(SharedL0Entry {
                    ref_count: 1,
                    deferred_user_frames: None,
                    deferred_pt_frames: None,
                    deferred_l0: None,
                });
        });
        instr::as_new_shared();
        Some(Self {
            l0_frame: PhysFrame { addr: parent_l0_phys },
            ledger: FrameLedger::new(true),
            asid,
        })
    }

    fn add_kernel_mappings(&self) -> Result<(), &'static str> {
        let l1_frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("Failed to allocate L1 table")?;
        track_frame(l1_frame, FrameSource::UserPageTable);
        self.ledger.track_page_table_frame(l1_frame);

        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        unsafe {
            let l1_entry = (l1_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l0_ptr, l1_entry);
        }

        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("Failed to allocate L2 table")?;
        track_frame(l2_frame, FrameSource::UserPageTable);
        self.ledger.track_page_table_frame(l2_frame);

        unsafe {
            let l2_entry = (l2_frame.addr as u64) | flags::VALID | flags::TABLE;
            core::ptr::write_volatile(l1_ptr.add(0), l2_entry);
        }

        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let _ = l2_ptr; // L1[0]'s L2 is now empty; all devices are under L0[1].

        // L0[1] -> shared device L1 table (all devices at VA 0x80_0000_0000+).
        // These pages are shared across all user address spaces and must NOT be
        // pushed to page_table_frames (they are never freed).
        let dev_l1_phys = SHARED_DEV_L1_PHYS.load(Ordering::Acquire);
        if dev_l1_phys != 0 {
            unsafe {
                let dev_l0_entry = (dev_l1_phys as u64) | flags::VALID | flags::TABLE;
                core::ptr::write_volatile(l0_ptr.add(1), dev_l0_entry);
            }
        }

        // Identity-map the full RAM range.
        // Use L2 tables with 2MB blocks covering the full RAM size, so that
        // user MAP_FIXED in this range can shatter individual blocks.
        // The full RAM range must be identity-mapped so that phys_to_virt()
        // works for any PMM-allocated page regardless of which TTBR0 is active.
        let ram_base = RAM_BASE.load(Ordering::Acquire);
        let ram_size = RAM_SIZE.load(Ordering::Acquire);
        let ram_end = ram_base + ram_size;

        if ram_size > 0 {
            let kernel_ram_flags = flags::VALID | flags::BLOCK | flags::AF
                | attr_index(MAIR_NORMAL_WB) | flags::UXN | flags::SH_INNER | (0b00 << 6);

            // Calculate range of 1GB L1 entries to fill
            let start_l1_idx = (ram_base >> 30) & 0x1FF;
            let end_l1_idx = ((ram_end - 1) >> 30) & 0x1FF;

            for l1_idx in start_l1_idx..=end_l1_idx {
                let l2_ram_frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("Failed to allocate kernel RAM L2 table")?;
                track_frame(l2_ram_frame, FrameSource::UserPageTable);
                self.ledger.track_page_table_frame(l2_ram_frame);

                unsafe {
                    let l2_ram_entry = (l2_ram_frame.addr as u64) | flags::VALID | flags::TABLE;
                    core::ptr::write_volatile(l1_ptr.add(l1_idx), l2_ram_entry);

                    let l2_ram_ptr = phys_to_virt(l2_ram_frame.addr) as *mut u64;
                    
                    // Fill this 1GB L2 table with 2MB blocks (up to 512 blocks)
                    for i in 0..512u64 {
                        let pa = ((l1_idx as usize) << 30) | ((i as usize) << 21);
                        // Only map if this 2MB block is within the RAM range
                        if pa >= ram_base && pa < ram_end {
                            core::ptr::write_volatile(l2_ram_ptr.add(i as usize), (pa as u64) | kernel_ram_flags);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn ttbr0(&self) -> u64 {
        ((self.asid as u64) << 48) | (self.l0_frame.addr as u64)
    }

    pub fn l0_phys(&self) -> usize { self.l0_frame.addr }

    pub fn is_shared(&self) -> bool { self.ledger.is_shared() }

    pub fn asid(&self) -> u16 { self.asid }

    pub fn map_page(&mut self, va: usize, pa: usize, user_flags: u64) -> Result<(), &'static str> {
        if va & (PAGE_SIZE - 1) != 0 || pa & (PAGE_SIZE - 1) != 0 { return Err("Addresses must be page-aligned"); }
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;

        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        let l1_frame = self.get_or_create_table(l0_ptr, l0_idx)?;
        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = self.get_or_create_table(l1_ptr, l1_idx)?;
        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let l3_frame = self.get_or_create_table(l2_ptr, l2_idx)?;
        let l3_ptr = phys_to_virt(l3_frame.addr) as *mut u64;

        let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags;
        unsafe { l3_ptr.add(l3_idx).write_volatile(entry); }
        Ok(())
    }

    fn get_or_create_table(&mut self, table_ptr: *mut u64, idx: usize) -> Result<PhysFrame, &'static str> {
        unsafe {
            let entry = table_ptr.add(idx).read_volatile();
            if entry & flags::VALID != 0 {
                if entry & flags::TABLE == 0 {
                    let frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("Out of memory for page table")?;
                    track_frame(frame, FrameSource::UserPageTable);
                    self.ledger.track_page_table_frame(frame);
                    shatter_block_to_pages(frame.addr, entry);
                    let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                    table_ptr.add(idx).write_volatile(new_entry);
                    Ok(frame)
                } else {
                    Ok(PhysFrame::new((entry & 0x0000_FFFF_FFFF_F000) as usize))
                }
            } else {
                let frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("Out of memory for page table")?;
                track_frame(frame, FrameSource::UserPageTable);
                self.ledger.track_page_table_frame(frame);
                let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                table_ptr.add(idx).write_volatile(new_entry);
                Ok(frame)
            }
        }
    }

    /// Map a single 4 KiB **device** page `va -> pa` (Device-nGnRnE, EL1-RW, no EL0, no
    /// execute) into this user table. Unlike [`map_page`] (which hardcodes Normal-WB
    /// attributes), this uses the device memory type — for a secondary core that owns a NIC and
    /// must reach its virtio-mmio registers from a syscall running under this table
    /// (rump-on-secondary; the shared device window that serves the BSP is BSP-only).
    /// Intermediate L1/L2/L3 frames are tracked for Drop; the target is device MMIO, not a frame.
    pub fn map_device_page(&mut self, va: usize, pa: usize) -> Result<(), &'static str> {
        if va & (PAGE_SIZE - 1) != 0 || pa & (PAGE_SIZE - 1) != 0 { return Err("Addresses must be page-aligned"); }
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;

        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        let l1_frame = self.get_or_create_table(l0_ptr, l0_idx)?;
        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = self.get_or_create_table(l1_ptr, l1_idx)?;
        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        let l3_frame = self.get_or_create_table(l2_ptr, l2_idx)?;
        let l3_ptr = phys_to_virt(l3_frame.addr) as *mut u64;

        // No AP bits => AP[2:1]=0b00 = EL1 RW, EL0 none. Device-nGnRnE + Outer-shareable, PXN|UXN.
        let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG
            | attr_index(MAIR_DEVICE_NGNRNE) | flags::SH_OUTER | flags::PXN | flags::UXN;
        unsafe { l3_ptr.add(l3_idx).write_volatile(entry); }
        Ok(())
    }

    pub fn map_range(&mut self, va_start: usize, pa_start: usize, size: usize, user_flags: u64) -> Result<(), &'static str> {
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        for i in 0..pages {
            self.map_page(va_start + i * PAGE_SIZE, pa_start + i * PAGE_SIZE, user_flags)?;
        }
        Ok(())
    }

    /// Map a 2 MiB block `va -> pa` as **EL1-only RW, no-execute** (a kernel/partition RAM
    /// block in a user table). A secondary's EL0 process needs its kernel code/data and its
    /// partition reachable when a syscall traps to EL1 *under this table* (Akuma is TTBR0-
    /// only, no switch on trap). `va`/`pa` must be 2 MiB-aligned. EL0 has no access (AP
    /// defaults to EL1); `PXN|UXN` block execution. Intermediate L1/L2 frames are tracked
    /// for Drop; the block target is kernel RAM (not a user frame) so it is not tracked.
    pub fn map_kernel_block_2mb(&mut self, va: usize, pa: usize) -> Result<(), &'static str> {
        const TWO_MB: usize = 1 << 21;
        if va & (TWO_MB - 1) != 0 || pa & (TWO_MB - 1) != 0 {
            return Err("2MB block must be 2MB-aligned");
        }
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *mut u64;
        let l1_frame = self.get_or_create_table(l0_ptr, l0_idx)?;
        let l1_ptr = phys_to_virt(l1_frame.addr) as *mut u64;
        let l2_frame = self.get_or_create_table(l1_ptr, l1_idx)?;
        let l2_ptr = phys_to_virt(l2_frame.addr) as *mut u64;
        // L2 BLOCK descriptor (TABLE bit clear): EL1 RW, inner-shareable, normal WB, no exec.
        let entry = (pa as u64) | flags::VALID | flags::AF | flags::SH_INNER
            | attr_index(MAIR_NORMAL_WB) | flags::PXN | flags::UXN;
        unsafe { l2_ptr.add(l2_idx).write_volatile(entry); }
        Ok(())
    }

    /// Write `bytes` at `offset` within the page mapped at `page_va`.
    ///
    /// Returns `false` if `page_va` is not mapped in this address space; the
    /// caller decides whether that is a bug or a skip.
    ///
    /// # Why this lives here
    ///
    /// It is the *only* way to put bytes into a user page from kernel code
    /// without an `unsafe` block at the call site, and it is safe for a reason
    /// that only this type can supply: **`&mut self` proves exclusive access to
    /// the address space, and the mapping is this object's own state.** Nothing
    /// else in the tree can make that argument — `akuma-pmm` cannot (it hands out
    /// frames but does not know who holds them), and a free function taking a
    /// `PhysFrame` cannot either (`PhysFrame::new` is safe, so a frame value
    /// proves nothing about ownership).
    ///
    /// Added 2026-08-30 so `akuma-elf` could reach `#![forbid(unsafe_code)]`: its
    /// six raw frame writes — a `PT_LOAD` segment copy, an `SHT_RELA` value, and
    /// four `UserStack` pushes — were all into pages this method's receiver had
    /// just returned from [`Self::alloc_and_map`]
    /// (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §8.8).
    pub fn write_page_bytes(&mut self, page_va: usize, offset: usize, bytes: &[u8]) -> bool {
        assert!(
            offset + bytes.len() <= PAGE_SIZE,
            "write_page_bytes {offset}+{} would leave the page",
            bytes.len()
        );
        let page_base = page_va & !(PAGE_SIZE - 1);
        let Some(pa) = self.translate(page_base) else {
            return false;
        };
        // SAFETY: `translate` just confirmed `page_base` is mapped in *this*
        // address space, so `pa` is a real frame reachable through the kernel's
        // physical window. `&mut self` makes this borrow of the address space
        // exclusive, so no other reference to those bytes exists. The assert
        // bounds the write to the single frame backing that page.
        unsafe {
            let dst = akuma_primitives::addr::phys_to_virt(pa + offset);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        true
    }

    pub fn alloc_and_map(&mut self, va: usize, user_flags: u64) -> Result<PhysFrame, &'static str> {
        let frame = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new).ok_or("Out of memory for user page")?;
        track_frame(frame, FrameSource::ElfLoader);
        self.map_and_track(va, frame, user_flags)?;
        Ok(frame)
    }

    /// Install an *already-allocated* frame at `va` and record it in `user_frames` —
    /// the "install" half of [`alloc_and_map`]. Split out so a caller can allocate the
    /// frame OUTSIDE a per-AS page-table lock (`Process::as_lock`) and hold that lock
    /// only across this PTE-editing step (shared-kernel SMP: alloc must not run under
    /// `as_lock` — the PMM OOM/reclaim path can re-enter it). Contains only PTE writes
    /// + the self-locked `user_frames` bookkeeping.
    pub fn map_and_track(&mut self, va: usize, frame: PhysFrame, user_flags: u64) -> Result<(), &'static str> {
        self.ledger.track_user_frame(frame);
        self.map_page(va, frame.addr, user_flags)
    }

    /// Demote every RW L3 PTE in `[va_start, va_start + pages*PAGE_SIZE)` of
    /// **this** address space to read-only; returns the count demoted. The
    /// CoW-fork share pass
    /// (`akuma_exec::process::cow_share_and_demote_range`) calls this per chunk
    /// under the per-AS `as_lock`, then issues the range TLB flush.
    ///
    /// The `&self`-safe form of the free [`demote_range_to_ro`] `unsafe fn`:
    /// that one takes a raw `*mut u64` L0 the caller vouches for, this one walks
    /// `self.l0_frame` — this address space's own L0 root, allocated in `new`
    /// and never reassigned, so the walk always has a real tree to follow
    /// (`AKUMA_EXEC_AUDIT.md` §6.E group 3).
    pub fn demote_range_to_ro(&mut self, va_start: usize, pages: usize) -> usize {
        // SAFETY: `self.l0_frame.addr` is this AS's L0 root; `&mut self` is the
        // exclusivity the PTE writes need.
        unsafe {
            demote_range_to_ro(phys_to_virt(self.l0_frame.addr) as *mut u64, va_start, pages)
        }
    }

    pub fn is_range_mapped(&self, va_start: usize, len: usize) -> bool {
        if len == 0 { return true; }
        let start_page = va_start & !(PAGE_SIZE - 1);
        let end_page = (va_start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let num_pages = (end_page - start_page) / PAGE_SIZE;
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *const u64;
        for i in 0..num_pages {
            if !self.is_page_mapped(l0_ptr, start_page + i * PAGE_SIZE) { return false; }
        }
        true
    }

    fn is_page_mapped(&self, l0_ptr: *const u64, va: usize) -> bool {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;
        let l3_idx = (va >> 12) & 0x1FF;
        unsafe {
            let l0_entry = l0_ptr.add(l0_idx).read_volatile();
            if l0_entry & flags::VALID == 0 { return false; }
            let l1_ptr = phys_to_virt((l0_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l1_entry = l1_ptr.add(l1_idx).read_volatile();
            if l1_entry & flags::VALID == 0 { return false; }
            if l1_entry & flags::TABLE == 0 { return true; }
            let l2_ptr = phys_to_virt((l1_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l2_entry = l2_ptr.add(l2_idx).read_volatile();
            if l2_entry & flags::VALID == 0 { return false; }
            if l2_entry & flags::TABLE == 0 { return true; }
            let l3_ptr = phys_to_virt((l2_entry & 0x0000_FFFF_FFFF_F000) as usize) as *const u64;
            let l3_entry = l3_ptr.add(l3_idx).read_volatile();
            l3_entry & flags::VALID != 0
        }
    }

    /// Thread-safe frame tracking — IRQs disabled to prevent preemption deadlock
    /// (same pattern as PMM: if timer fires while holding lock, scheduler switches
    /// to another thread which tries to lock → spins forever).
    pub fn track_user_frame(&self, frame: PhysFrame) { self.ledger.track_user_frame(frame); }

    /// Adopt `frame` as a mapping of this address space, maintaining **both** the
    /// per-AS frame list and the global share count as one uninterruptible unit.
    ///
    /// The two mechanisms count different things — `user_frames` counts VAs per PA
    /// *within this address space*, `COW_REFCOUNTS` counts *address spaces* — and the
    /// rule connecting them ("an address space contributes exactly one global
    /// reference however many VAs it maps") was maintained by hand at ~40 call sites.
    /// Splitting the two updates across the `as_lock` hold is what let the count drift
    /// below the truth and hand a live frame back to the PMM
    /// (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6). Here they cannot drift: one
    /// `IrqGuard`, one `user_frames` hold, and the decision "is this the first VA for
    /// this PA here?" read from the same map it then updates.
    ///
    /// `caller_holds_ref` says whether the caller already took a global reference for
    /// this adoption (`file_page_cache::lookup_and_ref` does, to keep the frame alive
    /// across the fill). Returns `true` when that reference was **surplus** — this
    /// address space already had the PA — and the caller must release it. That is the
    /// old `drop_surplus_shared_ref`, moved inside the hold and made a return value so
    /// it cannot be forgotten on one arm and not the other.
    ///
    /// Lock order: `COW_REFCOUNTS` is a leaf and is taken innermost here, which is the
    /// same direction as the existing `as_lock` → `COW_REFCOUNTS` order.
    pub fn adopt_user_frame(&self, frame: PhysFrame, caller_holds_ref: bool) -> bool {
        self.ledger.adopt_user_frame(frame, caller_holds_ref)
    }

    /// Does this address space already hold `pa` as a user frame?
    ///
    /// Teardown frees each distinct PA **exactly once** regardless of how many VAs
    /// map it, so an address space contributes exactly one reference per frame.
    /// Callers that take a reference per *fault* (the shared file-page cache) use
    /// this to detect the second VA mapping an already-held frame and hand the
    /// surplus reference back, instead of leaking it until reboot.
    pub fn tracks_user_frame(&self, pa: usize) -> bool { self.ledger.tracks_user_frame(pa) }
    pub fn track_page_table_frame(&self, frame: PhysFrame) { self.ledger.track_page_table_frame(frame); }

    /// Map `frame` at `va` in the **currently installed** address space and adopt
    /// everything the walk produced — the safe way to install a user page from a
    /// syscall.
    ///
    /// Not to be confused with [`map_and_track`](Self::map_and_track), which is the
    /// ELF loader's primitive: that one walks `self.l0_frame` (so it can build an
    /// address space that is not installed yet) but allocates page tables *inside*
    /// the call, overwrites an existing L3 entry without noticing, and issues no TLB
    /// flush. None of those three are acceptable under `as_lock`. This one is the
    /// `map_user_page` path: CAS install, refuses a VA that is already mapped,
    /// hands the table frames back so they can be tracked, and flushes.
    ///
    /// This is [`map_user_page`]'s four-clause `# Safety` contract turned into a
    /// signature. Every clause is discharged here rather than restated at the call
    /// site:
    ///
    /// 1. **The address space is held.** `&mut UserAddressSpace` is only reachable
    ///    through `Process::with_address_space`, which takes `as_lock` with IRQs
    ///    masked. A caller that has one has the lock, by construction.
    /// 2. **`va` is a user VA**, checked below — TTBR0 range and page-aligned.
    /// 3. **`self` is the address space the walk will actually edit.** `map_user_page`
    ///    reads `TTBR0_EL1`, so it always targets the *installed* address space, which
    ///    is not necessarily the one you hold a `&mut` to. That mismatch is the
    ///    stale-TTBR0 bug class this tree has hit three separate times (clone_thread,
    ///    fork_process, vfork_process — see `overlays/devbox/README.md`), and it is
    ///    checked here instead of assumed.
    /// 4. **The return value cannot be dropped on the floor.** Both frame lists are
    ///    tracked before returning, and the `installed` flag is `#[must_use]`.
    ///
    /// Returns `true` if this call installed the PTE. **`false` does not mean
    /// failure and does not mean nothing happened**: `frame` is tracked either way
    /// (it is this address space's to free at teardown regardless), but nothing maps
    /// it, so a caller that expected fresh zeroed memory at `va` is now looking at
    /// the previous occupant's page *and its permissions*. Only a caller that
    /// reserved `va` itself may ignore this.
    #[must_use = "`false` means the PTE was NOT installed — the frame is tracked for                   teardown but `va` still holds whatever was there before"]
    // `&mut self` is the safety argument, not a mutation requirement: it is what
    // proves the caller came through `Process::with_address_space` and therefore
    // holds `as_lock`. The frame trackers below are `&self` (they self-lock), so
    // clippy sees an unnecessary `&mut` — downgrading it would silently reopen
    // clause 1 of `map_user_page`'s contract to any holder of a shared reference.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub fn map_user_page_tracked(&mut self, va: usize, frame: PhysFrame, user_flags_val: u64) -> bool {
        self.map_user_page_tracked_inner(va, frame, user_flags_val, true)
    }

    /// [`map_user_page_tracked`](Self::map_user_page_tracked) without the per-page
    /// TLB invalidation,
    /// for a batch install. The caller must issue `flush_tlb_range` over the whole
    /// span before userspace can reach any of the new mappings.
    #[must_use = "`false` means the PTE was NOT installed — the frame is tracked for                   teardown but `va` still holds whatever was there before"]
    // `&mut self` is the safety argument, not a mutation requirement: it is what
    // proves the caller came through `Process::with_address_space` and therefore
    // holds `as_lock`. The frame trackers below are `&self` (they self-lock), so
    // clippy sees an unnecessary `&mut` — downgrading it would silently reopen
    // clause 1 of `map_user_page`'s contract to any holder of a shared reference.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub fn map_user_page_tracked_no_flush(
        &mut self,
        va: usize,
        frame: PhysFrame,
        user_flags_val: u64,
    ) -> bool {
        self.map_user_page_tracked_inner(va, frame, user_flags_val, false)
    }

    // `&mut self` is the safety argument, not a mutation requirement: it is what
    // proves the caller came through `Process::with_address_space` and therefore
    // holds `as_lock`. The frame trackers below are `&self` (they self-lock), so
    // clippy sees an unnecessary `&mut` — downgrading it would silently reopen
    // clause 1 of `map_user_page`'s contract to any holder of a shared reference.
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn map_user_page_tracked_inner(
        &mut self,
        va: usize,
        frame: PhysFrame,
        user_flags_val: u64,
        flush: bool,
    ) -> bool {
        // Clause 2: TTBR0 covers the low half only, and the walk indexes by page.
        if va >> 48 != 0 || va & (PAGE_SIZE - 1) != 0 {
            log::debug!("[MMU] map_and_track refused non-user/unaligned va=0x{:x}", va);
            debug_assert!(false, "map_and_track called with a non-user or unaligned va");
            return false;
        }
        // Clause 3: refuse rather than edit somebody else's page tables. Compared on
        // the L0 base, not the whole TTBR0 word, because the ASID field is ours and
        // the installed value's is whatever the last context switch wrote.
        let installed_l0 = akuma_cpu::sysreg::ttbr0_el1() & L0_BASE_MASK;
        if installed_l0 != (self.l0_phys() as u64 & L0_BASE_MASK) {
            log::debug!(
                "[MMU] map_and_track refused: this AS l0=0x{:x} but TTBR0 has 0x{:x}",
                self.l0_phys(), installed_l0
            );
            debug_assert!(false, "map_and_track on an address space that is not installed");
            return false;
        }
        // SAFETY: clauses 1-3 are established above and by the `&mut self` receiver;
        // clause 4 is discharged by the tracking below, which runs on both arms.
        let (table_frames, installed) =
            unsafe { map_user_page_inner(va, frame.addr, user_flags_val, flush) };
        self.track_user_frame(frame);
        for tf in table_frames {
            self.track_page_table_frame(tf);
        }
        installed
    }

    /// Number of distinct physical frames this address space tracks as user data
    /// (one entry per PA, regardless of how many VAs map it). Leak-debugging:
    /// compare against the VA actually mapped — a count far larger than the
    /// mapped VA means frames are being tracked-but-orphaned (re-fault leak).
    pub fn user_frame_count(&self) -> usize { self.ledger.user_frame_count() }
    /// Sum of all per-PA reference counts (total VA→frame mappings tracked).
    pub fn user_frame_total_refs(&self) -> usize { self.ledger.user_frame_total_refs() }
    /// Number of page-table frames (L1/L2/L3) this address space holds.
    pub fn page_table_frame_count(&self) -> usize { self.ledger.page_table_frame_count() }
    /// Physical frames this address space will hand back to the PMM when it drops:
    /// tracked user pages + intermediate page tables + the L0. A **shared** view owns
    /// none of them (the L0 owner does), so it reports 0.
    ///
    /// Snapshotted into `process::reclaim`'s per-slot stamp at retirement, so the
    /// memory-pressure path can size the reclaimable backlog without dereferencing a
    /// RETIRED `Process` — which would race the deferred drop it is scheduling.
    pub fn resident_pages(&self) -> usize { self.ledger.resident_pages() }
    /// Drop one tracked reference to `frame`'s physical address.  O(log n) map
    /// lookup (was an O(n) linear scan — the dominant `munmap`/exit cost, see
    /// docs/COW_OPTIMIZATIONS.md).  A PA can be tracked more than once (mapped
    /// at multiple VAs), so we decrement the count and only drop the entry at 0.
    ///
    /// Returns `true` iff this call dropped the **last** reference (count
    /// reached 0) — i.e. the caller now owns the obligation to free the
    /// physical frame.  Returns `false` when the frame is still referenced at
    /// another VA, **or** when this address space does not track the frame at
    /// all (e.g. a `new_shared` vfork view, whose `user_frames` is empty and
    /// whose frames are owned by the L0 owner).  Freeing on a `false` result
    /// would be a double-free — the historical bug behind the EL1 EC=0x22
    /// crashes under memory pressure (a still-live page handed back to the PMM
    /// and re-mapped into a kernel allocation).
    #[must_use = "free the frame only when this returns true; freeing otherwise is a double-free"]
    pub fn remove_user_frame(&self, frame: PhysFrame) -> bool { self.ledger.remove_user_frame(frame) }

    /// Walk **this** address space's own L0→L2 and return a pointer to the L3 slot
    /// for `va`, or `None` when any intermediate level is unmapped or is a block
    /// descriptor rather than a table.
    ///
    /// The `&self` analog of [`current_user_l3_pte`], which asks the same question of
    /// the live `TTBR0_EL1`; both delegate to [`l3_slot_in`]. Seven `&mut self` walks
    /// in this file open with the same four index extractions and the same three-level
    /// descent, and differ only in what they do at the leaf — see
    /// `docs/archive/TRIM_FAT_PTE_NEWTYPE.md` §2.
    ///
    /// Deliberately does **not** read the leaf: the callers disagree about what a
    /// valid or invalid L3 entry means for them (one overwrites it unconditionally,
    /// one checks AP first, the rest bail), so that decision stays at the call site.
    /// It likewise takes no [`IrqGuard`] and does no TLB maintenance — the `_no_flush`
    /// variants exist precisely because per-page barriers dominated large `munmap`s,
    /// so guard and flush discipline stays with the caller, byte for byte.
    fn l3_slot(&self, va: usize) -> Option<*mut u64> {
        // SAFETY: `self.l0_frame` is this address space's live L0, kept alive by
        // `&self`; the tables it reaches are freed only through `Drop`.
        unsafe { l3_slot_in(phys_to_virt(self.l0_frame.addr) as *mut u64, va) }
    }

    /// Clear the L3 PTE for `va` and flush its TLB entry.
    ///
    /// Infallible, and returns `()` rather than a `Result` since 2026-08-30. It
    /// used to return `Result<(), &'static str>` that was **structurally always
    /// `Ok`** — an unmapped or already-invalid `va` takes the `else { return }`
    /// path, which is the normal case for `MAP_FIXED` over a lazy range, not an
    /// error. Every caller wrote `let _ = `, and the audit that found them
    /// initially read those as swallowed failures. They were not; the signature
    /// was. A `Result` no implementation can populate teaches callers that
    /// checking is optional here, which is the habit that makes a *real*
    /// discarded error elsewhere look ordinary
    /// (`docs/archive/ERROR_HANDLING_AUDIT.md` §4.2).
    pub fn unmap_page(&mut self, va: usize) {
        self.unmap_page_no_flush(va);
        let _ = flush_tlb_page(va, TlbTarget::AllCores);
    }

    /// Clear the L3 PTE for `va` **without** flushing the TLB.  The caller must
    /// issue `flush_tlb_range_all_asid` (or equivalent) over the range before
    /// the unmapped VAs could be accessed again.  Used by `munmap`/teardown to
    /// batch one barrier per region instead of one per page.
    pub fn unmap_page_no_flush(&mut self, va: usize) {
        let _irq_guard = IrqGuard::new();
        let Some(pte) = self.l3_slot(va) else { return };
        // Unconditional: this copy has never read the leaf before clearing it.
        unsafe { pte.write_volatile(0); }
    }

    /// Unmap a page and return its physical frame, also removing it from user_frames.
    /// Returns `Some(PhysFrame)` if the page was mapped, `None` if it wasn't.
    /// The caller is responsible for freeing the returned frame via PMM.
    pub fn unmap_and_free_page(&mut self, va: usize) -> Option<PhysFrame> {
        let frame = self.unmap_and_free_page_no_flush(va);
        let _ = flush_tlb_page(va, TlbTarget::AllCores);
        frame
    }

    /// Like `unmap_and_free_page` but **without** the per-page TLB flush — the
    /// caller batches a single `flush_tlb_range_all_asid` over the whole region
    /// after the loop.  This is the hot path for large `munmap`s (the trace
    /// showed single 12,426-page unmaps); per-page barriers dominated otherwise.
    pub fn unmap_and_free_page_no_flush(&mut self, va: usize) -> Option<PhysFrame> {
        let _irq_guard = IrqGuard::new();
        let pte = self.l3_slot(va)?;
        let pa = unsafe {
            let l3_entry = pte.read_volatile();
            if l3_entry & flags::VALID == 0 { return None; }
            pte.write_volatile(0);
            (l3_entry & 0x0000_FFFF_FFFF_F000) as usize
        };
        let frame = PhysFrame::new(pa);
        // Only hand the frame back for freeing when we dropped its *last*
        // reference.  If it is still mapped at another VA (refcount > 1) or is
        // owned by the L0 owner (shared view, untracked here), `remove_user_frame`
        // returns false and we must NOT free it — doing so was the double-free
        // that produced the EL1 EC=0x22 crashes under low memory.  The PTE is
        // already cleared above; the surviving reference (or the owner's Drop)
        // frees the physical frame exactly once.
        if self.remove_user_frame(frame) {
            Some(frame)
        } else {
            None
        }
    }

    /// Evict a clean, read-only page at `va`: if it maps a VALID page whose
    /// permission bits are `AP_RO_ALL` (read-only at EL0/EL1), clear the L3 PTE,
    /// flush the TLB, drop this address space's frame reference, and return the
    /// freed frame (when this dropped the last reference) for the caller to
    /// return to the PMM. Returns `None` if the page is unmapped, NOT read-only
    /// (so possibly dirty — e.g. a CoW copy mapped `AP_RW_ALL`; never evicted to
    /// avoid data loss), or still referenced at another VA.
    ///
    /// Read-only ⇒ the backing file is authoritative, so the next access
    /// re-faults and re-reads the page. This is the mechanism that lets a
    /// file-backed mmap larger than physical RAM make progress under pressure
    /// (clean model-weight pages are paged out and back in) instead of OOM-ing.
    /// TLB is flushed BEFORE the frame is freed — a stale TLB entry pointing at a
    /// reallocated frame would corrupt memory (same hazard `munmap` guards).
    pub fn try_evict_ro_page(&mut self, va: usize) -> Option<PhysFrame> {
        let _irq_guard = IrqGuard::new();
        let pte = self.l3_slot(va)?;
        let pa = unsafe {
            let l3_entry = pte.read_volatile();
            if l3_entry & flags::VALID == 0 { return None; }
            // AP_RO_ALL (bits [7:6] == 0b11) is the only state guaranteed clean:
            // a written CoW page is AP_RW_ALL and must never be re-read from file.
            if (l3_entry & flags::AP_RO_ALL) != flags::AP_RO_ALL { return None; }
            pte.write_volatile(0);
            (l3_entry & 0x0000_FFFF_FFFF_F000) as usize
        };
        let _ = flush_tlb_page(va, TlbTarget::AllCores);
        let frame = PhysFrame::new(pa);
        if self.remove_user_frame(frame) {
            Some(frame)
        } else {
            None
        }
    }

    /// Zero the physical page backing `va` without unmapping it.
    /// Returns true if a page was found and zeroed, false if no mapping exists.
    pub fn zero_mapped_page(&self, va: usize) -> bool {
        let _irq_guard = IrqGuard::new();
        let Some(pte) = self.l3_slot(va) else { return false };
        unsafe {
            let l3_entry = pte.read_volatile();
            if l3_entry & flags::VALID == 0 { return false; }
            let pa = (l3_entry & 0x0000_FFFF_FFFF_F000) as usize;
            core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, 4096);
        }
        true
    }

    /// Update the permission bits of an existing L3 page table entry.
    /// Preserves the physical address and fixed flags, replaces only user permission bits.
    /// Infallible; returns `()` since 2026-08-30 — see [`Self::unmap_page`].
    pub fn update_page_flags(&mut self, va: usize, new_flags: u64) {
        self.update_page_flags_inner(va, new_flags, true);
    }

    /// Same as `update_page_flags` but skips the TLB flush.
    ///
    /// Use when updating a large range of pages (e.g. mprotect over many pages).
    /// After calling this for all pages, issue a single `flush_tlb_range` or
    /// `flush_tlb_asid` to make the permission changes visible to userspace.
    /// Infallible; returns `()` since 2026-08-30 — see [`Self::unmap_page`].
    pub fn update_page_flags_no_flush(&mut self, va: usize, new_flags: u64) {
        // No TLB flush — caller must call flush_tlb_range after the batch.
        self.update_page_flags_inner(va, new_flags, false);
    }

    /// The body behind the two `update_page_flags*` variants, which differed only in
    /// whether the successful edit issues the per-page TLB invalidation. Same shape as
    /// [`map_user_page_inner`]: `flush` is a literal at both call sites and this is
    /// `#[inline]` into two one-line wrappers, so the branch constant-folds away.
    ///
    /// The flush stays inside the [`IrqGuard`] hold, and is skipped on every early
    /// return, exactly as both copies had it. Neither copy ever needed `&mut self` —
    /// both edit through a raw pointer — so this takes `&self`; the two public
    /// wrappers keep `&mut self`, which is their existing API.
    #[inline]
    fn update_page_flags_inner(&self, va: usize, new_flags: u64, flush: bool) {
        let _irq_guard = IrqGuard::new();
        const PERM_MASK: u64 = flags::AP_RO_ALL | flags::AP_RW_ALL | flags::UXN | flags::PXN;
        let Some(pte) = self.l3_slot(va) else { return };
        unsafe {
            let old_entry = pte.read_volatile();
            if old_entry & flags::VALID == 0 { return; }
            let entry = (old_entry & !PERM_MASK) | new_flags;
            pte.write_volatile(entry);
        }
        if flush {
            let _ = flush_tlb_page(va, TlbTarget::AllCores);
        }
    }

    /// Raw L3 page descriptor for `va` (4KiB-aligned), if mapped at the final level.
    /// Used by kernel tests and diagnostics (e.g. verify `UXN` after `update_page_flags`).
    pub fn read_l3_page_entry(&self, va: usize) -> Option<u64> {
        let va = va & !(PAGE_SIZE - 1);
        let _irq_guard = IrqGuard::new();
        let pte = self.l3_slot(va)?;
        let l3_entry = unsafe { pte.read_volatile() };
        if l3_entry & flags::VALID == 0 {
            return None;
        }
        Some(l3_entry)
    }

    /// Physical address of the 4KiB frame backing `va`, if mapped.
    pub fn phys_addr_for_page_va(&self, va: usize) -> Option<usize> {
        let e = self.read_l3_page_entry(va)?;
        Some((e & 0x0000_FFFF_FFFF_F000) as usize)
    }

    /// Invalidate the instruction cache for the physical page backing `va`
    /// (after PTE permission changes or new code bytes). Matches the
    /// `dc cvau`/`ic ivau` pattern used when demand-paging file-backed text.
    pub fn invalidate_icache_for_page_va(&self, va: usize) {
        let Some(pa) = self.phys_addr_for_page_va(va) else {
            return;
        };
        let kva = phys_to_virt(pa) as usize;
        sync_icache_range(kva, PAGE_SIZE);
    }

    /// Check whether a virtual address has a valid page table entry (public).
    pub fn is_mapped(&self, va: usize) -> bool {
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *const u64;
        self.is_page_mapped(l0_ptr, va)
    }

    /// Physical address `va` maps to in **this** address space, or `None` when it
    /// is unmapped. Walks this space's own L0 rather than the live `TTBR0`, so it
    /// is answerable for a process that is not the one currently running.
    pub fn translate(&self, va: usize) -> Option<usize> {
        let l0_ptr = phys_to_virt(self.l0_frame.addr) as *const u64;
        translate_user_va(l0_ptr, va)
    }

    pub fn activate(&self) {
        let _ttbr0 = self.ttbr0();
        // IRQs masked so the install and the expected-L0 note (the switch-path
        // tripwire baseline, see threading::EXPECTED_L0) are one atomic step —
        // a preemption between them would save/check live tables against the
        // other value and false-positive.
        let _irq = IrqGuard::new();
        let _ = flush_tlb_all(TlbTarget::AllCores);
        #[cfg(all(target_os = "none", target_arch = "aarch64"))]
        unsafe {
            let _core = publish_l0_begin(_ttbr0);
            core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) _ttbr0);
            publish_l0_end(_core);
        }
        let _ = flush_tlb_all(TlbTarget::AllCores);
        crate::note_current_expected_l0(_ttbr0);
    }

    pub fn deactivate() {
        let _boot_ttbr0 = get_boot_ttbr0();
        // Same IRQ guard as activate() — install + note must not be split.
        let _irq = IrqGuard::new();
        let _ = flush_tlb_all(TlbTarget::AllCores);
        #[cfg(all(target_os = "none", target_arch = "aarch64"))]
        unsafe {
            let _core = publish_l0_begin(_boot_ttbr0);
            core::arch::asm!("dsb ish", "msr ttbr0_el1, {ttbr0}", "isb", ttbr0 = in(reg) _boot_ttbr0);
            publish_l0_end(_core);
        }
        let _ = flush_tlb_all(TlbTarget::AllCores);
        crate::note_current_expected_l0(_boot_ttbr0);
    }
}

#[cfg(target_arch = "aarch64")]
impl Drop for UserAddressSpace {
    fn drop(&mut self) {
        #[allow(clippy::let_unit_value)]
        let _ledger = instr::as_drop_enter(
            self.ledger.is_shared(),
            self.l0_frame.addr,
            self.asid,
            self.ledger.user_frame_count(),
        );
        // Opportunistic retry of earlier TTBR-deferred frees: address spaces die
        // constantly under load, so this keeps the parked list near-empty without
        // needing a dedicated collector to run first.
        drain_pending_ttbr_frees();
        let l0_addr = self.l0_frame.addr;
        if !self.ledger.is_shared() {
            // Owner dropping — check if shared views still exist
            let has_shared = with_irqs_disabled(|| {
                let table = SHARED_L0_TABLE.lock();
                table.get(&l0_addr).is_some_and(|e| e.ref_count > 0)
            });
            if has_shared {
                // #region agent log
                log::debug!("[FORK-DBG] AS owner L0=0x{:x} DEFERRING free (siblings alive)", l0_addr);
                // #endregion
                as_trace(format_args!("[AS-DEFER] l0=0x{:x} asid=0x{:x} core={}\n",
                    l0_addr, self.asid, akuma_bkl::bkl::current_core_id()));
                let user_frames = self.ledger.take_user_frames();
                let pt_frames = self.ledger.take_page_table_frames();
                let l0 = self.l0_frame;
                let n_uf = user_frames.len();
                let stored = with_irqs_disabled(|| {
                    let mut table = SHARED_L0_TABLE.lock();
                    if let Some(entry) = table.get_mut(&l0_addr) {
                        entry.deferred_user_frames = Some(user_frames);
                        entry.deferred_pt_frames = Some(pt_frames);
                        entry.deferred_l0 = Some(l0);
                        true
                    } else {
                        false
                    }
                });
                // Temporary: no entry to park in, so the map died inside the
                // closure above. Stop counting its entries as live.
                if !stored { instr::uf_silent(n_uf); }
            } else {
                // No shared views (or all already dropped) — release now, THROUGH
                // the per-core TTBR liveness gate: a peer core whose TTBR0_EL1 is
                // still resident on this L0 (killer-side hard-terminate, a reap
                // outrunning the exiting core's final switch) must not have the
                // tables freed and poisoned under it. See `free_or_defer_as_frames`.
                let user_frames = self.ledger.take_user_frames();
                let pt_frames = self.ledger.take_page_table_frames();
                with_irqs_disabled(|| { SHARED_L0_TABLE.lock().remove(&l0_addr); });
                free_or_defer_as_frames(l0_addr, self.asid, self.l0_frame, user_frames, pt_frames, "owner");
            }
        } else {
            // Shared view dropping — decrement refcount
            let deferred = with_irqs_disabled(|| {
                let mut table = SHARED_L0_TABLE.lock();
                if let Some(entry) = table.get_mut(&l0_addr) {
                    entry.ref_count = entry.ref_count.saturating_sub(1);
                    if entry.ref_count == 0 && entry.deferred_l0.is_some() {
                        // Last shared view and owner already deferred — take frames
                        let uf = entry.deferred_user_frames.take();
                        let pf = entry.deferred_pt_frames.take();
                        let l0 = entry.deferred_l0.take();
                        table.remove(&l0_addr);
                        return (uf, pf, l0);
                    }
                    if entry.ref_count == 0 && entry.deferred_l0.is_none() {
                        table.remove(&l0_addr);
                    }
                }
                (None, None, None)
            });
            // Free deferred frames outside the lock
            if let (Some(ref uf), Some(ref pf), Some(ref l0)) = deferred {
                // #region agent log
                log::debug!("[FORK-DBG] Last shared view L0=0x{:x} freeing {} user + {} pt frames",
                    l0.addr, uf.len(), pf.len());
                // #endregion
            }
            if let (Some(uf), Some(pf), Some(l0)) = deferred {
                // Same TTBR liveness gate as the owner-drop branch: the killer-side
                // hard-terminate in `kill_thread_group` can leave a straggler's core
                // parked on this very L0 while the last software view drops here.
                free_or_defer_as_frames(l0.addr, self.asid, l0, uf, pf, "last-view");
            }
        }
        // Whatever the branch above did not take (a shared view's own map) dies
        // with the struct here; stop counting its entries as live. Temporary.
        {
            let n = self.ledger.user_frame_count();
            instr::uf_drop_remainder(n);
        }
        // ORDER IS LOAD-BEARING: flush BEFORE returning the ASID to the allocator.
        //
        // `AsidAllocator::alloc` (mmu/asid.rs) only flips a bit — it does no TLB
        // maintenance of its own — so this is the sole invalidation for the dying
        // address space. Freeing first opens a window on SMP: a peer core can `alloc()`
        // this very ASID, install it in TTBR0 and start executing while our stale
        // translations are still live, so the new owner reads the DEAD process's memory
        // through them. That yields plausible-looking junk rather than obvious garbage —
        // small integers where pointers belong — and the victim is always a process only
        // tens of ms old, because this drop runs on the `PROCESS_RECLAIM_COOLDOWN_US`
        // (10 ms) reclaim path (see docs/reference/subsystems/memory.md).
        //
        // Flushing first closes it: by the time the ASID is allocatable again, no
        // translation for it remains. `tlbi aside1is` is inner-shareable, so peers are
        // covered too.
        let _ = flush_tlb_asid(self.asid, TlbTarget::AllCores);
        with_irqs_disabled(|| ASID_ALLOCATOR.lock().free(self.asid));
        instr::as_drop_exit(_ledger);
    }
}

// ============================================================================
// x86_64 UserAddressSpace — Phase 4 of proposals/AKUMA_MMU_ARCH_PORTABILITY.md
// ============================================================================
//
// A minimal x86_64 body, ported directly from `amd64/src/paging.rs` (which
// already built this exact vocabulary and encoding — see that file's own
// header) rather than redesigned. Deliberately NOT the aarch64 struct's peer
// in features: no ASID/PCID, no CoW sharing, no refcounted frames, no lazy
// regions, no multi-core epoch tracking. `UserAddressSpace` on this target
// is a different, much smaller type that happens to share a name — any
// caller reaching for a method only the aarch64 side has gets a compile
// error here, which is the honest boundary of what this port proves.

/// x86_64 page permissions, as permissions rather than as an encoding.
///
/// The x86 half of the vocabulary `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md`
/// §1 proposes — `amd64/src/paging.rs::PteProt` already built exactly this
/// shape; see its header for why keeping this a struct (rather than a `u64`) is
/// the point: `write`/`exec`/`user` cannot collide the way `user_flags`'
/// `RO`/`EXEC` constants alias on the AArch64 side.
///
/// # Why `PteProt` and not `Prot`
///
/// It was `Prot` until B3 (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`), and the
/// name was a **shadow**: this crate does `pub use types::*`, which re-exports
/// the neutral `akuma_mmap::Prot`, so on x86_64 `akuma_mmu::Prot` silently
/// resolved to *this* struct instead. That is invisible while nothing shared
/// names the type — and `akuma-syscalls-glue` names it, calling
/// `mmu::Prot::from_prot(prot)` to turn an `mmap` argument into a region's
/// protection. On AArch64 that is `akuma_mmap::Prot::from_prot`; on x86_64 it
/// resolved here and failed to compile, which is the lucky outcome. Had this
/// struct happened to carry a `from_prot`, the same source line would have
/// produced a page-table encoding on one architecture and a region record on
/// the other.
///
/// So the two levels are named apart, matching the sibling walker: [`Prot`] is
/// what a *region* records, `PteProt` is what the *hardware* is told, and
/// [`PteProt::from_region`] is the only bridge.
#[cfg(any(target_arch = "x86_64", test))]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PteProt {
    pub write: bool,
    pub exec: bool,
    pub user: bool,
}

/// One present 4 KiB leaf, as [`UserAddressSpace::for_each_leaf_in_range`]
/// reports it.
///
/// A struct rather than a `(usize, u64, PteProt)` tuple for one reason: `pa`
/// and the raw entry are both integers of the same width, and the two range
/// walks this replaces in `amd64/src/paging.rs` passed them adjacently. A
/// transposition there compiles and produces a mapping of the flag word.
#[cfg(any(target_arch = "x86_64", test))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    /// Virtual address of the page, page-aligned.
    pub va: usize,
    /// Physical frame the leaf points at.
    pub pa: usize,
    /// Decoded permissions.
    pub prot: PteProt,
    /// The copy-on-write marker (PTE bit 9). Reported separately because
    /// [`PteProt`] is the *hardware* permission triple and this bit is
    /// software-defined — see [`X86_COW`] and `akuma-cow`, which takes the
    /// marker as its own input for exactly this reason.
    pub cow: bool,
}

/// What [`UserAddressSpace::rewrite_leaves_in_range`] should do with a leaf.
///
/// The walk descends once and the caller decides per page, so unmapping a
/// range, re-permissioning it and inspecting it are one primitive rather than
/// three. **Policy stays with the caller**: whether a writable region may
/// become a writable *PTE* depends on the frame's CoW share count, which is a
/// PMM question this crate deliberately does not ask
/// (`amd64/src/mm.rs::pte_prot_for`).
#[cfg(any(target_arch = "x86_64", test))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LeafAction {
    /// Leave the entry exactly as it is.
    Keep,
    /// Clear the entry and `invlpg` it. The frame is **not** freed and its
    /// refcount is not touched — the caller was handed the `pa` and owns that
    /// decision.
    Unmap,
    /// Rewrite the entry in place with new permissions and CoW marker, keeping
    /// the same frame. One store, no re-walk.
    Reprotect(PteProt, bool),
}

#[cfg(any(target_arch = "x86_64", test))]
impl PteProt {
    /// User read/write, no execute. The default for data.
    pub const USER_RW: Self = Self { write: true, exec: false, user: true };
    /// User read + execute, not writable.
    pub const USER_RX: Self = Self { write: false, exec: true, user: true };
}

/// How an x86_64 mapping is cached — the other half of `encode(prot, attr)`.
/// See `amd64/src/paging.rs::MemAttr`'s header for why no consumer should
/// care whether this becomes an `AttrIndx` (AArch64) or two PTE bits (x86).
#[cfg(any(target_arch = "x86_64", test))]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemAttr {
    /// Normal RAM: writeback cached. The only attribute any user page in
    /// this crate's scope needs.
    WriteBack,
    /// Device MMIO: uncacheable, never speculatively read. Unused by
    /// `UserAddressSpace` today (no `map_device_page` on this target yet);
    /// present because `encode` is `pub(crate)` and a future device-mapping
    /// method should not have to reinvent this half.
    #[allow(dead_code)]
    Device,
}

#[cfg(any(target_arch = "x86_64", test))]
const X86_P: u64 = 1 << 0;
#[cfg(any(target_arch = "x86_64", test))]
const X86_RW: u64 = 1 << 1;
#[cfg(any(target_arch = "x86_64", test))]
const X86_US: u64 = 1 << 2;
#[cfg(any(target_arch = "x86_64", test))]
const X86_PWT: u64 = 1 << 3;
#[cfg(any(target_arch = "x86_64", test))]
const X86_PCD: u64 = 1 << 4;
#[cfg(any(target_arch = "x86_64", test))]
const X86_PS: u64 = 1 << 7;
/// No-execute. Requires `EFER.NXE`, which `amd64/src/boot.s` sets alongside
/// `LME` — without it this is a reserved bit and setting it faults. Any
/// x86_64 kernel that links this crate without doing the same will fault the
/// first time a non-executable page is touched, not the first time one is
/// mapped; there is nothing this crate can check for that at map time.
#[cfg(any(target_arch = "x86_64", test))]
const X86_NX: u64 = 1 << 63;
#[cfg(any(target_arch = "x86_64", test))]
const X86_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;
#[cfg(any(target_arch = "x86_64", test))]
const X86_ENTRIES: usize = 512;

/// Encode a [`PteProt`] and [`MemAttr`] into x86_64 PTE bits. The x86 backend of
/// `encode(prot, attr)` — this crate stays the only one naming `P`/`RW`/`US`/
/// `NX`/`PCD`/`PWT`, same discipline as the AArch64 `flags` module.
#[cfg(any(target_arch = "x86_64", test))]
const fn x86_encode(prot: PteProt, attr: MemAttr) -> u64 {
    let mut bits = X86_P;
    if prot.write {
        bits |= X86_RW;
    }
    if prot.user {
        bits |= X86_US;
    }
    if !prot.exec {
        bits |= X86_NX;
    }
    match attr {
        MemAttr::WriteBack => {}
        MemAttr::Device => bits |= X86_PCD | X86_PWT,
    }
    bits
}

/// Index into the level-`n` table for `va`. Level 4 = PML4 … level 1 = PT.
#[cfg(any(target_arch = "x86_64", test))]
const fn x86_index(va: usize, level: u32) -> usize {
    (va >> (12 + 9 * (level - 1))) & (X86_ENTRIES - 1)
}

/// # Safety
/// `pa` must be a page-aligned frame that is either a live page table or a
/// freshly-zeroed frame the caller is about to make one.
#[cfg(target_arch = "x86_64")]
unsafe fn x86_table_mut(pa: u64) -> *mut u64 {
    phys_to_virt(pa as usize).cast::<u64>()
}

/// Fetch the next table down, allocating and zeroing it if absent.
///
/// Returns `None` if a frame could not be allocated, or if the entry is a
/// huge page rather than a table pointer — splitting is not implemented.
#[cfg(target_arch = "x86_64")]
unsafe fn x86_next_table(entry_ptr: *mut u64, user: bool) -> Option<(u64, bool)> { unsafe {
    let entry = entry_ptr.read_volatile();
    if entry & X86_P != 0 {
        if entry & X86_PS != 0 {
            return None;
        }
        // Widen permissions on the way down: a parent that is not
        // user-accessible or not writable masks every child on x86, so an
        // intermediate entry has to be at least as permissive as any leaf
        // beneath it. Enforcement lives entirely in the leaf.
        let want = X86_P | X86_RW | if user { X86_US } else { 0 };
        if entry & want != want {
            entry_ptr.write_volatile((entry | want) & !X86_NX);
        }
        return Some((entry & X86_ADDR_MASK, false));
    }
    let frame = akuma_pmm::alloc_page()? as u64;
    core::ptr::write_bytes(x86_table_mut(frame).cast::<u8>(), 0, PAGE_SIZE);
    entry_ptr.write_volatile(frame | X86_P | X86_RW | if user { X86_US } else { 0 });
    Some((frame, true))
}}

/// Map one 4 KiB page, recording every page-table frame the walk allocates in
/// `ledger`.
///
/// **The ledger is not optional, and that is the whole point.** An untracked
/// table frame is not a leak the PMM can see: it is reachable only from the page
/// table that references it, so an address space torn down without this loses it
/// permanently. The AArch64 `map_page` discharges the same obligation with
/// `get_or_create_table`, which calls `track_page_table_frame` **and**
/// `track_frame(.., UserPageTable)` on every allocation; this is that, inside
/// the walk.
///
/// This took an `Option<&FrameLedger>` for about an hour, defaulting to `None`
/// so `map_page` could stay a one-liner — and `map_page` is the walk that
/// allocates the *first* three tables of every address space. The amd64 boot
/// test caught it as `page_table_frame_count() == 0` where three frames had just
/// been allocated (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`, B3). A parameter
/// that can be omitted will be.
#[cfg(target_arch = "x86_64")]
fn x86_map_page_in(
    root: u64,
    va: usize,
    pa: usize,
    prot: PteProt,
    attr: MemAttr,
    ledger: &FrameLedger,
) -> bool {
    assert_eq!(va % PAGE_SIZE, 0, "va must be page aligned");
    assert_eq!(pa % PAGE_SIZE, 0, "pa must be page aligned");
    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame reached through the physmap.
        let entry_ptr = unsafe { x86_table_mut(table).add(x86_index(va, level)) };
        match unsafe { x86_next_table(entry_ptr, prot.user) } {
            Some((next, allocated)) => {
                if allocated {
                    let frame = PhysFrame::new(next as usize);
                    track_frame(frame, FrameSource::UserPageTable);
                    ledger.track_page_table_frame(frame);
                }
                table = next;
            }
            None => return false,
        }
    }
    // SAFETY: `table` is the PT frame; the index is masked to 0..512.
    unsafe {
        x86_table_mut(table)
            .add(x86_index(va, 1))
            .write_volatile(pa as u64 | x86_encode(prot, attr));
    }
    akuma_cpu::tlb::invlpg(va);
    true
}

#[cfg(target_arch = "x86_64")]
fn x86_unmap_page_in(root: u64, va: usize) -> Option<usize> {
    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame reached through the physmap.
        let entry = unsafe { x86_table_mut(table).add(x86_index(va, level)).read_volatile() };
        if entry & X86_P == 0 || entry & X86_PS != 0 {
            return None;
        }
        table = entry & X86_ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame.
    let leaf = unsafe { x86_table_mut(table).add(x86_index(va, 1)) };
    let entry = unsafe { leaf.read_volatile() };
    if entry & X86_P == 0 {
        return None;
    }
    unsafe { leaf.write_volatile(0) };
    akuma_cpu::tlb::invlpg(va);
    Some((entry & X86_ADDR_MASK) as usize)
}

#[cfg(target_arch = "x86_64")]
fn x86_translate_in(root: u64, va: usize) -> Option<usize> {
    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame reached through the physmap.
        let entry = unsafe { x86_table_mut(table).add(x86_index(va, level)).read_volatile() };
        if entry & X86_P == 0 {
            return None;
        }
        if entry & X86_PS != 0 {
            // Only PD level (2) can carry PS here; a PDPT 1 GiB page is never
            // created by this code, so the size is fixed.
            let size = 1u64 << 21;
            return Some(((entry & X86_ADDR_MASK) + (va as u64 & (size - 1))) as usize);
        }
        table = entry & X86_ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame; the index is masked to 0..512.
    let entry = unsafe { x86_table_mut(table).add(x86_index(va, 1)).read_volatile() };
    if entry & X86_P == 0 {
        return None;
    }
    Some(((entry & X86_ADDR_MASK) + (va as u64 & (PAGE_SIZE as u64 - 1))) as usize)
}

/// Is `va` mapped **and reachable from ring 3** in the table rooted at `root`?
/// With `need_write`, also writable from ring 3.
///
/// The x86 counterpart of [`resolve_user_leaf`] + [`UserLeaf::user_accessible`],
/// and it is not a transliteration of it. On AArch64 the AP field of the **leaf**
/// decides on its own; on x86-64 the effective permission is the **AND of `U/S`
/// (and `R/W`) at every level of the walk**, so a PML4E with `U/S` clear makes
/// the whole 512 GiB region supervisor-only however the leaf is marked. Testing
/// only the leaf would report a kernel page as user-accessible — which is exactly
/// the hole `is_current_user_range_mapped`'s AP test was added to close on the
/// other architecture (`docs/archive/USER_COPY_FOLD.md` §7).
///
/// Added 2026-09-07 for C1: `get_current_ttbr0` had no x86 arm and answered `0`,
/// so `is_current_user_range_mapped` returned `false` for every pointer on this
/// target and every syscall folded into `akuma-syscalls-glue` came back `EFAULT`
/// from a real ring-3 caller. Silent, because the boot suite validates nothing —
/// it runs inside `BypassValidationGuard`
/// (`docs/archive/AKUMA_AMD64_C1_STEP3_PREREQUISITES.md`).
#[cfg(target_arch = "x86_64")]
fn x86_user_page_ok(root: u64, va: usize, need_write: bool) -> bool {
    let mut table = root;
    // Accumulated across levels, never reset — that is the whole point.
    let mut us = true;
    let mut rw = true;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame reached through the physmap;
        // the index is masked to 0..512 by `x86_index`.
        let entry = unsafe { x86_table_mut(table).add(x86_index(va, level)).read_volatile() };
        if entry & X86_P == 0 {
            return false;
        }
        us &= entry & X86_US != 0;
        rw &= entry & X86_RW != 0;
        if entry & X86_PS != 0 {
            // A large page ends the walk here.
            return us && (!need_write || rw);
        }
        table = entry & X86_ADDR_MASK;
    }
    // SAFETY: as above; `table` is the PT frame.
    let entry = unsafe { x86_table_mut(table).add(x86_index(va, 1)).read_volatile() };
    if entry & X86_P == 0 {
        return false;
    }
    us &= entry & X86_US != 0;
    rw &= entry & X86_RW != 0;
    us && (!need_write || rw)
}

/// Every page of `[va_start, va_start+len)`, checked with [`x86_user_page_ok`].
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
fn x86_user_range_ok(va_start: usize, len: usize, need_write: bool) -> bool {
    let root = get_current_ttbr0() as u64;
    if root == 0 {
        return false;
    }
    let start_page = va_start & !(PAGE_SIZE - 1);
    let end_page = (va_start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let num_pages = (end_page - start_page) / PAGE_SIZE;
    (0..num_pages).all(|i| x86_user_page_ok(root, start_page + i * PAGE_SIZE, need_write))
}

/// The active PML4, from `CR3`.
#[cfg(target_arch = "x86_64")]
fn x86_read_cr3() -> u64 {
    let v: u64;
    // SAFETY: reading CR3 copies a register into a local; it dereferences
    // nothing and has no side effect.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v & X86_ADDR_MASK
}

/// Install `root` as the active PML4.
///
/// Stays directly in this crate rather than `akuma-cpu`, for the same reason
/// the AArch64 `activate()`/`deactivate()`'s `msr ttbr0_el1` does: a safe
/// wrapper in a zero-dependency crate would let any safe code repoint what
/// the MMU dereferences.
///
/// # Safety
/// `root` must be a PML4 that maps every page the kernel is currently
/// executing from and every page it will touch before switching back — the
/// kernel image, its stacks, the heap, the PMM pool. A root missing any of
/// those faults on the instruction after `mov cr3`, with no way to report it.
#[cfg(target_arch = "x86_64")]
unsafe fn x86_write_cr3(root: u64) { unsafe {
    core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
}}

/// Copy-on-write marker. Bit 9 is one of three bits the hardware ignores in a
/// present leaf, and `amd64/src/paging.rs` already uses exactly this one — the
/// pinned divergence `akuma-cow` records, where AArch64 has no marker and is
/// handed `refs > 0` instead. Kept identical to that file's `COW` so the two
/// x86 walkers cannot disagree about what a demoted page looks like.
#[cfg(any(target_arch = "x86_64", test))]
const X86_COW: u64 = 1 << 9;

#[cfg(any(target_arch = "x86_64", test))]
impl PteProt {
    /// Kernel read-only, no execute. What a `PROT_NONE` user page becomes —
    /// x86 has no "present and wholly inaccessible" encoding, so clearing `U/S`
    /// is what makes a ring-3 touch fault.
    pub const KERNEL_RO: Self = Self { write: false, exec: false, user: false };
    /// User read only — no write, no execute.
    pub const USER_RO: Self = Self { write: false, exec: false, user: true };

    /// The x86 page-table spelling of a neutral [`Prot`].
    ///
    /// **A byte-for-byte port of `amd64::paging::PteProt::from_region`**, whose
    /// boot self-test pins every arm; `x86_prot_matches_amd64_encoding` pins the
    /// same six results here as literals, so the two walkers cannot drift. The
    /// two divergences from the AArch64 encoding are that file's, restated
    /// because this is where someone will look:
    ///
    /// * `RO` and `RX` collapse. They differ only in `PXN` — whether EL1 may
    ///   fetch — and x86 has one execute bit, not two.
    /// * `RW` (writable **and** executable on AArch64) becomes non-executable.
    ///   `sys_mmap` on this target refuses `PROT_WRITE | PROT_EXEC`, so no
    ///   region can carry it; if one ever does, dropping execute faults at the
    ///   fetch — visible and debuggable — where granting it would hand ring 3 a
    ///   writable code page.
    #[must_use]
    pub const fn from_region(prot: akuma_mmap::Prot) -> Self {
        match prot.tag() {
            0 => Self::KERNEL_RO,
            1 | 4 => Self::USER_RX,
            2 | 3 => Self::USER_RW,
            5 => Self::USER_RO,
            // Unreachable over `Prot::ALL`. A `const fn` cannot panic out of a
            // `u8` match, so the fallback is **fail-closed**: a seventh variant
            // maps to a page ring 3 cannot touch, which faults visibly instead
            // of over-granting.
            _ => Self::KERNEL_RO,
        }
    }
}

/// Walk to the leaf slot for `va`, without allocating. `None` if any level is
/// absent or a huge page (this walker never creates one, so a `PS` entry on the
/// way down means the mapping is not a 4 KiB page and is not ours to edit).
#[cfg(target_arch = "x86_64")]
fn x86_leaf_slot_in(root: u64, va: usize) -> Option<*mut u64> {
    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame reached through the physmap.
        let entry = unsafe { x86_table_mut(table).add(x86_index(va, level)).read_volatile() };
        if entry & X86_P == 0 || entry & X86_PS != 0 {
            return None;
        }
        table = entry & X86_ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame; the index is masked to 0..512.
    Some(unsafe { x86_table_mut(table).add(x86_index(va, 1)) })
}

/// One past the last user virtual address: the low half of a 4-level x86_64
/// address space, `2^47`. PML4 slots 0..256 — everything at or above this is
/// the kernel's shared upper half, which no user walk may touch.
#[cfg(any(target_arch = "x86_64", test))]
pub const USER_HALF_END: usize = 1 << 47;

/// Bytes one page-table entry at `level` spans: 512 GiB, 1 GiB, 2 MiB, 4 KiB.
///
/// Narrower `cfg` than its neighbours on purpose: it is private and its only
/// caller is [`x86_walk_leaves`], so compiling it under `test` on a non-x86 host
/// would be dead code.
#[cfg(target_arch = "x86_64")]
const fn x86_entry_span(level: u32) -> usize {
    1usize << (12 + 9 * (level - 1))
}

/// The one range walk behind [`UserAddressSpace::for_each_leaf_in_range`],
/// [`UserAddressSpace::for_each_user_leaf`] and
/// [`UserAddressSpace::rewrite_leaves_in_range`]. Returns the number of present
/// leaves visited.
///
/// Descends once per entry and skips an absent subtree whole — see the public
/// methods for why that matters. `f` decides what happens to each leaf; the
/// slot pointer never escapes, so a caller cannot hold a page-table pointer
/// across an operation that might free the table it lives in.
///
/// # Safety of editing under the walk
///
/// `Unmap` and `Reprotect` write the leaf slot this descent is standing on and
/// nothing else. No page **table** is ever freed here, so every pointer the walk
/// still holds stays live for the rest of the visit. Freeing an emptied table is
/// deliberately not done: it would invalidate the parent entry the cursor is
/// walking, and the frames are reclaimed at teardown by the ledger anyway.
#[cfg(target_arch = "x86_64")]
fn x86_walk_leaves(
    root: u64,
    start: usize,
    end: usize,
    mut f: impl FnMut(Leaf) -> LeafAction,
) -> usize {
    let mut va = start & !(PAGE_SIZE - 1);
    let mut seen = 0usize;
    while va < end {
        // SAFETY: every table is reached through the physmap; `root` is a live
        // PML4 and each descent is guarded by the entry's present bit.
        let step = unsafe {
            let mut table = root;
            let mut missing = 0u32;
            for level in (2..=4).rev() {
                let entry = x86_table_mut(table).add(x86_index(va, level)).read_volatile();
                if entry & X86_P == 0 || entry & X86_PS != 0 {
                    missing = level;
                    break;
                }
                table = entry & X86_ADDR_MASK;
            }
            if missing != 0 {
                // Nothing present under this entry: jump to the next one at the
                // level that was missing, so an empty gigabyte costs one read.
                let span = x86_entry_span(missing);
                span - (va & (span - 1))
            } else {
                let slot = x86_table_mut(table).add(x86_index(va, 1));
                let entry = slot.read_volatile();
                if entry & X86_P != 0 {
                    seen += 1;
                    let leaf = Leaf {
                        va,
                        pa: (entry & X86_ADDR_MASK) as usize,
                        prot: PteProt {
                            write: entry & X86_RW != 0,
                            exec: entry & X86_NX == 0,
                            user: entry & X86_US != 0,
                        },
                        cow: entry & X86_COW != 0,
                    };
                    match f(leaf) {
                        LeafAction::Keep => {}
                        LeafAction::Unmap => {
                            slot.write_volatile(0);
                            akuma_cpu::tlb::invlpg(va);
                        }
                        LeafAction::Reprotect(prot, cow) => {
                            let rewritten = (entry & X86_ADDR_MASK)
                                | x86_encode(prot, MemAttr::WriteBack)
                                | if cow { X86_COW } else { 0 };
                            slot.write_volatile(rewritten);
                            akuma_cpu::tlb::invlpg(va);
                        }
                    }
                }
                PAGE_SIZE
            }
        };
        // `checked_add` rather than `+`: a range ending at the top of the
        // address space would otherwise wrap the cursor back to 0 and loop
        // forever, and `end` comes from ring 3.
        match va.checked_add(step) {
            Some(next) => va = next,
            None => break,
        }
    }
    seen
}

/// The kernel's own boot PML4, captured the first time a `UserAddressSpace`
/// is created (at that point nothing but the boot root has ever been active).
/// `deactivate()`'s x86_64 analogue of [`get_boot_ttbr0`] — that one reads a
/// value `boot.s` records to a fixed symbol at assembly-time boot; this target
/// has no such symbol yet, so the first live `CR3` read stands in for it. Only
/// correct for a single boot address space at a time, which matches this
/// pass's scope (see `proposals/AKUMA_MMU_ARCH_PORTABILITY.md` Phase 4's
/// explicit non-goals).
#[cfg(target_arch = "x86_64")]
static X86_BOOT_ROOT: AtomicU64 = AtomicU64::new(0);

#[cfg(target_arch = "x86_64")]
pub struct UserAddressSpace {
    root: usize,
    /// Which physical frames this address space holds, and how many VAs map
    /// each. The same arch-neutral, host-tested `akuma-user-space` ledger the
    /// AArch64 struct carries — everything else here is the x86 walker.
    ledger: FrameLedger,
}

#[cfg(target_arch = "x86_64")]
impl UserAddressSpace {
    /// PML4 slots shared with the kernel: the physmap, the device window, and
    /// the kernel image itself. Matches `amd64::paging::AddressSpace`'s
    /// `SHARED_PML4_SLOTS` exactly — sharing at PML4 level rather than
    /// copying is what lets there be one kernel mapping that cannot drift.
    /// This type has exactly one real caller (`amd64/`'s own kernel layout),
    /// and hardcoding its convention here is what "a port, not a redesign"
    /// means in practice; a second x86_64 target with a different layout
    /// would need this list supplied rather than assumed.
    const SHARED_PML4_SLOTS: [usize; 3] = [256, 257, 511];

    pub fn new() -> Option<Self> {
        let root = akuma_pmm::alloc_page()?;
        let kroot = x86_read_cr3();
        let _ = X86_BOOT_ROOT.compare_exchange(
            0, kroot, Ordering::AcqRel, Ordering::Acquire,
        );
        // SAFETY: `root` is a fresh PMM frame reached through the physmap.
        // Zeroing it is what makes it a valid empty table; the shared entries
        // written below (aliased, not copied, from the live kernel root) are
        // the only non-zero ones, so the whole lower half starts unmapped.
        unsafe {
            core::ptr::write_bytes(phys_to_virt(root), 0, PAGE_SIZE);
            let kroot_ptr = x86_table_mut(kroot);
            let root_ptr = x86_table_mut(root as u64);
            for slot in Self::SHARED_PML4_SLOTS {
                let e = kroot_ptr.add(slot).read_volatile();
                root_ptr.add(slot).write_volatile(e);
            }
        }
        Some(Self { root, ledger: FrameLedger::new(false) })
    }

    /// A borrowed view of an existing address space (`CLONE_VM` / `vfork`):
    /// the same PML4, and a ledger that owns nothing.
    ///
    /// **Three pinned divergences from the AArch64 `new_shared`**, all of them
    /// consequences of things this target does not have:
    ///
    /// * No ASID to allocate, so this cannot fail for ASID exhaustion — and
    ///   therefore cannot fail at all. It still returns `Option` because the
    ///   shared callers in `akuma-exec` are written against a fallible
    ///   constructor; making it infallible here would fork the call sites.
    /// * No `SHARED_L0_TABLE` refcount registry. That registry exists to defer
    ///   a shared L0's user frames until the last viewer drops, and this target
    ///   has no `Drop` impl at all (see the note below the impl) — there is no
    ///   teardown for it to arbitrate. A registry with no consumer would be a
    ///   second design, not a port.
    /// * No TLB consequence. AArch64 gives the view its own ASID; here both
    ///   views name the same `CR3` value, so a switch between them is not even
    ///   observable to the MMU.
    pub fn new_shared(parent_l0_phys: usize) -> Option<Self> {
        Some(Self { root: parent_l0_phys, ledger: FrameLedger::new(true) })
    }

    // ── scalar identity ────────────────────────────────────────────────────

    /// The AArch64 `TTBR0_EL1` value, ported: `(asid << 48) | l0_phys`.
    ///
    /// x86 has no ASID in this port (see [`Self::asid`]), so the high half is
    /// zero and this is the PML4 physical base — which is exactly the `CR3`
    /// value `activate` writes. `ProcAddressSpace`'s lock-free scalar mirror
    /// packs and unpacks this word on both architectures, so the layout has to
    /// match even where one field is always zero.
    pub fn ttbr0(&self) -> u64 { self.root as u64 }

    pub fn l0_phys(&self) -> usize { self.root }

    pub fn is_shared(&self) -> bool { self.ledger.is_shared() }

    /// Always `0` — a **pinned divergence**, not a stub.
    ///
    /// AArch64 tags every TLB entry with an ASID so a context switch need not
    /// flush; this port rewrites `CR3`, which flushes non-global entries
    /// wholesale (the seams table in `docs/archive/AKUMA_SELF_HOSTING_AMD64.md`).
    /// PCID is the x86 equivalent and is deliberately not used here — enabling
    /// it without also porting the invalidation discipline (`invpcid`, and a
    /// shootdown for the cores that are not doing the switch) would leave stale
    /// translations live under a correct-looking ASID number.
    ///
    /// Callers use this for two things, and both stay correct at zero: naming a
    /// TLB flush target (this target flushes by address or by `CR3`, never by
    /// ASID) and diagnostics.
    pub fn asid(&self) -> u16 { 0 }

    // ── the frame ledger ───────────────────────────────────────────────────
    //
    // Straight forwards to `akuma-user-space`, identical to the AArch64 side's.
    // They are `&self` because the ledger self-locks.

    pub fn track_user_frame(&self, frame: PhysFrame) { self.ledger.track_user_frame(frame); }
    pub fn adopt_user_frame(&self, frame: PhysFrame, caller_holds_ref: bool) -> bool {
        self.ledger.adopt_user_frame(frame, caller_holds_ref)
    }
    pub fn tracks_user_frame(&self, pa: usize) -> bool { self.ledger.tracks_user_frame(pa) }
    pub fn track_page_table_frame(&self, frame: PhysFrame) { self.ledger.track_page_table_frame(frame); }
    pub fn remove_user_frame(&self, frame: PhysFrame) -> bool { self.ledger.remove_user_frame(frame) }
    pub fn user_frame_count(&self) -> usize { self.ledger.user_frame_count() }
    pub fn user_frame_total_refs(&self) -> usize { self.ledger.user_frame_total_refs() }
    pub fn page_table_frame_count(&self) -> usize { self.ledger.page_table_frame_count() }
    pub fn resident_pages(&self) -> usize { self.ledger.resident_pages() }

    // ── mapping ────────────────────────────────────────────────────────────

    /// Map `va` to `pa` with the protection `user_flags` names.
    ///
    /// `user_flags` is an AArch64 PTE flag word because that is what
    /// `akuma-exec` holds — it is decoded to a neutral `Prot` by
    /// [`user_flags::from_pte`] and re-encoded into x86 bits here. See that
    /// function's header for why a `u64` crossing this call is not the
    /// portability defect `akuma_mmap::types` warns about.
    pub fn map_page(&mut self, va: usize, pa: usize, user_flags: u64) -> Result<(), &'static str> {
        if va & (PAGE_SIZE - 1) != 0 || pa & (PAGE_SIZE - 1) != 0 {
            return Err("Addresses must be page-aligned");
        }
        let prot = PteProt::from_region(user_flags::from_pte(user_flags));
        if x86_map_page_in(self.root as u64, va, pa, prot, MemAttr::WriteBack, &self.ledger) {
            Ok(())
        } else {
            Err("x86_64 UserAddressSpace::map_page: page-table frame allocation failed")
        }
    }

    /// Allocate a **zeroed** frame, map it at `va`, and record it in the
    /// ledger. Zeroing is contractual — see `akuma_elf::UserPages`.
    pub fn alloc_and_map(&mut self, va: usize, user_flags: u64) -> Result<PhysFrame, &'static str> {
        let frame = akuma_pmm::alloc_page_zeroed()
            .map(PhysFrame::new)
            .ok_or("Out of memory for user page")?;
        track_frame(frame, FrameSource::ElfLoader);
        self.map_and_track(va, frame, user_flags)?;
        Ok(frame)
    }

    /// Install an *already-allocated* frame at `va` and record it — the
    /// "install" half of [`alloc_and_map`], split for the same reason it is
    /// split on AArch64: a caller can allocate outside the per-AS lock and hold
    /// it only across the PTE edit.
    pub fn map_and_track(&mut self, va: usize, frame: PhysFrame, user_flags: u64) -> Result<(), &'static str> {
        self.ledger.track_user_frame(frame);
        self.map_page(va, frame.addr, user_flags)
    }

    /// Map `va` in the **currently installed** address space and track both the
    /// data frame and any page-table frames the walk allocated.
    ///
    /// The x86 port of the AArch64 method's clause 2/clause 3 checks: the VA
    /// must be a page-aligned lower-half address, and `CR3` must already hold
    /// this address space's root — refusing rather than editing another
    /// process's tables.
    #[must_use = "`false` means the PTE was NOT installed"]
    // `&mut self` is the safety argument, not a mutation requirement: it is what
    // proves the caller holds the per-AS lock. Matches the AArch64 method.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub fn map_user_page_tracked(&mut self, va: usize, frame: PhysFrame, user_flags_val: u64) -> bool {
        self.map_user_page_tracked_inner(va, frame, user_flags_val, true)
    }

    /// [`map_user_page_tracked`](Self::map_user_page_tracked) without the
    /// per-page invalidation, for a batch install.
    ///
    /// On this target the per-page `invlpg` is issued by `x86_map_page_in`
    /// unconditionally, so `flush` currently only documents caller intent —
    /// a **pinned divergence**: the AArch64 `no_flush` form genuinely skips the
    /// `tlbi` and requires the caller to issue a range flush, and skipping
    /// `invlpg` here would need the same range flush to exist first. Suppressing
    /// it without that would leave stale translations live, so the batch form
    /// costs one `invlpg` per page here rather than being wrong.
    #[must_use = "`false` means the PTE was NOT installed"]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub fn map_user_page_tracked_no_flush(
        &mut self,
        va: usize,
        frame: PhysFrame,
        user_flags_val: u64,
    ) -> bool {
        self.map_user_page_tracked_inner(va, frame, user_flags_val, false)
    }

    #[allow(clippy::needless_pass_by_ref_mut)]
    fn map_user_page_tracked_inner(
        &mut self,
        va: usize,
        frame: PhysFrame,
        user_flags_val: u64,
        _flush: bool,
    ) -> bool {
        // Clause 2: the lower half is userspace and the walk indexes by page.
        if va >> 47 != 0 || va & (PAGE_SIZE - 1) != 0 {
            log::debug!("[MMU] map_and_track refused non-user/unaligned va=0x{va:x}");
            debug_assert!(false, "map_and_track called with a non-user or unaligned va");
            return false;
        }
        // Clause 3: refuse rather than edit somebody else's page tables.
        let installed = x86_read_cr3();
        if installed != (self.root as u64 & X86_ADDR_MASK) {
            log::debug!(
                "[MMU] map_and_track refused: this AS root=0x{:x} but CR3 has 0x{installed:x}",
                self.root
            );
            debug_assert!(false, "map_and_track on an address space that is not installed");
            return false;
        }
        // Unlike AArch64's `map_user_page_inner`, the x86 walk has no
        // "another thread won the race" arm to report: it overwrites the leaf.
        // Tracking the frame on both arms is therefore unconditional, which is
        // what clause 4 of the AArch64 contract asks for.
        let prot = PteProt::from_region(user_flags::from_pte(user_flags_val));
        let mapped = x86_map_page_in(
            self.root as u64, va, frame.addr, prot, MemAttr::WriteBack, &self.ledger,
        );
        if mapped {
            self.ledger.track_user_frame(frame);
        }
        mapped
    }

    /// Write `bytes` at `offset` within the page mapped at `page_va`.
    pub fn write_page_bytes(&mut self, page_va: usize, offset: usize, bytes: &[u8]) -> bool {
        assert!(
            offset + bytes.len() <= PAGE_SIZE,
            "write_page_bytes {offset}+{} would leave the page",
            bytes.len()
        );
        let page_base = page_va & !(PAGE_SIZE - 1);
        let Some(pa) = self.translate(page_base) else {
            return false;
        };
        // SAFETY: `translate` just confirmed `page_base` is mapped in *this*
        // address space, so `pa` is a real frame reachable through the physmap.
        // `&mut self` makes this borrow exclusive; the assert bounds the write
        // to the single frame backing that page.
        unsafe {
            let dst = akuma_primitives::addr::phys_to_virt(pa + offset);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
        }
        true
    }

    // ── permission changes ─────────────────────────────────────────────────

    /// Demote every writable user leaf in `[va_start, va_start + pages·4K)` to
    /// read-only **and mark it copy-on-write**; returns the count demoted.
    ///
    /// The marker is not optional here, and this is the one place the two
    /// architectures genuinely cannot share a body. AArch64 has no CoW bit: a
    /// demoted page is indistinguishable from an `mprotect(PROT_READ)` one, and
    /// `akuma-cow` is handed `refs > 0` in place of a marker. x86 has bit 9
    /// free, `amd64/src/paging.rs` uses it, and `akuma_cow`'s pinned divergence
    /// note names this target as the one that passes a real PTE bit. Demoting
    /// without setting it would make every CoW fault here look like a genuine
    /// protection violation.
    pub fn demote_range_to_ro(&mut self, va_start: usize, pages: usize) -> usize {
        let mut demoted = 0;
        for i in 0..pages {
            let va = (va_start & !(PAGE_SIZE - 1)) + i * PAGE_SIZE;
            let Some(leaf) = x86_leaf_slot_in(self.root as u64, va) else { continue };
            // SAFETY: `x86_leaf_slot_in` returned a pointer into a live PT frame
            // reached through the physmap; `&mut self` is the exclusivity the
            // write needs.
            unsafe {
                let entry = leaf.read_volatile();
                // Only writable *user* leaves are candidates: a kernel leaf is
                // not this process's to demote, and a read-only one is either
                // already shared or genuinely `PROT_READ`.
                if entry & X86_P == 0 || entry & X86_US == 0 || entry & X86_RW == 0 {
                    continue;
                }
                leaf.write_volatile((entry & !X86_RW) | X86_COW);
            }
            akuma_cpu::tlb::invlpg(va);
            demoted += 1;
        }
        demoted
    }

    /// Replace the permission bits of an existing leaf, preserving its physical
    /// address and every non-permission bit. `new_flags` is an AArch64 flag
    /// word, decoded as in [`map_page`](Self::map_page).
    ///
    /// Infallible, matching the AArch64 signature: an unmapped `va` is the
    /// normal `MAP_FIXED`-over-a-lazy-range case, not an error.
    pub fn update_page_flags(&mut self, va: usize, new_flags: u64) {
        self.update_page_flags_inner(va, new_flags, true);
    }

    /// [`update_page_flags`](Self::update_page_flags) without the per-page
    /// invalidation, for an `mprotect` over many pages. The caller issues one
    /// flush over the whole range afterwards.
    pub fn update_page_flags_no_flush(&mut self, va: usize, new_flags: u64) {
        self.update_page_flags_inner(va, new_flags, false);
    }

    /// The body behind both `update_page_flags*` variants — same split, and the
    /// same reason, as the AArch64 pair: `flush` is a literal at both call sites
    /// so the branch folds away.
    ///
    /// `PERM_MASK` is the x86 spelling of the AArch64 body's: the bits that say
    /// *what may be done* with the page, and nothing else. Everything outside it
    /// survives — the frame address, `P`, the hardware's Accessed/Dirty bits,
    /// the cacheability pair, and the [`X86_COW`] marker.
    ///
    /// Preserving `X86_COW` is deliberate. An `mprotect(PROT_WRITE)` over a
    /// CoW-demoted page then yields writable-and-marked, which never faults and
    /// so never breaks the sharing — but that is **exactly** what the AArch64
    /// body produces from the same call (it has no marker; the page simply
    /// becomes `AP_RW_ALL` while `refs > 0`). Clearing the bit here would make
    /// the two architectures diverge under `mprotect`, which is a worse failure
    /// than the shared one they already have.
    #[inline]
    fn update_page_flags_inner(&self, va: usize, new_flags: u64, flush: bool) {
        let _irq_guard = IrqGuard::new();
        const PERM_MASK: u64 = X86_RW | X86_US | X86_NX;
        let va = va & !(PAGE_SIZE - 1);
        let Some(leaf) = x86_leaf_slot_in(self.root as u64, va) else { return };
        let prot = PteProt::from_region(user_flags::from_pte(new_flags));
        // SAFETY: a live PT slot reached through the physmap. `&self` matches
        // the AArch64 body, which also edits through a raw pointer; the public
        // wrappers keep `&mut self`, which is the API callers hold.
        unsafe {
            let old_entry = leaf.read_volatile();
            if old_entry & X86_P == 0 {
                return;
            }
            // `x86_encode` re-asserts `P`, which is already set; `MemAttr` is
            // outside `PERM_MASK`, so a `WriteBack` encode adds nothing and a
            // device page keeps its `PCD`/`PWT`.
            let entry = (old_entry & !PERM_MASK) | x86_encode(prot, MemAttr::WriteBack);
            leaf.write_volatile(entry);
        }
        if flush {
            akuma_cpu::tlb::invlpg(va);
        }
    }

    /// Unmap a page and hand back its frame, dropping this address space's
    /// reference to it. `None` when the page was not mapped **or** when another
    /// VA still references the frame — see the AArch64 twin for why that second
    /// case must not be a free.
    pub fn unmap_and_free_page(&mut self, va: usize) -> Option<PhysFrame> {
        let frame = self.unmap_and_free_page_no_flush(va);
        akuma_cpu::tlb::invlpg(va & !(PAGE_SIZE - 1));
        frame
    }

    /// [`unmap_and_free_page`](Self::unmap_and_free_page) without the per-page
    /// invalidation — the hot path for a large `munmap`, where the caller
    /// batches one flush over the whole region.
    pub fn unmap_and_free_page_no_flush(&mut self, va: usize) -> Option<PhysFrame> {
        let _irq_guard = IrqGuard::new();
        let va = va & !(PAGE_SIZE - 1);
        let leaf = x86_leaf_slot_in(self.root as u64, va)?;
        // SAFETY: a live PT slot reached through the physmap.
        let pa = unsafe {
            let entry = leaf.read_volatile();
            if entry & X86_P == 0 {
                return None;
            }
            leaf.write_volatile(0);
            (entry & X86_ADDR_MASK) as usize
        };
        // Only hand the frame back when this dropped its *last* reference. If
        // it is still mapped at another VA, or belongs to the owner of a shared
        // view, `remove_user_frame` says so and freeing it here would be a
        // double free — the AArch64 twin's `EL1 EC=0x22` crashes under low
        // memory. The PTE is cleared either way.
        let frame = PhysFrame::new(pa);
        if self.remove_user_frame(frame) { Some(frame) } else { None }
    }

    /// Clear the leaf PTE for `va` without freeing anything or flushing.
    pub fn unmap_page_no_flush(&mut self, va: usize) {
        let _irq_guard = IrqGuard::new();
        let va = va & !(PAGE_SIZE - 1);
        let Some(leaf) = x86_leaf_slot_in(self.root as u64, va) else { return };
        // SAFETY: a live PT slot reached through the physmap. Unconditional:
        // this copy has never read the leaf before clearing it.
        unsafe { leaf.write_volatile(0) };
    }

    /// Unmap a clean read-only user page and hand its frame back, so the next
    /// access re-faults and re-reads it from the backing file. The mechanism
    /// that lets a file-backed mmap larger than RAM make progress under
    /// pressure. The TLB entry is invalidated **before** the frame is released.
    ///
    /// The eligibility test is the exact port of AArch64's — `AP_RO_ALL` is
    /// "present, reachable by the user, not writable", which on x86 is
    /// `P | US | !RW`. The CoW marker is deliberately **not** consulted: the
    /// AArch64 side has no marker to consult, so reading it here would be a
    /// second design rather than a port, and a CoW page that has not been
    /// written is clean by definition — which is the property this test is
    /// actually asking about.
    pub fn try_evict_ro_page(&mut self, va: usize) -> Option<PhysFrame> {
        let _irq_guard = IrqGuard::new();
        let va = va & !(PAGE_SIZE - 1);
        let leaf = x86_leaf_slot_in(self.root as u64, va)?;
        // SAFETY: a live PT slot reached through the physmap.
        let pa = unsafe {
            let entry = leaf.read_volatile();
            if entry & X86_P == 0 || entry & X86_US == 0 || entry & X86_RW != 0 {
                return None;
            }
            leaf.write_volatile(0);
            (entry & X86_ADDR_MASK) as usize
        };
        akuma_cpu::tlb::invlpg(va);
        let frame = PhysFrame::new(pa);
        if self.remove_user_frame(frame) { Some(frame) } else { None }
    }

    /// Zero the physical page backing `va` without unmapping it.
    pub fn zero_mapped_page(&self, va: usize) -> bool {
        let _irq_guard = IrqGuard::new();
        let Some(pa) = self.translate(va & !(PAGE_SIZE - 1)) else { return false };
        // SAFETY: `translate` confirmed the mapping, so `pa` is a live frame
        // reachable through the physmap, and a whole page of it is ours.
        unsafe {
            core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, PAGE_SIZE);
        }
        true
    }

    // ── queries ────────────────────────────────────────────────────────────

    /// Raw leaf page-table entry for `va`, if mapped at the final level.
    ///
    /// The name says `l3` because the AArch64 walker calls its last level L3;
    /// on x86 this is the PT entry. Kept rather than renamed so the shared
    /// callers in `akuma-exec` need no `cfg` — a **pinned naming divergence**,
    /// and the bits it returns are x86 bits, so a caller that decodes them must
    /// do so per-architecture.
    pub fn read_l3_page_entry(&self, va: usize) -> Option<u64> {
        let va = va & !(PAGE_SIZE - 1);
        let _irq_guard = IrqGuard::new();
        let leaf = x86_leaf_slot_in(self.root as u64, va)?;
        // SAFETY: a live PT slot reached through the physmap.
        let entry = unsafe { leaf.read_volatile() };
        if entry & X86_P == 0 { None } else { Some(entry) }
    }

    /// Physical address of the 4KiB frame backing `va`, if mapped.
    pub fn phys_addr_for_page_va(&self, va: usize) -> Option<usize> {
        Some((self.read_l3_page_entry(va)? & X86_ADDR_MASK) as usize)
    }

    /// A **no-op**, and a pinned divergence rather than an omission: x86
    /// instruction caches are coherent with stores, so there is nothing for the
    /// AArch64 `dc cvau`/`ic ivau` pair to correspond to. The method exists
    /// because the shared callers (`akuma-exec`'s W^X flip and file-backed text
    /// paging) call it unconditionally, and a `cfg` at each of those sites would
    /// scatter the same fact across the tree.
    #[allow(clippy::unused_self)]
    pub fn invalidate_icache_for_page_va(&self, _va: usize) {}

    /// Whether `va` has a live leaf in **this** address space.
    pub fn is_mapped(&self, va: usize) -> bool {
        x86_translate_in(self.root as u64, va).is_some()
    }

    /// Whether every page the byte range `[va_start, va_start + len)` touches is
    /// mapped. An empty range is trivially mapped, matching AArch64.
    pub fn is_range_mapped(&self, va_start: usize, len: usize) -> bool {
        if len == 0 {
            return true;
        }
        let start_page = va_start & !(PAGE_SIZE - 1);
        let end_page = (va_start + len).div_ceil(PAGE_SIZE) * PAGE_SIZE;
        let mut va = start_page;
        while va < end_page {
            if !self.is_mapped(va) {
                return false;
            }
            va += PAGE_SIZE;
        }
        true
    }

    pub fn unmap_page(&mut self, va: usize) {
        x86_unmap_page_in(self.root as u64, va);
    }

    // ── range walks ────────────────────────────────────────────────────────

    /// Visit every **present 4 KiB leaf** in `[start, end)`.
    ///
    /// # Why a walk at all, when the AArch64 side has none
    ///
    /// The two kernels enumerate a mapping differently and always have. AArch64
    /// iterates *virtual addresses from the region list* (`akuma-mmap`) and asks
    /// the table about each; this target iterates *leaves from the table*. That
    /// is not a gap on one side — it is what `munmap`, `mprotect`, `mremap`,
    /// `madvise`, `fork`'s CoW demote and `/proc/self/maps` are all written
    /// against here, and porting them to the other shape is C2's business, with
    /// the syscall fold in front of it. So this is deliberately **x86-only**:
    /// giving AArch64 a twin nothing calls would be a fake unification and would
    /// move that kernel's `.text` for no reader.
    ///
    /// # It skips absent subtrees whole
    ///
    /// The naive shape — `for va in (start..end).step_by(4096)` with a
    /// four-level walk each — costs a full descent per page whether or not
    /// anything is mapped. A lazy `PROT_NONE` reservation of a gigabyte is
    /// 262 144 pages of which zero are present, and `rustc` makes reservations
    /// that size. This descends once per *entry*: a missing PML4 entry advances
    /// the cursor 512 GiB, a missing PDPT entry 1 GiB, a missing PD entry 2 MiB.
    /// An empty range costs a handful of reads however long it is.
    ///
    /// A 2 MiB page (`PS` at PD level) is **skipped, not reported**. This walker
    /// never creates one for a user address space — `x86_map_page_in` always
    /// builds down to a PT — and reporting one as a 4 KiB leaf would hand the
    /// caller a frame 512 times the size it thinks.
    pub fn for_each_leaf_in_range(&self, start: usize, end: usize, mut f: impl FnMut(Leaf)) {
        x86_walk_leaves(self.root as u64, start, end, |leaf| {
            f(leaf);
            LeafAction::Keep
        });
    }

    /// Visit every present leaf of the whole **user half** (`0 .. 2^47`).
    ///
    /// `fork`'s copy-on-write demote and `/proc/self/maps` want the address
    /// space rather than a range. Expressed through
    /// [`for_each_leaf_in_range`](Self::for_each_leaf_in_range) rather than as a
    /// second walk: the subtree skipping makes the whole-space form cost 256
    /// PML4 reads plus whatever is actually mapped, so there is nothing a
    /// separate implementation would buy.
    ///
    /// Never touches the upper half, which is the kernel's shared tables.
    pub fn for_each_user_leaf(&self, f: impl FnMut(Leaf)) {
        self.for_each_leaf_in_range(0, USER_HALF_END, f);
    }

    /// Visit every present leaf in `[start, end)` and apply the caller's
    /// [`LeafAction`] to each, in **one** descent. Returns how many leaves were
    /// visited.
    ///
    /// This is the mutating half of
    /// [`for_each_leaf_in_range`](Self::for_each_leaf_in_range), and it exists
    /// so `munmap` allocates nothing. The two-phase alternative — walk into a
    /// `Vec<Leaf>`, then call `unmap_page` per entry — costs a heap allocation
    /// sized by *residency* on the one syscall that runs when memory is short,
    /// plus a second four-level descent per page. Here the leaf slot is already
    /// in hand.
    ///
    /// `Unmap` clears the entry and issues `invlpg`; it does **not** free the
    /// frame or touch its refcount, because the caller was handed the `pa` and
    /// that decision is the PMM's. `Reprotect` rewrites permissions and the CoW
    /// marker onto the same frame.
    pub fn rewrite_leaves_in_range(
        &mut self,
        start: usize,
        end: usize,
        f: impl FnMut(Leaf) -> LeafAction,
    ) -> usize {
        x86_walk_leaves(self.root as u64, start, end, f)
    }


    pub fn translate(&self, va: usize) -> Option<usize> {
        x86_translate_in(self.root as u64, va)
    }

    /// Switch the active address space to this one.
    ///
    /// # Safety obligation carried by construction
    /// `activate`/`deactivate` on the AArch64 side take no unsafe block at
    /// their call site because `UserAddressSpace::new`'s shared-mapping
    /// discipline discharges it; the same is true here — `new()` aliases the
    /// kernel's own PML4 slots, so `self.root` maps everything the kernel
    /// needs before the next line executes.
    pub fn activate(&self) {
        unsafe { x86_write_cr3(self.root as u64) };
    }

    /// Switch back to the kernel's own boot address space.
    ///
    /// No-op if nothing has captured a boot root yet (no `UserAddressSpace`
    /// has ever been created) — matches the aarch64 side's `get_boot_ttbr0`
    /// returning `0` before boot identity mapping runs, which its own
    /// `deactivate()` installs unconditionally; this one skips the write
    /// instead, since writing `0` to `CR3` on x86_64 is not "boot identity
    /// map", it is "no page tables at all".
    pub fn deactivate() {
        let root = X86_BOOT_ROOT.load(Ordering::Acquire);
        if root != 0 {
            // SAFETY: `root` is the PML4 that was active before any
            // `UserAddressSpace` on this target ever existed — necessarily
            // one that already maps the whole running kernel.
            unsafe { x86_write_cr3(root) };
        }
    }
}

// Deliberately no `Drop` impl, matching `amd64::paging::AddressSpace`'s own
// choice and its stated reason: freeing an address space that is still
// installed in `CR3` unmaps the code doing the freeing. Dropping a
// `UserAddressSpace` on this target leaks its page-table frames rather than
// risk that — asking for the free explicitly is `amd64::paging::AddressSpace
// ::free`'s job, not replicated here since nothing in this pass's scope
// tears one down.

/// Populate an L3 page table from a 2MB block descriptor, preserving the
/// block's identity mapping as 512 individual 4KB page entries.
pub unsafe fn shatter_block_to_pages(l3_frame_addr: usize, block_entry: u64) {
    let l3_ptr = phys_to_virt(l3_frame_addr) as *mut u64;
    let block_pa = block_entry & 0x0000_FFFF_FFE0_0000; // 2MB-aligned PA
    let attrs = block_entry & 0xFFF0_0000_0000_0FFC; // upper[63:52] + lower[11:2]
    for i in 0..512u64 {
        let page_pa = block_pa + (i << 12);
        unsafe {
            l3_ptr.add(i as usize).write_volatile(page_pa | attrs | flags::VALID | flags::TABLE);
        }
    }
}

/// Intermediate page-table frames allocated by one [`map_user_page`] call.
///
/// AArch64's 4-level walk (L0 always pre-exists for a live address space) can
/// allocate at most one new frame per level of L1/L2/L3 — 3 total — so this is a
/// fixed 3-slot array rather than a heap `Vec`, on a path the page-fault handler
/// calls once per page (`docs/archive/VEC_AUDIT.md` #1).
#[derive(Default)]
pub struct TableFrames {
    frames: [Option<PhysFrame>; 3],
}

impl TableFrames {
    fn push(&mut self, frame: PhysFrame) {
        for slot in &mut self.frames {
            if slot.is_none() {
                *slot = Some(frame);
                return;
            }
        }
        debug_assert!(false, "more than 3 page-table frames allocated by one map_user_page call");
    }

    fn iter(&self) -> core::iter::Flatten<core::slice::Iter<'_, Option<PhysFrame>>> {
        self.frames.iter().flatten()
    }
}

impl IntoIterator for TableFrames {
    type Item = PhysFrame;
    type IntoIter = core::iter::Flatten<core::array::IntoIter<Option<PhysFrame>, 3>>;
    fn into_iter(self) -> Self::IntoIter {
        self.frames.into_iter().flatten()
    }
}

impl<'a> IntoIterator for &'a TableFrames {
    type Item = &'a PhysFrame;
    type IntoIter = core::iter::Flatten<core::slice::Iter<'a, Option<PhysFrame>>>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Map a user page at `va` to physical address `pa`.
///
/// Returns `(table_frames, installed)`:
/// - `table_frames`: any intermediate page table frames allocated during the walk.
/// - `installed`: `true` if this call installed the PTE, `false` if the PTE was
///   already valid (another thread won the race).  When `false`, the caller's
///   data frame was NOT mapped and should be freed.
///
/// # Safety
///
/// This edits the live page tables reached through `TTBR0_EL1`. The caller must
/// uphold all four of:
///
/// 1. **The address space is the caller's own and is held.** The walk reads
///    `TTBR0_EL1`, so it always targets *the currently installed* user address
///    space — it cannot be pointed at another process. On `smp-shared` the caller
///    must be inside that process's `as_lock` (`Process::with_address_space`), or
///    a concurrent unmap can free a table frame this walk is descending through.
/// 2. **`va` is a user VA.** Nothing here range-checks it; a kernel VA would edit
///    the kernel's own translation through the user tables.
/// 3. **`pa` is a live frame the caller owns**, page-aligned, and not already
///    mapped elsewhere in a way that contradicts `user_flags_val`.
/// 4. **The return value is consumed.** Every frame in `table_frames` must be
///    tracked (`track_page_table_frame`) or freed, or it leaks; and `installed ==
///    false` means `pa` was *not* mapped, so the caller still owns it. Discarding
///    that flag is how a VA-lifetime bug becomes silent memory corruption — see
///    the `[WPF] ... cow_ref=0 lazy_self=NONE` note at `sys_mmap`'s eager install.
pub unsafe fn map_user_page(va: usize, pa: usize, user_flags_val: u64) -> (TableFrames, bool) {
    unsafe { map_user_page_inner(va, pa, user_flags_val, true) }
}

/// The body behind [`map_user_page`] and [`map_user_page_no_flush`]. The two were
/// 43 identical lines apart from one thing: whether the successful-install arm issues
/// the per-page TLB invalidation.
///
/// `flush` is a literal at both call sites and this is `#[inline]` into two one-line
/// wrappers, so the branch is constant-folded away in each.
#[inline]
unsafe fn map_user_page_inner(
    va: usize,
    pa: usize,
    user_flags_val: u64,
    flush: bool,
) -> (TableFrames, bool) { unsafe {
    let _irq_guard = IrqGuard::new();
    let mut allocated_tables = TableFrames::default();
    let ttbr0: u64 = akuma_cpu::sysreg::ttbr0_el1();
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    let l0_ptr = phys_to_virt(l0_addr) as *mut u64;
    let (l1_addr, l1_frame) = get_or_create_table_atomic(l0_ptr, l0_idx);
    if let Some(frame) = l1_frame { allocated_tables.push(frame); }
    let l1_ptr = phys_to_virt(l1_addr) as *mut u64;
    let (l2_addr, l2_frame) = get_or_create_table_atomic(l1_ptr, l1_idx);
    if let Some(frame) = l2_frame { allocated_tables.push(frame); }
    let l2_ptr = phys_to_virt(l2_addr) as *mut u64;
    let (l3_addr, l3_frame) = get_or_create_table_atomic(l2_ptr, l2_idx);
    if let Some(frame) = l3_frame { allocated_tables.push(frame); }
    let l3_ptr = phys_to_virt(l3_addr) as *mut u64;
    let pte_atomic = &*((l3_ptr.add(l3_idx)) as *const core::sync::atomic::AtomicU64);
    let existing = pte_atomic.load(core::sync::atomic::Ordering::Acquire);
    if existing & flags::VALID != 0 {
        let existing_pa = (existing & 0x0000_FFFF_FFFF_F000) as usize;
        if existing_pa != pa {
            log::debug!("[MMU] WARN: va=0x{:x} already mapped to pa=0x{:x}, wanted pa=0x{:x}",
                va, existing_pa, pa);
        }
        return (allocated_tables, false);
    }
    let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags_val;
    let cas_result = pte_atomic.compare_exchange(existing, entry,
        core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire);
    if cas_result.is_ok() {
        if flush {
            // All-ASID (`vaae1is`), for the reason spelled out on `flush_tlb_range`:
            // `vale1is` takes its ASID from operand bits [63:48], which `va >> 12`
            // leaves zero, so it never matched the non-zero ASID a user process
            // actually runs under. Benign here (this arm only runs when the PTE was
            // *invalid*, and a faulting translation may not be cached), but it must
            // not look like it targets an ASID it cannot reach.
            akuma_cpu::barrier::dsb_ishst();
            akuma_cpu::tlb::vaae1is((va >> 12) as u64);
            akuma_cpu::barrier::dsb_ish();
            akuma_cpu::barrier::isb();
        }
        (allocated_tables, true)
    } else {
        // CAS failed: another path installed a page between our check and CAS.
        // Return false so caller knows to free their unused page.
        (allocated_tables, false)
    }
}}

/// Update the permission bits of the **current** address space's PTE for `va` — mirrors
/// [`AddressSpace::update_page_flags`] but resolves the L0 from `TTBR0_EL1`, so the
/// page-fault path can call it with only a shared `&Process` (no `&mut`). A no-op if the
/// page isn't fully mapped. Serialize with the per-AS `as_lock` under shared-kernel SMP.
pub fn update_current_user_page_flags(va: usize, new_flags: u64) {
    let _irq_guard = IrqGuard::new();
    const PERM_MASK: u64 = flags::AP_RO_ALL | flags::AP_RW_ALL | flags::UXN | flags::PXN;
    let Some(pte) = current_user_l3_pte(va) else { return };
    unsafe {
        let old_entry = pte.read_volatile();
        // Unlike `remap_current_user_page`, this only *edits* permissions, so there
        // has to be an entry to edit.
        if old_entry & flags::VALID == 0 { return; }
        pte.write_volatile((old_entry & !PERM_MASK) | new_flags);
    }
    let _ = flush_tlb_page(va, TlbTarget::AllCores);
}

/// Resolve `va`'s **L3 entry slot** in the current address space, creating nothing.
///
/// Returns `None` if any intermediate level is absent, or if the VA resolves through
/// an L1/L2 block rather than a table — in which case there is no L3 slot to point at,
/// and writing one would mean treating a block's output address as a table base.
/// Both callers below edit page tables from the fault path with only a shared
/// `&Process`, which is why they resolve the L0 from `TTBR0_EL1` rather than taking
/// `&mut UserAddressSpace`; serialize with the per-AS `as_lock` under shared-kernel SMP.
///
/// The L3 entry itself is returned unvalidated: `remap_current_user_page` deliberately
/// overwrites an invalid one, `update_current_user_page_flags` deliberately does not.
fn current_user_l3_pte(va: usize) -> Option<*mut u64> {
    const TABLE_PA: u64 = 0x0000_FFFF_FFFF_F000;
    let ttbr0: u64 = akuma_cpu::sysreg::ttbr0_el1();
    // SAFETY: the L0 is the one this core is *currently* translating through, so it
    // is live for as long as this core stays on it — which the caller's `IrqGuard`
    // and the per-AS `as_lock` are what guarantee.
    unsafe { l3_slot_in(phys_to_virt((ttbr0 & TABLE_PA) as usize) as *mut u64, va) }
}

/// Walk `l0_ptr`'s L0→L2 and return a pointer to the L3 slot for `va`, creating
/// nothing, or `None` when any intermediate level is unmapped or is a block
/// descriptor rather than a table.
///
/// The one write-side walk in this file: [`AddressSpace::l3_slot`] supplies an address
/// space's own L0, [`current_user_l3_pte`] supplies the live `TTBR0_EL1`, and between
/// them they serve the seven `&mut self` copies and the two current-TTBR0 editors that
/// each used to open with these same four index extractions and three descents
/// (`docs/archive/TRIM_FAT_PTE_NEWTYPE.md` §2). Read-side range work goes through
/// [`for_each_mapped_user_pte`] and single-VA reads through [`resolve_user_leaf`]; this
/// is the single-VA *write* case, which is why it hands back the slot rather than the
/// descriptor.
///
/// **The `TABLE` test at L1/L2 is load-bearing**, and folding it in here is the one
/// behavioural difference the consolidation introduces. Five of the seven `&mut self`
/// copies tested only `VALID`, so a VA landing on a 1 GB or 2 MB **block** — which
/// every address space has, since [`AddressSpace::add_kernel_mappings`] identity-maps
/// kernel RAM as EL1-only 2 MB blocks — took the block's *output address* for a table
/// base and then read, or wrote, into the middle of that RAM. This is the same latent
/// bug the 2026-08-14 read-side merge found in [`update_current_user_page_flags`] and
/// closed by way of [`resolve_user_leaf`]; [`current_user_l3_pte`] has tested it since.
///
/// Neither `VALID` nor `TABLE` is tested at L0: with a 4 KiB granule and a 48-bit VA,
/// AArch64 has no L0 block descriptor, so a valid L0 entry is necessarily a table.
/// [`resolve_user_leaf`] and the pre-merge copies all made the same assumption.
///
/// # Safety
/// `l0_ptr` must be a live L0 table whose reachable tables are not freed for the
/// duration of the walk, and of any use made of the returned pointer.
#[inline]
unsafe fn l3_slot_in(l0_ptr: *mut u64, va: usize) -> Option<*mut u64> { unsafe {
    const TABLE_PA: u64 = 0x0000_FFFF_FFFF_F000;
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    let l0_entry = l0_ptr.add(l0_idx).read_volatile();
    if l0_entry & flags::VALID == 0 { return None; }
    let l1_ptr = phys_to_virt((l0_entry & TABLE_PA) as usize) as *mut u64;
    let l1_entry = l1_ptr.add(l1_idx).read_volatile();
    if l1_entry & flags::VALID == 0 || l1_entry & flags::TABLE == 0 { return None; }
    let l2_ptr = phys_to_virt((l1_entry & TABLE_PA) as usize) as *mut u64;
    let l2_entry = l2_ptr.add(l2_idx).read_volatile();
    if l2_entry & flags::VALID == 0 || l2_entry & flags::TABLE == 0 { return None; }
    let l3_ptr = phys_to_virt((l2_entry & TABLE_PA) as usize) as *mut u64;
    Some(l3_ptr.add(l3_idx))
}}

/// Overwrite the current address space's PTE for an ALREADY-MAPPED `va` with a new
/// `pa`/`flags` — the copy-on-write remap. Unlike [`map_user_page`] (which refuses to
/// replace a valid entry), this rewrites it. All intermediate tables must already exist
/// (true for a CoW fault: the page is mapped read-only). Returns `true` if rewritten.
/// Resolves L0 from `TTBR0_EL1`, so the fault path can use it with a shared `&Process`;
/// serialize with the per-AS `as_lock` under shared-kernel SMP.
pub fn remap_current_user_page(va: usize, pa: usize, user_flags_val: u64) -> bool {
    let _irq_guard = IrqGuard::new();
    let Some(pte) = current_user_l3_pte(va) else { return false };
    unsafe {
        let entry = (pa as u64) | flags::VALID | flags::TABLE | flags::AF | flags::NG | attr_index(MAIR_NORMAL_WB) | flags::SH_INNER | user_flags_val;
        pte.write_volatile(entry);
    }
    let _ = flush_tlb_page(va, TlbTarget::AllCores);
    true
}

/// Same as `map_user_page` but **skips the per-page TLB invalidation**.
///
/// Use this when mapping multiple pages in a batch.  After all pages are
/// mapped, call `flush_tlb_range` (or `flush_tlb_asid`) once to flush the
/// entire range with a single DSB+ISB sequence instead of N full barriers.
///
/// The caller is responsible for issuing the TLB flush before the new
/// mappings can be safely used by userspace.
///
/// # Safety
///
/// Every requirement of [`map_user_page`], plus: the caller must issue the range
/// flush before userspace can reach the new mappings.
pub unsafe fn map_user_page_no_flush(va: usize, pa: usize, user_flags_val: u64) -> (TableFrames, bool) {
    // No TLB flush — caller must call flush_tlb_range after mapping all pages.
    unsafe { map_user_page_inner(va, pa, user_flags_val, false) }
}

/// Flush TLB entries for a contiguous range of virtual addresses.
///
/// Use after a batch of `map_user_page_no_flush` / `update_page_flags_no_flush`
/// calls to avoid N×(dsb+isb) overhead.
///
/// **All-ASID, and that is required, not conservative.** This used to issue
/// `tlbi vale1is, va>>12`, which encodes the target ASID in bits [63:48] of the
/// operand — and `va >> 12` of any user VA leaves those bits **zero**. Every
/// user process runs under a non-zero ASID (`UserAddressSpace::new` allocates
/// one), so the invalidation matched nothing and the whole call was a barrier
/// pair with no effect. `sys_mprotect` publishes its permission edits through
/// exactly this function (`syscall/mem.rs`), so a downgrade — musl's
/// `mprotect(guard_page, PROT_NONE)` after a thread-stack `mmap`, or a dynamic
/// loader's RELRO `mprotect(PROT_READ)` — left the old writable translation
/// live in the TLB.
///
/// Widening to all-ASID rather than "pass the right ASID" is deliberate: a
/// single L0 table can be live under **several** ASIDs at once, because
/// `UserAddressSpace::new_shared` allocates a fresh ASID while reusing the
/// parent's `l0_frame` (CLONE_VM threads and vfork-fastpath children), and
/// `activate()` installs that view's own ASID. One PTE edit therefore has to
/// invalidate every ASID aliasing those tables, which is what `vaae1is`
/// ("VA, All-ASID, EL1") does. Same instruction as `flush_tlb_page`.
#[inline]
pub fn flush_tlb_range(start_va: usize, pages: usize, target: TlbTarget) -> TlbFlush {
    flush_tlb_range_all_asid(start_va, pages, target)
}

/// Flush TLB entries for a contiguous VA range across **all** ASIDs, with a
/// single barrier pair for the whole range.
///
/// Issues `tlbi vaae1` (VA, All-ASID, EL1) per page — exactly the instruction
/// `flush_tlb_page` uses, so unmap semantics are unchanged — but with one
/// `dsb ish` + `isb` after the batch instead of a `dsb`/`isb` per page.  Use
/// after a batch of `unmap_page_no_flush` / `unmap_and_free_page_no_flush`
/// calls to avoid O(pages) barrier sequences during a large `munmap`/teardown.
/// See docs/COW_OPTIMIZATIONS.md (cheap-win E).
#[inline]
pub fn flush_tlb_range_all_asid(start_va: usize, pages: usize, target: TlbTarget) -> TlbFlush {
    // Above this many pages, one full-TLB flush (a single `tlbi vmalle1`) is
    // cheaper than issuing `tlbi` per page — the same trade-off Linux makes via
    // `tlb_single_page_flush_ceiling`.  A full flush is a correct (more
    // aggressive) superset of a per-VA range flush; the cost is that unrelated
    // TLB entries refill, which is worth it for a large `munmap`.
    const FULL_FLUSH_THRESHOLD: usize = 512;
    if pages == 0 {
        return TlbFlush::done();
    }
    if pages > FULL_FLUSH_THRESHOLD {
        return flush_tlb_all(target);
    }
    akuma_cpu::barrier::dsb_ishst();
    let mut va = start_va;
    // Inner-shareable under shared SMP: a range unmapped on one core must
    // invalidate peers running the same address space in EL0 (see
    // `flush_tlb_all`). Other builds, and a `ThisCore` target, keep the
    // cheaper core-local form.
    let broadcast = cfg!(kernel_smp_shared) && target == TlbTarget::AllCores;
    for _ in 0..pages {
        if broadcast {
            akuma_cpu::tlb::vaae1is((va >> 12) as u64);
        } else {
            akuma_cpu::tlb::vaae1((va >> 12) as u64);
        }
        va += 0x1000;
    }
    TlbFlush::pending()
}

/// Atomically get or create a page table at `table_ptr[idx]`.
///
/// Uses compare_exchange to prevent the race where two concurrent paths
/// (e.g. mmap syscall preempted by a demand-paging fault handler) both
/// see the entry as invalid, both allocate a new table, and the second
/// write overwrites the first — orphaning all PTEs in the lost table.
unsafe fn get_or_create_table_atomic(table_ptr: *mut u64, idx: usize) -> (usize, Option<PhysFrame>) { unsafe {
    use core::sync::atomic::{AtomicU64, Ordering};
    let atomic = &*((table_ptr.add(idx)) as *const AtomicU64);

    loop {
        let entry = atomic.load(Ordering::Acquire);

        if entry & flags::VALID != 0 {
            if entry & flags::TABLE == 0 {
                // BLOCK descriptor — shatter into L3 page entries preserving the mapping
                if let Some(frame) = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new) {
                    shatter_block_to_pages(frame.addr, entry);
                    akuma_cpu::barrier::dsb_ishst();
                    let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
                    match atomic.compare_exchange(entry, new_entry, Ordering::AcqRel, Ordering::Acquire) {
                        Ok(_) => return (frame.addr, Some(frame)),
                        Err(_) => {
                            akuma_pmm::free_page(frame.addr, akuma_primitives::preempt::current_tid() as u32);
                            continue;
                        }
                    }
                }
                return (0, None);
            }
            return ((entry & 0x0000_FFFF_FFFF_F000) as usize, None);
        }

        if let Some(frame) = akuma_pmm::alloc_page_zeroed().map(PhysFrame::new) {
            let new_entry = (frame.addr as u64) | flags::VALID | flags::TABLE;
            match atomic.compare_exchange(entry, new_entry, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return (frame.addr, Some(frame)),
                Err(_) => {
                    akuma_pmm::free_page(frame.addr, akuma_primitives::preempt::current_tid() as u32);
                    continue;
                }
            }
        } else {
            return (0, None);
        }
    }
}}

pub fn protect_kernel_code() {
    unsafe extern "C" {
        static _text_start: u8; static _text_end: u8;
        static _rodata_start: u8; static _rodata_end: u8;
        static _data_start: u8;
        static _kernel_phys_end: u8;
    }
    let (text_start, text_end, rodata_start, rodata_end, data_start) = unsafe {
        (&_text_start as *const u8 as usize,
         &_text_end as *const u8 as usize,
         &_rodata_start as *const u8 as usize,
         &_rodata_end as *const u8 as usize,
         &_data_start as *const u8 as usize)
    };

    const BLOCK_SIZE_2MB: usize = 2 * 1024 * 1024;
    const RAM_BASE: usize = 0x40000000;
    
    let text_block_start = (text_start - RAM_BASE) / BLOCK_SIZE_2MB;
    let rodata_block_end = (rodata_end - RAM_BASE + BLOCK_SIZE_2MB - 1) / BLOCK_SIZE_2MB;
    let data_block_start = (data_start - RAM_BASE) / BLOCK_SIZE_2MB;
    let l3_block_start = text_block_start;
    let l3_block_end = if data_block_start > rodata_block_end { rodata_block_end } else { data_block_start + 1 };
    let num_l3_blocks = l3_block_end - l3_block_start;
    
    let l2_table = match akuma_pmm::alloc_page() {
        Some(pa) => pa,
        None => return,
    };
    unsafe { core::ptr::write_bytes(l2_table as *mut u8, 0, PAGE_SIZE); }

    let mut l3_tables: [usize; 16] = [0; 16];
    if num_l3_blocks > 16 { return; }
    for i in 0..num_l3_blocks {
        l3_tables[i] = match akuma_pmm::alloc_page() {
            Some(pa) => pa,
            None => return,
        };
        unsafe { core::ptr::write_bytes(l3_tables[i] as *mut u8, 0, PAGE_SIZE); }
    }
    
    let l2_ptr = l2_table as *mut u64;
    const BLOCK_RW: u64 = flags::VALID | (3 << 2) | flags::SH_INNER | flags::AF;
    const PAGE_RW: u64 = flags::VALID | flags::TABLE | (3 << 2) | flags::SH_INNER | flags::AF;
    const PAGE_RO: u64 = flags::VALID | flags::TABLE | (3 << 2) | flags::SH_INNER | flags::AF | flags::AP_RO_EL1;
    
    for i in 0..512 {
        let block_addr = RAM_BASE + i * BLOCK_SIZE_2MB;
        if i >= l3_block_start && i < l3_block_end {
            let l3_ptr = l3_tables[i - l3_block_start] as *mut u64;
            for j in 0..512 {
                let page_addr = block_addr + j * PAGE_SIZE;
                let is_ro = (page_addr >= text_start && page_addr < text_end) || (page_addr >= rodata_start && page_addr < rodata_end);
                unsafe { l3_ptr.add(j).write_volatile((page_addr as u64) | if is_ro { PAGE_RO } else { PAGE_RW }); }
            }
            unsafe { l2_ptr.add(i).write_volatile((l3_tables[i - l3_block_start] as u64) | flags::VALID | flags::TABLE); }
        } else {
            unsafe { l2_ptr.add(i).write_volatile((block_addr as u64) | BLOCK_RW); }
        }
    }
    
    let l0_table = get_boot_ttbr0() as *mut u64;
    unsafe {
        let l1_table = ((*l0_table) & 0x0000_FFFF_FFFF_F000) as *mut u64;
        akuma_cpu::barrier::dsb_ishst();
        l1_table.add(1).write_volatile((l2_table as u64) | flags::VALID | flags::TABLE);
    }
    boot_table_flush_sync();
}

#[cfg(all(target_os = "none", target_arch = "aarch64"))]
pub fn get_current_ttbr0() -> usize {
    akuma_cpu::sysreg::ttbr0_el1() as usize
}

/// x86_64: the active PML4 from `CR3`. The name is the AArch64 one because the
/// callers are shared; what it returns is "the root of the current user address
/// space", which is what they actually want.
#[cfg(all(target_os = "none", target_arch = "x86_64"))]
pub fn get_current_ttbr0() -> usize { x86_read_cr3() as usize }

#[cfg(not(all(target_os = "none", any(target_arch = "aarch64", target_arch = "x86_64"))))]
pub fn get_current_ttbr0() -> usize { 0 }

/// Is every page of `[va_start, va_start+len)` mapped **as user memory** in the
/// current address space?
///
/// This is the page-table half of user-pointer validation
/// ([`user_access::validate_user_range`]), and it tests EL0 accessibility rather
/// than mere presence — which used to be a real hole.
/// [`AddressSpace::add_kernel_mappings`] identity-maps kernel RAM as **EL1-only
/// 2 MB blocks in every user address space**, so a kernel VA is present in TTBR0 and
/// passed a presence test; the EL1-only permission then failed to stop anything,
/// because the copy loop runs at EL1. All that kept it unreachable was the user VA
/// allocator avoiding `[KERNEL_VA_START, kernel_va_end())` — a layout convention,
/// not a check (docs/archive/USER_COPY_FOLD.md §7).
///
/// Testing AP rather than excluding a VA range is what makes this correct in the
/// presence of the case that killed the range-exclusion attempt: Bun's JSC `mmap`s
/// at `0x5000_0000`, which overlaps kernel RAM's identity window, and every such
/// pointer is legitimate. Those are genuine user pages with `AP_RW_ALL`, so they
/// pass; the kernel's own EL1-only blocks at the same addresses do not.
///
/// It also, correctly, now rejects a `PROT_NONE` page (`user_flags::NONE` is
/// `AP_RO_EL1`) as a syscall buffer — Linux returns `EFAULT` there too.
pub fn is_current_user_range_mapped(va_start: usize, len: usize) -> bool {
    // The x86 arm returns above the AArch64 body rather than wrapping it, so
    // that body stays textually identical and the AArch64 kernel's `.text` is
    // provably unmoved. Wrapping it in a `#[cfg]` block did move it.
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    return x86_user_range_ok(va_start, len, false);
    #[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
    {
    let ttbr0 = get_current_ttbr0();
    if ttbr0 == 0 { return false; }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let start_page = va_start & !(PAGE_SIZE - 1);
    let end_page = (va_start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let num_pages = (end_page - start_page) / PAGE_SIZE;
    let l0_ptr = phys_to_virt(l0_addr) as *const u64;
    for i in 0..num_pages {
        if !is_page_user_accessible_ptr(l0_ptr, start_page + i * PAGE_SIZE) { return false; }
    }
    true
    }
}

/// Read a `Copy` POD value of type `T` from `va` in the **current** address
/// space, at EL1. `None` if `[va, va + size_of::<T>())` is not currently mapped
/// EL0-accessible.
///
/// This is the `read_current_pid` fallback's read of `ProcessInfo` at its fixed
/// user VA. It deliberately does **not** route through `akuma-user-access`:
/// `validate_user_range` there resolves the faulting address space's owner
/// through the process layer, and that path calls back into this exact
/// function — an infinite recursion. The AP-gated presence check above is the
/// whole precondition the read needs.
#[must_use]
pub fn read_current_user_val<T: Copy>(va: usize) -> Option<T> {
    if !is_current_user_range_mapped(va, core::mem::size_of::<T>()) {
        return None;
    }
    // SAFETY: `[va, va + size_of::<T>())` is mapped and EL0-accessible in the
    // live TTBR0 (checked directly above), so this EL1 read cannot fault.
    // `read_unaligned` drops the alignment obligation; `T: Copy` is POD.
    Some(unsafe { core::ptr::read_unaligned(va as *const T) })
}

/// Is anything mapped at `va` in the current address space? **Presence** — see
/// [`is_page_mapped_ptr`] for why this one is not the AP-gated question.
pub fn is_current_user_page_mapped(va: usize) -> bool {
    let ttbr0 = get_current_ttbr0();
    if ttbr0 == 0 { return false; }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let l0_ptr = phys_to_virt(l0_addr) as *const u64;
    is_page_mapped_ptr(l0_ptr, va & !(PAGE_SIZE - 1))
}

/// The leaf descriptor a user VA resolves to, and the granule it was found at.
///
/// Three functions used to walk L0→L3 by hand to answer three different questions
/// about that leaf — "what PA?" ([`translate_user_va`]), "what descriptor?"
/// ([`user_pte_raw`]), "is anything there?" ([`is_page_mapped_ptr`]) — and each
/// re-derived the same four indices, the same four `read_volatile`s and the same
/// four validity tests, differing only in what they did on a block descriptor.
/// [`resolve_user_leaf`] does the walk; the three keep their signatures and answer
/// from this.
#[derive(Clone, Copy)]
pub struct UserLeaf {
    /// The raw leaf descriptor.
    pub entry: u64,
    /// Selects the output address within [`Self::entry`] at this granule.
    pa_mask: u64,
    /// Selects the byte offset within the granule from the VA. `0xFFF` for an L3
    /// page, wider for an L1 (1 GB) or L2 (2 MB) block.
    offset_mask: usize,
}

impl UserLeaf {
    /// `true` if this is a 4 KiB L3 page rather than an L1/L2 block descriptor.
    ///
    /// User VAs are always L3: [`map_user_page`] only ever writes L3 entries, and
    /// [`get_or_create_table_atomic`] shatters any block it walks through. A block
    /// leaf under a user VA therefore means a *kernel* mapping —
    /// [`AddressSpace::add_kernel_mappings`] identity-maps kernel RAM as EL1-only
    /// 2 MB blocks in every address space.
    #[must_use]
    pub fn is_page(self) -> bool {
        self.offset_mask == 0xFFF
    }

    /// The physical address `va` translates to through this leaf.
    #[must_use]
    pub fn phys(self, va: usize) -> usize {
        ((self.entry & self.pa_mask) as usize) | (va & self.offset_mask)
    }

    /// Whether **EL0** may access this mapping at all.
    ///
    /// AP is a two-bit field and bit 6 is the "EL0 gets the same access as EL1" bit:
    /// `AP_RW_ALL` (0b01) and `AP_RO_ALL` (0b11) have it set, `AP_RW_EL1` (0b00) and
    /// `AP_RO_EL1` (0b10) do not. So this is a test for *reachability from
    /// userspace*, not for writability — a read-only user page passes, which it must,
    /// because an EL1 write to one is how a CoW break gets triggered.
    #[must_use]
    pub fn user_accessible(self) -> bool {
        self.entry & flags::AP_RW_ALL != 0
    }

    /// Whether **EL0** (and therefore an EL1 store) may *write* this mapping —
    /// AP[7:6] == `0b01` (`AP_RW_ALL`) exactly. A read-only user page fails this
    /// where it passes [`user_accessible`](Self::user_accessible).
    #[must_use]
    pub fn user_writable(self) -> bool {
        self.entry & (flags::AP_RO_ALL | flags::AP_RW_ALL) == flags::AP_RW_ALL
    }
}

/// Walk `l0_ptr` for `va` and return whatever leaf it lands on — an L1 block, an L2
/// block, or an L3 page — or `None` if any level is invalid.
fn resolve_user_leaf(l0_ptr: *const u64, va: usize) -> Option<UserLeaf> {
    const TABLE_PA: u64 = 0x0000_FFFF_FFFF_F000;
    let l0_idx = (va >> 39) & 0x1FF;
    let l1_idx = (va >> 30) & 0x1FF;
    let l2_idx = (va >> 21) & 0x1FF;
    let l3_idx = (va >> 12) & 0x1FF;
    unsafe {
        let l0_entry = l0_ptr.add(l0_idx).read_volatile();
        if l0_entry & flags::VALID == 0 { return None; }
        let l1_ptr = phys_to_virt((l0_entry & TABLE_PA) as usize) as *const u64;
        let l1_entry = l1_ptr.add(l1_idx).read_volatile();
        if l1_entry & flags::VALID == 0 { return None; }
        if l1_entry & flags::TABLE == 0 {
            // 1 GB block.
            return Some(UserLeaf { entry: l1_entry, pa_mask: 0x0000_FFFF_C000_0000, offset_mask: 0x3FFF_FFFF });
        }
        let l2_ptr = phys_to_virt((l1_entry & TABLE_PA) as usize) as *const u64;
        let l2_entry = l2_ptr.add(l2_idx).read_volatile();
        if l2_entry & flags::VALID == 0 { return None; }
        if l2_entry & flags::TABLE == 0 {
            // 2 MB block.
            return Some(UserLeaf { entry: l2_entry, pa_mask: 0x0000_FFFF_FFE0_0000, offset_mask: 0x1F_FFFF });
        }
        let l3_ptr = phys_to_virt((l2_entry & TABLE_PA) as usize) as *const u64;
        let l3_entry = l3_ptr.add(l3_idx).read_volatile();
        if l3_entry & flags::VALID == 0 { return None; }
        Some(UserLeaf { entry: l3_entry, pa_mask: TABLE_PA, offset_mask: 0xFFF })
    }
}

/// Translate a user VA to its physical address using the given L0 page table.
/// Returns None if the page is not mapped.
/// [`translate_user_va`] for the **current** address space: resolves the L0 from
/// `TTBR0_EL1` itself, so a caller with no `&Process` can ask "what physical frame
/// is behind this user VA right now?".
///
/// Added for the frame-lifecycle diagnostic in
/// `docs/archive/BUSYBOX_HASH_MISCOMPUTE.md`: comparing the answer before and
/// after a `copy_to_user` says whether the frame under the destination was
/// swapped out from under the copy.
#[must_use]
pub fn translate_current_user_va(va: usize) -> Option<usize> {
    let ttbr0 = get_current_ttbr0();
    if ttbr0 == 0 {
        return None;
    }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    translate_user_va(phys_to_virt(l0_addr) as *const u64, va)
}

pub fn translate_user_va(l0_ptr: *const u64, va: usize) -> Option<usize> {
    resolve_user_leaf(l0_ptr, va).map(|leaf| leaf.phys(va))
}

/// The raw L3 descriptor for `va`, or `None` if any level is invalid **or** the VA
/// resolves through a block descriptor rather than an L3 page.
///
/// [`translate_user_va`] masks the entry down to its physical address, which throws
/// away the field an anomaly report most needs: the AP bits. "Write faulted on a
/// mapped page" has several distinct causes that a PA cannot tell apart —
/// `AP_RO_ALL` (read-only to everyone), `AP_RO_EL1`/`user_flags::NONE` (a
/// `PROT_NONE` guard page), and `AP_RW_EL1` (kernel-only, EL0 no access) all fault
/// on a user write and mean very different things. Returning the descriptor lets
/// the caller name which one it is. See docs/archive/CARGO_HEAP_NULL_RC.md.
pub fn user_pte_raw(l0_ptr: *const u64, va: usize) -> Option<u64> {
    resolve_user_leaf(l0_ptr, va)
        .filter(|leaf| leaf.is_page())
        .map(|leaf| leaf.entry)
}

/// Human-readable name for a PTE's access-permission field.
pub fn ap_name(pte: u64) -> &'static str {
    match pte & flags::AP_MASK {
        x if x == flags::AP_RW_EL1 => "AP_RW_EL1(kernel-only, EL0 no access)",
        x if x == flags::AP_RW_ALL => "AP_RW_ALL(writable)",
        x if x == flags::AP_RO_EL1 => "AP_RO_EL1(PROT_NONE guard)",
        _ => "AP_RO_ALL(read-only)",
    }
}

/// Visit every **mapped L3 entry** in `[va_start, va_start + pages*PAGE_SIZE)`,
/// skipping unmapped L0 (512 GB), L1 (1 GB) and L2 (2 MB) regions wholesale rather
/// than walking them page by page. Much faster than `translate_user_va` per page for
/// sparse regions (e.g. Go heap arenas).
///
/// `f` receives `(page_va, entry, pte)` — the raw descriptor and a pointer to it, so
/// a caller can rewrite it in place. Invalid entries are filtered out before `f` runs,
/// and block descriptors are skipped entirely (user VAs never resolve through one —
/// see [`UserLeaf::is_page`]).
///
/// This skeleton existed three times verbatim — the two `collect_mapped_pages_*`
/// functions and [`demote_range_to_ro`] — differing only in the body of the innermost
/// loop. Since one of the three *writes* PTEs in the fork/CoW path, three copies of
/// the index arithmetic was the highest-consequence duplication left in this file
/// (`docs/archive/TRIM_FAT_EMBARASSING_DUPLICATIONS.md` §8 item 8, §5.6).
///
/// # Safety
/// `l0_ptr` must be a valid L0 table for a live address space, and the tables it
/// reaches must not be freed for the duration of the walk.
unsafe fn for_each_mapped_user_pte(
    l0_ptr: *const u64,
    va_start: usize,
    pages: usize,
    mut f: impl FnMut(usize, u64, *mut u64),
) { unsafe {
    const TABLE_PA: u64 = 0x0000_FFFF_FFFF_F000;
    if pages == 0 { return; }
    let Some(va_end) = va_start.checked_add(pages.saturating_mul(PAGE_SIZE)) else { return };

    // Walk at L2 granularity (2MB = 512 pages).
    let mut va = va_start;
    while va < va_end {
        let l0_idx = (va >> 39) & 0x1FF;
        let l1_idx = (va >> 30) & 0x1FF;
        let l2_idx = (va >> 21) & 0x1FF;

        let l0_entry = l0_ptr.add(l0_idx).read_volatile();
        if l0_entry & flags::VALID == 0 {
            // Skip entire L0 region (512GB) — clamp to va_end
            va = ((va | 0x7F_FFFF_FFFF) + 1).min(va_end);
            continue;
        }
        let l1_ptr = phys_to_virt((l0_entry & TABLE_PA) as usize) as *const u64;
        let l1_entry = l1_ptr.add(l1_idx).read_volatile();
        // Skip the entire L1 region (1GB) when it is unmapped, or when it is a 1GB
        // block mapping — a block is never a user page.
        if l1_entry & flags::VALID == 0 || l1_entry & flags::TABLE == 0 {
            va = ((va | 0x3FFF_FFFF) + 1).min(va_end);
            continue;
        }
        let l2_ptr = phys_to_virt((l1_entry & TABLE_PA) as usize) as *const u64;
        let l2_entry = l2_ptr.add(l2_idx).read_volatile();
        // Same, one level down: unmapped or a 2MB block, skip the whole 2MB.
        if l2_entry & flags::VALID == 0 || l2_entry & flags::TABLE == 0 {
            va = ((va | 0x1F_FFFF) + 1).min(va_end);
            continue;
        }
        // Valid L3 table — scan pages within this 2MB range
        let l3_ptr = phys_to_virt((l2_entry & TABLE_PA) as usize) as *mut u64;
        let l3_start = (va >> 12) & 0x1FF;
        let l2_range_end = ((va | 0x1F_FFFF) + 1).min(va_end);
        let l3_end_idx = if l2_range_end == va_end {
            ((va_end.wrapping_sub(1) >> 12) & 0x1FF) + 1
        } else {
            512
        };
        for l3_idx in l3_start..l3_end_idx {
            let pte = l3_ptr.add(l3_idx);
            let entry = pte.read_volatile();
            if entry & flags::VALID != 0 {
                f((va & !0x1F_FFFF) | (l3_idx << 12), entry, pte);
            }
        }
        va = l2_range_end;
    }
}}

/// Collect (va, pa) pairs for mapped pages in [va_start, va_start + pages*PAGE_SIZE),
/// skipping empty L2 entries (2MB / 512 pages at a time).  Much faster than calling
/// `translate_user_va` per page for sparse regions (e.g. Go heap arenas).
pub fn collect_mapped_pages_sparse(
    l0_ptr: *const u64,
    va_start: usize,
    pages: usize,
) -> alloc::vec::Vec<(usize, usize)> {
    let mut result = alloc::vec::Vec::new();
    // SAFETY: same contract this function has always had on `l0_ptr`; the callback
    // only reads.
    unsafe {
        for_each_mapped_user_pte(l0_ptr, va_start, pages, |page_va, entry, _pte| {
            result.push((page_va, ((entry & 0x0000_FFFF_FFFF_F000) as usize) | (page_va & 0xFFF)));
        });
    }
    result
}

/// Collect (va, pa, pte_flags) triples for mapped pages, including the raw PTE
/// attribute bits (AP, UXN, PXN, etc.).  Used by CoW fork to reproduce the
/// original permission set in the child's page table.
pub fn collect_mapped_pages_with_flags(
    l0_ptr: *const u64,
    va_start: usize,
    pages: usize,
) -> alloc::vec::Vec<(usize, usize, u64)> {
    let mut result = alloc::vec::Vec::new();
    collect_mapped_pages_with_flags_into(l0_ptr, va_start, pages, &mut result);
    result
}

/// [`collect_mapped_pages_with_flags`] into a caller-owned buffer, so the walk itself
/// performs **no heap allocation** when `out` was pre-reserved to hold `pages` entries.
///
/// `out` is cleared first (its capacity is retained). This variant exists for the
/// `no-bkl-process` fork carve-out, whose per-chunk PTE snapshot runs inside an
/// IRQ-masked `as_lock` hold: a `Vec` growing under that hold would allocate — and the
/// hold's whole purpose is to be short, bounded, and allocation-free. Reserve once
/// outside the hold, reuse the buffer per chunk.
pub fn collect_mapped_pages_with_flags_into(
    l0_ptr: *const u64,
    va_start: usize,
    pages: usize,
    out: &mut alloc::vec::Vec<(usize, usize, u64)>,
) {
    out.clear();
    // SAFETY: same contract this function has always had on `l0_ptr`; the callback
    // only reads.
    unsafe {
        for_each_mapped_user_pte(l0_ptr, va_start, pages, |page_va, entry, _pte| {
            // Extract only user-relevant permission bits (AP + UXN + PXN).
            // map_page() adds VALID/TABLE/AF/NG/attr_index/SH_INNER itself.
            let pte_flags = entry & (flags::AP_RO_ALL | flags::UXN | flags::PXN);
            out.push((page_va, (entry & 0x0000_FFFF_FFFF_F000) as usize, pte_flags));
        });
    }
}

/// Demote all RW L3 PTEs in [va_start, va_start + pages*PAGE_SIZE) to RO.
///
/// Walks the page table via raw L0 pointer (no `&mut UserAddressSpace` needed).
/// Returns the number of PTEs actually demoted.  Caller must flush the TLB
/// after calling this (e.g. `flush_tlb_asid`).
///
/// # SMP safety
/// Under `kernel_smp_shared`, this function ensures all PTE writes are visible
/// before returning, so a subsequent `flush_tlb_all()` guarantees that no core
/// can use a stale RW TLB entry after the demotion. This is critical for CoW
/// fork correctness: the demote+flush window must be atomic with respect to
/// genuinely-parallel EL0 on peer cores.
pub unsafe fn demote_range_to_ro(l0_ptr: *mut u64, va_start: usize, pages: usize) -> usize { unsafe {
    const AP_MASK: u64 = flags::AP_RO_ALL | flags::AP_RW_ALL; // bits [7:6]
    let mut demoted = 0usize;
    for_each_mapped_user_pte(l0_ptr, va_start, pages, |_page_va, entry, pte| {
        if entry & AP_MASK == flags::AP_RW_ALL {
            // Demote RW → RO: clear AP_RW_ALL, set AP_RO_ALL
            pte.write_volatile((entry & !AP_MASK) | flags::AP_RO_ALL);
            demoted += 1;
        }
    });
    // Ensure all PTE writes are visible before returning. Under kernel_smp_shared,
    // the caller will issue flush_tlb_all() which includes DSB, but this DSB here
    // guarantees ordering even if the call pattern changes.
    #[cfg(kernel_smp_shared)]
    akuma_cpu::barrier::dsb_ish();
    demoted
}}

/// Is *anything* mapped at `va` — a page or a block, at any permission?
///
/// **Presence, deliberately.** Its callers are demand-paging and teardown paths
/// asking "has this VA already been filled in", and a `PROT_NONE` guard page must
/// read as present there or the next fault would re-map it read-write. The question
/// a user *pointer* needs is the stricter [`is_page_user_accessible_ptr`].
fn is_page_mapped_ptr(l0_ptr: *const u64, va: usize) -> bool {
    resolve_user_leaf(l0_ptr, va).is_some()
}

/// Is `va` mapped **and reachable from EL0**?
///
/// The predicate a user pointer has to satisfy, and the one
/// [`is_page_mapped_ptr`] is not. See [`UserLeaf::user_accessible`] for the AP
/// encoding and [`is_current_user_range_mapped`] for the hole this closes.
fn is_page_user_accessible_ptr(l0_ptr: *const u64, va: usize) -> bool {
    resolve_user_leaf(l0_ptr, va).is_some_and(UserLeaf::user_accessible)
}

fn is_page_user_writable_ptr(l0_ptr: *const u64, va: usize) -> bool {
    resolve_user_leaf(l0_ptr, va).is_some_and(UserLeaf::user_writable)
}

/// Every page in `[va_start, va_start + len)` is mapped **writable from EL0** in
/// the current address space. The write-side counterpart of
/// [`is_current_user_range_mapped`] — a `PROT_READ` page fails this.
#[must_use]
pub fn is_current_user_range_writable(va_start: usize, len: usize) -> bool {
    #[cfg(all(target_os = "none", target_arch = "x86_64"))]
    return x86_user_range_ok(va_start, len, true);
    #[cfg(not(all(target_os = "none", target_arch = "x86_64")))]
    {
    let ttbr0 = get_current_ttbr0();
    if ttbr0 == 0 { return false; }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let start_page = va_start & !(PAGE_SIZE - 1);
    let end_page = (va_start + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let num_pages = (end_page - start_page) / PAGE_SIZE;
    let l0_ptr = phys_to_virt(l0_addr) as *const u64;
    (0..num_pages).all(|i| is_page_user_writable_ptr(l0_ptr, start_page + i * PAGE_SIZE))
    }
}

/// Write a `Copy` POD value to `va` in the **current** address space with a
/// single aligned EL1 store. `false` (no write performed) if
/// `[va, va + size_of::<T>())` is not mapped writable from EL0.
///
/// The write counterpart of [`read_current_user_val`], and deliberately **not**
/// `akuma_user_access::copy_to_user`: that path is a byte-by-byte `strb` loop
/// through the fault trampoline, which has been observed to return a spurious
/// `EFAULT` mid-page for the musl / Go `set_tid` stores in
/// `process::clone_thread`. Those callers have already validated the clone
/// flags that guarantee a writable page and need one `str`, not a loop.
#[must_use]
pub fn write_current_user_val<T: Copy>(va: usize, val: &T) -> bool {
    if !is_current_user_range_writable(va, core::mem::size_of::<T>()) {
        return false;
    }
    // SAFETY: `[va, va + size_of::<T>())` is mapped writable and EL0-accessible
    // in the live TTBR0 (checked directly above), so this EL1 store cannot
    // fault. `read_unaligned`/`write_unaligned` drop the alignment obligation;
    // `T: Copy` is POD.
    unsafe { core::ptr::write_unaligned(va as *mut T, *val) }
    true
}

#[cfg(test)]
mod icache_tests {
    use super::*;

    // On the host target the cache-maintenance asm is compiled out, so this only
    // guards that `sync_icache_range` is callable and that the `len == 0` and
    // sub-cache-line cases don't panic / over-run. The real coherency proof is the
    // on-target boot self-test `test_icache_sync_rewrites_code` (src/process_tests.rs),
    // which writes + rewrites executable code and runs it (docs/AKUMA_SELF_HOSTING.md §7j).
    #[test]
    fn sync_icache_range_handles_edge_lengths() {
        sync_icache_range(0x4000_0000, 0); // no-op
        sync_icache_range(0x4000_0000, 1); // sub-line: widened to the containing line
        sync_icache_range(0x4000_0007, 64); // unaligned start
        sync_icache_range(0x4000_0000, PAGE_SIZE); // whole page
    }
}


/// The x86 permission encoding, pinned against the walker that already ships.
///
/// `amd64/src/paging.rs` has its own `PteProt`/`encode` pair driving the amd64
/// kernel's page tables, and its boot self-test asserts each of these six
/// values. This crate's copy is the one the *shared* crates reach through
/// `UserAddressSpace`, so the two must agree exactly or a page mapped through
/// `akuma-exec` and a page mapped through `amd64/src/mm.rs` get different
/// permissions from the same region.
///
/// Spelled as hex literals for the same reason `prot_roundtrips_to_todays_bits`
/// is: a test written against `x86_encode`'s own constants would pass even if
/// they drifted. These were read off `amd64/src/paging.rs`'s
/// `region_prot_roundtrip_check`.
///
/// Compiled on any host via the `cfg(test)` half of the gate on the encoding —
/// the bits are portable data even where the walker that writes them is not,
/// which is exactly how the AArch64 `user_flags` table is already treated.
#[cfg(test)]
mod x86_encoding_tests {
    use super::{MemAttr, PteProt, Prot, user_flags, x86_encode, X86_COW};

    #[test]
    fn x86_prot_matches_amd64_encoding() {
        let wb = |p| x86_encode(PteProt::from_region(p), MemAttr::WriteBack);
        // NONE — present, kernel-only (NX set, U/S clear), so ring 3 faults.
        assert_eq!(wb(Prot::NONE), 0x8000_0000_0000_0001);
        // RO and RX collapse: one execute bit, no PXN to distinguish them.
        assert_eq!(wb(Prot::RO), 0x0000_0000_0000_0005);
        assert_eq!(wb(Prot::RX), 0x0000_0000_0000_0005);
        // RW and RW_NO_EXEC are both non-executable here.
        assert_eq!(wb(Prot::RW), 0x8000_0000_0000_0007);
        assert_eq!(wb(Prot::RW_NO_EXEC), 0x8000_0000_0000_0007);
        assert_eq!(wb(Prot::RO_NO_EXEC), 0x8000_0000_0000_0005);
    }

    /// `from_region` matches on an opaque tag and so cannot be exhaustive.
    /// Prove the `_ =>` fallback is unreachable over `Prot::ALL`, and that the
    /// two collapses above are the *only* ones.
    #[test]
    fn x86_from_region_covers_every_variant() {
        let mut distinct = alloc::vec::Vec::new();
        for p in Prot::ALL {
            let e = PteProt::from_region(p);
            if !distinct.contains(&e) {
                distinct.push(e);
            }
        }
        // Six variants, four distinct encodings: RO/RX collapse and
        // RW/RW_NO_EXEC collapse, each for its own documented reason.
        assert_eq!(distinct.len(), 4, "the set of x86 encodings changed");
        // NONE must not have collapsed into a user-reachable one.
        assert!(!PteProt::from_region(Prot::NONE).user);
    }

    /// The whole chain a shared caller actually travels: `akuma-exec` holds an
    /// AArch64 flag word, the x86 walker decodes it and encodes x86 bits. Each
    /// hop is pinned on its own above; this pins the composition, which is what
    /// `UserAddressSpace::map_page` performs.
    #[test]
    fn aarch64_flag_word_reaches_the_right_x86_bits() {
        let via_u64 = |f: u64| x86_encode(
            PteProt::from_region(user_flags::from_pte(f)),
            MemAttr::WriteBack,
        );
        assert_eq!(via_u64(user_flags::RW), 0x8000_0000_0000_0007);
        assert_eq!(via_u64(user_flags::RW_NO_EXEC), 0x8000_0000_0000_0007);
        assert_eq!(via_u64(user_flags::RX), 0x0000_0000_0000_0005);
        assert_eq!(via_u64(user_flags::RO_NO_EXEC), 0x8000_0000_0000_0005);
        assert_eq!(via_u64(user_flags::NONE), 0x8000_0000_0000_0001);
    }

    /// The CoW marker is bit 9 and is not one of the bits `x86_encode` writes —
    /// `demote_range_to_ro` ORs it onto a live entry, and
    /// `update_page_flags_inner` must not clear it. Both properties are one
    /// fact: `X86_COW` is disjoint from every encoded permission.
    #[test]
    fn cow_marker_is_disjoint_from_every_encoding() {
        assert_eq!(X86_COW, 1 << 9, "amd64/src/paging.rs::COW is bit 9");
        for p in Prot::ALL {
            let e = x86_encode(PteProt::from_region(p), MemAttr::WriteBack);
            assert_eq!(e & X86_COW, 0, "{p:?} encodes into the CoW marker bit");
        }
    }
}
