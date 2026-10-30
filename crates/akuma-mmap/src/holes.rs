//! The mmap slab's **hole table**: the first-fit free list under a bump cursor.
//!
//! A task's mmap slab is `[base, limit)` with a cursor `cur`: everything below
//! `cur` was handed out at some point, everything above is untouched. An
//! `munmap` that does not reach the cursor leaves a *hole*; the next `mmap`
//! takes the first hole that fits, else bumps the cursor. A hole that touches
//! the cursor just lowers it. The table is a fixed array (`N` entries, no
//! allocation, `Copy` — a fork child inherits it by value); when it is full a
//! new hole is dropped (the address range is leaked until the process exits,
//! never double-handed-out).
//!
//! Order matters and is pinned: removal is `swap_remove` (the last entry moves
//! into the gap), and first-fit scans in table order, so the address a given
//! history yields is a function of this exact sequence. Moved verbatim out of
//! `taoxichuan/src/main.rs` (`add_hole`/`carve_hole`/`mmap_alloc`); the cursor
//! stays the caller's (`Task::mmap_cur`), passed in as `&mut u64`.

/// A fixed-capacity set of disjoint, non-abutting `[start, end)` holes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoleSet<const N: usize> {
    holes: [(u64, u64); N],
    n: usize,
}

impl<const N: usize> Default for HoleSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> HoleSet<N> {
    #[must_use]
    pub const fn new() -> Self {
        Self { holes: [(0, 0); N], n: 0 }
    }

    /// The live holes, in table order.
    #[must_use]
    pub fn as_slice(&self) -> &[(u64, u64)] {
        &self.holes[..self.n]
    }

    pub fn clear(&mut self) {
        self.n = 0;
    }

    fn swap_remove(&mut self, i: usize) {
        self.n -= 1;
        self.holes[i] = self.holes[self.n];
    }

    /// Record `[start, end)` as unmapped. `end` is clamped to the cursor; a
    /// hole that reaches the cursor lowers it instead of entering the table;
    /// any hole it touches (overlap or abut) is merged first. A full table
    /// drops the new hole.
    pub fn add(&mut self, cur: &mut u64, mut start: u64, mut end: u64) {
        if end > *cur {
            end = *cur;
        }
        if start >= end {
            return;
        }
        // Absorb every hole this one touches (a loop: one merge can bridge two).
        loop {
            let mut merged = false;
            let mut i = 0;
            while i < self.n {
                let (s, e) = self.holes[i];
                if s <= end && e >= start {
                    start = start.min(s);
                    end = end.max(e);
                    self.swap_remove(i);
                    merged = true;
                } else {
                    i += 1;
                }
            }
            if !merged {
                break;
            }
        }
        if end == *cur {
            *cur = start;
        } else if self.n < N {
            self.holes[self.n] = (start, end);
            self.n += 1;
        }
    }

    /// Take `[start, end)` out of the table (a `MAP_FIXED` landed there). A
    /// hole it splits in two keeps its left part in place and appends the
    /// right part (dropped if the table is full).
    pub fn carve(&mut self, start: u64, end: u64) {
        let mut i = 0;
        while i < self.n {
            let (s, e) = self.holes[i];
            if s >= end || e <= start {
                i += 1;
            } else if s >= start && e <= end {
                self.swap_remove(i);
            } else if s < start && e > end {
                self.holes[i] = (s, start);
                if self.n < N {
                    self.holes[self.n] = (end, e);
                    self.n += 1;
                }
                i += 1;
            } else if s < start {
                self.holes[i] = (s, start);
                i += 1;
            } else {
                self.holes[i] = (end, e);
                i += 1;
            }
        }
    }

    /// A fresh `bytes`-long (page multiple) range: first-fit from the holes,
    /// else the bump cursor (16-aligned up, as it always was). `None` when the
    /// slab ending at `limit` is exhausted.
    pub fn alloc(&mut self, cur: &mut u64, limit: u64, bytes: u64) -> Option<u64> {
        for i in 0..self.n {
            let (s, e) = self.holes[i];
            if e - s >= bytes {
                if e - s == bytes {
                    self.swap_remove(i);
                } else {
                    self.holes[i] = (s + bytes, e);
                }
                return Some(s);
            }
        }
        let base = (*cur + 0xF) & !0xF;
        if base + bytes > limit {
            return None;
        }
        *cur = base + bytes;
        Some(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    const P: u64 = 4096;
    const BASE: u64 = 0x1000_0000;

    #[test]
    fn hole_touching_cursor_lowers_it() {
        let mut h = HoleSet::<4>::new();
        let mut cur = BASE + 10 * P;
        h.add(&mut cur, BASE + 8 * P, BASE + 10 * P);
        assert_eq!(cur, BASE + 8 * P);
        assert!(h.as_slice().is_empty());
    }

    #[test]
    fn end_is_clamped_to_cursor() {
        let mut h = HoleSet::<4>::new();
        let mut cur = BASE + 4 * P;
        h.add(&mut cur, BASE + 2 * P, BASE + 99 * P);
        assert_eq!(cur, BASE + 2 * P);
    }

    #[test]
    fn empty_and_inverted_ranges_are_ignored() {
        let mut h = HoleSet::<4>::new();
        let mut cur = BASE + 4 * P;
        h.add(&mut cur, BASE + P, BASE + P);
        h.add(&mut cur, BASE + 3 * P, BASE + P);
        assert!(h.as_slice().is_empty());
        assert_eq!(cur, BASE + 4 * P);
    }

    #[test]
    fn overlapping_and_abutting_holes_merge_even_when_one_bridges_two() {
        let mut h = HoleSet::<8>::new();
        let mut cur = BASE + 100 * P;
        h.add(&mut cur, BASE + 10 * P, BASE + 12 * P);
        h.add(&mut cur, BASE + 14 * P, BASE + 16 * P);
        assert_eq!(h.as_slice().len(), 2);
        h.add(&mut cur, BASE + 12 * P, BASE + 14 * P); // abuts both
        assert_eq!(h.as_slice(), &[(BASE + 10 * P, BASE + 16 * P)]);
    }

    #[test]
    fn first_fit_takes_table_order_not_best_fit() {
        let mut h = HoleSet::<8>::new();
        let mut cur = BASE + 100 * P;
        h.add(&mut cur, BASE + 10 * P, BASE + 20 * P); // 10 pages
        h.add(&mut cur, BASE + 30 * P, BASE + 32 * P); // 2 pages (exact fit below)
        let got = h.alloc(&mut cur, BASE + 200 * P, 2 * P);
        assert_eq!(got, Some(BASE + 10 * P)); // first, not the exact fit
        assert_eq!(h.as_slice()[0], (BASE + 12 * P, BASE + 20 * P));
    }

    #[test]
    fn exact_fit_swap_removes_last_into_the_gap() {
        let mut h = HoleSet::<8>::new();
        let mut cur = BASE + 100 * P;
        h.add(&mut cur, BASE + 10 * P, BASE + 11 * P);
        h.add(&mut cur, BASE + 20 * P, BASE + 22 * P);
        h.add(&mut cur, BASE + 30 * P, BASE + 33 * P);
        assert_eq!(h.alloc(&mut cur, BASE + 200 * P, P), Some(BASE + 10 * P));
        // the 3-page hole moved into slot 0
        assert_eq!(h.as_slice(), &[(BASE + 30 * P, BASE + 33 * P), (BASE + 20 * P, BASE + 22 * P)]);
    }

    #[test]
    fn bump_aligns_to_16_and_refuses_past_the_limit() {
        let mut h = HoleSet::<2>::new();
        let mut cur = BASE + 3;
        assert_eq!(h.alloc(&mut cur, BASE + 2 * P, P), Some(BASE + 0x10));
        assert_eq!(cur, BASE + 0x10 + P);
        assert_eq!(h.alloc(&mut cur, BASE + 2 * P, 2 * P), None);
        assert_eq!(cur, BASE + 0x10 + P); // refusal leaves the cursor alone
    }

    #[test]
    fn carve_splits_trims_and_removes() {
        let mut h = HoleSet::<8>::new();
        let mut cur = BASE + 100 * P;
        h.add(&mut cur, BASE + 10 * P, BASE + 20 * P);
        h.carve(BASE + 12 * P, BASE + 14 * P); // split
        assert_eq!(h.as_slice(), &[(BASE + 10 * P, BASE + 12 * P), (BASE + 14 * P, BASE + 20 * P)]);
        h.carve(BASE + 9 * P, BASE + 11 * P); // trims the left hole's head
        assert_eq!(h.as_slice()[0], (BASE + 11 * P, BASE + 12 * P));
        h.carve(BASE + 18 * P, BASE + 30 * P); // trims the right hole's tail
        assert_eq!(h.as_slice()[1], (BASE + 14 * P, BASE + 18 * P));
        h.carve(BASE, BASE + 99 * P); // swallows both
        assert!(h.as_slice().is_empty());
    }

    #[test]
    fn full_table_drops_the_new_hole_and_never_hands_it_out() {
        let mut h = HoleSet::<2>::new();
        let mut cur = BASE + 100 * P;
        h.add(&mut cur, BASE + 10 * P, BASE + 11 * P);
        h.add(&mut cur, BASE + 20 * P, BASE + 21 * P);
        h.add(&mut cur, BASE + 30 * P, BASE + 31 * P); // dropped
        assert_eq!(h.as_slice().len(), 2);
        assert_eq!(h.alloc(&mut cur, BASE + 200 * P, P), Some(BASE + 10 * P));
        assert_eq!(h.alloc(&mut cur, BASE + 200 * P, P), Some(BASE + 20 * P));
        // the dropped hole's page is lost, so this bumps the cursor instead
        assert_eq!(h.alloc(&mut cur, BASE + 200 * P, P), Some(BASE + 100 * P));
    }

    /// Model check: a page-granular bitmap of what is free (below the cursor)
    /// against a long pseudo-random op sequence. After EVERY op the table must
    /// be disjoint, non-abutting, inside the model's free set and below the
    /// cursor; every allocation must lie wholly in free space or at/above the
    /// old cursor, and be marked used. A tiny capacity (3) exercises the
    /// drop-when-full path, which may lose free pages but never invent them.
    #[test]
    fn model_check_against_a_page_bitmap() {
        const PAGES: u64 = 64;
        let limit = BASE + PAGES * P;
        for seed in 1u64..200 {
            let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut rnd = |m: u64| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x % m
            };
            let mut h = HoleSet::<3>::new();
            let mut cur = BASE;
            // used[i]: page i was handed out and not returned.
            let mut used = [false; PAGES as usize];
            for _ in 0..400 {
                match rnd(3) {
                    0 => {
                        let n = 1 + rnd(6);
                        let old_cur = cur;
                        if let Some(a) = h.alloc(&mut cur, limit, n * P) {
                            let (lo, hi) = ((a - BASE) / P, (a - BASE) / P + n);
                            for p in lo..hi {
                                assert!(!used[p as usize], "seed {seed}: page {p} handed out twice");
                                used[p as usize] = true;
                            }
                            assert!(a + n * P <= limit);
                            if a >= old_cur {
                                assert_eq!(cur, a + n * P);
                            }
                        }
                    }
                    1 => {
                        // return a random USED run (what munmap does)
                        let lo = rnd(PAGES);
                        let mut hi = lo;
                        while hi < PAGES && hi - lo < 1 + rnd(5) && used[hi as usize] {
                            hi += 1;
                        }
                        if hi > lo {
                            for p in lo..hi {
                                used[p as usize] = false;
                            }
                            h.add(&mut cur, BASE + lo * P, BASE + hi * P);
                        }
                    }
                    _ => {
                        // MAP_FIXED over a random range: pages become used.
                        let lo = rnd(PAGES - 1);
                        let hi = (lo + 1 + rnd(4)).min(PAGES);
                        if BASE + hi * P <= cur {
                            for p in lo..hi {
                                used[p as usize] = true;
                            }
                            h.carve(BASE + lo * P, BASE + hi * P);
                        }
                    }
                }
                // invariants
                let mut hs: Vec<_> = h.as_slice().to_vec();
                hs.sort_unstable();
                for w in hs.windows(2) {
                    assert!(w[0].1 < w[1].0, "seed {seed}: holes touch or overlap: {hs:?}");
                }
                for &(s, e) in &hs {
                    assert!(s < e && e <= cur && s >= BASE, "seed {seed}: bad hole ({s:#x},{e:#x}) cur {cur:#x}");
                    for p in (s - BASE) / P..(e - BASE) / P {
                        assert!(!used[p as usize], "seed {seed}: hole covers used page {p}");
                    }
                }
            }
        }
    }
}
