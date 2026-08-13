//! Memory arithmetic and the decisions built directly on it.
//!
//! # Why this module exists
//!
//! All of this used to live in `src/` — the kernel binary — which no host test
//! can reach, so integer comparisons were checked by booting a VM. Two of them
//! carried doc comments claiming they were written to be unit-testable
//! (`user_alloc_would_starve`: *"Pure fn over the free-page count so it can be
//! unit-tested at the boundary without actually draining RAM"*) while living
//! somewhere no unit test could call them. The intent was right; the address was
//! wrong.
//!
//! `docs/archive/TRIMMING_FAT_EMBARASSING_DUPLICATIONS.md` §5.11 has the full
//! argument, including why this is a module in `akuma-exec` rather than a new
//! crate: nothing outside `akuma-exec` and `src/` consumes it, so a crate would
//! cut no `cargo tree` edge — the one criterion `akuma-primitives` exists to
//! satisfy.
//!
//! # The config gates came too
//!
//! Two of these decisions are gated by a kill switch, and the gates moved here
//! with them rather than staying behind as wrappers in `src/`: `ExecConfig` is
//! *injectable* (`runtime::register_config_for_test`), so a gate is no reason to
//! leave a decision unreachable. `ExecConfig::for_test()` sets both gates **on**,
//! for the same reason it sets `syscall_debug_info_enabled` — a gate left off
//! makes every test of the gated path skip the branch it exists to cover.
//!
//! Consequence worth knowing: `config()` is single-shot per test binary, so the
//! *enabled* direction is what these tests can exercise. Each gated function
//! therefore delegates to a **pure predicate** that is tested exhaustively in
//! both directions ([`mapping_is_read_only_to_user`], [`poison_decode`]), and the
//! gated wrapper is checked for agreeing with it.
//!
//! Nothing here reads a runtime function pointer or takes a lock. The RAM window
//! comes from `mmu::ram_base`/`ram_end`, which are plain atomics with a
//! documented pre-`init` fallback for exactly this purpose.
//!
//! Fork's own copy-range math (`process::fork_code_start`,
//! `process::fork_page_count_for_len`) stays next to `fork_process`, its only
//! consumer; this module is for arithmetic shared across subsystems.

use crate::mmu;
use crate::runtime::config;

// ============================================================================
// The user-page reserve
// ============================================================================

/// Pages held back from *user* demand-paging so kernel-critical work can always
/// make progress when a process tries to consume all of RAM: the page tables to
/// complete an in-flight fault, kernel-heap growth, and the OOM process-kill path
/// itself. Without this, a memory-hungry process (tcc, meow) drains the PMM to
/// near-zero and the kernel's *own* next allocation fails — and a failed kernel
/// allocation aborts the whole kernel (a `BRK` trap) instead of the offending
/// process being killed. 16 pages = 64 KB: small enough not to raise the working
/// floor, large enough for one minimal heap-growth + the kill path's bookkeeping.
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
/// File-backed demand paging batches many pages per fault (readahead), so it must
/// clamp the batch to this budget — otherwise an mmap larger than RAM drains the
/// PMM to 0 and a later kernel-side alloc (IRQ/scheduler, no current process)
/// panics into a whole-kernel `BRK` abort instead of the offending process being
/// SIGSEGV'd.
///
/// Saturating, so at or below the reserve the budget is 0 and the caller falls
/// through to its single-page path rather than wrapping to an enormous batch.
#[must_use]
#[inline]
pub fn user_readahead_budget(free: usize) -> usize {
    free.saturating_sub(USER_PAGE_RESERVE)
}

// ============================================================================
// The user-page reclaim escalation
// ============================================================================

/// One rung of the escalation `alloc_page_zeroed_user` walks when free PMM has
/// fallen to [`USER_PAGE_RESERVE`], plus the two terminal answers.
///
/// The rungs are ordered **cheapest recovery first**, and "cheap" here means *what
/// the next fault pays*, not what this call pays:
///
/// 1. [`Self::ReclaimHeap`] — return fully-free kernel-heap watermark to the PMM.
///    Costs nothing anyone will miss.
/// 2. [`Self::DrainRetired`] — free the address spaces of processes that are
///    already **dead** but whose slots a collector has not dropped yet. Zero
///    re-fault cost, which is why it sits above eviction: since Phase 7e a reaped
///    process's whole address space sits in a RETIRED slot, and the only
///    steady-state collector (`netpoll_maint`, 100 ms) is exactly what this kind of
///    pressure starves.
/// 3. [`Self::EvictCleanFilePages`] — unmap clean, read-only file-backed pages from
///    a **live** process and let them re-fault. Buys a disk read per page.
/// 4. [`Self::ShrinkPageCache`] — drop shared file-page cache entries that still
///    own frames the sweep above unmapped but could not free.
///
/// Getting that order wrong is not a performance bug — evicting a live process's
/// working set while a dead process's pages sit parked is a self-inflicted disk
/// read per page, repeated for as long as the pressure lasts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReclaimStep {
    /// Enough free pages: hand one out. The only non-failing exit.
    Allocate,
    /// `allocator::reclaim_to_pmm()`.
    ReclaimHeap,
    /// `process::reclaim::drain_retired_under_pressure()`.
    DrainRetired,
    /// `process::reclaim_clean_file_pages(USER_RECLAIM_BATCH)`.
    EvictCleanFilePages,
    /// `file_page_cache::shrink(USER_RECLAIM_BATCH)`.
    ShrinkPageCache,
    /// Out of options: the caller returns `None` and its caller SIGSEGVs the
    /// faulting process. An *invented* OOM is indistinguishable from a real one in
    /// the log, so this must only be reached after every rung above has run.
    GiveUp,
}

/// The pure decision behind `alloc_page_zeroed_user`'s reclaim escalation: given the
/// current free-page count and the rung already performed, what to do next.
///
/// `done` is `None` before any reclaim has been attempted, and otherwise the rung
/// that just ran — whether or not it actually freed anything, because a rung cannot
/// reliably report that (`drain_retired_under_pressure` declines silently inside its
/// 10 ms cooldown). Progress is therefore judged **only** by re-reading `free`, which
/// is the whole reason this function re-checks the reserve *before* it looks at
/// `done`: a rung that freed enough short-circuits to [`ReclaimStep::Allocate`] and
/// the remaining rungs never run.
///
/// # The bug class this exists to make testable
///
/// The effects stay in `src/pmm.rs`; only the decision lives here, because the ways
/// this escalation goes wrong are all decisions: a missing re-check between two
/// rungs (so a rung that already freed memory is followed by an unnecessary, more
/// expensive one), the rungs in the wrong order (see [`ReclaimStep`]), or a
/// premature [`ReclaimStep::GiveUp`] that SIGSEGVs a process while memory was
/// reclaimable. None of those had a unit test while this was five nested `if`s in
/// the kernel binary; draining real RAM to the reserve is not something a boot test
/// can safely do, which is why `test_oom_user_page_reserve` never exercised it.
///
/// # Known gap, deliberately preserved
///
/// A fruitless rung is followed by the next rung, and after the last one this
/// returns [`ReclaimStep::GiveUp`] even though memory may be merely *cooling* —
/// parked in a RETIRED slot younger than `PROCESS_RECLAIM_COOLDOWN_US`. No rung
/// waits for that cooldown, so the escalation can invent an OOM. That is a real
/// open defect (`docs/README.md`'s symptom matrix; `memory.md` → "KNOWN GAP: a
/// premature give-up while memory is merely cooling"), and it is pinned by
/// `give_up_after_the_last_rung_is_the_known_premature_oom` below rather than fixed
/// here: fixing it changes *when processes get killed*, which is not something to
/// bundle into a behaviour-preserving merge.
#[must_use]
pub fn next_reclaim_step(free: usize, done: Option<ReclaimStep>) -> ReclaimStep {
    // Re-check pressure FIRST, before consulting `done`: this is what makes every
    // rung's progress count, and what keeps a recovered allocation from paying for
    // the rungs below it.
    if !user_alloc_would_starve(free) {
        return ReclaimStep::Allocate;
    }
    match done {
        None => ReclaimStep::ReclaimHeap,
        Some(ReclaimStep::ReclaimHeap) => ReclaimStep::DrainRetired,
        Some(ReclaimStep::DrainRetired) => ReclaimStep::EvictCleanFilePages,
        Some(ReclaimStep::EvictCleanFilePages) => ReclaimStep::ShrinkPageCache,
        // `Allocate`/`GiveUp` are terminal and the caller never performs them, so
        // they can only appear here through a caller bug; answering `GiveUp` keeps
        // that bug a failed allocation rather than an infinite reclaim loop.
        Some(ReclaimStep::ShrinkPageCache | ReclaimStep::Allocate | ReclaimStep::GiveUp) => {
            ReclaimStep::GiveUp
        }
    }
}

// ============================================================================
// Quarantine poison codec
// ============================================================================

/// Poison base; XORed with the PA so a frame written with *another* frame's
/// poison (a stale copy, a mis-targeted memset) is still a mismatch.
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
/// word can be traced back to its frame, and this is the reverse. The check that
/// makes it trustworthy is **page alignment**: an arbitrary 64-bit value that
/// happens to carry the `0xFEEDFACE` prefix still has to XOR down to a 4 KiB
/// aligned, in-range PA — a 1-in-4096 accident on top of a 1-in-2^32 one.
///
/// The window is a parameter so the range check itself is testable at arbitrary
/// bounds; [`poison_word_frame`] supplies the live one.
#[must_use]
pub fn poison_decode(word: u64, ram_base: usize, ram_end: usize) -> Option<usize> {
    // Cheap reject first: everything else here is only reached for a word that
    // already carries the magic's high half.
    if word >> 32 != POISON_MAGIC >> 32 {
        return None;
    }
    let pa = (word ^ POISON_MAGIC) as usize;
    if pa & (mmu::PAGE_SIZE - 1) != 0 {
        return None;
    }
    if pa < ram_base || pa >= ram_end {
        return None;
    }
    Some(pa)
}

/// If `word` is a quarantine poison word, the frame it was written for.
///
/// This is what turns the null-`Rc` crash from "a qword read back as garbage"
/// into "frame P, freed by thread T at free-seq S". The value that motivated it
/// was `0xfeedfacea8d0e010` — a poisoned pointer for frame `0x767de000`,
/// dereferenced at `+0x10`
/// (`docs/archive/CARGO_NULL_RC_MEMORY_REFERENCE_AUDIT.md` §13.8).
///
/// Returns `None` when the quarantine is compiled off, since nothing writes
/// poison then and any match would be a coincidence.
#[must_use]
pub fn poison_word_frame(word: u64) -> Option<usize> {
    if !config().pmm_uaf_quarantine {
        return None;
    }
    poison_decode(word, mmu::ram_base(), mmu::ram_end())
}

// ============================================================================
// Mapping predicates
// ============================================================================

/// Pure: does a mapping with `map_flags` give EL0 **no write access**? True only
/// for `AP_RO_ALL`, i.e. `mmu::user_flags::RO` and `RX`.
#[must_use]
#[inline]
pub fn mapping_is_read_only_to_user(map_flags: u64) -> bool {
    const AP_MASK: u64 = 3 << 6;
    (map_flags & AP_MASK) == mmu::flags::AP_RO_ALL
}

/// Is a page mapped with `map_flags` eligible for the shared file-page cache?
///
/// A writable private file mapping would need copy-on-write before sharing, so
/// ELF data segments carrying relocations stay private. Gated by
/// `config().shared_file_pages_enabled` (the `SHARED_FILE_PAGES_ENABLED` kill
/// switch), which makes every page ineligible when off.
#[must_use]
#[inline]
pub fn is_shareable_mapping(map_flags: u64) -> bool {
    config().shared_file_pages_enabled && mapping_is_read_only_to_user(map_flags)
}

#[cfg(test)]
mod tests {
    //! The reserve assertions here were previously made from the boot suite
    //! (`src/process_tests.rs::test_oom_user_page_reserve`, which keeps its live
    //! allocator check and handed the arithmetic over).
    use super::*;

    fn setup() {
        crate::runtime::register_config_for_test();
    }

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
        assert_eq!(user_readahead_budget(USER_PAGE_RESERVE - 1), 0);
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

    /// The ladder, walked from the top under unrelieved pressure. Order is the
    /// assertion: dead processes' pages (zero re-fault cost) before a live process's
    /// clean file pages (a disk read per page).
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

    /// No pressure → allocate, whatever has already run. Guards the re-check
    /// ordering: consulting `done` before `free` would keep reclaiming after the
    /// pressure was already relieved.
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

    /// The rung that frees enough must short-circuit — the remaining, more expensive
    /// rungs must not run. This is the "missing re-check between steps" bug class:
    /// with the reserve test after the `done` match, a successful `DrainRetired`
    /// would still be followed by evicting a live process's working set.
    #[test]
    fn a_rung_that_frees_enough_skips_the_remaining_rungs() {
        // DrainRetired collected a dead process and freed plenty.
        assert_eq!(
            next_reclaim_step(USER_PAGE_RESERVE + 512, Some(ReclaimStep::DrainRetired)),
            ReclaimStep::Allocate,
            "a successful drain must not be followed by file-page eviction"
        );
    }

    /// **The case that matters** (`COW_PILE_AUDIT.md` §8.1 item 3): `DrainRetired`
    /// returned nothing because every retired slot is still inside its 10 ms
    /// `PROCESS_RECLAIM_COOLDOWN_US`. A fruitless rung must not be read as "out of
    /// options" — the escalation owes the remaining rungs before it kills anything,
    /// because a `GiveUp` here is an invented OOM.
    #[test]
    fn fruitless_drain_retired_continues_instead_of_giving_up() {
        let step = next_reclaim_step(0, Some(ReclaimStep::DrainRetired));
        assert_ne!(
            step,
            ReclaimStep::GiveUp,
            "a drain that freed nothing (cooldown) must not SIGSEGV the process"
        );
        assert_eq!(step, ReclaimStep::EvictCleanFilePages);
    }

    /// Pins the **known gap**, so the fix has to change this test on purpose.
    ///
    /// After the last rung the escalation gives up even though the memory it could
    /// not collect may be merely *cooling* — parked in a RETIRED slot younger than
    /// `PROCESS_RECLAIM_COOLDOWN_US`. No rung waits for that cooldown, so this
    /// `GiveUp` can be an invented OOM: the process is SIGSEGV'd and the pages
    /// become collectable microseconds later. `memory.md` → "KNOWN GAP: a premature
    /// give-up while memory is merely cooling".
    #[test]
    fn give_up_after_the_last_rung_is_the_known_premature_oom() {
        assert_eq!(
            next_reclaim_step(0, Some(ReclaimStep::ShrinkPageCache)),
            ReclaimStep::GiveUp
        );
    }

    /// `GiveUp` is terminal and idempotent: a caller that loops on it gets a failed
    /// allocation, never an endless reclaim.
    #[test]
    fn terminal_steps_stay_terminal_under_pressure() {
        for done in [ReclaimStep::GiveUp, ReclaimStep::Allocate] {
            assert_eq!(next_reclaim_step(0, Some(done)), ReclaimStep::GiveUp);
        }
    }

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
        let last = end - mmu::PAGE_SIZE;
        assert_eq!(poison_decode(poison_word(last), base, end), Some(last));
    }

    /// The gated wrapper against the live RAM window. On the host that window is
    /// `mmu`'s documented pre-`init` fallback, which is why this is reachable at
    /// all — and the injected config has the quarantine **on**, so this exercises
    /// the decode rather than the early return.
    #[test]
    fn gated_decode_uses_the_live_ram_window() {
        setup();
        let pa = mmu::ram_base() + 0x2000;
        assert_eq!(poison_word_frame(poison_word(pa)), Some(pa));
        // Outside the window, and a non-poison value.
        assert_eq!(poison_word_frame(poison_word(mmu::ram_end())), None);
        assert_eq!(poison_word_frame(0xdead_beef), None);
    }

    #[test]
    fn only_user_read_only_mappings_are_shareable() {
        assert!(mapping_is_read_only_to_user(mmu::user_flags::RO));
        assert!(mapping_is_read_only_to_user(mmu::user_flags::RX));
        assert!(!mapping_is_read_only_to_user(mmu::user_flags::RW));
        assert!(!mapping_is_read_only_to_user(mmu::user_flags::RW_NO_EXEC));
    }

    /// The predicate must read *only* the AP field: a page that is RO to EL0 stays
    /// shareable whatever its execute/attr bits say, and a writable one is never
    /// rescued by them.
    #[test]
    fn predicate_ignores_bits_outside_the_ap_field() {
        let other = mmu::flags::UXN | mmu::flags::PXN | mmu::flags::AF;
        assert!(mapping_is_read_only_to_user(mmu::user_flags::RO | other));
        assert!(!mapping_is_read_only_to_user(mmu::user_flags::RW | other));
    }

    /// With the gate injected **on**, the gated form must agree with the pure
    /// predicate for every flag combination — i.e. the gate adds nothing but the
    /// kill switch.
    #[test]
    fn gated_shareable_agrees_with_the_predicate_when_enabled() {
        setup();
        assert!(config().shared_file_pages_enabled, "for_test must enable the gate");
        for flags in [
            mmu::user_flags::RO,
            mmu::user_flags::RX,
            mmu::user_flags::RW,
            mmu::user_flags::RW_NO_EXEC,
        ] {
            assert_eq!(is_shareable_mapping(flags), mapping_is_read_only_to_user(flags));
        }
    }
}
