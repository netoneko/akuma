//! The layout half of a user address space, for a kernel that maps eagerly (a page is
//! backed when it is mapped, never on a fault).
//!
//! A [`Layout`] is the bookkeeping: the regions a user pointer may name, the `brk` window,
//! the anonymous-`mmap` slab with its bump cursor and hole table, and the writable
//! `MAP_SHARED` file mappings that must be copied back to their files. Its methods are the
//! decisions over that state: where a mapping goes, what a `MAP_FIXED` replaces, what
//! `munmap` and `mremap` give back, whether a `brk` fits. It holds no page table, frame or
//! file: the kernel maps, fills, copies and frees, and tells the layout what it did.
//!
//! Extracted from Taoxichuan's `Task` record (`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md`, B8),
//! so a second minimal kernel gets the same layout and these rules get host tests.
#![no_std]
#![forbid(unsafe_code)]

use akuma_mmap::HoleSet;

/// The page size every range here is a multiple of.
pub const PAGE: u64 = 4096;

/// `len` rounded up to whole pages; `None` when that overflows.
#[must_use]
pub const fn page_span(len: u64) -> Option<u64> {
    match len.checked_add(PAGE - 1) {
        Some(l) => Some(l & !(PAGE - 1)),
        None => None,
    }
}

/// A writable `MAP_SHARED` file mapping.
///
/// Its pages are private frames filled from the file (no page cache), so the kernel copies them back on `munmap`, `msync`, exec and exit,
/// clamped to `size`, the file's length when it was mapped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedFile {
    pub base: u64,
    pub len: u64,
    pub inode: u32,
    pub offset: u64,
    pub size: u64,
}

impl SharedFile {
    #[must_use]
    const fn within(&self, lo: u64, hi: u64) -> bool {
        self.base >= lo && self.base + self.len <= hi
    }
}

/// What a `brk(want)` does ([`Layout::brk_plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brk {
    /// Outside the window: the call answers the current break (Linux's failure value).
    Refuse,
    /// Map `[from, to)` (page-aligned), then [`Layout::set_brk`].
    Grow { from: u64, to: u64 },
    /// At or below the current break: [`Layout::set_brk`] only; nothing is unmapped.
    Shrink,
}

/// What an `mremap` does ([`Layout::mremap`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mremap {
    /// The mapping keeps its address; free `release` and return it to the holes if given.
    Shrink { release: Option<(u64, u64)> },
    /// The mapping ends at the cursor: map `[from, to)`, then [`Layout::note_mapped`] with `to`.
    InPlace { from: u64, to: u64 },
    /// Allocate a new range ([`Layout::alloc`]), copy, and free the old one; with
    /// `release_old` the old range is the slab's and goes back to the holes.
    Move { release_old: bool },
}

/// The arguments do not describe a range (Linux's `EINVAL`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Einval;

/// One task's layout, with room for `REGIONS` user regions, `HOLES` slab holes and `SHARED`
/// shared-file mappings. `Copy`: a record moves between a kernel's tables by value.
#[derive(Clone, Copy)]
pub struct Layout<const REGIONS: usize, const HOLES: usize, const SHARED: usize> {
    regions: [(u64, u64); REGIONS],
    n_regions: usize,
    brk_base: u64,
    brk_cur: u64,
    brk_end: u64,
    mmap_base: u64,
    mmap_end: u64,
    mmap_cur: u64,
    holes: HoleSet<HOLES>,
    shared: [SharedFile; SHARED],
    n_shared: usize,
}

impl<const REGIONS: usize, const HOLES: usize, const SHARED: usize> Layout<REGIONS, HOLES, SHARED> {
    /// An empty layout whose anonymous `mmap` slab is `[mmap_base, mmap_base + mmap_size)`.
    #[must_use]
    pub const fn new(mmap_base: u64, mmap_size: u64) -> Self {
        Self {
            regions: [(0, 0); REGIONS],
            n_regions: 0,
            brk_base: 0,
            brk_cur: 0,
            brk_end: 0,
            mmap_base,
            mmap_end: mmap_base + mmap_size,
            mmap_cur: mmap_base,
            holes: HoleSet::new(),
            shared: [SharedFile { base: 0, len: 0, inode: 0, offset: 0, size: 0 }; SHARED],
            n_shared: 0,
        }
    }

    // ── regions ───────────────────────────────────────────────────────────────

    /// Is `[va, va + len)` wholly inside one region this task mapped? A bogus user pointer
    /// is then `EFAULT` rather than a kernel fault.
    #[must_use]
    pub fn user_ok(&self, va: u64, len: u64) -> bool {
        akuma_user_walk::in_regions(&self.regions[..self.n_regions], va, len)
    }

    /// Record `[start, end)` as user memory. `false` when the table is full (the range is
    /// then not addressable by user pointers).
    pub fn add_region(&mut self, start: u64, end: u64) -> bool {
        if self.n_regions == REGIONS {
            return false;
        }
        self.regions[self.n_regions] = (start, end);
        self.n_regions += 1;
        true
    }

    /// The recorded regions, in the order they were added.
    #[must_use]
    pub fn regions(&self) -> &[(u64, u64)] {
        &self.regions[..self.n_regions]
    }

    /// A fault at `va` under the downward-growing region that ends at `top` (the user
    /// stack), at most `max` bytes deep: the page range `[first, start)` to map before
    /// [`extend_down`](Self::extend_down). `None` when `va` is not such a fault (above the
    /// region's start, past the limit, or no region ends at `top`).
    #[must_use]
    pub fn growth_below(&self, top: u64, max: u64, va: u64) -> Option<(u64, u64)> {
        if va >= top || va < top.saturating_sub(max) {
            return None;
        }
        let &(start, _) = self.regions().iter().find(|&&(_, e)| e == top)?;
        (va < start).then_some((va & !(PAGE - 1), start))
    }

    /// The region ending at `top` now starts at `first` (its new pages are mapped).
    pub fn extend_down(&mut self, top: u64, first: u64) -> bool {
        match self.regions[..self.n_regions].iter_mut().find(|r| r.1 == top) {
            Some(r) => {
                r.0 = first;
                true
            }
            None => false,
        }
    }

    // ── brk ───────────────────────────────────────────────────────────────────

    /// Open the `brk` window `[base, base + size)` with the break at `base` (an image load).
    pub const fn set_brk_window(&mut self, base: u64, size: u64) {
        self.brk_base = base;
        self.brk_cur = base;
        self.brk_end = base + size;
    }

    /// The current break.
    #[must_use]
    pub const fn brk(&self) -> u64 {
        self.brk_cur
    }

    /// What `brk(want)` does. `want` must lie in `[base, end)`; growing maps the pages
    /// between the current break's page and `want`'s.
    #[must_use]
    pub const fn brk_plan(&self, want: u64) -> Brk {
        if want < self.brk_base || want >= self.brk_end {
            return Brk::Refuse;
        }
        if want > self.brk_cur {
            // `want < brk_end`, so rounding up cannot overflow.
            Brk::Grow { from: self.brk_cur & !(PAGE - 1), to: (want + PAGE - 1) & !(PAGE - 1) }
        } else {
            Brk::Shrink
        }
    }

    /// Move the break to `want` (after a [`Brk::Grow`] was mapped, or for [`Brk::Shrink`]).
    pub const fn set_brk(&mut self, want: u64) {
        self.brk_cur = want;
    }

    // ── the mmap slab ─────────────────────────────────────────────────────────

    /// Is `[addr, addr + len)` inside the slab (overflow counts as outside)?
    #[must_use]
    pub const fn in_slab(&self, addr: u64, len: u64) -> bool {
        match addr.checked_add(len) {
            Some(end) => addr >= self.mmap_base && end <= self.mmap_end,
            None => false,
        }
    }

    /// The bump cursor: everything at or above it is unmapped slab.
    #[must_use]
    pub const fn mmap_cur(&self) -> u64 {
        self.mmap_cur
    }

    /// A fresh `bytes`-long (page multiple) range: first fit from the holes, else the cursor.
    /// `None` when the slab is exhausted.
    pub fn alloc(&mut self, bytes: u64) -> Option<u64> {
        self.holes.alloc(&mut self.mmap_cur, self.mmap_end, bytes)
    }

    /// `[start, end)` is mapped again (a `MAP_FIXED` landed there): it leaves the holes.
    pub fn carve(&mut self, start: u64, end: u64) {
        self.holes.carve(start, end);
    }

    /// `[start, end)` was unmapped: lower the cursor when it touches it, else record a hole.
    pub fn release(&mut self, start: u64, end: u64) {
        self.holes.add(&mut self.mmap_cur, start, end);
    }

    /// Something was mapped up to `end` (a `MAP_FIXED`, an in-place `mremap`): only a
    /// mapping above the cursor moves it.
    pub const fn note_mapped(&mut self, end: u64) {
        if end > self.mmap_cur {
            self.mmap_cur = end;
        }
    }

    /// The slab holes, for diagnostics.
    #[must_use]
    pub fn holes(&self) -> &[(u64, u64)] {
        self.holes.as_slice()
    }

    /// `munmap(addr, len)`: `Err` for an unaligned or empty or overflowing range; `Ok(None)`
    /// for one outside the slab (a stack, the brk area: accepted and ignored); otherwise the
    /// page-aligned range to free and then [`release`](Self::release).
    pub const fn munmap(&self, addr: u64, len: u64) -> Result<Option<(u64, u64)>, Einval> {
        if addr & (PAGE - 1) != 0 || len == 0 {
            return Err(Einval);
        }
        let Some(bytes) = page_span(len) else {
            return Err(Einval);
        };
        let Some(end) = addr.checked_add(bytes) else {
            return Err(Einval);
        };
        if addr < self.mmap_base || end > self.mmap_end {
            return Ok(None);
        }
        Ok(Some((addr, end)))
    }

    /// `mremap(old, old_len, new_len)` without `MREMAP_FIXED` (the caller has checked the
    /// flags, `old`'s alignment and that `[old, old + old_len)` is user memory). Shrinking
    /// frees the tail, growing extends in place when the mapping is the topmost thing in the
    /// slab and still fits, and otherwise the mapping moves.
    pub const fn mremap(&self, old: u64, old_len: u64, new_len: u64) -> Result<Mremap, Einval> {
        let (Some(old_pages), Some(new_pages)) = (page_span(old_len), page_span(new_len)) else {
            return Err(Einval);
        };
        let in_slab = self.in_slab(old, old_pages);
        if new_pages <= old_pages {
            let release = if in_slab && new_pages < old_pages {
                Some((old + new_pages, old + old_pages))
            } else {
                None
            };
            return Ok(Mremap::Shrink { release });
        }
        if in_slab
            && old + old_pages == self.mmap_cur
            && let Some(to) = old.checked_add(new_pages)
            && to <= self.mmap_end
        {
            return Ok(Mremap::InPlace { from: old + old_pages, to });
        }
        Ok(Mremap::Move { release_old: in_slab })
    }

    // ── shared file mappings ──────────────────────────────────────────────────

    /// Record a writable shared file mapping. `false` when the table is full: writes to that
    /// mapping then never reach the file.
    pub fn add_shared(&mut self, f: SharedFile) -> bool {
        if self.n_shared == SHARED {
            return false;
        }
        self.shared[self.n_shared] = f;
        self.n_shared += 1;
        true
    }

    /// The recorded shared mappings.
    #[must_use]
    pub fn shared(&self) -> &[SharedFile] {
        &self.shared[..self.n_shared]
    }

    /// Does `va` lie in a shared mapping? (A fork must copy those pages, not share them.)
    #[must_use]
    pub fn in_shared(&self, va: u64) -> bool {
        self.shared().iter().any(|f| va >= f.base && va < f.base + f.len)
    }

    /// Drop the records wholly inside `[lo, hi)` (after their pages were written back and
    /// the range unmapped). A record only partly inside stays.
    pub fn forget_shared_within(&mut self, lo: u64, hi: u64) {
        let mut i = 0;
        while i < self.n_shared {
            if self.shared[i].within(lo, hi) {
                self.n_shared -= 1;
                self.shared[i] = self.shared[self.n_shared];
            } else {
                i += 1;
            }
        }
    }

    // ── lifecycle ─────────────────────────────────────────────────────────────

    /// A fresh address space for an `exec`: no regions, no holes, no shared mappings, the
    /// cursor at the slab's base. The loader then adds the image's regions and opens the brk
    /// window.
    pub fn reset_for_exec(&mut self) {
        self.n_regions = 0;
        self.mmap_cur = self.mmap_base;
        self.holes.clear();
        self.n_shared = 0;
    }

    /// The layout of a `fork` child: the same regions, break and slab, and **no** shared
    /// mappings. The child's copies of those pages are private (copy-on-write), so writing
    /// them back would put the child's bytes in the file - Linux would share the pages
    /// instead; this kernel does not, and keeping the records would also outlive the
    /// parent's own write-back.
    #[must_use]
    pub const fn fork(&self) -> Self {
        let mut child = *self;
        child.n_shared = 0;
        child
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x6000_0000_0000;
    const SIZE: u64 = 0x10_0000;
    type L = Layout<4, 8, 2>;

    fn layout() -> L {
        L::new(BASE, SIZE)
    }

    #[test]
    fn regions_bound_user_pointers_and_fill_up() {
        let mut l = layout();
        assert!(!l.user_ok(0x1000, 1), "nothing is user memory before a region is added");
        assert!(l.add_region(0x1000, 0x3000));
        assert!(l.user_ok(0x1000, 0x2000));
        assert!(!l.user_ok(0x2fff, 2), "a range may not run past its region");
        assert!(!l.user_ok(u64::MAX - 1, 4), "overflow is not user memory");
        for _ in 0..3 {
            assert!(l.add_region(0x10_0000, 0x10_1000));
        }
        assert!(!l.add_region(0x20_0000, 0x20_1000), "the fifth region does not fit four slots");
    }

    #[test]
    fn the_stack_grows_down_to_its_limit_and_no_further() {
        const TOP: u64 = 0x7000_0000_0000;
        let mut l = layout();
        l.add_region(TOP - 0x1_0000, TOP);
        assert_eq!(l.growth_below(TOP, 0x80_0000, TOP - 0x1_0008), Some((TOP - 0x1_1000, TOP - 0x1_0000)));
        assert_eq!(l.growth_below(TOP, 0x80_0000, TOP - 0x8000), None, "inside the region: not growth");
        assert_eq!(l.growth_below(TOP, 0x80_0000, TOP - 0x80_0001), None, "past the limit");
        assert_eq!(l.growth_below(TOP + 0x1000, 0x80_0000, TOP), None, "no region ends there");
        assert!(l.extend_down(TOP, TOP - 0x1_1000));
        assert!(l.user_ok(TOP - 0x1_1000, 0x1000));
    }

    #[test]
    fn brk_grows_page_wise_shrinks_by_bookkeeping_and_refuses_outside() {
        let mut l = layout();
        l.set_brk_window(0x1000_0000, 0x10_0000);
        assert_eq!(l.brk(), 0x1000_0000);
        assert_eq!(l.brk_plan(0x0fff_ffff), Brk::Refuse);
        assert_eq!(l.brk_plan(0x1010_0000), Brk::Refuse, "the window's end is exclusive");
        assert_eq!(l.brk_plan(0x1000_0010), Brk::Grow { from: 0x1000_0000, to: 0x1000_1000 });
        l.set_brk(0x1000_0010);
        assert_eq!(
            l.brk_plan(0x1000_2000),
            Brk::Grow { from: 0x1000_0000, to: 0x1000_2000 },
            "growth starts at the current break's page: its tail was mapped with it"
        );
        assert_eq!(l.brk_plan(0x1000_0008), Brk::Shrink);
    }

    #[test]
    fn munmap_reuses_first_fit_and_a_top_unmap_lowers_the_cursor() {
        let mut l = layout();
        let a = l.alloc(0x2000).unwrap();
        let b = l.alloc(0x1000).unwrap();
        assert_eq!((a, b), (BASE, BASE + 0x2000));
        assert_eq!(l.munmap(a, 0x2000), Ok(Some((a, a + 0x2000))));
        l.release(a, a + 0x2000);
        assert_eq!(l.holes(), &[(a, a + 0x2000)]);
        assert_eq!(l.alloc(0x1000), Some(a), "first fit from the hole");
        l.release(b, b + 0x1000);
        assert_eq!(
            (l.mmap_cur(), l.holes()),
            (a + 0x1000, &[][..]),
            "a hole at the cursor lowers it, through the hole it merges with first"
        );
    }

    #[test]
    fn munmap_validates_and_ignores_ranges_outside_the_slab() {
        let l = layout();
        assert_eq!(l.munmap(BASE + 1, 0x1000), Err(Einval), "unaligned");
        assert_eq!(l.munmap(BASE, 0), Err(Einval), "empty");
        assert_eq!(l.munmap(BASE, u64::MAX), Err(Einval), "the length overflows");
        assert_eq!(l.munmap(u64::MAX & !0xfff, 0x2000), Err(Einval), "the end overflows");
        assert_eq!(l.munmap(0x7000_0000_0000, 0x1000), Ok(None), "a stack page: accepted, ignored");
        assert_eq!(l.munmap(BASE + SIZE - 0x1000, 0x2000), Ok(None), "past the slab's end");
        assert_eq!(l.munmap(BASE, 1), Ok(Some((BASE, BASE + 0x1000))), "the length rounds up to pages");
    }

    #[test]
    fn map_fixed_carves_its_range_out_of_the_holes_and_raises_the_cursor_only_above_it() {
        let mut l = layout();
        let a = l.alloc(0x4000).unwrap();
        l.release(a, a + 0x2000);
        l.carve(a, a + 0x1000);
        assert_eq!(l.holes(), &[(a + 0x1000, a + 0x2000)]);
        let cur = l.mmap_cur();
        l.note_mapped(a + 0x1000);
        assert_eq!(l.mmap_cur(), cur, "a fixed mapping below the cursor leaves it");
        l.note_mapped(cur + 0x3000);
        assert_eq!(l.mmap_cur(), cur + 0x3000);
    }

    #[test]
    fn mremap_shrinks_extends_in_place_at_the_cursor_and_otherwise_moves() {
        let mut l = layout();
        let a = l.alloc(0x2000).unwrap();
        assert_eq!(l.mremap(a, 0x2000, 0x1000), Ok(Mremap::Shrink { release: Some((a + 0x1000, a + 0x2000)) }));
        assert_eq!(l.mremap(a, 0x2000, 0x1800), Ok(Mremap::Shrink { release: None }), "same page count");
        assert_eq!(l.mremap(a, 0x2000, 0x3000), Ok(Mremap::InPlace { from: a + 0x2000, to: a + 0x3000 }));
        let _b = l.alloc(0x1000).unwrap();
        assert_eq!(l.mremap(a, 0x2000, 0x3000), Ok(Mremap::Move { release_old: true }), "no longer topmost");
        assert_eq!(
            l.mremap(0x1000_0000, 0x1000, 0x2000),
            Ok(Mremap::Move { release_old: false }),
            "outside the slab: moved, the old range is not the slab's to reclaim"
        );
        assert_eq!(l.mremap(a, 0x1000, u64::MAX), Err(Einval));
        let mut full = layout();
        let top = full.alloc(SIZE).unwrap();
        assert_eq!(full.mremap(top, SIZE, SIZE + 0x1000), Ok(Mremap::Move { release_old: true }), "would pass the slab");
    }

    #[test]
    fn shared_records_fill_and_are_forgotten_only_when_wholly_inside() {
        let mut l = layout();
        let f = |base| SharedFile { base, len: 0x2000, inode: 7, offset: 0, size: 0x1800 };
        assert!(l.add_shared(f(BASE)));
        assert!(l.add_shared(f(BASE + 0x4000)));
        assert!(!l.add_shared(f(BASE + 0x8000)), "the table holds two");
        assert!(l.in_shared(BASE + 0x1fff) && !l.in_shared(BASE + 0x2000));
        l.forget_shared_within(BASE, BASE + 0x1000);
        assert_eq!(l.shared().len(), 2, "a partial overlap keeps the record");
        l.forget_shared_within(BASE, BASE + 0x2000);
        assert_eq!(l.shared(), &[f(BASE + 0x4000)]);
    }

    #[test]
    fn fork_keeps_the_layout_but_not_the_shared_mappings_and_exec_resets() {
        let mut l = layout();
        l.add_region(0x1000, 0x2000);
        l.set_brk_window(0x1000_0000, 0x1000);
        let a = l.alloc(0x3000).unwrap();
        l.release(a, a + 0x1000);
        l.add_shared(SharedFile { base: a + 0x1000, len: 0x1000, inode: 1, offset: 0, size: 0x1000 });
        let c = l.fork();
        assert_eq!(c.regions(), l.regions());
        assert_eq!((c.brk(), c.mmap_cur(), c.holes()), (l.brk(), l.mmap_cur(), l.holes()));
        assert!(c.shared().is_empty());
        l.reset_for_exec();
        assert!(l.regions().is_empty() && l.holes().is_empty() && l.shared().is_empty());
        assert_eq!(l.mmap_cur(), BASE);
    }
}
