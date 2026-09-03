//! Physical Memory Manager (PMM) — bitmap allocator, frame tracking, UAF
//! quarantine, and the free/CoW event ledgers.
//!
//! # Extraction status (2026-08-14)
//!
//! All 7 steps of `docs/archive/PMM_EXTRACT.md` §7 are landed — the extraction
//! is complete. Step 7 added 15 host tests (the bitmap allocator's alloc/free
//! symmetry, contiguous-run search and fragmentation behaviour; the CoW
//! refcount table's accounting — "the tree's historically-buggiest", never
//! unit-tested before this crate existed; the quarantine ring's UAF and
//! double-free detection; and the reclaim escalation's *effects*, that the
//! four `PmmHooks` actually fire, in order, only under pressure — its
//! *decision* already had 6 tests from Step 4). Step 2 bundled "the allocator
//! core + FrameTracker + ledgers + quarantine" into one
//! step, but three pieces of that turned out to have real cross-crate
//! dependencies the plan didn't surface. Two were resolved with a small
//! `Registered` hook that degrades (`.get()`) rather than panics, so the crate
//! stays a strict "no dependency on `akuma-exec`" leaf; the third (and a fourth,
//! added in Step 4) used the temporary-duplicate pattern instead, both now
//! resolved for real by Step 6:
//!
//! - **The CoW refcount table** moved for real in Step 3: `COW_REFCOUNTS` is
//!   crate-native now, so the `register_cow_ref_get_hook` bridge that covered it
//!   for the one step in between is gone — `free_page`'s gate and the two anomaly
//!   reports (`release_from_quarantine`'s UAF print, `report_premature_free`) all
//!   call [`cow_ref_get`] directly.
//! - **The process table (`akuma_exec::process::table::find_process`) can never
//!   move here** (this crate sits below `akuma-exec` in the dependency graph, and
//!   moving it would need the reverse). [`register_surviving_mapper_hook`] is the
//!   bridge — **permanent**, the one hook this crate will always need.
//! - **The poison codec** ([`POISON_MAGIC`], [`poison_word`], [`poison_decode`])
//!   moved for real in Step 6, from `akuma_exec::memmath` — deleting the Step 4
//!   temporary duplicate along with it. What did **not** follow:
//!   `poison_word_frame`, the thin wrapper that supplies the *live*
//!   `akuma_exec::mmu::ram_base`/`ram_end` window and gates on
//!   `pmm_uaf_quarantine` — it needs `akuma-exec` state this crate structurally
//!   cannot reach, so it lives in `src/pmm.rs` now (it used to live in
//!   `memmath`, for the identical reason, before this crate existed). One
//!   caller (`report_poison_value`, also in `src/pmm.rs`), so a hook for it
//!   would cost more than it buys — same call the plan already made for
//!   `report_poison_value` itself.
//! - **The reclaim escalation's decision** (`ReclaimStep`/`next_reclaim_step`,
//!   in the private `reclaim_escalation` module) also moved for real in Step 6,
//!   alongside [`USER_PAGE_RESERVE`]/[`user_alloc_would_starve`]/
//!   [`user_readahead_budget`] (the reserve it's built on). Step 4 had already
//!   moved the escalation's *loop and effects* in (`alloc_page_zeroed_user`,
//!   calling `free_count`/`alloc_page_zeroed` directly and the four
//!   [`PmmHooks`] for the cold collaborators) behind a temporary-duplicate
//!   decision function; Step 6 deleted that duplicate and `alloc_page_zeroed_user`
//!   now calls the crate's own, permanent `next_reclaim_step`.
//!
//! `akuma_exec::memmath` was left holding only the mapping predicates
//! (`mapping_is_read_only_to_user`, `is_shareable_mapping`) — never PMM concepts.
//!
//! **Correction, 2026-08-30:** they were not `akuma-exec`'s own either, and the
//! module is now gone. Both moved to `akuma_mmap::user_flags`, beside the
//! `is_write`/`is_exec`/`from_prot` vocabulary they read
//! (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §7.4). What had kept them up in
//! `akuma-exec` was the same shape this document describes for the PMM's own
//! arithmetic: `is_shareable_mapping` read its kill switch from an injectable
//! `ExecConfig`, so it looked like it needed the execution crate. It needed one
//! `bool`. Passing the gate as a parameter moved the predicate down and left the
//! config read at its one call site.
//!
//! # Why a leaf crate
//!
//! Before this crate existed, `crates/akuma-exec`'s `ExecRuntime` carried 13
//! function pointers whose only job was letting `akuma-exec` avoid depending on
//! the PMM, which lived in the kernel binary. Three of those (`alloc_page_zeroed`,
//! `track_frame`, `cow_ref_inc`) were on the CoW fault path — real indirection
//! cost for an avoidable reason. Making the PMM a crate made the dependency
//! ordinary: Step 5 deleted 12 of the 13 (`is_memory_low` stayed — see that
//! step's note in `docs/archive/PMM_EXTRACT.md` §7 for why it couldn't follow
//! the other 12), and every one of those 12 call sites now calls `akuma_pmm::*`
//! directly. The main payoff was never the deleted indirection on its own,
//! though: it's that the allocator, refcounts and quarantine ring now run under
//! a host test instead of only inside a booted VM.

#![cfg_attr(not(test), no_std)]
#![allow(
    clippy::future_not_send,
    clippy::must_use_candidate,
    clippy::missing_const_for_fn,
    clippy::uninlined_format_args,
    clippy::cast_ptr_alignment,
    clippy::items_after_statements,
    clippy::significant_drop_in_scrutinee,
    clippy::too_many_lines,
    clippy::use_self,
    clippy::struct_field_names,
    clippy::struct_excessive_bools,
    clippy::similar_names,
    clippy::unreadable_literal,
    clippy::unnecessary_cast,
    clippy::redundant_else,
    clippy::semicolon_if_nothing_returned,
    clippy::single_match_else,
    clippy::declare_interior_mutable_const,
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::unused_self,
    clippy::vec_init_then_push,
    clippy::pub_underscore_fields,
    clippy::doc_markdown,
    clippy::too_long_first_doc_paragraph,
    clippy::needless_pass_by_value,
    clippy::if_not_else,
    clippy::manual_div_ceil,
    clippy::option_if_let_else,
    clippy::match_wildcard_for_single_variants,
    clippy::cast_possible_wrap,
    clippy::redundant_closure_for_method_calls,
    clippy::iter_without_into_iter,
    clippy::collapsible_if,
    clippy::significant_drop_tightening,
    clippy::ref_as_ptr,
    clippy::needless_range_loop,
    clippy::new_without_default,
    clippy::match_same_arms,
    clippy::redundant_closure,
    clippy::manual_is_variant_and,
    clippy::missing_safety_doc,
    clippy::let_and_return,
    clippy::manual_range_contains,
    clippy::empty_line_after_doc_comments,
    clippy::inline_always,
    clippy::bool_to_int_with_if,
    clippy::manual_saturating_arithmetic,
    clippy::cast_lossless,
    clippy::option_map_or_none,
    clippy::redundant_field_names,
    clippy::let_underscore_untyped,
    unused_unsafe,
    unused_mut,
    clippy::implicit_saturating_sub,
    clippy::manual_let_else,
    clippy::verbose_bit_mask,
    clippy::ptr_cast_constness,
    clippy::derive_partial_eq_without_eq,
    clippy::or_fun_call,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::identity_op,
    clippy::while_let_loop,
    clippy::collapsible_else_if,
    clippy::needless_continue,
    clippy::inherent_to_string,
    clippy::manual_find,
    clippy::manual_is_multiple_of,
    clippy::eq_op,
    clippy::doc_overindented_list_items,
    clippy::map_unwrap_or,
    clippy::used_underscore_binding,
    clippy::branches_sharing_code,
    clippy::doc_comment_double_space_linebreaks,
    clippy::no_effect_underscore_binding,
    clippy::unwrap_or_default,
    clippy::should_implement_trait,
)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spinning_top::Spinlock;

use akuma_primitives::{Registered, phys_to_virt};

/// ARM64 page size. Duplicated from `akuma_exec::mmu::types::PAGE_SIZE` (a plain
/// `4096` literal there too) rather than imported, since this crate sits below
/// `akuma-exec` in the dependency graph. Matches the existing pattern elsewhere in
/// the tree (`crates/akuma-virtio/src/rng.rs`, `src/allocator.rs`,
/// `src/syscall/aio.rs` each carry their own copy of the same constant).
pub const PAGE_SIZE: usize = 4096;

/// Host-test scaffolding: a real `akuma_pmm` over a leaked host-heap arena.
///
/// Step 7 (`docs/archive/PMM_EXTRACT.md` §7). This crate's own tests need the
/// same thing `akuma-exec`'s `test_support::ensure_test_pmm` does — a live
/// `config()`/`hooks()` registration (`cow_ledger_record`, `free_page`, and
/// `alloc_page_zeroed_user` all read one or the other unconditionally) and,
/// for the tests that actually allocate, a real backing arena
/// (`akuma_primitives::phys_to_virt` is the identity, so a real host address
/// works as a "physical" page directly) — but it cannot reuse `akuma-exec`'s
/// copy: that lives in a different crate's test binary, a separate process
/// with its own copy of every static in this crate.
#[cfg(test)]
mod test_arena {
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Registered as `PmmHooks` below. All four are no-ops (return 0, nothing
    /// left to reclaim in a host test — there is no heap/process table/file
    /// cache behind them), but each stamps the shared call-order sequence
    /// first, so the one test that ever drives real pressure
    /// (`escalation_walks_all_four_hooks_in_order_then_gives_up`) can verify
    /// the four fired, in order, exactly once. No other test reaches these —
    /// none of this crate's other tests call `alloc_page_zeroed_user`.
    static CALL_SEQ: AtomicUsize = AtomicUsize::new(0);
    pub static HEAP_RECLAIM_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static DRAIN_RETIRED_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static EVICT_FILE_AT: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static SHRINK_CACHE_AT: AtomicUsize = AtomicUsize::new(usize::MAX);

    fn stamp(slot: &AtomicUsize) -> usize {
        let seq = CALL_SEQ.fetch_add(1, Ordering::SeqCst);
        slot.store(seq, Ordering::SeqCst);
        0
    }
    fn hook_heap_reclaim() -> usize { stamp(&HEAP_RECLAIM_AT) }
    fn hook_drain_retired() -> usize { stamp(&DRAIN_RETIRED_AT) }
    fn hook_evict_clean_file_pages(_n: usize) -> usize { stamp(&EVICT_FILE_AT) }
    fn hook_shrink_page_cache(_n: usize) -> usize { stamp(&SHRINK_CACHE_AT) }

    /// Register a real PMM, once. `std::sync::Once`, not `OnceCopy::set`'s
    /// idempotent-ignore: `crate::init` mutates the bitmap unconditionally on
    /// every call, so two test threads racing their first call could reset
    /// frames the other's test already allocated from.
    pub fn ensure_pmm() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // 16 MiB: this crate's own tests are smaller and fewer than
            // `akuma-exec`'s (which needs headroom for ~225 tests reaching
            // through many call sites); the quarantine/escalation tests below
            // are the only heavy users and both size their own demand well
            // under this.
            const ARENA_WORDS: usize = 16 * 1024 * 1024 / 8;
            let arena: alloc::vec::Vec<u64> = alloc::vec![0u64; ARENA_WORDS];
            let arena: &'static mut [u64] = alloc::boxed::Box::leak(arena.into_boxed_slice());
            let base = arena.as_ptr() as usize;
            let size = core::mem::size_of_val(arena);

            crate::register_config(crate::PmmConfig {
                cow_ref_ledger: true,
                pmm_uaf_quarantine: true,
                pmm_premature_free_check: true,
            });
            crate::register_hooks(crate::PmmHooks {
                heap_reclaim: hook_heap_reclaim,
                drain_retired: hook_drain_retired,
                evict_clean_file_pages: hook_evict_clean_file_pages,
                shrink_page_cache: hook_shrink_page_cache,
            });
            crate::init(base, size, base); // kernel_end == base: nothing pre-reserved
        });
    }
}

// ============================================================================
// PmmConfig / PmmHooks
// ============================================================================

/// The three kill-switch booleans the allocator/quarantine gate on. See
/// `src/config.rs`'s `COW_REF_LEDGER`, `PMM_UAF_QUARANTINE`,
/// `PMM_PREMATURE_FREE_CHECK` — `src/main.rs` builds this from those constants.
#[derive(Clone, Copy)]
pub struct PmmConfig {
    pub cow_ref_ledger: bool,
    pub pmm_uaf_quarantine: bool,
    pub pmm_premature_free_check: bool,
}

/// The cold collaborators the user-page reclaim escalation (still in
/// `src/pmm.rs::alloc_page_zeroed_user` until Step 4) calls under pressure, plus
/// the same heap-reclaim step plain allocation exhaustion falls back to. Cold
/// means "runs at most once per starving allocation, and does hundreds of
/// microseconds of real work when it does" — an indirect call here is free next to
/// that, unlike the hot `free_count()` gate this is deliberately NOT part of
/// (`docs/archive/PMM_EXTRACT.md` §4).
#[derive(Clone, Copy)]
pub struct PmmHooks {
    pub heap_reclaim: fn() -> usize,
    pub drain_retired: fn() -> usize,
    pub evict_clean_file_pages: fn(usize) -> usize,
    pub shrink_page_cache: fn(usize) -> usize,
}

static PMM_CONFIG: Registered<PmmConfig> =
    Registered::new("akuma-pmm: PmmConfig not registered — call akuma_pmm::register_config() first");
static PMM_HOOKS: Registered<PmmHooks> =
    Registered::new("akuma-pmm: PmmHooks not registered — call akuma_pmm::register_hooks() first");

/// Register the config booleans. Must be called once, before any path that reads
/// them runs — in practice, before `init()`.
pub fn register_config(cfg: PmmConfig) {
    PMM_CONFIG.register(cfg);
}

/// Register the four reclaim hooks. Must be called once, before any path that
/// reaches them runs (i.e. before userspace exists — the pressure ladder cannot
/// fire before then).
pub fn register_hooks(hooks: PmmHooks) {
    PMM_HOOKS.register(hooks);
}

#[must_use]
pub fn config() -> PmmConfig {
    PMM_CONFIG.require()
}

#[must_use]
pub fn hooks() -> PmmHooks {
    PMM_HOOKS.require()
}

// ============================================================================
// Permanent diagnostic hook — see the module doc's "Extraction status" section.
// (The temporary `cow_ref_get` bridge that used to live here was deleted in
// Step 3, once `COW_REFCOUNTS` below made the real lookup local to this crate.)
// ============================================================================

/// **Permanent.** The first live address space (other than one this thread is
/// tearing down) that still tracks a PA as a user frame, as `(pid, tgid)`. Walks
/// `akuma_exec::process::table::find_process`, which can never move into this
/// crate (wrong side of the dependency graph) — so this stays a hook forever, not
/// just until the next step.
static SURVIVING_MAPPER: Registered<fn(usize) -> Option<(u32, u32)>> = Registered::new("unused");

/// Register the permanent surviving-mapper bridge.
pub fn register_surviving_mapper_hook(f: fn(usize) -> Option<(u32, u32)>) {
    SURVIVING_MAPPER.register(f);
}

fn surviving_mapper(pa: usize) -> Option<(u32, u32)> {
    SURVIVING_MAPPER.get().and_then(|f| f(pa))
}

// ============================================================================
// Debug Frame Tracking
// ============================================================================

/// Enable debug frame tracking (adds overhead but helps find leaks). Always
/// `false` today — kept as a compile-time switch, not a runtime one, so the
/// tracking code costs nothing when off.
pub const DEBUG_FRAME_TRACKING: bool = false;

/// Allocation source for debug frame tracking. A plain discriminant rather than
/// `akuma_exec::FrameSource` (defined in `crates/akuma-exec/src/runtime.rs`,
/// unreachable from this crate) — `src/pmm.rs`'s `track_frame` wrapper converts at
/// the boundary. `DEBUG_FRAME_TRACKING` is `false` unconditionally, so none of
/// this ever executes; the conversion exists only to keep the type compiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSource {
    Kernel,
    UserPageTable,
    UserData,
    ElfLoader,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub source: FrameSource,
}

struct FrameTracker {
    allocations: alloc::collections::BTreeMap<usize, FrameInfo>,
    kernel_count: usize,
    user_page_table_count: usize,
    user_data_count: usize,
    elf_loader_count: usize,
    unknown_count: usize,
    total_tracked: usize,
    total_untracked: usize,
}

impl FrameTracker {
    const fn new() -> Self {
        Self {
            allocations: alloc::collections::BTreeMap::new(),
            kernel_count: 0,
            user_page_table_count: 0,
            user_data_count: 0,
            elf_loader_count: 0,
            unknown_count: 0,
            total_tracked: 0,
            total_untracked: 0,
        }
    }

    fn track(&mut self, addr: usize, source: FrameSource) {
        if let Some(_old) = self.allocations.insert(addr, FrameInfo { source }) {
            akuma_primitives::safe_print!(48, "[PMM WARN] Double allocation detected!\n");
        }
        match source {
            FrameSource::Kernel => self.kernel_count += 1,
            FrameSource::UserPageTable => self.user_page_table_count += 1,
            FrameSource::UserData => self.user_data_count += 1,
            FrameSource::ElfLoader => self.elf_loader_count += 1,
            FrameSource::Unknown => self.unknown_count += 1,
        }
        self.total_tracked += 1;
    }

    fn untrack(&mut self, addr: usize) -> Option<FrameInfo> {
        if let Some(info) = self.allocations.remove(&addr) {
            match info.source {
                FrameSource::Kernel => self.kernel_count = self.kernel_count.saturating_sub(1),
                FrameSource::UserPageTable => {
                    self.user_page_table_count = self.user_page_table_count.saturating_sub(1);
                }
                FrameSource::UserData => {
                    self.user_data_count = self.user_data_count.saturating_sub(1);
                }
                FrameSource::ElfLoader => {
                    self.elf_loader_count = self.elf_loader_count.saturating_sub(1);
                }
                FrameSource::Unknown => self.unknown_count = self.unknown_count.saturating_sub(1),
            }
            self.total_untracked += 1;
            Some(info)
        } else {
            akuma_primitives::safe_print!(48, "[PMM WARN] Freeing untracked frame\n");
            None
        }
    }

    #[allow(dead_code)]
    fn leak_count(&self) -> usize {
        self.allocations.len()
    }

    fn stats(&self) -> FrameTrackingStats {
        FrameTrackingStats {
            current_tracked: self.allocations.len(),
            kernel_count: self.kernel_count,
            user_page_table_count: self.user_page_table_count,
            user_data_count: self.user_data_count,
            elf_loader_count: self.elf_loader_count,
            unknown_count: self.unknown_count,
            total_tracked: self.total_tracked,
            total_untracked: self.total_untracked,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FrameTrackingStats {
    pub current_tracked: usize,
    pub kernel_count: usize,
    pub user_page_table_count: usize,
    pub user_data_count: usize,
    pub elf_loader_count: usize,
    pub unknown_count: usize,
    pub total_tracked: usize,
    pub total_untracked: usize,
}

static FRAME_TRACKER: Spinlock<FrameTracker> = Spinlock::new(FrameTracker::new());

pub fn track_frame(addr: usize, source: FrameSource) {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().track(addr, source);
    }
}

pub fn untrack_frame(addr: usize) {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().untrack(addr);
    }
}

pub fn tracking_stats() -> Option<FrameTrackingStats> {
    if DEBUG_FRAME_TRACKING {
        Some(FRAME_TRACKER.lock().stats())
    } else {
        None
    }
}

#[allow(dead_code)]
pub fn leak_count() -> usize {
    if DEBUG_FRAME_TRACKING {
        FRAME_TRACKER.lock().leak_count()
    } else {
        0
    }
}

// ============================================================================
// Bitmap allocator
// ============================================================================

struct BitmapAllocator {
    bitmap: Vec<u64>,
    base_addr: usize,
    total_pages: usize,
    free_pages: usize,
    next_free_hint: usize,
}

impl BitmapAllocator {
    const fn new() -> Self {
        Self {
            bitmap: Vec::new(),
            base_addr: 0,
            total_pages: 0,
            free_pages: 0,
            next_free_hint: 0,
        }
    }

    fn init(&mut self, base: usize, size: usize, kernel_end: usize) {
        self.base_addr = base;
        self.total_pages = size / PAGE_SIZE;

        let bitmap_size = self.total_pages.div_ceil(64);
        self.bitmap = alloc::vec![0u64; bitmap_size];

        for i in 0..bitmap_size {
            self.bitmap[i] = !0u64;
        }

        let kernel_pages = kernel_end.saturating_sub(base).div_ceil(PAGE_SIZE);
        for i in 0..kernel_pages {
            self.mark_used(i);
        }

        self.free_pages = self.total_pages - kernel_pages;
        self.next_free_hint = kernel_pages;

        let remaining = self.total_pages % 64;
        if remaining != 0 {
            let last_idx = bitmap_size - 1;
            let mask = (1u64 << remaining) - 1;
            self.bitmap[last_idx] &= mask;
        }
    }

    fn mark_used(&mut self, page_idx: usize) {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        if word_idx < self.bitmap.len() {
            self.bitmap[word_idx] &= !(1u64 << bit_idx);
        }
    }

    fn mark_free(&mut self, page_idx: usize) {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        if word_idx < self.bitmap.len() {
            self.bitmap[word_idx] |= 1u64 << bit_idx;
        }
    }

    fn is_free(&self, page_idx: usize) -> bool {
        let word_idx = page_idx / 64;
        let bit_idx = page_idx % 64;
        if word_idx < self.bitmap.len() {
            (self.bitmap[word_idx] & (1u64 << bit_idx)) != 0
        } else {
            false
        }
    }

    /// Address-taking counterpart of [`Self::is_free`]. Out-of-range addresses
    /// report `false`: this allocator has no claim on them.
    fn is_page_free_pa(&self, pa: usize) -> bool {
        if pa < self.base_addr {
            return false;
        }
        let page_idx = (pa - self.base_addr) / PAGE_SIZE;
        page_idx < self.total_pages && self.is_free(page_idx)
    }

    fn alloc_page(&mut self) -> Option<usize> {
        let start_word = self.next_free_hint / 64;

        for word_idx in start_word..self.bitmap.len() {
            if self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;

                if page_idx < self.total_pages {
                    self.mark_used(page_idx);
                    self.free_pages -= 1;
                    self.next_free_hint = page_idx + 1;
                    return Some(self.base_addr + page_idx * PAGE_SIZE);
                }
            }
        }

        for word_idx in 0..start_word {
            if self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;

                if page_idx < self.total_pages {
                    self.mark_used(page_idx);
                    self.free_pages -= 1;
                    self.next_free_hint = page_idx + 1;
                    return Some(self.base_addr + page_idx * PAGE_SIZE);
                }
            }
        }

        None
    }

    /// `result` must already have capacity for `count`, reserved by the caller
    /// BEFORE it took the PMM lock — see `alloc_pages_zeroed`'s doc comment for
    /// why (the PMM-heap lock inversion that deadlocked `-j4` self-host builds).
    fn alloc_pages_into(&mut self, count: usize, result: &mut Vec<usize>) -> bool {
        debug_assert!(result.capacity() >= count, "caller must reserve before locking PMM");
        if count == 0 { return true; }
        if self.free_pages < count { return false; }

        let start_word = self.next_free_hint / 64;

        for word_idx in start_word..self.bitmap.len() {
            while self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;
                if page_idx >= self.total_pages { break; }
                self.mark_used(page_idx);
                self.free_pages -= 1;
                result.push(self.base_addr + page_idx * PAGE_SIZE);
                if result.len() == count {
                    self.next_free_hint = page_idx + 1;
                    return true;
                }
            }
        }

        for word_idx in 0..start_word {
            while self.bitmap[word_idx] != 0 {
                let bit_idx = self.bitmap[word_idx].trailing_zeros() as usize;
                let page_idx = word_idx * 64 + bit_idx;
                if page_idx >= self.total_pages { break; }
                self.mark_used(page_idx);
                self.free_pages -= 1;
                result.push(self.base_addr + page_idx * PAGE_SIZE);
                if result.len() == count {
                    self.next_free_hint = page_idx + 1;
                    return true;
                }
            }
        }

        for &pa in result.iter() {
            let page_idx = (pa - self.base_addr) / PAGE_SIZE;
            self.mark_free(page_idx);
            self.free_pages += 1;
        }
        result.clear();
        false
    }

    fn alloc_pages_contiguous(&mut self, count: usize) -> Option<usize> {
        if count == 0 { return None; }
        if count == 1 { return self.alloc_page(); }
        if self.free_pages < count { return None; }

        let hint = self.next_free_hint;

        for &start in &[hint, 0usize] {
            let mut run_start = 0;
            let mut run_len = 0;

            for page_idx in start..self.total_pages {
                if self.is_free(page_idx) {
                    if run_len == 0 {
                        run_start = page_idx;
                    }
                    run_len += 1;
                    if run_len == count {
                        for i in run_start..run_start + count {
                            self.mark_used(i);
                        }
                        self.free_pages -= count;
                        self.next_free_hint = run_start + count;
                        return Some(self.base_addr + run_start * PAGE_SIZE);
                    }
                } else {
                    run_len = 0;
                }
            }

            if start == 0 { break; }
        }

        None
    }

    fn free_pages_contiguous(&mut self, pa: usize, count: usize) {
        if pa < self.base_addr { return; }
        let start_page = (pa - self.base_addr) / PAGE_SIZE;
        for i in 0..count {
            let page_idx = start_page + i;
            if page_idx < self.total_pages && !self.is_free(page_idx) {
                self.mark_free(page_idx);
                self.free_pages += 1;
            }
        }
        if start_page < self.next_free_hint {
            self.next_free_hint = start_page;
        }
    }

    /// Returns the outcome so the caller can keep `ALLOCATED_PAGES` exact
    /// (decrement only on a real allocated→free transition) and observe
    /// double-frees instead of corrupting the counter.
    fn free_page(&mut self, pa: usize) -> FreeOutcome {
        if pa < self.base_addr {
            return FreeOutcome::OutOfRange;
        }
        let page_idx = (pa - self.base_addr) / PAGE_SIZE;
        if page_idx >= self.total_pages {
            return FreeOutcome::OutOfRange;
        }
        if self.is_free(page_idx) {
            return FreeOutcome::DoubleFree;
        }
        self.mark_free(page_idx);
        self.free_pages += 1;
        if page_idx < self.next_free_hint {
            self.next_free_hint = page_idx;
        }
        FreeOutcome::Freed
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum FreeOutcome {
    Freed,
    DoubleFree,
    OutOfRange,
}

// Step 7 (`docs/archive/PMM_EXTRACT.md` §7): host tests for the bitmap
// allocator's own logic. Each test builds a fresh, LOCAL `BitmapAllocator` —
// not the crate's global `PMM` static — so there is no shared state between
// tests and no dependency on `config()`/`hooks()`/`init()`. That is possible
// because `BitmapAllocator`'s methods never dereference the "physical
// addresses" they hand out (no `phys_to_virt`, no volatile access) — they are
// pure index arithmetic over the `Vec<u64>` bitmap, so an arbitrary,
// non-backed `base` address is fine for every case below except a genuine
// zeroing/poisoning path, which lives above `BitmapAllocator` and is covered
// by the global-arena test further down instead.
#[cfg(test)]
mod bitmap_allocator_tests {
    use super::*;

    const BASE: usize = 0x1000_0000;

    fn fresh(pages: usize) -> BitmapAllocator {
        let mut a = BitmapAllocator::new();
        // kernel_end == base: nothing pre-reserved, every page starts free.
        a.init(BASE, pages * PAGE_SIZE, BASE);
        a
    }

    #[test]
    fn init_reserves_exactly_the_kernel_prefix() {
        let mut a = BitmapAllocator::new();
        // 10-page arena, first 3 pages "already used" by the kernel image.
        a.init(BASE, 10 * PAGE_SIZE, BASE + 3 * PAGE_SIZE);
        assert_eq!(a.total_pages, 10);
        assert_eq!(a.free_pages, 7);
        for i in 0..3 {
            assert!(!a.is_free(i), "kernel prefix page {i} must start used");
        }
        for i in 3..10 {
            assert!(a.is_free(i), "page {i} past the kernel prefix must start free");
        }
    }

    #[test]
    fn alloc_then_free_round_trips_to_the_same_free_count() {
        let mut a = fresh(8);
        let free_before = a.free_pages;
        let pa = a.alloc_page().expect("8-page arena must have room for one");
        assert_eq!(a.free_pages, free_before - 1);
        assert_eq!(a.free_page(pa), FreeOutcome::Freed);
        assert_eq!(a.free_pages, free_before, "a symmetric alloc+free must restore free_pages");
        assert!(a.is_page_free_pa(pa));
    }

    #[test]
    fn alloc_exhausts_then_refuses() {
        let mut a = fresh(4);
        let mut got = alloc::vec::Vec::new();
        for _ in 0..4 {
            got.push(a.alloc_page().expect("must succeed while pages remain"));
        }
        assert_eq!(a.free_pages, 0);
        assert!(a.alloc_page().is_none(), "a fully-allocated arena must refuse the 5th page");
        // Every returned PA must be distinct — a bug here would hand out the
        // same frame twice while claiming to be out of memory.
        let mut sorted = got.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), got.len(), "alloc_page must never repeat a frame");
    }

    #[test]
    fn free_page_outcomes_distinguish_freed_double_free_and_out_of_range() {
        let mut a = fresh(4);
        let pa = a.alloc_page().unwrap();
        assert_eq!(a.free_page(pa), FreeOutcome::Freed);
        assert_eq!(
            a.free_page(pa),
            FreeOutcome::DoubleFree,
            "freeing an already-free page must be reported, not silently accepted"
        );
        assert_eq!(
            a.free_page(BASE + 1000 * PAGE_SIZE),
            FreeOutcome::OutOfRange,
            "a PA outside [base, base+size) must be reported, not treated as this arena's"
        );
        assert_eq!(
            a.free_page(BASE - PAGE_SIZE),
            FreeOutcome::OutOfRange,
            "a PA below base must be reported too, not wrap"
        );
    }

    #[test]
    fn alloc_pages_contiguous_finds_a_run() {
        let mut a = fresh(16);
        let pa = a.alloc_pages_contiguous(4).expect("16 free pages must fit a run of 4");
        for i in 0..4 {
            assert!(!a.is_page_free_pa(pa + i * PAGE_SIZE), "run page {i} must be marked used");
        }
        assert_eq!(a.free_pages, 12);
    }

    /// The fragmentation case the plan's §6 payoff list names explicitly:
    /// enough TOTAL free pages, but no contiguous RUN long enough, must fail —
    /// not silently return a shorter or non-contiguous "run".
    #[test]
    fn alloc_pages_contiguous_fails_on_fragmentation_despite_enough_total_free() {
        let mut a = fresh(8);
        // Free every other page: 4 pages free, but no 2 adjacent.
        for i in (0..8).step_by(2) {
            a.mark_used(i);
        }
        a.free_pages = 4;
        assert_eq!(a.alloc_pages_contiguous(2), None, "no 2-page run exists despite 4 free pages");
        // A single page must still succeed — the failure is about the RUN,
        // not about being out of memory.
        assert!(a.alloc_pages_contiguous(1).is_some());
    }

    #[test]
    fn alloc_pages_into_reserves_the_exact_count_or_rolls_back_completely() {
        let mut a = fresh(4);
        let mut out = alloc::vec::Vec::with_capacity(4);
        assert!(a.alloc_pages_into(4, &mut out), "4 requested from 4 free must succeed");
        assert_eq!(out.len(), 4);
        assert_eq!(a.free_pages, 0);

        let mut a = fresh(4);
        let mut out = alloc::vec::Vec::with_capacity(8);
        assert!(
            !a.alloc_pages_into(8, &mut out),
            "8 requested from 4 free must fail, not partially allocate"
        );
        assert_eq!(out.len(), 0, "a failed batch must leave the output empty");
        assert_eq!(a.free_pages, 4, "a failed batch must roll back every page it marked used");
    }

    /// Exercises the allocator's two-pass word search (`start_word..len` then
    /// `0..start_word`). The bitmap is one `u64` word per 64 pages, and
    /// `trailing_zeros` always returns the LOWEST free bit in whichever word it
    /// scans — so within a single word, a freed low page is found immediately
    /// without needing the wraparound pass at all. The wraparound pass only
    /// matters once the *word* the hint points at (and every word after it) is
    /// fully exhausted while an earlier word still has a free bit — which is
    /// what this test builds directly via the private fields, since driving it
    /// through the public API alone would need a 3rd allocator behaviour
    /// (`alloc_pages_contiguous`) just to set the scene.
    #[test]
    fn next_free_hint_wraps_to_an_earlier_word_once_the_later_ones_are_exhausted() {
        let mut a = fresh(70); // 2 bitmap words: pages 0..64, pages 64..70.
        // Hand-construct "page 0 free, pages 1..64 used, hint already past word 0".
        for i in 1..64 {
            a.mark_used(i);
        }
        a.next_free_hint = 64;
        a.free_pages = 1 + (70 - 64); // page 0, plus all of word 1.

        // Exhaust every page in word 1 (pages 64..70) via the public API — the
        // primary pass (`start_word..len`) satisfies each of these without ever
        // touching word 0.
        for _ in 64..70 {
            a.alloc_page().expect("word 1 must still have free pages");
        }
        assert_eq!(a.free_pages, 1, "only page 0 should be left");

        // Word 1 is now fully used and the hint sits at word 1 (70 / 64 == 1):
        // the primary pass finds nothing, so this can only succeed via the
        // wraparound pass into word 0.
        let wrapped = a.alloc_page().expect("must wrap back to word 0's free page");
        assert_eq!(wrapped, BASE, "the only free page left is page 0");
    }

    #[test]
    fn free_pages_contiguous_frees_every_page_in_the_run() {
        let mut a = fresh(8);
        let pa = a.alloc_pages_contiguous(4).unwrap();
        a.free_pages_contiguous(pa, 4);
        assert_eq!(a.free_pages, 8);
        for i in 0..4 {
            assert!(a.is_page_free_pa(pa + i * PAGE_SIZE));
        }
    }
}

static PMM: Spinlock<BitmapAllocator> = Spinlock::new(BitmapAllocator::new());
static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Count of detected double-frees: a page returned to the PMM while already
/// free. Contained by the bitmap guard above, but any non-zero value means some
/// caller's free obligations are out of sync with its allocations. Surfaced in
/// the periodic `[Mem]` stats line.
static DOUBLE_FREE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// File-page fills in `akuma_exec::process::user_access::prefault_user_range` that
/// came back short or errored, counted per page.
///
/// The prefault fill is the one file-fill site in the tree the demand-fault
/// instrument (`[FILL-SHORT]` in `src/exceptions.rs`) cannot see: a page the
/// prefault installs is *present*, so no later fault re-fills or re-checks it.
/// A short fill here therefore installs a zero page that reads back as
/// `[0,0,0,0]` forever — the self-host ICE shape. The fill result was dropped
/// on the floor (`let _ =`) until 2026-08-15; this counter is the instrument
/// that closes the blind spot. `pub` because the print site lives in
/// `akuma-exec` while the `[Mem]` dump lives in the bin crate.
pub static DP_PREFAULT_FILL_SHORT: AtomicUsize = AtomicUsize::new(0);

// ============================================================================
// Leak-debugging: per-site demand-paging frame counters (temporary instrument)
// ============================================================================
// Each demand-paging map site bumps the matching counter once per page it maps,
// and the page-free path bumps PAGES_FREED. Dumped in the crash handler and the
// periodic [Mem] line so a memory spike can be attributed to a specific path.
//
// Moved here from `src/pmm.rs` on 2026-09-01, with the exception path's move to
// `akuma-exceptions`: their remaining bumpers (`src/exceptions.rs` until then,
// `src/syscall/mem.rs`) were exactly the "call sites outside pmm.rs" the old
// comment kept them out for, and both now reach `akuma_pmm` directly. The
// one-line dump (`dp_counters_line`) stayed in `src/pmm.rs` — it also reads
// `akuma_ext2`'s inode-deferred-free counters, which sit ABOVE this crate.

pub static DP_FILE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_ANON_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_COW_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static DP_PROTNONE_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static EAGER_MMAP_PAGES: AtomicUsize = AtomicUsize::new(0);
pub static USER_PAGES_FREED: AtomicUsize = AtomicUsize::new(0);

/// Instruction-abort faults whose lazy region records **non-executable** flags, so the
/// page maps non-exec and the fetch cannot succeed until the permission-fault arm
/// upgrades it to `RX`.
///
/// Not a frame counter (unlike its neighbours) — it counts *faults*, once per fault
/// rather than once per page, and it exists to answer a reachability question the tree
/// had never measured: the merged demand-paging body skips I-cache maintenance for
/// these pages, on the argument that the upgrade path does it instead
/// (`docs/archive/COW_PILE_AUDIT.md` §12.1). A non-zero value means that argument is
/// load-bearing in this workload; zero means the change is inert here.
pub static DP_IA_NOEXEC_FAULTS: AtomicUsize = AtomicUsize::new(0);

/// File-backed demand-page fills whose `read_at` **errored or came up short**.
///
/// Pass B fills into a frame from `alloc_pages_zeroed`, so an incomplete read leaves
/// the tail (or the whole page) as zeros. Both fill sites used to discard the
/// `Result<usize, _>` entirely, which made that indistinguishable from a file that
/// genuinely contains zeros — and the shareable arm then published the frame to
/// `file_page_cache` under `(inode, file_off)`, so one transient short read became a
/// *persistent, cross-process* zero page that every later mapper got as a cache hit
/// without touching the disk.
///
/// That is the mechanism behind `rustc` ICEing with
/// `Expected header tag [79, 68, 72, 84] but found [0, 0, 0, 0]` ("ODHT", the crate
/// metadata header) during the self-host build: one poisoned `.rlib` page, then every
/// concurrent `rustc` reading the same page of the same file.
///
/// **Non-zero here is always a defect.** A short read at this layer is not a normal
/// EOF condition — the range was already clamped to `filesz` by the caller.
pub static DP_FILE_FILL_SHORT: AtomicUsize = AtomicUsize::new(0);

/// `sys_munmap`'s whole-region arm found the region's **recorded** frame for a VA
/// disagreeing with the frame actually in the live PTE.
///
/// The record goes stale whenever something replaces a mapping without rewriting the
/// region's frame list: `complete_cow_break` installing a private copy,
/// `MADV_DONTNEED`'s share-break, a CoW write fault. Freeing the recorded frame then
/// releases a page this process no longer maps — while a peer may still hold it — and
/// simultaneously leaks the frame that *was* mapped.
///
/// Every premature free in the 2026-08-15 hunt attributed to `site=munmap-region`
/// (6/6). See `docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6.
pub static DP_MUNMAP_STALE_REGION_FRAME: AtomicUsize = AtomicUsize::new(0);

/// Faults on a lazy region whose flags say `PROT_NONE` but whose source is a **file**.
///
/// The `PROT_NONE` arm of the translation-fault path auto-commits a reservation with a
/// zeroed frame (Go's `sysReserve`/`sysMap` shape). That is right for an anonymous
/// reservation and catastrophic for a file-backed one: the process gets zeros where the
/// file's bytes belong, with no short read, no error, and no cache involvement — which
/// is exactly the residue left after `DP_FILE_FILL_SHORT` and `DP_FILE_CACHE_MISMATCH`
/// both came back clean across a reproducing build.
pub static DP_PROTNONE_FILE_REGION: AtomicUsize = AtomicUsize::new(0);

/// `file_page_cache` hits whose frame did **not** match the file on disk, counted only
/// when the fpcache verify-hits debug gate is on. Non-zero proves the cache is serving
/// wrong bytes and names the key; zero across a reproducing build clears the cache
/// entirely and moves the search downstream.
pub static DP_FILE_CACHE_MISMATCH: AtomicUsize = AtomicUsize::new(0);

/// Pages withheld from `file_page_cache` because [`DP_FILE_FILL_SHORT`] fired for them.
/// The gap between the two counters is the incomplete fills that were never eligible
/// to be published anyway (not fully covered by file data, or a private mapping).
pub static DP_FILE_FILL_UNPUBLISHED: AtomicUsize = AtomicUsize::new(0);

/// Bump a demand-paging counter by `n`.
#[inline]
pub fn dp_count(counter: &AtomicUsize, n: usize) {
    counter.fetch_add(n, Ordering::Relaxed);
}

// ============================================================================
// UAF hunt: free ledger — a ring of recent frees, named by thread
// ============================================================================

const FREE_LEDGER_SLOTS: usize = 4096;

static FREE_LEDGER_PA: [AtomicUsize; FREE_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; FREE_LEDGER_SLOTS];
/// `tid << 32 | seq` for the matching `FREE_LEDGER_PA` slot.
static FREE_LEDGER_META: [AtomicUsize; FREE_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; FREE_LEDGER_SLOTS];
static FREE_LEDGER_NEXT: AtomicUsize = AtomicUsize::new(0);

/// **Which code path returned a frame to the PMM.**
///
/// The ledger named the freeing *thread* and sequence number, which answers "when"
/// but not "who" — and on a premature free the thread is rarely the interesting
/// half, because the victim is usually a different address space entirely
/// (`docs/archive/SELFHOST_ZERO_PAGE_HUNT.md` §6). Every free site that can plausibly
/// release a frame another mapping still holds gets its own value here, so one boot
/// distinguishes `munmap` from a CoW break from a lost fault race instead of a
/// bisect per candidate.
///
/// `Unknown` is the default for sites that have not been tagged; a premature free
/// attributed to `Unknown` means the tag list is incomplete, not that the path is
/// exotic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum FreeSite {
    Unknown = 0,
    /// `munmap` / region teardown returning a mapping's frames.
    Munmap = 1,
    /// `sys_munmap`, whole-region arm.
    MunmapRegion = 11,
    /// `sys_munmap`, partial-shrink arm.
    MunmapPartial = 12,
    /// `sys_munmap`, unmapped-span sweep arm.
    MunmapSpan = 13,
    /// Whole-address-space teardown at process exit.
    AsTeardown = 2,
    /// `complete_cow_break` dropping the old shared frame.
    CowBreak = 3,
    /// Demand-paging install pass, frame lost the race for its VA.
    FaultRaceLost = 4,
    /// Demand-paging: readahead pool frames never consumed.
    FaultPoolSurplus = 5,
    /// `file_page_cache` eviction (over cap, or `shrink` under pressure).
    FpcacheEvict = 6,
    /// `file_page_cache::invalidate_inode` — the file was written/removed/renamed.
    FpcacheInvalidate = 7,
    /// `MADV_DONTNEED` share-break replacing a frame.
    MadviseDontneed = 8,
    /// `mremap` moving or shrinking a mapping.
    Mremap = 9,
    /// A surplus reference handed back at adoption time.
    SurplusRef = 10,
}

impl FreeSite {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Munmap => "munmap",
            Self::AsTeardown => "as-teardown",
            Self::CowBreak => "cow-break",
            Self::FaultRaceLost => "fault-race-lost",
            Self::FaultPoolSurplus => "fault-pool-surplus",
            Self::FpcacheEvict => "fpcache-evict",
            Self::FpcacheInvalidate => "fpcache-invalidate",
            Self::MadviseDontneed => "madv-dontneed",
            Self::Mremap => "mremap",
            Self::SurplusRef => "surplus-ref",
            Self::MunmapRegion => "munmap-region",
            Self::MunmapPartial => "munmap-partial",
            Self::MunmapSpan => "munmap-span",
        }
    }
    const fn from_bits(b: u8) -> Self {
        match b {
            1 => Self::Munmap,
            2 => Self::AsTeardown,
            3 => Self::CowBreak,
            4 => Self::FaultRaceLost,
            5 => Self::FaultPoolSurplus,
            6 => Self::FpcacheEvict,
            7 => Self::FpcacheInvalidate,
            8 => Self::MadviseDontneed,
            9 => Self::Mremap,
            10 => Self::SurplusRef,
            11 => Self::MunmapRegion,
            12 => Self::MunmapPartial,
            13 => Self::MunmapSpan,
            _ => Self::Unknown,
        }
    }
}

/// Note that `pa` was returned to the PMM by thread `tid`, from `site`. Lock-free
/// and IRQ-safe: two relaxed stores and a fetch_add.
///
/// `META` packs `site << 48 | tid << 32 | seq`. `tid` is bounded by `MAX_THREADS`
/// (≤ 256), so the 16 bits above it were dead space and the site rides for free —
/// no extra array, no extra store on the free path.
pub fn record_free_at(pa: usize, tid: u32, site: FreeSite) {
    let seq = FREE_LEDGER_NEXT.fetch_add(1, Ordering::Relaxed);
    let idx = seq & (FREE_LEDGER_SLOTS - 1);
    FREE_LEDGER_META[idx].store(
        ((site as usize) << 48) | ((tid as usize & 0xFFFF) << 32) | (seq & 0xFFFF_FFFF),
        Ordering::Relaxed,
    );
    FREE_LEDGER_PA[idx].store(pa, Ordering::Release);
}

/// Back-compat shim: record with no site attribution.
pub fn record_free(pa: usize, tid: u32) {
    record_free_at(pa, tid, FreeSite::Unknown);
}

/// Most recent ledger entry for `pa`, as `(tid, seq)` — `None` if this frame has
/// not been freed inside the ring's window.
#[must_use]
pub fn last_free_record(pa: usize) -> Option<(u32, u32)> {
    last_free_record_at(pa).map(|(tid, seq, _)| (tid, seq))
}

/// As [`last_free_record`], plus **which code path** did the freeing.
#[must_use]
pub fn last_free_record_at(pa: usize) -> Option<(u32, u32, FreeSite)> {
    let mut best: Option<(u32, u32, FreeSite)> = None;
    for i in 0..FREE_LEDGER_SLOTS {
        if FREE_LEDGER_PA[i].load(Ordering::Acquire) != pa {
            continue;
        }
        let meta = FREE_LEDGER_META[i].load(Ordering::Relaxed);
        let tid = ((meta >> 32) & 0xFFFF) as u32;
        let seq = (meta & 0xFFFF_FFFF) as u32;
        let site = FreeSite::from_bits(((meta >> 48) & 0xFF) as u8);
        if best.is_none_or(|(_, prev, _)| seq.wrapping_sub(prev) < u32::MAX / 2) {
            best = Some((tid, seq, site));
        }
    }
    best
}

/// Sequence number the next free will be stamped with.
#[must_use]
pub fn free_ledger_seq() -> u32 {
    (FREE_LEDGER_NEXT.load(Ordering::Relaxed) & 0xFFFF_FFFF) as u32
}

// ============================================================================
// CoW/share refcount EVENT ledger — NOT the refcount table itself (that is
// `COW_REFCOUNTS`, staying in `src/pmm.rs` until Step 3). This records every
// inc/dec event so an anomaly report can print a frame's whole reference
// history; the table it is recording events *about* is a separate concern.
// ============================================================================

static COW_EVER: Spinlock<Vec<u64>> = Spinlock::new(Vec::new());
static COW_EVER_BASE: AtomicUsize = AtomicUsize::new(0);

fn cow_ever_index(pa: usize, len_words: usize) -> Option<usize> {
    let base = COW_EVER_BASE.load(Ordering::Relaxed);
    let idx = pa.checked_sub(base)? / PAGE_SIZE;
    (idx / 64 < len_words).then_some(idx)
}

fn cow_ever_mark(pa: usize) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut bits = COW_EVER.lock();
        let Some(idx) = cow_ever_index(pa, bits.len()) else { return };
        bits[idx / 64] |= 1u64 << (idx % 64);
    });
}

/// Has `pa` ever taken part in a reference event? `None` when the instrument is
/// off.
#[must_use]
pub fn cow_ever_touched(pa: usize) -> Option<bool> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let bits = COW_EVER.lock();
        if bits.is_empty() {
            return None;
        }
        let Some(idx) = cow_ever_index(pa, bits.len()) else { return Some(false) };
        Some(bits[idx / 64] & (1u64 << (idx % 64)) != 0)
    })
}

const COW_LEDGER_SLOTS: usize = 4096;
static COW_LEDGER_PA: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
/// `tid << 32 | op << 24 | before << 12 | after` (op: 0 = inc, 1 = dec).
static COW_LEDGER_META: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
static COW_LEDGER_SEQ: [AtomicUsize; COW_LEDGER_SLOTS] =
    [const { AtomicUsize::new(0) }; COW_LEDGER_SLOTS];
static COW_LEDGER_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Called by `src/pmm.rs`'s still-local `cow_ref_inc`/`cow_ref_dec` after they
/// update `COW_REFCOUNTS`, so the event ledger stays populated across the Step
/// 2/3 boundary with zero behaviour change.
pub fn cow_ledger_record(pa: usize, is_dec: bool, before: u16, after: u16) {
    if !config().cow_ref_ledger {
        return;
    }
    cow_ever_mark(pa);
    let seq = COW_LEDGER_NEXT.fetch_add(1, Ordering::Relaxed);
    let idx = seq & (COW_LEDGER_SLOTS - 1);
    let tid = akuma_primitives::preempt::current_tid();
    let meta = (tid << 32)
        | (usize::from(is_dec) << 24)
        | (((before as usize) & 0xFFF) << 12)
        | ((after as usize) & 0xFFF);
    COW_LEDGER_SEQ[idx].store(seq, Ordering::Relaxed);
    COW_LEDGER_META[idx].store(meta, Ordering::Relaxed);
    COW_LEDGER_PA[idx].store(pa, Ordering::Release);
}

// No `#[cfg(kernel_tests)]` here: that cfg is emitted only by the kernel
// binary's own `build.rs` (`src/`), unreachable from this crate. The
// kernel-test-suite-only call site (`src/process_tests.rs` via
// `crate::pmm::cow_event_count`) is gated in `src/pmm.rs`'s thin wrapper
// instead; this stays always-compiled and relies on LTO to strip it from
// builds where that wrapper compiles out (extreme-size / `no-tests`).
pub fn cow_event_count(pa: usize) -> usize {
    if !config().cow_ref_ledger {
        return 0;
    }
    (0..COW_LEDGER_SLOTS)
        .filter(|&i| COW_LEDGER_PA[i].load(Ordering::Acquire) == pa)
        .count()
}

/// Print every recorded reference event for `pa`, oldest first (up to 12).
pub fn print_cow_history(pa: usize) {
    if !config().cow_ref_ledger {
        return;
    }
    let mut printed = 0usize;
    let mut last_seq: Option<usize> = None;
    while printed < 12 {
        let mut best: Option<(usize, usize)> = None;
        for i in 0..COW_LEDGER_SLOTS {
            if COW_LEDGER_PA[i].load(Ordering::Acquire) != pa {
                continue;
            }
            let seq = COW_LEDGER_SEQ[i].load(Ordering::Relaxed);
            if last_seq.is_some_and(|l| seq <= l) {
                continue;
            }
            if best.is_none_or(|(bs, _)| seq < bs) {
                best = Some((seq, i));
            }
        }
        let Some((seq, idx)) = best else { break };
        let meta = COW_LEDGER_META[idx].load(Ordering::Relaxed);
        akuma_primitives::safe_print!(160, "  [COW-HIST] pa={:#x} seq={} tid={} {} {}->{}\n",
            pa, seq, meta >> 32,
            if (meta >> 24) & 1 == 1 { "dec" } else { "inc" },
            (meta >> 12) & 0xFFF, meta & 0xFFF);
        last_seq = Some(seq);
        printed += 1;
    }
    if printed == 0 {
        let verdict = match cow_ever_touched(pa) {
            Some(false) => "NEVER shared (durable bitset clear)",
            Some(true) => "shared at some point (frame, not necessarily this owner)",
            None => "instrument off — says nothing",
        };
        akuma_primitives::safe_print!(160, "  [COW-HIST] pa={:#x} no events in window: {}\n", pa, verdict);
    }
}

/// Is the frame containing `pa` currently marked **free** in the PMM bitmap?
#[must_use]
pub fn is_page_free(pa: usize) -> bool {
    akuma_primitives::irq::with_irqs_disabled(|| PMM.lock().is_page_free_pa(pa))
}

// ============================================================================
// Copy-on-Write Reference Counting — the table itself. Landed here in Step 3
// (2026-08-14), its own isolated move so the tree's historically-buggiest
// accounting (the §5.6 underflow class — one reference per *address space*, not
// per VA, found in production, fixed three times, never unit-tested before this
// crate existed) gets individually verified rather than riding along with the
// allocator move.
//
// `COW_REFCOUNTS`' lock must stay a leaf: it takes nothing, and — since F1b
// (2026-08-14, `docs/archive/COW_PILE_AUDIT.md`) — `cow_ref_get` is called while
// `as_lock` is already held (the `TakingAsLock` re-validation in
// `src/exceptions.rs::complete_cow_break`), so nothing may be acquired inside
// this lock's hold either, or that ordering deadlocks.
// ============================================================================

static COW_REFCOUNTS: Spinlock<BTreeMap<usize, u16>> = Spinlock::new(BTreeMap::new());

/// Increment the CoW reference count for a physical address. First call for a
/// new address inserts it with count=2 (parent + child). Subsequent calls
/// increment by 1 (additional fork children).
pub fn cow_ref_inc(pa: usize) {
    let (created, before, after) = akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = COW_REFCOUNTS.lock();
        match table.get_mut(&pa) {
            Some(entry) => {
                let before = *entry;
                *entry = entry.saturating_add(1);
                (false, before, *entry)
            }
            // First share: parent + child. (Formerly `or_insert(1)` + add, which
            // made a created entry indistinguishable from an existing count of 1
            // — the resurrection detector below needs the distinction.)
            None => {
                table.insert(pa, 2);
                (true, 0, 2)
            }
        }
    });
    // Resurrection detector: an inc that CREATED the entry on a
    // frame currently parked in quarantine means the caller took a reference to
    // a frame whose last reference was already dropped — the freed+poisoned
    // frame is about to be handed out as live data. This is the premature-free
    // class the quarantine itself cannot see (the victim *reads*, never
    // writes). One relaxed atomic load, and only on the inc-from-zero path, so
    // it cannot perturb the race the way `PMM_PREMATURE_FREE_CHECK` does.
    // A frame that already LEFT quarantine and was reallocated is legitimately
    // re-shareable, which is why this checks the parked set and not the ledger.
    if created
        && config().pmm_uaf_quarantine
        && QUAR_PRESENT[quar_slot(pa)].load(Ordering::Acquire) == pa
    {
        let n = COW_RESURRECTIONS.fetch_add(1, Ordering::Relaxed);
        if n < 32 {
            let (tid_freed, seq_freed, site) = last_free_record_at(pa)
                .unwrap_or((u32::MAX, 0, FreeSite::Unknown));
            akuma_primitives::safe_print!(224,
                "[PMM-RESURRECT] pa={:#x} cow_ref_inc on a QUARANTINED frame — the inc \
                 raced the last free. freed_by=(tid={} seq={} site={}) now_seq={}\n",
                pa, tid_freed, seq_freed, site.name(), free_ledger_seq());
            print_cow_history(pa);
        }
    }
    // Recorded outside the `COW_REFCOUNTS` hold: the ledger takes no lock, but
    // keeping it out keeps the hot path's critical section exactly as short as
    // it was before the instrument existed.
    cow_ledger_record(pa, false, before, after);
}

/// Count of [`cow_ref_inc`] calls that took a reference to a frame still parked
/// in the UAF quarantine — each one is a caught frame resurrection (a
/// use-after-free averted or about to happen). Must stay 0.
static COW_RESURRECTIONS: AtomicUsize = AtomicUsize::new(0);

#[must_use]
pub fn cow_resurrection_count() -> usize {
    COW_RESURRECTIONS.load(Ordering::Relaxed)
}

/// Decrement the CoW reference count. Returns true if the count reached 0
/// (meaning the caller should free the physical frame). Removes the entry from
/// the table when count reaches 0 to avoid unbounded growth.
pub fn cow_ref_dec(pa: usize) -> bool {
    let (before, after, last) = akuma_primitives::irq::with_irqs_disabled(|| {
        let mut table = COW_REFCOUNTS.lock();
        match table.get_mut(&pa) {
            Some(count) => {
                let before = *count;
                *count = count.saturating_sub(1);
                if *count == 0 {
                    table.remove(&pa);
                    (before, 0, true)
                } else {
                    (before, *count, false)
                }
            }
            // Not tracked -> single owner -> safe to free. Recorded as `0->0`
            // so an untracked decrement is still visible in the history: a run
            // of these on a frame that *should* be shared is itself the
            // desync.
            None => (0, 0, true),
        }
    });
    cow_ledger_record(pa, true, before, after);
    last
}

/// Get the current CoW reference count for a physical address. Returns 0 if
/// the address is not in the CoW table (not shared).
#[must_use]
pub fn cow_ref_get(pa: usize) -> u16 {
    akuma_primitives::irq::with_irqs_disabled(|| {
        COW_REFCOUNTS.lock().get(&pa).copied().unwrap_or(0)
    })
}

/// Number of entries in the CoW refcount table (for diagnostics).
#[must_use]
pub fn cow_ref_count() -> usize {
    akuma_primitives::irq::with_irqs_disabled(|| COW_REFCOUNTS.lock().len())
}

// Step 7 (`docs/archive/PMM_EXTRACT.md` §7): host tests for `COW_REFCOUNTS` —
// "the tree's historically-buggiest accounting" per that plan's §6, never
// unit-tested before this crate existed (the §5.6 underflow class was found in
// production, fixed three times, and only ever covered by boot tests).
// `COW_REFCOUNTS` is a single global map, shared with every other test in this
// binary, but that is safe here: each test below uses its own PA, never
// reused by another test, so concurrent access under `cargo test`'s default
// parallelism can only interleave at the `Spinlock`, never corrupt a result.
// `cow_ref_inc`/`dec` both route through `cow_ledger_record`, which reads
// `config()` unconditionally — hence `ensure_pmm()`.
#[cfg(test)]
mod cow_refcount_tests {
    use super::*;
    use crate::test_arena::ensure_pmm;

    #[test]
    fn first_inc_sets_count_to_two_not_one() {
        ensure_pmm();
        const PA: usize = 0xCAFE_1000;
        assert_eq!(cow_ref_get(PA), 0, "untouched PA must read as not-shared");
        cow_ref_inc(PA);
        assert_eq!(
            cow_ref_get(PA),
            2,
            "the first share is parent+child — one inc must mean two owners, not one"
        );
    }

    #[test]
    fn a_third_owner_adds_one_more() {
        ensure_pmm();
        const PA: usize = 0xCAFE_2000;
        cow_ref_inc(PA); // 2 (parent + child)
        cow_ref_inc(PA); // a second fork child
        assert_eq!(cow_ref_get(PA), 3);
    }

    #[test]
    fn dec_to_zero_removes_the_entry_and_reports_the_last_owner() {
        ensure_pmm();
        const PA: usize = 0xCAFE_3000;
        cow_ref_inc(PA); // 2
        assert!(!cow_ref_dec(PA), "2 -> 1 must not claim the last reference");
        assert_eq!(cow_ref_get(PA), 1);
        assert!(cow_ref_dec(PA), "1 -> 0 must claim the last reference");
        assert_eq!(
            cow_ref_get(PA),
            0,
            "a fully-dropped entry must read as untracked, not linger at 0"
        );
    }

    /// Not tracked at all -> single owner -> the caller may free unconditionally.
    /// This is the path a frame that was never shared takes on every free.
    #[test]
    fn dec_of_a_never_shared_pa_is_a_safe_single_owner_free() {
        ensure_pmm();
        const PA: usize = 0xCAFE_4000;
        assert_eq!(cow_ref_get(PA), 0);
        assert!(cow_ref_dec(PA), "an untracked PA must decode as \"last owner\"");
        assert_eq!(cow_ref_get(PA), 0, "must not have inserted an entry for it");
    }

    /// The bug class this table exists to catch: a caller that increments once
    /// per VA mapping the frame (instead of once per address space) inflates the
    /// count, and the frame then outlives every real owner. This test pins the
    /// correct arithmetic — one inc per *fork*, not per mapping — so a
    /// regression back to the old per-VA accounting fails here first.
    #[test]
    fn refcount_matches_the_number_of_address_spaces_not_mappings() {
        ensure_pmm();
        const PA: usize = 0xCAFE_5000;
        // Three address spaces share this frame after two forks.
        cow_ref_inc(PA); // fork 1: parent + child == 2
        cow_ref_inc(PA); // fork 2: + grandchild == 3
        assert_eq!(cow_ref_get(PA), 3, "3 address spaces, not double-counted per VA");
        // Each address space's exit decs once, regardless of how many VAs in
        // that address space mapped the frame (`free_page` calls `cow_ref_dec`
        // once per frame, not once per VA — that dedup happens in the caller).
        assert!(!cow_ref_dec(PA));
        assert!(!cow_ref_dec(PA));
        assert!(cow_ref_dec(PA), "the third exit must be the last owner");
    }
}

// ============================================================================
// UAF hunt: poison quarantine
// ============================================================================

const QUARANTINE_SLOTS: usize = 512;

/// Poison base; XORed with the PA so a frame written with *another* frame's
/// poison (a stale copy, a mis-targeted memset) is still a mismatch. Migrated
/// here for real in Step 6 (`docs/archive/PMM_EXTRACT.md` §7) from
/// `akuma_exec::memmath`, where it lived only because this crate didn't exist
/// yet — see the module doc's "Extraction status".
pub const POISON_MAGIC: u64 = 0xFEED_FACE_DEAD_0000;

/// The poison word a quarantined frame is filled with.
#[must_use]
#[inline]
pub fn poison_word(pa: usize) -> u64 {
    POISON_MAGIC ^ (pa as u64)
}

/// Pure decode: if `word` is a poison word for a page-aligned frame inside
/// `[ram_base, ram_end)`, that frame's PA.
///
/// [`poison_word`] XORs the magic with the frame's own PA precisely so a stray
/// word can be traced back to its frame, and this is the reverse. The check
/// that makes it trustworthy is **page alignment**: an arbitrary 64-bit value
/// that happens to carry the `0xFEEDFACE` prefix still has to XOR down to a
/// 4 KiB aligned, in-range PA — a 1-in-4096 accident on top of a 1-in-2^32 one.
///
/// The window is a parameter, not read live, so this stays a leaf-crate pure
/// function: `src/pmm.rs`'s `report_poison_value`/(the former
/// `memmath::poison_word_frame`) supplies the live `mmu::ram_base()`/`ram_end()`
/// window — that wrapper needs akuma-exec's `mmu` and stays behind in `src/`,
/// same reasoning as `report_poison_value` itself (`docs/archive/PMM_EXTRACT.md`
/// §7 Step 6): one caller, not worth threading RAM bounds through a hook for.
#[must_use]
pub fn poison_decode(word: u64, ram_base: usize, ram_end: usize) -> Option<usize> {
    // Cheap reject first: everything else here is only reached for a word that
    // already carries the magic's high half.
    if word >> 32 != POISON_MAGIC >> 32 {
        return None;
    }
    let pa = (word ^ POISON_MAGIC) as usize;
    if pa & (PAGE_SIZE - 1) != 0 {
        return None;
    }
    if pa < ram_base || pa >= ram_end {
        return None;
    }
    Some(pa)
}

#[cfg(test)]
mod poison_codec_tests {
    //! Ported verbatim from `akuma_exec::memmath::tests` (Step 6). Not ported:
    //! `gated_decode_uses_the_live_ram_window`, which tested the gated
    //! *live-bounds* wrapper (`poison_word_frame`) — that wrapper needs
    //! `akuma_exec::mmu::ram_base`/`ram_end` and stays in `src/pmm.rs`,
    //! unreachable from a host test the same way `report_poison_value` already
    //! was. The pure decode this module tests is exactly what backed that
    //! wrapper, so the coverage loss is one thin, single-caller shim, not the
    //! codec itself.
    use super::*;

    #[test]
    fn poison_round_trips_through_its_own_frame() {
        let pa = 0x767d_e000usize;
        let w = poison_word(pa);
        assert_eq!(w, 0xfeed_face_a8d0_e000, "the observed crash's poison word");
        assert_eq!(poison_decode(w, 0x4000_0000, 0x8000_0000), Some(pa));
    }

    /// The value from the null-`Rc` autopsy: a poisoned pointer *dereferenced at
    /// an offset*. It must NOT decode — only the undisplaced word does, which is
    /// why the fault path probes every base register rather than FAR alone.
    #[test]
    fn displaced_poison_pointer_does_not_decode() {
        let observed = 0xfeed_face_a8d0_e010u64;
        assert_eq!(
            poison_decode(observed, 0x4000_0000, 0x8000_0000),
            None,
            "+0x10 is not page-aligned, so it must be rejected"
        );
    }

    #[test]
    fn non_poison_words_are_rejected() {
        // Wrong magic half.
        assert_eq!(poison_decode(0, 0x4000_0000, 0x8000_0000), None);
        assert_eq!(poison_decode(u64::MAX, 0x4000_0000, 0x8000_0000), None);
        // Right magic, but decodes outside the RAM window.
        assert_eq!(poison_decode(poison_word(0x1000), 0x4000_0000, 0x8000_0000), None);
        assert_eq!(poison_decode(poison_word(0x9000_0000), 0x4000_0000, 0x8000_0000), None);
    }

    /// `ram_base` is inclusive and `ram_end` exclusive — an off-by-one here either
    /// drops the last frame's diagnostics or accepts an out-of-range PA.
    #[test]
    fn ram_window_bounds_are_half_open() {
        let (base, end) = (0x4000_0000usize, 0x8000_0000usize);
        assert_eq!(poison_decode(poison_word(base), base, end), Some(base));
        assert_eq!(poison_decode(poison_word(end), base, end), None);
        let last = end - PAGE_SIZE;
        assert_eq!(poison_decode(poison_word(last), base, end), Some(last));
    }
}

struct Quarantine {
    pa: [usize; QUARANTINE_SLOTS],
    head: usize,
    len: usize,
}

static QUARANTINE: Spinlock<Quarantine> =
    Spinlock::new(Quarantine { pa: [0; QUARANTINE_SLOTS], head: 0, len: 0 });

static UAF_DETECTED: AtomicUsize = AtomicUsize::new(0);

const QUAR_PRESENT_SLOTS: usize = 2048;
static QUAR_PRESENT: [AtomicUsize; QUAR_PRESENT_SLOTS] =
    [const { AtomicUsize::new(0) }; QUAR_PRESENT_SLOTS];

#[inline]
fn quar_slot(pa: usize) -> usize {
    (pa >> 12) & (QUAR_PRESENT_SLOTS - 1)
}


// ============================================================================
// Raw frame access — every `unsafe` in this crate, in one module.
// ============================================================================

/// Writing to a physical frame this crate has just allocated.
///
/// # Why this module exists
///
/// These three functions are **the entire `unsafe` surface of `akuma-pmm`**. They
/// were five sites before 2026-08-30: three byte-identical copies of a
/// zero-then-clean loop inlined into `alloc_page_zeroed` and its contiguous and
/// multi-page siblings, plus a hand-rolled pointer loop in each of
/// `poison_page`/`verify_poison`. Copies of an `unsafe` block are the worst kind
/// to have — they drift, and each one re-argues the same safety case slightly
/// differently.
///
/// # Why these are safe `fn`s
///
/// Writing zeros to an arbitrary physical address is emphatically *not* safe, so
/// nothing here may become `pub`. What makes them safe is a **crate invariant**,
/// not an argument about the operation: every caller has just taken `pa` from
/// this crate's own bitmap allocator and holds it exclusively, or — in
/// `poison_page`'s case — is putting a frame the allocator has already reclaimed
/// into quarantine.
///
/// **The module is private, and that is what discharges the obligation.** The
/// functions inside are `pub` only so `redundant_pub_crate` is satisfied; `mod
/// frame` has no `pub`, so nothing outside this crate can name them. Exporting
/// any of them would be a lie — an external caller holds no such invariant, and
/// `zero_and_clean` on an arbitrary `pa` is memory corruption with a safe
/// signature. If one ever needs to escape this crate, it escapes as an
/// `unsafe fn`, not as-is.
///
/// This is the same shape as `akuma_net_nic`'s `FrameArena`: own the storage,
/// hand out safe operations on it, keep the raw access in one place.
///
/// # What is NOT here
///
/// The cache and barrier instructions. `dc cvac` and `dsb ish` moved to
/// `akuma_cpu`, where they are safe by their own argument — executing a barrier
/// cannot violate memory safety. This crate now contains no `asm!` at all.
mod frame {
    use super::{PAGE_SIZE, phys_to_virt};

    /// Clean `pages` frames' worth of data cache starting at `pa`, then `dsb`.
    ///
    /// Steps by the **runtime** `CTR_EL0` line size rather than a hardcoded 64.
    /// That is a deliberate fix, not a mechanical move: all three copies this
    /// replaced hardcoded `CACHE_LINE_SIZE = 64`, which silently *under-cleans*
    /// on any core whose `DminLine` is smaller than 64 bytes — the loop would
    /// skip lines. Stepping by `DminLine` (the architectural minimum across the
    /// D-cache levels) can only ever over-clean, which is slower and correct.
    fn clean_range(pa: usize, pages: usize) {
        let step = akuma_cpu::cache::line_size();
        let start = phys_to_virt(pa) as usize;
        let end = start + pages * PAGE_SIZE;
        let mut addr = start;
        while addr < end {
            akuma_cpu::cache::dc_cvac(addr);
            addr += step;
        }
        akuma_cpu::barrier::dsb_ish();
    }

    /// Zero `pages` frames starting at `pa` and clean them to the point of
    /// coherency, so a device or another core reading the frame without going
    /// through this core's cache sees the zeros.
    pub fn zero_and_clean(pa: usize, pages: usize) {
        // SAFETY: `pa` was just returned by this crate's allocator, so the
        // `pages` frames from it are contiguous, mapped through the kernel's
        // physical window (`phys_to_virt`), and owned exclusively by this caller
        // — nothing else can hold a reference to a frame the bitmap has not
        // handed out. The write is `pages * PAGE_SIZE` bytes, exactly the extent
        // that was allocated.
        unsafe {
            core::ptr::write_bytes(phys_to_virt(pa), 0, pages * PAGE_SIZE);
        }
        clean_range(pa, pages);
    }

    /// Fill one frame with `word`, for the UAF quarantine.
    ///
    /// **Volatile, and it must stay volatile.** Nothing in this crate reads the
    /// poison back in the same compilation unit as the write — `verify_poison`
    /// runs on a later drain — so a plain store to a frame the compiler believes
    /// is dead is exactly the kind LLVM is entitled to remove. Eliding it would
    /// not fail loudly; it would make the quarantine silently stop detecting
    /// use-after-free, which is the one thing it exists to do.
    pub fn poison_fill(pa: usize, word: u64) {
        // SAFETY: `pa` is a frame the allocator has reclaimed and parked in the
        // quarantine ring, so no live mapping should reference it — writing the
        // poison is how a stale writer is caught. It is a real, mapped physical
        // frame reached through the kernel window, and the loop covers exactly
        // one page.
        unsafe {
            let p = phys_to_virt(pa).cast::<u64>();
            for i in 0..(PAGE_SIZE / 8) {
                p.add(i).write_volatile(word);
            }
        }
    }

    /// Check one frame's poison. Returns `(byte offset, value found)` at the
    /// first word that is not `want`, or `None` if the whole frame is intact.
    pub fn poison_check(pa: usize, want: u64) -> Option<(usize, u64)> {
        // SAFETY: as `poison_fill` — a quarantined frame, read back over exactly
        // the page that was written.
        unsafe {
            let p = phys_to_virt(pa).cast::<u64>();
            for i in 0..(PAGE_SIZE / 8) {
                let got = p.add(i).read_volatile();
                if got != want {
                    return Some((i * 8, got));
                }
            }
        }
        None
    }
}

fn poison_page(pa: usize) {
    frame::poison_fill(pa, poison_word(pa));
}

fn verify_poison(pa: usize) -> Option<(usize, u64)> {
    frame::poison_check(pa, poison_word(pa))
}

static PREMATURE_FREES: AtomicUsize = AtomicUsize::new(0);

#[must_use]
pub fn premature_free_count() -> usize {
    PREMATURE_FREES.load(Ordering::Relaxed)
}

/// Report a frame that was freed while a live address space still mapped it.
/// `tid`: the thread that performed the free (the caller's `current_thread_id`,
/// threaded through explicitly rather than read here — see `free_page`).
fn report_premature_free(pa: usize, tid: u32) {
    let Some((pid, tgid)) = surviving_mapper(pa) else { return };
    let n = PREMATURE_FREES.fetch_add(1, Ordering::Relaxed);
    if n >= 64 {
        return;
    }
    akuma_primitives::safe_print!(255,
        "[PMM-PREMATURE] pa={:#x} freed by tid={} while still mapped by pid={} tgid={} \
         cow_ref={} seq={}\n",
        pa, tid, pid, tgid, cow_ref_get(pa), free_ledger_seq());
    print_cow_history(pa);
}

/// Verify a frame leaving quarantine and hand it back to the bitmap.
fn release_from_quarantine(pa: usize) {
    if let Some((off, got)) = verify_poison(pa) {
        UAF_DETECTED.fetch_add(1, Ordering::Relaxed);
        let (tid_freed, seq_freed) = last_free_record(pa).unwrap_or((u32::MAX, 0));
        akuma_primitives::safe_print!(255,
            "[PMM-UAF] pa={:#x} WRITTEN AFTER FREE: off={:#x} got={:#x} want={:#x} \
             freed_by=(tid={} seq={}) cow_ref={}\n",
            pa, off, got, poison_word(pa), tid_freed, seq_freed, cow_ref_get(pa));
        if let Some((pid, tgid)) = surviving_mapper(pa) {
            akuma_primitives::safe_print!(160,
                "  [PMM-UAF] pa={:#x} STILL MAPPED BY pid={} tgid={}\n", pa, pid, tgid);
        }
        print_cow_history(pa);
    }
    QUAR_PRESENT[quar_slot(pa)].compare_exchange(pa, 0, Ordering::AcqRel, Ordering::Relaxed).ok();

    let outcome = akuma_primitives::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        if pmm.is_page_free_pa(pa) {
            return FreeOutcome::DoubleFree;
        }
        pmm.free_page(pa)
    });
    match outcome {
        FreeOutcome::Freed => {
            ALLOCATED_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
        FreeOutcome::DoubleFree => { DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed); }
        FreeOutcome::OutOfRange => {}
    }
}

/// Poison `pa` and park it. Returns the frame displaced from the ring's tail, if
/// the ring was full, for the caller to verify and release **outside** the lock.
fn quarantine_push(pa: usize) -> Option<usize> {
    if QUAR_PRESENT[quar_slot(pa)].load(Ordering::Acquire) == pa {
        DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed);
        let (tid_freed, seq_freed) = last_free_record(pa).unwrap_or((u32::MAX, 0));
        akuma_primitives::safe_print!(192,
            "[PMM-QUAR-DF] pa={:#x} freed twice while quarantined, prev freed_by=(tid={} seq={})\n",
            pa, tid_freed, seq_freed);
        return None;
    }
    poison_page(pa);
    QUAR_PRESENT[quar_slot(pa)].store(pa, Ordering::Release);
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut q = QUARANTINE.lock();
        if q.len < QUARANTINE_SLOTS {
            let idx = (q.head + q.len) % QUARANTINE_SLOTS;
            q.pa[idx] = pa;
            q.len += 1;
            None
        } else {
            let head = q.head;
            let evicted = q.pa[head];
            q.pa[head] = pa;
            q.head = (head + 1) % QUARANTINE_SLOTS;
            Some(evicted)
        }
    })
}

/// Empty the quarantine, verifying every frame on the way out. Returns the
/// number of frames released.
pub fn quarantine_drain_all() -> usize {
    let mut released = 0usize;
    loop {
        let pa = akuma_primitives::irq::with_irqs_disabled(|| {
            let mut q = QUARANTINE.lock();
            if q.len == 0 {
                return None;
            }
            let pa = q.pa[q.head];
            q.head = (q.head + 1) % QUARANTINE_SLOTS;
            q.len -= 1;
            Some(pa)
        });
        match pa {
            Some(pa) => { release_from_quarantine(pa); released += 1; }
            None => break,
        }
    }
    released
}

// Always-compiled; see `cow_event_count`'s comment above for why.
#[doc(hidden)]
pub fn discount_uaf_detections(n: usize) {
    UAF_DETECTED.fetch_sub(n, Ordering::Relaxed);
}

#[must_use]
pub fn quarantine_stats() -> (usize, usize) {
    let len = akuma_primitives::irq::with_irqs_disabled(|| QUARANTINE.lock().len);
    (len, UAF_DETECTED.load(Ordering::Relaxed))
}

// ============================================================================
// Core alloc/free API
// ============================================================================

pub fn init(ram_base: usize, ram_size: usize, kernel_end: usize) {
    let mut pmm = PMM.lock();
    pmm.init(ram_base, ram_size, kernel_end);

    TOTAL_PAGES.store(pmm.total_pages, Ordering::Release);
    ALLOCATED_PAGES.store(pmm.total_pages - pmm.free_pages, Ordering::Release);
    MANAGED_BASE.store(pmm.base_addr, Ordering::Release);
    MANAGED_END.store(pmm.base_addr + pmm.total_pages * PAGE_SIZE, Ordering::Release);

    if config().cow_ref_ledger {
        let words = pmm.total_pages.div_ceil(64);
        COW_EVER_BASE.store(pmm.base_addr, Ordering::Release);
        *COW_EVER.lock() = alloc::vec![0u64; words];
    }
}

pub fn alloc_page() -> Option<usize> {
    if let Some(pa) = alloc_page_once() {
        return Some(pa);
    }
    if config().pmm_uaf_quarantine
        && quarantine_drain_all() > 0
        && let Some(pa) = alloc_page_once()
    {
        return Some(pa);
    }
    if (hooks().heap_reclaim)() > 0 {
        alloc_page_once()
    } else {
        None
    }
}

fn alloc_page_once() -> Option<usize> {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        let result = pmm.alloc_page();
        if result.is_some() {
            ALLOCATED_PAGES.fetch_add(1, Ordering::Relaxed);
        }
        result
    })
}

/// Free a single physical page. If the frame is CoW-shared (refcount > 0), only
/// decrements the refcount instead of actually freeing — the physical frame is
/// freed when the last reference is dropped. `tid`: the caller's
/// `current_thread_id`, threaded through explicitly (this crate does not
/// depend on `akuma_exec::threading`).
pub fn free_page(pa: usize, tid: u32) {
    free_page_at(pa, tid, FreeSite::Unknown);
}

/// As [`free_page`], but records **which code path** released the frame, so a
/// premature-free report can name the culprit instead of only the thread.
/// See [`FreeSite`].
pub fn free_page_at(pa: usize, tid: u32, site: FreeSite) {
    if !cow_ref_dec(pa) {
        // Still shared by other processes — don't free the physical page.
        return;
    }

    // Untrack BEFORE freeing to prevent race condition: if we free first then
    // untrack, another CPU could reallocate the same frame and track it before
    // we untrack, causing us to remove their tracking.
    untrack_frame(pa);
    record_free_at(pa, tid, site);

    if config().pmm_premature_free_check {
        report_premature_free(pa, tid);
    }

    if config().pmm_uaf_quarantine {
        if let Some(evicted) = quarantine_push(pa) {
            release_from_quarantine(evicted);
        }
        return;
    }

    let outcome = akuma_primitives::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        pmm.free_page(pa)
    });
    match outcome {
        FreeOutcome::Freed => {
            ALLOCATED_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
        FreeOutcome::DoubleFree => { DOUBLE_FREE_COUNT.fetch_add(1, Ordering::Relaxed); }
        FreeOutcome::OutOfRange => {}
    }
}

#[must_use]
pub fn double_free_count() -> usize {
    DOUBLE_FREE_COUNT.load(Ordering::Relaxed)
}

// Always-compiled; see `cow_event_count`'s comment above for why.
#[doc(hidden)]
pub fn discount_double_frees(n: usize) {
    DOUBLE_FREE_COUNT.fetch_sub(n, Ordering::Relaxed);
}

pub fn alloc_pages_contiguous_zeroed(count: usize) -> Option<usize> {
    let alloc_once = || akuma_primitives::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        let result = pmm.alloc_pages_contiguous(count)?;
        ALLOCATED_PAGES.fetch_add(count, Ordering::Relaxed);
        Some(result)
    });

    let pa = match alloc_once() {
        Some(pa) => pa,
        None => {
            if (hooks().heap_reclaim)() > 0 {
                alloc_once()?
            } else {
                return None;
            }
        }
    };

    frame::zero_and_clean(pa, count);
    Some(pa)
}

pub fn free_pages_contiguous(pa: usize, count: usize) {
    akuma_primitives::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        pmm.free_pages_contiguous(pa, count);
        ALLOCATED_PAGES.fetch_sub(count, Ordering::Relaxed);
    });
}

#[must_use]
pub fn stats() -> (usize, usize, usize) {
    let total = TOTAL_PAGES.load(Ordering::Relaxed);
    let allocated = ALLOCATED_PAGES.load(Ordering::Relaxed);
    let free = total.saturating_sub(allocated);
    (total, allocated, free)
}

#[must_use]
pub fn free_count() -> usize {
    let (total, allocated, _) = stats();
    total.saturating_sub(allocated)
}

/// Physical bounds of the RAM this PMM manages, as `[base, end)`.
///
/// `(0, 0)` before [`init`], which makes [`contains`] answer `false` for
/// everything rather than accidentally vouching for an address during boot.
static MANAGED_BASE: AtomicUsize = AtomicUsize::new(0);
static MANAGED_END: AtomicUsize = AtomicUsize::new(0);

/// Physical address range this PMM manages, as `[base, end)`.
#[must_use]
pub fn managed_range() -> (usize, usize) {
    (
        MANAGED_BASE.load(Ordering::Acquire),
        MANAGED_END.load(Ordering::Acquire),
    )
}

/// Does `[pa, pa + len)` lie entirely inside the RAM this PMM manages?
///
/// The bounds check behind the safe physical-memory copies in `akuma-mmu`: it is
/// what lets a caller pass an arbitrary `usize` without being able to reach
/// device MMIO, the kernel image, or nothing at all. Says `false` before
/// [`init`], and on any wrap.
#[must_use]
pub fn contains(pa: usize, len: usize) -> bool {
    let (base, end) = managed_range();
    if base == 0 && end == 0 {
        return false;
    }
    match pa.checked_add(len) {
        Some(last) => pa >= base && last <= end,
        None => false,
    }
}

#[must_use]
pub fn total_count() -> usize {
    TOTAL_PAGES.load(Ordering::Relaxed)
}

pub fn alloc_page_zeroed() -> Option<usize> {
    let pa = alloc_page()?;
    frame::zero_and_clean(pa, 1);
    Some(pa)
}

/// Allocate multiple zeroed pages in a single lock acquisition. Returns `None`
/// (without partial allocation) if `count` pages aren't available.
// ============================================================================
// The user-page reclaim escalation — Step 4. The four hooks above are the
// effects; this is the loop that walks them under pressure, moved in from
// `src/pmm.rs::alloc_page_zeroed_user` so `free_count`/`alloc_page_zeroed`
// become plain in-crate calls and only the cold collaborators
// (`PmmHooks`) stay indirect (`docs/archive/PMM_EXTRACT.md` §4).
// ============================================================================

// ============================================================================
// The user-page reserve. Migrated here for real in Step 6
// (`docs/archive/PMM_EXTRACT.md` §7) from `akuma_exec::memmath`, where it
// lived only because this crate didn't exist yet.
// ============================================================================

/// Pages held back from *user* demand-paging so kernel-critical work can always
/// make progress when a process tries to consume all of RAM: the page tables to
/// complete an in-flight fault, kernel-heap growth, and the OOM process-kill path
/// itself. Without this, a memory-hungry process drains the PMM to near-zero
/// and the kernel's *own* next allocation fails — and a failed kernel
/// allocation aborts the whole kernel instead of the offending process being
/// killed. 16 pages = 64 KB: small enough not to raise the working floor,
/// large enough for one minimal heap-growth + the kill path's bookkeeping.
pub const USER_PAGE_RESERVE: usize = 16;

/// Reserve predicate: would handing a page to *user* demand-paging starve the
/// kernel reserve?
///
/// Denies **at** the reserve, not merely below it — the reserve is the floor the
/// kernel keeps for itself, so handing out the last reserved page defeats it.
#[must_use]
#[inline]
pub fn user_alloc_would_starve(free: usize) -> bool {
    free <= USER_PAGE_RESERVE
}

/// Max pages a *user* readahead batch may take right now without driving free
/// PMM below [`USER_PAGE_RESERVE`].
///
/// Saturating, so at or below the reserve the budget is 0 and the caller falls
/// through to its single-page path rather than wrapping to an enormous batch.
#[must_use]
#[inline]
pub fn user_readahead_budget(free: usize) -> usize {
    free.saturating_sub(USER_PAGE_RESERVE)
}

#[cfg(test)]
mod reserve_tests {
    //! Ported verbatim from `akuma_exec::memmath::tests` (Step 6).
    use super::*;

    #[test]
    fn reserve_denies_at_and_below_itself_and_allows_one_page_above() {
        assert!(user_alloc_would_starve(0), "0 free pages must deny");
        assert!(
            user_alloc_would_starve(USER_PAGE_RESERVE),
            "must deny at exactly the reserve — the floor is kept, not spent"
        );
        assert!(!user_alloc_would_starve(USER_PAGE_RESERVE + 1));
    }

    #[test]
    fn readahead_budget_is_free_minus_reserve_and_saturates() {
        assert_eq!(user_readahead_budget(0), 0);
        assert_eq!(user_readahead_budget(USER_PAGE_RESERVE), 0);
        assert_eq!(user_readahead_budget(USER_PAGE_RESERVE + 5), 5);
        // Saturating: never wraps into an enormous batch near the floor.
        assert_eq!(user_readahead_budget(USER_PAGE_RESERVE.saturating_sub(1)), 0);
    }

    /// The budget must be 0 for every free count the allocator would refuse, or a
    /// readahead batch could be sized past a floor the single-page path enforces.
    #[test]
    fn budget_is_zero_exactly_when_alloc_would_starve() {
        for free in 0..=(USER_PAGE_RESERVE + 3) {
            assert_eq!(
                user_readahead_budget(free) == 0,
                user_alloc_would_starve(free),
                "predicate and budget disagree at free={free}"
            );
        }
    }
}

/// The user-page reclaim escalation `alloc_page_zeroed_user` walks when free
/// PMM has fallen to [`USER_PAGE_RESERVE`]. Migrated here for real in Step 6,
/// alongside the reserve above — this used to be a temporary duplicate (Step
/// 4) of `akuma_exec::memmath::{ReclaimStep, next_reclaim_step}`, which has
/// now been deleted from `memmath`.
mod reclaim_escalation {
    use super::user_alloc_would_starve;

    /// One rung of the escalation, plus the two terminal answers. Ordered
    /// **cheapest recovery first** — see `docs/reference/subsystems/memory.md`
    /// → "OOM decision map" for the full reasoning behind the order.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ReclaimStep {
        /// Enough free pages: hand one out. The only non-failing exit.
        Allocate,
        /// `PmmHooks::heap_reclaim`.
        ReclaimHeap,
        /// `PmmHooks::drain_retired`.
        DrainRetired,
        /// `PmmHooks::evict_clean_file_pages`.
        EvictCleanFilePages,
        /// `PmmHooks::shrink_page_cache`.
        ShrinkPageCache,
        /// Out of options: the caller returns `None` and its caller SIGSEGVs the
        /// faulting process.
        GiveUp,
    }

    /// The pure decision behind `alloc_page_zeroed_user`'s reclaim escalation:
    /// given the current free-page count and the rung already performed, what
    /// to do next. See `akuma_exec::memmath::next_reclaim_step`'s former doc
    /// comment (preserved in `docs/archive/PMM_EXTRACT.md`'s history) for the
    /// full reasoning — re-checking pressure before consulting `done` is what
    /// makes every rung's progress count.
    #[must_use]
    pub fn next_reclaim_step(free: usize, done: Option<ReclaimStep>) -> ReclaimStep {
        if !user_alloc_would_starve(free) {
            return ReclaimStep::Allocate;
        }
        match done {
            None => ReclaimStep::ReclaimHeap,
            Some(ReclaimStep::ReclaimHeap) => ReclaimStep::DrainRetired,
            Some(ReclaimStep::DrainRetired) => ReclaimStep::EvictCleanFilePages,
            Some(ReclaimStep::EvictCleanFilePages) => ReclaimStep::ShrinkPageCache,
            Some(ReclaimStep::ShrinkPageCache | ReclaimStep::Allocate | ReclaimStep::GiveUp) => {
                ReclaimStep::GiveUp
            }
        }
    }

    // Mirrors `akuma_exec::memmath::tests`' former escalation coverage exactly
    // (that copy is deleted now that this is the real, permanent home).
    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::USER_PAGE_RESERVE;

        #[test]
        fn escalation_walks_every_rung_in_cheapest_first_order() {
            let starved = 0;
            let mut done = None;
            let mut walked = alloc::vec::Vec::new();
            loop {
                let step = next_reclaim_step(starved, done);
                walked.push(step);
                if matches!(step, ReclaimStep::Allocate | ReclaimStep::GiveUp) {
                    break;
                }
                done = Some(step);
            }
            assert_eq!(
                walked,
                alloc::vec![
                    ReclaimStep::ReclaimHeap,
                    ReclaimStep::DrainRetired,
                    ReclaimStep::EvictCleanFilePages,
                    ReclaimStep::ShrinkPageCache,
                    ReclaimStep::GiveUp,
                ],
            );
        }

        #[test]
        fn free_above_the_reserve_allocates_from_every_rung() {
            for done in [
                None,
                Some(ReclaimStep::ReclaimHeap),
                Some(ReclaimStep::DrainRetired),
                Some(ReclaimStep::EvictCleanFilePages),
                Some(ReclaimStep::ShrinkPageCache),
            ] {
                assert_eq!(
                    next_reclaim_step(USER_PAGE_RESERVE + 1, done),
                    ReclaimStep::Allocate,
                    "reclaimed enough after {done:?} but did not stop"
                );
            }
        }

        #[test]
        fn a_rung_that_frees_enough_skips_the_remaining_rungs() {
            assert_eq!(
                next_reclaim_step(USER_PAGE_RESERVE + 512, Some(ReclaimStep::DrainRetired)),
                ReclaimStep::Allocate,
                "a successful drain must not be followed by file-page eviction"
            );
        }

        #[test]
        fn fruitless_drain_retired_continues_instead_of_giving_up() {
            let step = next_reclaim_step(0, Some(ReclaimStep::DrainRetired));
            assert_ne!(step, ReclaimStep::GiveUp);
            assert_eq!(step, ReclaimStep::EvictCleanFilePages);
        }

        #[test]
        fn give_up_after_the_last_rung_is_the_known_premature_oom() {
            assert_eq!(
                next_reclaim_step(0, Some(ReclaimStep::ShrinkPageCache)),
                ReclaimStep::GiveUp
            );
        }

        #[test]
        fn terminal_steps_stay_terminal_under_pressure() {
            for done in [ReclaimStep::GiveUp, ReclaimStep::Allocate] {
                assert_eq!(next_reclaim_step(0, Some(done)), ReclaimStep::GiveUp);
            }
        }
    }
}

/// Pages to reclaim per memory-pressure event below. Duplicate of
/// `src/pmm.rs`'s former `USER_RECLAIM_BATCH` — moved in with the loop.
const USER_RECLAIM_BATCH: usize = 512;

/// Allocate a zeroed page for a **user** demand-paging fault, escalating through
/// [`PmmHooks`]' four cold collaborators as free PMM falls to the reserve.
/// Returns `None` once every rung is exhausted, so the caller treats it as OOM
/// and SIGSEGVs the faulting process. `free_count`/`alloc_page_zeroed` are plain
/// calls here — only the collaborators below `alloc_page_zeroed_user` needed a
/// hook (`docs/archive/PMM_EXTRACT.md` §4).
///
/// The four hooks' `-> usize` return values are not the progress signal: a rung
/// can decline silently (`drain_retired` inside its cooldown) without being able
/// to report that. Progress is judged by re-reading `free_count()` on the next
/// loop iteration, via `next_reclaim_step`'s own re-check — see that function's
/// doc in `akuma_exec::memmath` for why the order matters.
/// Fewest free pages ever seen at a user demand-paging fault, and how many of
/// those faults ended in OOM.
///
/// Every user-fault OOM in the kernel funnels through `alloc_page_zeroed_user`
/// returning `None`, and each of the three call sites then SIGSEGVs the process
/// **without printing anything** — which is why an OOM-induced SIGSEGV has been
/// indistinguishable from a corruption-induced one in every campaign log. Counting
/// it here rather than at the three sites means a new fault path is covered for
/// free. `usize::MAX` = no user fault has been served yet.
static PMM_LOW_WATER: AtomicUsize = AtomicUsize::new(usize::MAX);
static USER_ALLOC_OOM: AtomicUsize = AtomicUsize::new(0);

/// `(low_water_pages, user_fault_oom_count)`. See [`PMM_LOW_WATER`].
///
/// Without `feature = "pmm-instr"` nothing writes either counter, so this reports
/// `(usize::MAX, 0)`. The two statics stay compiled on purpose: this keeps one
/// signature, so no caller needs a `cfg` of its own.
#[must_use]
pub fn pressure_stats() -> (usize, usize) {
    (
        PMM_LOW_WATER.load(Ordering::Relaxed),
        USER_ALLOC_OOM.load(Ordering::Relaxed),
    )
}

pub fn alloc_page_zeroed_user() -> Option<usize> {
    use reclaim_escalation::{ReclaimStep, next_reclaim_step};

    // Low-water sample. Taken here because this function already reads
    // `free_count()` for the escalation decision below, so the only added cost on
    // the fault path is a relaxed load plus a compare that almost always fails.
    // Gated all the same: this runs on every user demand-paging fault.
    #[cfg(feature = "pmm-instr")]
    {
        let free = free_count();
        let mut low = PMM_LOW_WATER.load(Ordering::Relaxed);
        while free < low {
            match PMM_LOW_WATER.compare_exchange_weak(
                low, free, Ordering::Relaxed, Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => low = seen,
            }
        }
    }

    let mut done = None;
    loop {
        let step = next_reclaim_step(free_count(), done);
        match step {
            ReclaimStep::Allocate => return alloc_page_zeroed(),
            ReclaimStep::GiveUp => {
                // The caller will SIGSEGV the faulting process. Print the first
                // few so a log can say "this SIGSEGV was OOM", then fall back to
                // the counter — an OOM storm must not turn the console into the
                // bottleneck.
                #[cfg(feature = "pmm-instr")]
                {
                    let n = USER_ALLOC_OOM.fetch_add(1, Ordering::Relaxed);
                    if n < 8 {
                        akuma_primitives::safe_print!(176,
                            "[PMM-USER-OOM] #{} free={} reserve={} — user fault cannot be served, caller will SIGSEGV\n",
                            n + 1, free_count(), USER_PAGE_RESERVE);
                    }
                }
                return None;
            }
            ReclaimStep::ReclaimHeap => {
                (hooks().heap_reclaim)();
            }
            ReclaimStep::DrainRetired => {
                (hooks().drain_retired)();
            }
            ReclaimStep::EvictCleanFilePages => {
                (hooks().evict_clean_file_pages)(USER_RECLAIM_BATCH);
            }
            ReclaimStep::ShrinkPageCache => {
                (hooks().shrink_page_cache)(USER_RECLAIM_BATCH);
            }
        }
        done = Some(step);
    }
}

pub fn alloc_pages_zeroed(count: usize) -> Option<Vec<usize>> {
    // Reserve the result buffer BEFORE taking PMM — see `alloc_pages_into`'s doc
    // comment for why (the PMM<->heap lock inversion that deadlocked `-j4`
    // self-host builds, `docs/reference/subsystems/memory.md` -> "PMM ↔ heap lock
    // flow").
    let mut frames: Vec<usize> = Vec::new();
    if frames.try_reserve_exact(count).is_err() {
        return None;
    }

    let ok = akuma_primitives::irq::with_irqs_disabled(|| {
        let mut pmm = PMM.lock();
        if !pmm.alloc_pages_into(count, &mut frames) {
            return false;
        }
        ALLOCATED_PAGES.fetch_add(count, Ordering::Relaxed);
        true
    });
    if !ok {
        return None;
    }

    // Each frame is zeroed and cleaned individually: `alloc_pages_into` returns
    // `count` frames that are NOT necessarily contiguous, so one `pages = count`
    // call would clean straight off the end of the first.
    for &pa in &frames {
        frame::zero_and_clean(pa, 1);
    }
    Some(frames)
}

// Step 7 (`docs/archive/PMM_EXTRACT.md` §7): the quarantine ring (512-slot,
// `QUAR_PRESENT` collisions, drain-under-pressure) and the escalation's
// *effects* (that the four `PmmHooks` are actually invoked, in order, only
// under pressure — the *decision* already had 6 tests from Step 4). Both are
// folded into ONE test function, deliberately: both touch the crate's single
// global `PMM` bitmap and `ALLOCATED_PAGES`, and the escalation phase drives
// free pages down to the reserve by allocating almost the entire arena — if
// that ran as a separate `#[test]` from anything else touching the same
// arena, `cargo test`'s default parallelism could interleave an allocation or
// a free from the other test in between the escalation loop's `free_count()`
// re-checks and flip which rung it takes, exactly the "measurement, not the
// code" failure class `scripts/verify_trim.py`'s own doc was written after.
// No other test in this crate reaches the global arena this heavily (the
// bitmap-allocator tests use local instances; the refcount tests only touch
// `COW_REFCOUNTS`), so this is the only test that needs the caution — but it
// needs all of it.
#[cfg(test)]
mod quarantine_and_escalation_effects_tests {
    use super::*;
    use crate::test_arena::{
        DRAIN_RETIRED_AT, EVICT_FILE_AT, HEAP_RECLAIM_AT, SHRINK_CACHE_AT, ensure_pmm,
    };
    use core::sync::atomic::Ordering;

    const TID: u32 = 1;

    /// Both halves live in one `#[test]` fn, not two — see the module comment
    /// above for why splitting them would let `cargo test`'s default
    /// parallelism interleave the escalation phase's `free_count()` re-checks
    /// with the quarantine phase's own alloc/free churn.
    #[test]
    fn quarantine_ring_and_escalation_effects_over_a_real_arena() {
        ensure_pmm();
        quarantine_detects_uaf_and_double_free();
        resurrection_detector_fires_on_inc_of_a_parked_frame();
        escalation_walks_all_four_hooks_in_order_then_gives_up();
    }

    // --- Resurrection: taking a NEW reference (inc-from-zero) on a frame that
    // is currently parked in quarantine is the race signature behind the
    // mapped-page premature frees (a mapper's inc losing to the last dec) —
    // it must be counted, because the victim only ever READS the poison and
    // every other instrument stays silent. ---
    fn resurrection_detector_fires_on_inc_of_a_parked_frame() {
        let pa = alloc_page().expect("arena must have room for the resurrection page");
        free_page(pa, TID); // parked in quarantine — last reference gone
        let before = cow_resurrection_count();
        cow_ref_inc(pa); // the racing mapper's late reference
        assert_eq!(
            cow_resurrection_count(),
            before + 1,
            "an inc that creates the entry for a quarantined frame must be counted"
        );
        // A SECOND inc on the same frame does not create the entry (count is
        // now 2) and must not double-count.
        cow_ref_inc(pa);
        assert_eq!(cow_resurrection_count(), before + 1);
        // Undo the two incs plus the or_insert baseline so the frame's entry
        // is gone again, then drain the ring so later phases see a clean arena.
        assert!(!cow_ref_dec(pa));
        assert!(!cow_ref_dec(pa));
        assert!(cow_ref_dec(pa));
        quarantine_drain_all();
    }

    // --- UAF: a write through a frame after it was freed must be caught
    // when the frame finally leaves quarantine. ---
    fn quarantine_detects_uaf_and_double_free() {
        let pa = alloc_page().expect("16 MiB arena must have room for one page");
        free_page(pa, TID); // parked in quarantine, poisoned — NOT yet returned to the bitmap
        assert!(!is_page_free(pa), "a quarantined frame must still read as allocated");
        let (_, uaf_before) = quarantine_stats();
        // The use-after-free: write through the dangling mapping, exactly the bug
        // class this instrument exists to catch (the null-`Rc` autopsy this whole
        // mechanism was built for). Goes through `frame::poison_fill` rather than
        // a raw `write_volatile` so the test carries no `unsafe` of its own — it
        // writes the WRONG word over the whole frame, which is a strictly harder
        // corruption for the detector to miss than the single stale word this
        // used to write.
        frame::poison_fill(pa, 0xdead_beef_dead_beef);
        let released = quarantine_drain_all();
        assert!(released >= 1, "the write-corrupted frame must have been in the ring to release");
        let (_, uaf_after) = quarantine_stats();
        assert!(uaf_after > uaf_before, "the corrupted frame must be detected on release");
        assert!(is_page_free(pa), "even a UAF-flagged frame must end up back in the bitmap");

        // --- Double-free: freeing the SAME frame twice while it is still
        // parked (before it has been drained) must be caught by
        // `QUAR_PRESENT`, not silently pushed twice. ---
        let pa2 = alloc_page().expect("arena must have room for a second page");
        free_page(pa2, TID); // parked
        let df_before = double_free_count();
        free_page(pa2, TID); // freed again while still parked
        assert!(
            double_free_count() > df_before,
            "a second free of a still-quarantined frame must be caught"
        );
        quarantine_drain_all(); // leave the ring empty for tidiness, not required for the assertions above
    }

    // Drives free pages down to the reserve with every reclaim hook wired to
    // a no-op (nothing behind them in a host test), and checks that
    // `alloc_page_zeroed_user` walks **all four** rungs, in the documented
    // cheapest-first order, exactly once each, before giving up — the
    // "effects" half of the escalation; `reclaim_escalation::tests` already
    // covers the *decision* this loop is built on.
    fn escalation_walks_all_four_hooks_in_order_then_gives_up() {
        // Drain the arena to at/below the reserve. A live `while` on
        // `free_count()` rather than a fixed iteration count, so this doesn't
        // assume how much of the arena `quarantine_detects_uaf_and_double_free`
        // (which runs first, in the same test) left behind.
        let mut held = alloc::vec::Vec::new();
        while free_count() > USER_PAGE_RESERVE {
            held.push(alloc_page().expect("must still have pages while free_count > reserve"));
        }
        assert!(user_alloc_would_starve(free_count()), "must be at or under the reserve now");

        let result = alloc_page_zeroed_user();
        assert!(
            result.is_none(),
            "every hook is a no-op, so the escalation must exhaust every rung and give up"
        );

        let (heap, retired, evict, shrink) = (
            HEAP_RECLAIM_AT.load(Ordering::SeqCst),
            DRAIN_RETIRED_AT.load(Ordering::SeqCst),
            EVICT_FILE_AT.load(Ordering::SeqCst),
            SHRINK_CACHE_AT.load(Ordering::SeqCst),
        );
        assert!(
            heap != usize::MAX && retired != usize::MAX && evict != usize::MAX && shrink != usize::MAX,
            "every rung must have fired exactly once: heap={heap} retired={retired} evict={evict} shrink={shrink}"
        );
        assert!(heap < retired, "ReclaimHeap must run before DrainRetired");
        assert!(retired < evict, "DrainRetired (dead processes) must run before EvictCleanFilePages (a live process's working set)");
        assert!(evict < shrink, "EvictCleanFilePages must run before ShrinkPageCache");

        let _ = held; // keep the allocations alive for the duration of the assertions above
    }
}
