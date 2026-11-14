//! Where an `mmap` without an address goes: **one placer for both kernels**.
//!
//! First fit over the sorted region list, starting at a per-process cursor
//! and wrapping once. Every mapping a process has is a region in that list,
//! so a gap between two regions is free address space, and a freed range
//! comes back on its own: there is no free list to keep in step with the
//! regions, which is what the AArch64 kernel had until 2026-11-12 (and what
//! let a `MAP_FIXED` into a free-list hole hand the same range out twice).
//!
//! # Why a cursor
//!
//! First fit from the window's base is O(regions below the first adequate
//! gap), and for `rustc`, which grows a dense arena, that is all of them —
//! measured on amd64 2026-09-17 at ~18 us of a 26 us `mmap`, 80 000 times per
//! crate. The cursor is a **hint, not a contract**: any value is safe, since a
//! wrong one costs a wasted first pass. A hole below it is not reused until
//! the scan wraps, which also means a freed address is recycled late, so a
//! stale pointer keeps faulting for longer rather than landing in a new
//! mapping.
//!
//! # Why the sort check
//!
//! Every writer keeps the list sorted (`insert_region_sorted`, and the
//! clip-and-split puts survivors back in place). An appending writer would
//! make this slow, not wrong: an out-of-order tail is sorted in place, with no
//! allocation. That check is O(1); a full `windows(2)` pass was measured at
//! 7.3 us of a 4 400-region call (`docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md`).

use crate::MmapRegion;
use core::ops::Range;

/// The address range automatic placement may hand out, and the part of it no
/// user mapping may take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    /// Lowest address handed out, and where the wrap restarts.
    pub lo: usize,
    /// One past the highest byte handed out.
    pub hi: usize,
    /// Never placed into: the AArch64 kernel's RAM identity map, which every
    /// user address space carries as EL1-only blocks. Empty where there is
    /// none (amd64).
    pub hole: Range<usize>,
}

/// Place `len` bytes, starting the search at `hint` and wrapping once.
/// Returns the address, if any, and how many regions the scan examined (the
/// complexity assertion: each region at most once per pass).
pub fn find_free_va_from(
    regions: &mut [MmapRegion],
    len: usize,
    hint: usize,
    w: &Window,
) -> (Option<usize>, usize) {
    let hint = if (w.lo..w.hi).contains(&hint) { hint } else { w.lo };
    let (found, examined) = scan_from(regions, len, hint, w);
    if found.is_some() || hint == w.lo {
        return (found, examined);
    }
    let (found, more) = scan_from(regions, len, w.lo, w);
    (found, examined + more)
}

fn scan_from(regions: &mut [MmapRegion], len: usize, start: usize, w: &Window) -> (Option<usize>, usize) {
    if len == 0 {
        return (None, 0);
    }
    let n = regions.len();
    if n >= 2 && regions[n - 2].start_va > regions[n - 1].start_va {
        regions.sort_unstable_by_key(|r| r.start_va);
    }
    // A candidate overlapping the hole moves to its end.
    let past_hole = |c: usize| {
        if c < w.hole.end && c.saturating_add(len) > w.hole.start { w.hole.end } else { c }
    };
    let mut cand = past_hole(start);
    let mut examined = 0usize;
    // Regions ending at or below the start bound no gap after it: skip them by
    // binary search, which is the cursor's payoff.
    let first = regions.partition_point(|r| r.start_va + r.len_bytes() <= cand);
    for r in &regions[first..] {
        let Some(end) = cand.checked_add(len).filter(|e| *e <= w.hi) else {
            return (None, examined);
        };
        examined += 1;
        // Sorted: a region starting at or past the candidate's end proves the
        // gap below it clear.
        if r.start_va >= end {
            return (Some(cand), examined);
        }
        // `max`: a region wholly below the candidate (a `MAP_FIXED` under the
        // window) must not drag it backwards.
        cand = past_hole(cand.max(r.start_va.saturating_add(r.len_bytes())));
    }
    match cand.checked_add(len) {
        Some(end) if end <= w.hi => (Some(cand), examined),
        _ => (None, examined),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Prot;
    use alloc::vec::Vec;

    const PG: usize = 4096;
    const BASE: usize = 0x1_0000_0000;
    const TOP: usize = 0x7000_0000_0000;
    const W: Window = Window { lo: BASE, hi: TOP, hole: 0..0 };

    fn region(start: usize, pages: usize) -> MmapRegion {
        MmapRegion::inherited_with_prot(start, pages, Prot::RW_NO_EXEC)
    }
    fn place(regions: &mut [MmapRegion], pages: usize) -> Option<usize> {
        find_free_va_from(regions, pages.checked_mul(PG).unwrap_or(usize::MAX), BASE, &W).0
    }

    #[test]
    fn first_fit_reuses_holes_that_fit() {
        assert_eq!(place(&mut [], 4), Some(BASE), "an empty space places at the base");
        assert_eq!(place(&mut [region(BASE, 4)], 1), Some(BASE + 4 * PG), "the next mapping abuts the first");
        let mut holed = [region(BASE, 2), region(BASE + 4 * PG, 2)];
        assert_eq!(place(&mut holed, 2), Some(BASE + 2 * PG), "a 2-page hole is reused");
        assert_eq!(place(&mut holed, 3), Some(BASE + 6 * PG), "and skipped by 3 pages");
    }

    #[test]
    fn order_and_low_regions_do_not_change_the_answer() {
        let mut reversed = [region(BASE + 4 * PG, 2), region(BASE, 2)];
        assert_eq!(place(&mut reversed, 2), Some(BASE + 2 * PG));
        let mut low = [region(0x3010_0000, 4), region(BASE, 2)];
        assert_eq!(place(&mut low, 1), Some(BASE + 2 * PG), "a region under the base does not move the candidate");
    }

    #[test]
    fn a_staircase_is_one_pass() {
        const N: usize = 1000;
        let mut stairs: Vec<MmapRegion> = (0..N).rev().map(|i| region(BASE + i * 2 * PG, 1)).collect();
        let (placed, examined) = find_free_va_from(&mut stairs, 2 * PG, BASE, &W);
        assert_eq!(placed, Some(BASE + (2 * N - 1) * PG));
        assert!(examined <= N, "examined {examined}");
    }

    #[test]
    fn the_cursor_is_a_hint_and_wraps_once() {
        let mut c = [region(BASE, 1), region(BASE + 2 * PG, 1)];
        assert_eq!(find_free_va_from(&mut c, PG, BASE + 3 * PG, &W).0, Some(BASE + 3 * PG), "past a hole, not in it");
        assert_eq!(find_free_va_from(&mut c, PG, BASE, &W).0, Some(BASE + PG), "below the hole takes it");
        assert_eq!(find_free_va_from(&mut c, PG, 0, &W).0, Some(BASE + PG), "out of window is clamped");
        let top_pages = (TOP - (BASE + 2 * PG)) / PG;
        let mut wrap = [region(BASE, 1), region(BASE + 2 * PG, top_pages)];
        assert_eq!(find_free_va_from(&mut wrap, PG, BASE + 4 * PG, &W).0, Some(BASE + PG), "wraps to the hole");
        let mut full = [region(BASE, (TOP - BASE) / PG)];
        assert_eq!(find_free_va_from(&mut full, PG, BASE + 4 * PG, &W).0, None, "full is None after the wrap");
    }

    #[test]
    fn the_window_bounds_and_overflow_refuse() {
        assert_eq!(place(&mut [], (TOP - BASE) / PG + 1), None);
        assert_eq!(place(&mut [], usize::MAX), None);
        assert_eq!(find_free_va_from(&mut [], 0, BASE, &W).0, None, "zero bytes has no placement");
    }

    #[test]
    fn the_hole_is_never_placed_into() {
        // The AArch64 shape: window from 0x1000_0000, kernel RAM at 1..3 GiB.
        let w = Window { lo: 0x1000_0000, hi: 0x40_0000_0000, hole: 0x4000_0000..0xC000_0000 };
        let (a, _) = find_free_va_from(&mut [], 0x2000_0000, 0x3000_0000, &w);
        assert_eq!(a, Some(0xC000_0000), "a request that would straddle the hole goes past it");
        let (b, _) = find_free_va_from(&mut [], PG, 0x5000_0000, &w);
        assert_eq!(b, Some(0xC000_0000), "a hint inside the hole moves past it");
        let mut below = [region(0x1000_0000, (0x3000_0000) / PG)];
        let (c, _) = find_free_va_from(&mut below, PG, 0x1000_0000, &w);
        assert_eq!(c, Some(0xC000_0000), "a region ending at the hole's start skips the hole");
        let (d, _) = find_free_va_from(&mut [], PG, 0x1000_0000, &w);
        assert_eq!(d, Some(0x1000_0000), "below the hole is fine");
    }
}
