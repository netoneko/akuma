//! Physical Memory Manager (PMM) — thin shim over `akuma_pmm`.
//!
//! The bitmap allocator, frame tracker, UAF quarantine, free/CoW event ledgers
//! (Step 2), the CoW refcount table itself (Step 3), and the reclaim escalation
//! loop + its four `PmmHooks` (Step 4) all moved to `crates/akuma-pmm` —
//! `docs/archive/PMM_EXTRACT.md` §7, 2026-08-14. This file is what's left:
//!
//! - `PhysFrame`/`FrameSource` conversions at the crate boundary (the crate
//!   works in raw `usize` PAs — see its module doc for why).
//! - The `dp_counters_line` dump — it also reads `akuma_ext2`'s
//!   deferred-free counters, which sit above the pmm crate (and above
//!   `akuma-exec`, which is why it cannot live there either).
//! - Forwarders whose remaining `src/` callers are the boot self-tests
//!   (`kernel_tests`-gated).
//!
//! The demand-paging attribution counters (`DP_*`) moved into `akuma_pmm`
//! itself on 2026-09-01, once their production bumpers stopped being
//! `src/` call sites; `surviving_mapper`/`report_poison_value` moved to
//! `akuma_exec::process::reclaim` the same day. What the exception path needs
//! from here (`dp_counters_line`) it takes through `ExceptionHooks`.

pub use akuma_exec::{PhysFrame, FrameSource};

pub use akuma_pmm::{
    PmmConfig, PmmHooks, register_config, register_hooks,
    cow_ref_get,
};
// `cow_ref_dec`'s only `src/` callers are the CoW refcount self-tests
// (`tests.rs`, `process_tests.rs`) — kernel_tests-gated, so `no-tests` builds
// deny the import. exceptions.rs reads `akuma_pmm::cow_ref_dec` directly.
#[allow(unused_imports)]
pub use akuma_pmm::cow_ref_dec;
/// `cow_ref_count` has never had a production caller here. `cow_ref_inc` lost
/// its last one on 2026-09-01, when the file-page cache moved to
/// `akuma-fpcache` and began calling `akuma_pmm` directly — what is left are the
/// CoW refcount self-tests in `src/tests.rs` and `src/process_tests.rs`, which
/// `extreme-size` compiles out (`no-tests`) while still building with
/// `-D unused-imports`.
#[allow(unused_imports)]
pub use akuma_pmm::{cow_ref_count, cow_ref_inc};

// ============================================================================
// Frame tracking — thin wrappers, PhysFrame/FrameSource <-> usize/akuma_pmm::FrameSource
// ============================================================================

pub fn track_frame(frame: PhysFrame, source: FrameSource) {
    let src = match source {
        FrameSource::Kernel => akuma_pmm::FrameSource::Kernel,
        FrameSource::UserPageTable => akuma_pmm::FrameSource::UserPageTable,
        FrameSource::UserData => akuma_pmm::FrameSource::UserData,
        FrameSource::ElfLoader => akuma_pmm::FrameSource::ElfLoader,
        FrameSource::Unknown => akuma_pmm::FrameSource::Unknown,
    };
    akuma_pmm::track_frame(frame.addr, src);
}

// `tracking_stats` moved out on 2026-09-01: exceptions.rs — its only caller —
// reads `akuma_pmm::tracking_stats()` directly now.
#[allow(dead_code)]
pub fn leak_count() -> usize {
    akuma_pmm::leak_count()
}

// ============================================================================
// UAF hunt: free ledger, is_page_free, CoW event ledger — pure re-exports
// ============================================================================

// `cow_ref_dec`'s and the two ledger probes' only `src/` callers are the PMM
// self-tests (`tests.rs`, `process_tests.rs`), which `no-tests` builds compile
// out while denying dead code — exceptions.rs, the production caller, reads
// `akuma_pmm` directly since 2026-09-01. Gated like `cow_ever_touched` above.
#[cfg(kernel_tests)]
pub fn is_page_free(pa: usize) -> bool {
    akuma_pmm::is_page_free(pa)
}

#[cfg(kernel_tests)]
pub fn last_free_record(pa: usize) -> Option<(u32, u32)> {
    akuma_pmm::last_free_record(pa)
}

// Only caller is `process_tests.rs`, which is `#[cfg(kernel_tests)]`-gated
// wholesale; `alloc_page`'s own use of the underlying quarantine drain moved
// into `akuma_pmm::alloc_page` and no longer routes through this wrapper.
#[cfg(kernel_tests)]
pub fn cow_ever_touched(pa: usize) -> Option<bool> {
    akuma_pmm::cow_ever_touched(pa)
}

// `free_ledger_seq` and `print_cow_history` were deleted on 2026-09-01:
// `report_poison_value` — their last caller — moved to
// `akuma_exec::process::reclaim`, which reads `akuma_pmm` directly.

#[cfg(kernel_tests)]
pub fn cow_event_count(pa: usize) -> usize {
    akuma_pmm::cow_event_count(pa)
}

// ============================================================================
// UAF hunt: quarantine — pure re-exports
// ============================================================================

// Only caller is `process_tests.rs` (kernel_tests-gated wholesale); `alloc_page`'s
// own out-of-memory fallback now calls `akuma_pmm::alloc_page`, which reaches the
// crate's internal `quarantine_drain_all` directly, unconditionally, without
// going through this wrapper at all.
#[cfg(kernel_tests)]
pub fn quarantine_drain_all() -> usize {
    akuma_pmm::quarantine_drain_all()
}

#[doc(hidden)]
#[cfg(kernel_tests)]
pub fn discount_uaf_detections(n: usize) {
    akuma_pmm::discount_uaf_detections(n);
}

pub fn quarantine_stats() -> (usize, usize) {
    akuma_pmm::quarantine_stats()
}

pub fn premature_free_count() -> usize {
    akuma_pmm::premature_free_count()
}

pub fn double_free_count() -> usize {
    akuma_pmm::double_free_count()
}

#[doc(hidden)]
#[cfg(kernel_tests)]
pub fn discount_double_frees(n: usize) {
    akuma_pmm::discount_double_frees(n);
}

// ============================================================================
// (The bridge hook and the poison-decode diagnostic moved out on 2026-09-01.)
// ============================================================================
//
// `surviving_mapper` walked the process table and `report_poison_value` needed
// the live RAM window — both `akuma-exec` state, which is why neither could
// move into `akuma-pmm`. But neither needed `src/`: both now live in
// `akuma_exec::process::reclaim`, which registers the bridge hook from
// `akuma_exec::init`. The exception path reaches `report_poison_value` through
// its `ExceptionHooks` registration in `src/main.rs`.

// `cow_ref_inc`/`cow_ref_dec`/`cow_ref_get`/`cow_ref_count` are `akuma_pmm`'s
// now — `COW_REFCOUNTS` and its operations moved wholesale in Step 3. Re-exported
// above so `pmm::cow_ref_*` call sites (the ExecRuntime table, `process/mod.rs`,
// the fault path, boot tests) are unchanged.

// ============================================================================
// Core alloc/free API — thin wrappers, PhysFrame <-> usize
// ============================================================================

pub fn init(ram_base: usize, ram_size: usize, kernel_end: usize) {
    akuma_pmm::init(ram_base, ram_size, kernel_end);
}

// Only caller is `process_tests.rs` (kernel_tests-gated wholesale) since Step 5
// (`docs/archive/PMM_EXTRACT.md` §7) moved `akuma-exec`'s own call sites off
// `ExecRuntime::alloc_page` onto `akuma_pmm::alloc_page` directly — this thin
// `PhysFrame` wrapper has no other caller left.
#[cfg(kernel_tests)]
pub fn alloc_page() -> Option<PhysFrame> {
    akuma_pmm::alloc_page().map(PhysFrame::new)
}

/// Free a single physical page. If the frame is CoW-shared (refcount > 0),
/// only decrements the refcount instead of actually freeing — the physical
/// frame is freed when the last reference is dropped.
pub fn free_page(frame: PhysFrame) {
    akuma_pmm::free_page(frame.addr, akuma_primitives::preempt::current_tid() as u32);
}

/// As [`free_page`], but attributes the release to a specific code path so a
/// premature-free / poison report can name it. Prefer this at every site that could
/// plausibly free a frame another mapping still holds — see `akuma_pmm::FreeSite` and
/// `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6.
pub fn free_page_at(frame: PhysFrame, site: akuma_pmm::FreeSite) {
    akuma_pmm::free_page_at(
        frame.addr,
        akuma_primitives::preempt::current_tid() as u32,
        site,
    );
}

/// Contiguous multi-page allocation, as a [`PhysFrame`].
///
/// `#[cfg(kernel_tests)]` since 2026-08-31: the kernel heap was this shim's only
/// production caller, and it moved to `crates/akuma-alloc`, which talks to
/// `akuma_pmm` in raw `usize` PAs and needs no `PhysFrame` conversion. What is
/// left are the PMM boot self-tests in `src/tests.rs`, which do want the typed
/// form — so the wrapper follows them. Without the gate, `extreme-size`
/// (`no-tests` + `-D dead-code`) fails to build.
#[cfg(kernel_tests)]
pub fn alloc_pages_contiguous_zeroed(count: usize) -> Option<PhysFrame> {
    akuma_pmm::alloc_pages_contiguous_zeroed(count).map(PhysFrame::new)
}

pub fn free_pages_contiguous(frame: PhysFrame, count: usize) {
    akuma_pmm::free_pages_contiguous(frame.addr, count);
}

pub fn stats() -> (usize, usize, usize) {
    akuma_pmm::stats()
}

pub fn free_count() -> usize {
    akuma_pmm::free_count()
}

pub fn total_count() -> usize {
    akuma_pmm::total_count()
}

pub fn alloc_page_zeroed() -> Option<PhysFrame> {
    akuma_pmm::alloc_page_zeroed().map(PhysFrame::new)
}

// The reserve stays re-exported (mem.rs + process_tests). `user_readahead_budget`
// moved out with exceptions.rs (2026-09-01), which reads `akuma_pmm::` directly.
pub use akuma_pmm::USER_PAGE_RESERVE;

/// Allocate a zeroed page for a **user** demand-paging fault. Returns `None`
/// once free PMM has fallen to [`USER_PAGE_RESERVE`], so the caller treats it
/// as OOM and SIGSEGVs the faulting process.
///
/// The reclaim escalation itself — the loop, the four `PmmHooks` it walks, and
/// `free_count`/`alloc_page_zeroed` as plain calls — moved into `akuma_pmm` in
/// Step 4 (`docs/archive/PMM_EXTRACT.md` §7). This is a `PhysFrame` wrapper.
pub fn alloc_page_zeroed_user() -> Option<PhysFrame> {
    akuma_pmm::alloc_page_zeroed_user().map(PhysFrame::new)
}

pub fn alloc_pages_zeroed(count: usize) -> Option<alloc::vec::Vec<PhysFrame>> {
    Some(akuma_pmm::alloc_pages_zeroed(count)?.into_iter().map(PhysFrame::new).collect())
}

// ============================================================================
// Leak-debugging: per-site demand-paging frame counters (temporary instrument)
// ============================================================================
// The counters themselves moved to `akuma_pmm` on 2026-09-01 with the exception
// path's extraction (their only bumpers); what stays here is the one-line dump,
// which also reads `akuma_ext2`'s inode-deferred-free counters that sit ABOVE
// the pmm crate. Reached from `akuma-exceptions` through `ExceptionHooks`.

/// One-line dump of the demand-paging frame attribution counters, written into
/// the caller's buffer. Takes a `&mut dyn Write` (instead of returning a
/// `String`) so it stays heap-free — this is reached from the sync-EL1 crash
/// handler, which must not touch the allocator (see ALLOC_PRINT_AUDIT.md §6.3).
pub fn dp_counters_line(w: &mut dyn core::fmt::Write) {
    use core::sync::atomic::Ordering;
    let _ = write!(
        w,
        "file={} anon={} cow={} protnone={} eager={} freed={} ia_noexec={} fill_short={} unpub={} fpc_bad={} pn_file={} munmap_stale={} pf_fill_short={} pin={} pin_ovf={} defer={} defer_leak={}",
        akuma_pmm::DP_FILE_PAGES.load(Ordering::Relaxed),
        akuma_pmm::DP_ANON_PAGES.load(Ordering::Relaxed),
        akuma_pmm::DP_COW_PAGES.load(Ordering::Relaxed),
        akuma_pmm::DP_PROTNONE_PAGES.load(Ordering::Relaxed),
        akuma_pmm::EAGER_MMAP_PAGES.load(Ordering::Relaxed),
        akuma_pmm::USER_PAGES_FREED.load(Ordering::Relaxed),
        akuma_pmm::DP_IA_NOEXEC_FAULTS.load(Ordering::Relaxed),
        akuma_pmm::DP_FILE_FILL_SHORT.load(Ordering::Relaxed),
        akuma_pmm::DP_FILE_FILL_UNPUBLISHED.load(Ordering::Relaxed),
        akuma_pmm::DP_FILE_CACHE_MISMATCH.load(Ordering::Relaxed),
        akuma_pmm::DP_PROTNONE_FILE_REGION.load(Ordering::Relaxed),
        akuma_pmm::DP_MUNMAP_STALE_REGION_FRAME.load(Ordering::Relaxed),
        akuma_pmm::DP_PREFAULT_FILL_SHORT.load(Ordering::Relaxed),
        // Inode-lifecycle guards (SELFHOST_ZERO_PAGE_HUNT.md §14). `pin=` is the
        // number of inodes a live mapping is holding open — it rises and falls
        // with the build. The other three are the ones to watch: `pin_ovf=`
        // means the pin table ran out and every inode is now treated as pinned;
        // `defer=` is unlinked-but-still-mapped inodes awaiting their free, and
        // should drain to 0; `defer_leak=` is inodes leaked because the deferral
        // list was full and **must stay 0** — non-zero means raise the bound.
        akuma_primitives::inode_pin::pinned_inodes(),
        akuma_primitives::inode_pin::OVERFLOW.load(Ordering::Relaxed),
        akuma_ext2::deferred_free_pending(),
        akuma_ext2::DEFERRED_FREE_LEAKED.load(Ordering::Relaxed),
    );
}
