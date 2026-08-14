//! Physical Memory Manager (PMM) — thin shim over `akuma_pmm`.
//!
//! The bitmap allocator, frame tracker, UAF quarantine, free/CoW event ledgers
//! (Step 2), the CoW refcount table itself (Step 3), and the reclaim escalation
//! loop + its four `PmmHooks` (Step 4) all moved to `crates/akuma-pmm` —
//! `docs/archive/PMM_EXTRACT.md` §7, 2026-08-14. This file is what's left:
//!
//! - `PhysFrame`/`FrameSource` conversions at the crate boundary (the crate
//!   works in raw `usize` PAs — see its module doc for why).
//! - `surviving_mapper` and `report_poison_value` — the two pieces that reach
//!   into `akuma-exec` state the crate can never depend on (the process table)
//!   or hasn't received yet (the poison codec, moving in Step 6).
//! - The demand-paging attribution counters (`DP_*`) — never part of "the
//!   PMM's state" in the extraction plan's inventory; they're bumped by call
//!   sites outside pmm.rs entirely (`exceptions.rs`, `syscall/mem.rs`) and read
//!   back only for a diagnostic dump, so they stayed put.
//!
//! `akuma_pmm::config()`/`hooks()` need one bridge hook registered
//! (`src/main.rs`, next to `akuma_pmm::register_config`/`register_hooks`):
//! `surviving_mapper` (permanent — it walks the process table, which can never
//! move into a crate below `akuma-exec`). The `cow_ref_get` bridge Step 2
//! needed is gone: `COW_REFCOUNTS` is crate-native now, so the crate's own
//! internal code (`release_from_quarantine`, `report_premature_free`,
//! `free_page`) calls it directly.

pub use akuma_exec::{PhysFrame, FrameSource};

pub use akuma_pmm::{
    PmmConfig, PmmHooks, register_config, register_hooks, FrameTrackingStats,
    cow_ref_inc, cow_ref_dec, cow_ref_get,
};
#[allow(unused_imports)]
pub use akuma_pmm::cow_ref_count;

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

pub fn tracking_stats() -> Option<FrameTrackingStats> {
    akuma_pmm::tracking_stats()
}

#[allow(dead_code)]
pub fn leak_count() -> usize {
    akuma_pmm::leak_count()
}

// ============================================================================
// UAF hunt: free ledger, is_page_free, CoW event ledger — pure re-exports
// ============================================================================

pub fn is_page_free(pa: usize) -> bool {
    akuma_pmm::is_page_free(pa)
}

pub fn last_free_record(pa: usize) -> Option<(u32, u32)> {
    akuma_pmm::last_free_record(pa)
}

pub fn free_ledger_seq() -> u32 {
    akuma_pmm::free_ledger_seq()
}

// Only caller is `process_tests.rs`, which is `#[cfg(kernel_tests)]`-gated
// wholesale; `alloc_page`'s own use of the underlying quarantine drain moved
// into `akuma_pmm::alloc_page` and no longer routes through this wrapper.
#[cfg(kernel_tests)]
pub fn cow_ever_touched(pa: usize) -> Option<bool> {
    akuma_pmm::cow_ever_touched(pa)
}

pub fn print_cow_history(pa: usize) {
    akuma_pmm::print_cow_history(pa);
}

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
// The bridge hook the crate needs. Registered from `src/main.rs`.
// ============================================================================

/// **Permanent.** The first live address space (other than one this thread is
/// tearing down) that still tracks `pa` as one of its user frames, as
/// `(pid, tgid)`. `find_process` is lock-free (slot-state atomics + raw
/// pointers under an IRQ guard); RETIRED slots are skipped, so a process being
/// reaped cannot report itself.
pub fn surviving_mapper(pa: usize) -> Option<(u32, u32)> {
    akuma_exec::process::table::find_process(|p| {
        if p.address_space.tracks_user_frame(pa) {
            Some((p.pid, p.tgid))
        } else {
            None
        }
    })
}

// `cow_ref_inc`/`cow_ref_dec`/`cow_ref_get`/`cow_ref_count` are `akuma_pmm`'s
// now — `COW_REFCOUNTS` and its operations moved wholesale in Step 3. Re-exported
// above so `pmm::cow_ref_*` call sites (the ExecRuntime table, `process/mod.rs`,
// the fault path, boot tests) are unchanged.

// ============================================================================
// Poison decode (fault-path diagnostic). The codec itself
// (`POISON_MAGIC`/`poison_word`/`poison_decode`) migrated to `akuma_pmm` for
// real in Step 6 (`docs/archive/PMM_EXTRACT.md` §7). This wrapper did not
// follow: it needs the *live* RAM window (`akuma_exec::mmu::ram_base`/`ram_end`)
// and gates on `pmm_uaf_quarantine`, both `akuma-exec`/`src`-side state the
// crate below `akuma-exec` cannot reach — same reasoning `report_poison_value`
// itself already used to justify staying here. One caller
// (`report_poison_value` below), so a hook for it would cost more than it buys.
// ============================================================================

/// If `word` is a quarantine poison word, the frame it was written for.
///
/// This is what turns the null-`Rc` crash from "a qword read back as garbage"
/// into "frame P, freed by thread T at free-seq S". Returns `None` when the
/// quarantine is compiled off, since nothing writes poison then and any match
/// would be a coincidence.
fn poison_word_frame(word: u64) -> Option<usize> {
    if !akuma_pmm::config().pmm_uaf_quarantine {
        return None;
    }
    akuma_pmm::poison_decode(word, akuma_exec::mmu::ram_base(), akuma_exec::mmu::ram_end())
}

/// Report a value that decoded as quarantine poison, naming the frame it
/// belonged to, who freed it and how its reference count got to zero. Called
/// from the fault path with whatever registers the faulting instruction used.
pub fn report_poison_value(tag: &str, word: u64) {
    let Some(pa) = poison_word_frame(word) else { return };
    let (tid_freed, seq_freed) = last_free_record(pa).unwrap_or((u32::MAX, 0));
    crate::safe_print!(255,
        "[PMM-POISON] {}={:#x} is quarantine poison for pa={:#x} — the kernel FREED \
         this frame while the process still had it. freed_by=(tid={} seq={}) now_seq={} cow_ref={}\n",
        tag, word, pa, tid_freed, seq_freed, free_ledger_seq(), cow_ref_get(pa));
    if let Some((pid, tgid)) = surviving_mapper(pa) {
        crate::safe_print!(128,
            "  [PMM-POISON] pa={:#x} still tracked by pid={} tgid={}\n", pa, pid, tgid);
    }
    print_cow_history(pa);
}

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

// The reserve, its predicate and the readahead budget: pure arithmetic,
// host-tested in `akuma_pmm` instead of from the boot suite.
pub use akuma_pmm::{USER_PAGE_RESERVE, user_readahead_budget};

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
// Each demand-paging map site bumps the matching counter once per page it maps,
// and the page-free path bumps PAGES_FREED. Dumped in the crash handler and the
// periodic [Mem] line so a memory spike can be attributed to a specific path.
// NOT part of the crate: bumped by call sites outside pmm.rs entirely
// (exceptions.rs, syscall/mem.rs) and read back only for a diagnostic dump.
use core::sync::atomic::{AtomicUsize, Ordering};

pub static DP_FILE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_ANON_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_COW_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_PROTNONE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static EAGER_MMAP_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static USER_PAGES_FREED: AtomicUsize = AtomicUsize::new(0);

#[inline]
pub fn dp_count(counter: &AtomicUsize, n: usize) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// One-line dump of the demand-paging frame attribution counters, written into
/// the caller's buffer. Takes a `&mut dyn Write` (instead of returning a
/// `String`) so it stays heap-free — this is reached from the sync-EL1 crash
/// handler, which must not touch the allocator (see ALLOC_PRINT_AUDIT.md §6.3).
pub fn dp_counters_line(w: &mut dyn core::fmt::Write) {
    let _ = write!(
        w,
        "file={} anon={} cow={} protnone={} eager={} freed={}",
        DP_FILE_PAGES.load(Ordering::Relaxed),
        DP_ANON_PAGES.load(Ordering::Relaxed),
        DP_COW_PAGES.load(Ordering::Relaxed),
        DP_PROTNONE_PAGES.load(Ordering::Relaxed),
        EAGER_MMAP_PAGES.load(Ordering::Relaxed),
        USER_PAGES_FREED.load(Ordering::Relaxed),
    );
}
