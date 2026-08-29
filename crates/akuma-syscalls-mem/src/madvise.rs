//! `madvise`'s advice decode, and `MADV_DONTNEED`'s range and per-page rules.

use akuma_primitives::errno::negated::EINVAL;
use akuma_syscalls_linux::flags::madvise::{MADV_DONTNEED, MADV_FREE, MADV_WILLNEED};

/// What `sys_madvise` should do with an advice value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Pre-fault unmapped pages of **anonymous** lazy regions in the range.
    ///
    /// File-backed lazy pages must be left alone: installing a zeroed frame marks
    /// the page present, so the demand-fault path never runs and the file content
    /// is never read. That silently zeroed every weight page of a `llama.cpp` model
    /// mmap — `docs/archive/BKL_VFS_CARVE_OUT.md` §10.
    Willneed,
    /// Zero or drop the range, per [`dontneed_page_action`].
    Dontneed,
    /// Return this errno.
    Fail(u64),
    /// Return success without doing anything.
    ///
    /// **Divergence 4.** Linux implements several of the advices that land here
    /// (`MADV_DONTFORK`, `MADV_HUGEPAGE`, …); reporting success for them is a
    /// deliberate "harmless hint" choice, not an oversight.
    Ignore,
}

/// Decode an `madvise` advice value.
///
/// **Divergence 3.** `MADV_FREE` returns `EINVAL` rather than a fabricated success.
/// Linux returns `EINVAL` for advice it does not support and callers read that
/// correctly: Redis probes `MADV_FREE`, treats `EINVAL` as "older kernel" and
/// starts, where a fabricated 0 sent it into a THP self-check it cannot pass
/// without `/proc/<pid>/smaps` (`docs/archive/LONG_ROAD_TO_REDIS.md` §5).
///
/// Known consequence, deliberately accepted: allocators that probe `MADV_FREE`
/// (jemalloc, mimalloc) fall back to `MADV_DONTNEED`, whose own divergence is
/// larger. The `DONTNEED_SHARED_FRAME` counter exists to make that traffic visible.
#[must_use]
pub const fn action(advice: i32) -> Action {
    match advice {
        MADV_WILLNEED => Action::Willneed,
        MADV_DONTNEED => Action::Dontneed,
        MADV_FREE => Action::Fail(EINVAL),
        _ => Action::Ignore,
    }
}

/// The page range `MADV_DONTNEED` actually zeroes for `(addr, len)`, as
/// `(start_va, pages)`.
///
/// **Divergence 2.** Linux requires a page-aligned `start` and rejects anything
/// else with `EINVAL`, then zeroes `[start, PAGE_ALIGN(start+len))`. This rounds an
/// unaligned start **down**, so for `addr & 0xFFF != 0` the range cleared is a
/// strict superset of Linux's — it includes the caller's partial head page, whose
/// live bytes Linux would never touch. Counted by `DONTNEED_UNALIGNED` rather than
/// fixed; it has never been observed to read back non-zero
/// (`docs/archive/CARGO_HEAP_NULL_RC.md`, follow-on 1).
/// # Overflow is preserved, not fixed
///
/// `saturating_add` guards the first addition but **not** the `+ 0xFFF` rounding,
/// so a huge `len` wraps `end` to 0 and `end - start` underflows to a near-`usize`
/// page count. The kernel ships `--release` (overflow checks off), where that wraps
/// silently; the host tests run in debug, where the same expression would panic.
/// The ops below are therefore spelled `wrapping_*` so **both** builds compute what
/// the shipped kernel computes, and the result is pinned by
/// `preserved_overflow_huge_len_wraps_to_an_enormous_page_count`.
///
/// This is a live defect, not a curiosity — see that test. It is preserved here
/// because an extraction that quietly fixes something cannot be A/B'd against what
/// it replaced; the fix belongs in its own change with its own verification.
#[must_use]
pub const fn dontneed_zero_range(addr: usize, len: usize) -> (usize, usize) {
    let start = addr & !0xFFF;
    let end = addr.saturating_add(len).wrapping_add(0xFFF) & !0xFFF;
    (start, end.wrapping_sub(start) / 4096)
}

/// What `MADV_DONTNEED` must do with one page, given the page's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAction {
    /// Nothing mapped at this VA: a lazy region (or a later fault) already yields
    /// zeroes, so there is nothing to do.
    Nothing,
    /// This address space is the frame's only holder — zeroing it in place is
    /// indistinguishable from Linux's drop-and-refault and costs no allocation.
    ZeroInPlace,
    /// Another address space maps this frame too. Zeroing it would wipe *their*
    /// live page, which is the null-`Rc` corruption. Give this address space a
    /// private zero frame instead and drop its share reference.
    BreakSharing,
}

/// The per-page rule, over the two facts the handler can observe: whether the VA is
/// mapped, and how many address spaces hold the frame.
///
/// `cow_ref` counts **address spaces**, and the first share inserts 2 (see
/// `akuma_pmm::cow_ref_inc`) — so `>= 2` is the only value meaning "someone else
/// can see this frame". `1` is a peer that has already gone away (a
/// `file_page_cache` entry mapped nowhere else, or a fork sibling that exited or
/// broke CoW), and `0` is a frame that was never shared; neither has a peer to
/// corrupt, so both take the cheap path.
#[must_use]
pub const fn dontneed_page_action(mapped: bool, cow_ref: u16) -> PageAction {
    if !mapped {
        PageAction::Nothing
    } else if cow_ref >= 2 {
        PageAction::BreakSharing
    } else {
        PageAction::ZeroInPlace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_implemented_advices() {
        assert_eq!(action(MADV_WILLNEED), Action::Willneed);
        assert_eq!(action(MADV_DONTNEED), Action::Dontneed);
    }

    /// **Divergence 3.** `MADV_FREE` must report `EINVAL`, not success. Redis reads
    /// the difference and will not start if this is fabricated.
    #[test]
    fn diverge_madv_free_is_einval_not_success() {
        assert_eq!(action(MADV_FREE), Action::Fail(EINVAL));
        assert_ne!(action(MADV_FREE), Action::Ignore);
    }

    /// **Divergence 4.** Everything else succeeds silently, including advices Linux
    /// implements.
    #[test]
    fn diverge_unknown_advice_reports_success() {
        assert_eq!(action(0), Action::Ignore); // MADV_NORMAL
        assert_eq!(action(1), Action::Ignore); // MADV_RANDOM
        assert_eq!(action(10), Action::Ignore); // MADV_DONTFORK
        assert_eq!(action(-1), Action::Ignore);
    }

    /// An aligned range is exactly the pages the caller named.
    #[test]
    fn aligned_range_is_exact() {
        assert_eq!(dontneed_zero_range(0x1000, 4096), (0x1000, 1));
        assert_eq!(dontneed_zero_range(0x1000, 4 * 4096), (0x1000, 4));
        assert_eq!(dontneed_zero_range(0x1000, 4097), (0x1000, 2));
    }

    /// **Divergence 2.** An unaligned start rounds DOWN, so the cleared range
    /// includes the caller's partial head page — bytes Linux never touches, because
    /// Linux rejects the call with `EINVAL` instead.
    #[test]
    fn diverge_unaligned_start_rounds_down_and_covers_the_head_page() {
        let (start, pages) = dontneed_zero_range(0x1800, 4096);
        assert_eq!(start, 0x1000, "start rounded down below the caller's address");
        assert_eq!(pages, 2, "and the range grew to cover the spill into the next page");
        // The caller's own live bytes at 0x1000..0x1800 are inside the cleared range.
        assert!(start < 0x1800);
    }

    /// A zero length still clears the page containing `addr` when `addr` is
    /// unaligned — the round-down and round-up do not cancel.
    #[test]
    fn zero_length_clears_nothing_when_aligned() {
        assert_eq!(dontneed_zero_range(0x1000, 0), (0x1000, 0));
        assert_eq!(dontneed_zero_range(0x1800, 0), (0x1000, 1));
    }

    /// **PRESERVED DEFECT — not a divergence, a bug.** A huge `len` overflows the
    /// `+ 0xFFF` rounding, wrapping `end` to 0 and underflowing `end - start` into a
    /// page count of ~4.5 quadrillion.
    ///
    /// `sys_madvise` takes `len` straight from a user register with no validation
    /// (`syscall/mod.rs`: `nr::MADVISE => mem::sys_madvise(args[0], args[1], ...)`),
    /// and `madvise_dontneed_range`'s pass 0 then runs
    /// `(0..pages).map(..).filter(..).collect()` — a per-page lazy-region lookup,
    /// inside an `MmBklGuard` window. So `madvise(addr, -1, MADV_DONTNEED)` from
    /// unprivileged userspace is an unbounded loop in the kernel.
    ///
    /// Pinned rather than fixed so the extraction stays behaviour-preserving and
    /// A/B-able. The fix is a length cap in `sys_madvise`, in its own change.
    #[test]
    fn preserved_overflow_huge_len_wraps_to_an_enormous_page_count() {
        let (start, pages) = dontneed_zero_range(0x1000, usize::MAX);
        assert_eq!(start, 0x1000);
        assert_eq!(pages, 4_503_599_627_370_495, "the wrapped count, ~4.5e15 pages");
        // Sanity: the honest answer would have been ~4.5e15 too, but ANCHORED at
        // the top of the address space, not wrapped through zero. The tell is that
        // `end` came out BELOW `start`.
        let end = 0x1000usize.saturating_add(usize::MAX).wrapping_add(0xFFF) & !0xFFF;
        assert_eq!(end, 0);
        assert!(end < start, "end wrapped below start — this is the defect");
    }

    /// An unmapped page needs no work at all — this is the hot arm on a lazy region.
    #[test]
    fn unmapped_page_does_nothing_whatever_the_refcount() {
        assert_eq!(dontneed_page_action(false, 0), PageAction::Nothing);
        assert_eq!(dontneed_page_action(false, 9), PageAction::Nothing);
    }

    /// The refcount boundary, both sides. `cow_ref` counts address spaces and the
    /// first share inserts **2**, so 1 means a peer that has already gone.
    #[test]
    fn share_breaking_starts_at_two_holders() {
        assert_eq!(dontneed_page_action(true, 0), PageAction::ZeroInPlace);
        assert_eq!(dontneed_page_action(true, 1), PageAction::ZeroInPlace);
        assert_eq!(dontneed_page_action(true, 2), PageAction::BreakSharing);
        assert_eq!(dontneed_page_action(true, u16::MAX), PageAction::BreakSharing);
    }

    /// The rule that stops the null-`Rc` corruption: a frame another address space
    /// can see must never be zeroed in place.
    #[test]
    fn a_shared_frame_is_never_zeroed_in_place() {
        for refs in 2..=32u16 {
            assert_ne!(dontneed_page_action(true, refs), PageAction::ZeroInPlace);
        }
    }
}
