//! The eager-mapping record and the two pure operations over a region list.
//!
//! Both operations take the list itself — a slice or a `&mut Vec` — never a process.
//! That shape predates this crate: `detach_eager_regions_in_range` was already split
//! out of `sys_munmap` so it could be tested without a live process, and the tests at
//! the bottom of this file came with it. The crate boundary is what makes the shape
//! permanent rather than a convention.

use alloc::vec::Vec;

use crate::PhysFrame;

/// An eagerly-mapped `mmap` region (all pages resident at mmap time).
///
/// `pages` — not `frames.len()` — is the authoritative extent of the region.
/// The two are equal for a region this process created itself via `mmap`, but a
/// **CoW-forked child inherits `pages` with an empty `frames`**: the child maps
/// every page (read-only, shared with the parent) but owns none of them, so it
/// has no per-region frame list to record. Frame ownership for such a child is
/// tracked solely in `UserAddressSpace::user_frames`, which is refcounted.
///
/// Deriving the extent from `frames.len()` therefore reports 0 pages for any
/// inherited region, which is how a *grandchild* fork used to lose its parent's
/// mmap regions entirely — `cow_share_range` skipped them as zero-length, and
/// the grandchild took an unrecoverable translation fault on first touch (see
/// `docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`). Use `pages` for extent
/// (sharing, demotion, munmap sizing) and `frames` only when a real PA is
/// required, guarding the index against a short/empty list.
#[derive(Clone)]
pub struct MmapRegion {
    pub start_va: usize,
    pub pages: usize,
    pub frames: Vec<PhysFrame>,
    /// The protection this mapping is *supposed* to have, in `mmu::user_flags`
    /// terms — the eager counterpart of `LazyRegion::flags`.
    ///
    /// Without it an eager region records extent and frames but no permission, so
    /// the EL0 write-permission-fault handler cannot tell a PTE that is wrongly
    /// read-only (page state lost some other way) from a mapping that is
    /// legitimately read-only (`mprotect(PROT_READ)`). Lazy regions carry flags and
    /// therefore get a permission upgrade; eager regions had no such path and died
    /// with SIGSEGV instead. See
    /// `docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §3.
    pub flags: u64,

    /// `MAP_SHARED | MAP_ANONYMOUS`: this mapping must survive `fork` as **one
    /// object**, not as a copy-on-write copy.
    ///
    /// Everything else in an address space is private, so fork demotes it to RO and
    /// lets the first write break CoW. Doing that to a `MAP_SHARED` anonymous
    /// mapping silently gives parent and child separate pages — a child's write is
    /// then invisible to the parent, which is the opposite of what the flag asks
    /// for. Regions carrying this take
    /// `akuma_exec::process::share_rw_range` at fork instead: same
    /// frames, mapped writable in the child, parent left alone.
    ///
    /// Must propagate to inherited regions too, or a grandchild silently stops
    /// sharing. Probe: `userspace/forktest/c_stress/shmanon.c`.
    pub shared_anon: bool,

    /// Whether [`flags`](Self::flags) is a **statement** about this mapping's
    /// protection, or merely the safe default.
    ///
    /// This exists because `NONE` is otherwise two different facts in one `u64`:
    /// [`MmapRegion::owned`] uses it to mean "protection unrecorded" (see its doc
    /// for why that default is the safe one), and `from_prot(PROT_NONE)` produces
    /// the identical value to mean "the caller asked for no access".
    ///
    /// Telling them apart did not matter while `flags` was only ever used to
    /// **grant** a write the fault handler would otherwise refuse — `NONE` grants
    /// nothing either way. It matters the moment `flags` is used to **deny** one:
    /// treating "unrecorded" as "not writable" refuses legitimate CoW breaks on
    /// every region built without explicit flags, which killed `rustc` mid-build
    /// with `[WPF] … eager=0x60000000000080 cow_ref=1` — `NONE`, exactly.
    ///
    /// Use [`recorded_prot`](Self::recorded_prot) rather than reading this
    /// directly.
    pub prot_recorded: bool,
}

impl MmapRegion {
    /// Region created by this process: it owns every frame, protection unrecorded.
    ///
    /// Defaults to `NONE` **deliberately**. `flags` exists so the fault handler can
    /// grant a write it would otherwise refuse, so an unknown protection has to be
    /// the one that grants nothing: a wrong `RW` default would silently defeat
    /// `mprotect(PROT_READ)` on any region built through this constructor. `NONE`
    /// leaves such a region behaving exactly as it did before `flags` existed.
    /// Callers that know the real protection use [`MmapRegion::owned_with_flags`].
    #[must_use]
    pub fn owned(start_va: usize, frames: Vec<PhysFrame>) -> Self {
        let mut r = Self::owned_with_flags(start_va, frames, crate::user_flags::NONE);
        // `NONE` here is the safe default, NOT a statement — see `prot_recorded`.
        r.prot_recorded = false;
        r
    }

    /// Region created by this process, with its real protection recorded.
    #[must_use]
    pub fn owned_with_flags(start_va: usize, frames: Vec<PhysFrame>, flags: u64) -> Self {
        Self {
            start_va, pages: frames.len(), frames, flags,
            shared_anon: false, prot_recorded: true,
        }
    }

    /// Region inherited by a CoW-forked child: extent known, no owned frames,
    /// protection unrecorded (`NONE` — see [`MmapRegion::owned`] for why).
    #[must_use]
    pub fn inherited(start_va: usize, pages: usize) -> Self {
        let mut r = Self::inherited_with_flags(start_va, pages, crate::user_flags::NONE);
        r.prot_recorded = false;
        r
    }

    /// Region inherited by a CoW-forked child, carrying the parent's protection.
    #[must_use]
    pub fn inherited_with_flags(start_va: usize, pages: usize, flags: u64) -> Self {
        Self {
            start_va, pages, frames: Vec::new(), flags,
            shared_anon: false, prot_recorded: true,
        }
    }

    /// The protection this region **states**, or `None` if it never recorded one.
    ///
    /// The accessor a *deny* decision must use. A *grant* decision can read
    /// [`flags`](Self::flags) directly, because the unrecorded default (`NONE`)
    /// grants nothing anyway.
    #[must_use]
    pub const fn recorded_prot(&self) -> Option<u64> {
        if self.prot_recorded { Some(self.flags) } else { None }
    }

    /// Mark this region `MAP_SHARED | MAP_ANONYMOUS`. See [`MmapRegion::shared_anon`].
    #[must_use]
    pub fn shared_anon(mut self) -> Self {
        self.shared_anon = true;
        self
    }


    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.pages * 4096
    }

    #[must_use]
    pub fn contains(&self, va: usize) -> bool {
        va >= self.start_va && va < self.start_va + self.len_bytes()
    }

    /// Physical frame backing `va`, if this process owns a frame list covering it.
    /// Returns `None` for CoW-inherited regions (no owned frames) and for any VA
    /// outside the owned prefix.
    #[must_use]
    pub fn frame_for(&self, va: usize) -> Option<PhysFrame> {
        if !self.contains(va) {
            return None;
        }
        self.frames.get((va - self.start_va) / 4096).copied()
    }
}

/// Derive a CoW-forked child's `mmap_regions` from its parent's.
///
/// The child maps every page of every parent region (read-only, CoW-shared by
/// `cow_share_range`) but *owns* none of them — frames are shared, and a write
/// fault allocates the child a private frame tracked in `user_frames`. So each
/// child region carries the parent's extent with an empty frame list.
///
/// Carrying the **extent** across is the part that matters, and the part that
/// used to be dropped: the child's regions were built with
/// `Vec::with_capacity(frames.len())`, which is a *length-zero* Vec, and every
/// consumer derived the region's size from `frames.len()`. A child forked from
/// such a child therefore saw four zero-length regions, `cow_share_range` skipped
/// all of them, and the grandchild had no mapping at all for the VAs its parent
/// was about to hand it live pointers into — a deterministic write to an unmapped
/// page (`docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md`). The shell shape
/// `( cmd; cmd ) &` produces exactly that lineage: the shell mmaps musl's first
/// malloc arena, forks a subshell, and the subshell forks again to exec `cmd`.
#[must_use]
pub fn inherit_mmap_regions_for_cow_child(parent_regions: &[MmapRegion]) -> alloc::vec::Vec<MmapRegion> {
    parent_regions
        .iter()
        .map(|r| {
            let mut inherited = MmapRegion::inherited_with_flags(r.start_va, r.pages, r.flags);
            // Carry `prot_recorded` too: a child of an unrecorded region is itself
            // unrecorded, and a child of an `mprotect`ed one keeps the statement.
            inherited.prot_recorded = r.prot_recorded;
            // Must carry `shared_anon` across, or a grandchild silently stops sharing:
            // the child would CoW-share a mapping its parent shares by identity.
            if r.shared_anon { inherited.shared_anon() } else { inherited }
        })
        .collect()
}

/// Detach every eager region overlapping `[range_start, range_end)`.
///
/// Partial regions are clipped at both ends, and one `(base_va, pages, owned_frames)`
/// piece comes back per region touched. Surviving head/tail pieces are left in
/// `regions`.
///
/// This is `munmap`'s region bookkeeping, split out from `sys_munmap` so it can be
/// tested directly: region splitting is where this code has gone wrong before, and
/// the shapes that matter (full / prefix / suffix / middle / multi-region /
/// CoW-inherited) are all reachable from a plain `Vec` without a live process.
///
/// The caller does the page unmapping and frame freeing **after** releasing
/// `vm_lock` — those take other locks and must not run under it.
///
/// # Why it clips rather than matching one region
///
/// The original matched a single region by exact `start_va` and stopped there, so
/// an unmap starting mid-region or spanning two of them freed only the first
/// region's pages, returned success, and left the rest mapped with their VA never
/// recycled. A leftover region is also a live protection record for an address
/// that has moved on, which `eager_region_flags_for_page_fault` will happily answer
/// from. See docs/archive/CARGO_HEAP_NULL_RC.md (D8/D9).
///
/// Page counts come from `MmapRegion::pages`, never `frames.len()`: a CoW-inherited
/// region has every page mapped but owns no frames.
pub fn detach_eager_regions_in_range(
    regions: &mut alloc::vec::Vec<MmapRegion>,
    range_start: usize,
    range_end: usize,
) -> alloc::vec::Vec<(usize, usize, alloc::vec::Vec<PhysFrame>)> {
    let mut pieces = alloc::vec::Vec::new();
    if range_end <= range_start {
        return pieces;
    }
    let mut i = 0usize;
    while i < regions.len() {
        let reg_start = regions[i].start_va;
        let reg_pages = regions[i].pages;
        let reg_end = reg_start + reg_pages * crate::PAGE_SIZE;
        if reg_start >= range_end || reg_end <= range_start {
            i += 1;
            continue;
        }
        let clip_start = range_start.max(reg_start);
        let clip_end = range_end.min(reg_end);
        let head_pages = (clip_start - reg_start) / crate::PAGE_SIZE;
        let clip_pages = (clip_end - clip_start) / crate::PAGE_SIZE;
        let tail_pages = (reg_end - clip_end) / crate::PAGE_SIZE;

        // Split the frame vector in step with the extent. `filter_map(next)`
        // tolerates the CoW-inherited case (`frames` empty): every piece then
        // carries its page count and no frames, which is exactly right.
        let reg = regions.remove(i);
        let flags = reg.flags;
        // A partial unmap changes extent, not identity: both survivors are still the
        // same `MAP_SHARED|MAP_ANONYMOUS` object if the original was.
        let shared_anon = reg.shared_anon;
        // A partial unmap changes extent, not what the region states about itself.
        let prot_recorded = reg.prot_recorded;
        let mut it = reg.frames.into_iter();
        let head: alloc::vec::Vec<PhysFrame> = (0..head_pages).filter_map(|_| it.next()).collect();
        let mid: alloc::vec::Vec<PhysFrame> = (0..clip_pages).filter_map(|_| it.next()).collect();
        let tail: alloc::vec::Vec<PhysFrame> = it.collect();

        // Survivors keep the protection of the region they came from: a partial
        // unmap changes extent, not permission. Both lie entirely outside
        // [range_start, range_end), so re-examining them costs one overlap test
        // and cannot loop.
        if head_pages > 0 {
            regions.push(MmapRegion {
                start_va: reg_start, pages: head_pages, frames: head, flags,
                shared_anon, prot_recorded });
        }
        if tail_pages > 0 {
            regions.push(MmapRegion {
                start_va: clip_end, pages: tail_pages, frames: tail, flags,
                shared_anon, prot_recorded });
        }
        if clip_pages > 0 {
            pieces.push((clip_start, clip_pages, mid));
        }
        // `remove(i)` shifted the next candidate into slot `i`; do not advance.
    }
    pieces
}

#[cfg(test)]
mod mmap_region_inheritance_tests {
    //! Regression tests for the grandchild-loses-its-mmap-regions bug
    //! (docs/archive/FORK_EXEC_HEAP_LAZY_REGION_SIGSEGV.md).
    //!
    //! A CoW-forked child owns none of its inherited regions' frames, so its
    //! frame lists are empty. When the region's extent was *derived* from those
    //! lists, a child's own fork computed a zero-length range for every inherited
    //! region and shared none of them, leaving the grandchild with no mapping for
    //! VAs its parent had resident. `MmapRegion::pages` carries the extent
    //! independently of frame ownership so that chain holds up.
    use super::*;
    use crate::user_flags;
    use crate::PhysFrame;

    fn frames(n: usize) -> alloc::vec::Vec<PhysFrame> {
        (0..n).map(|i| PhysFrame::new(0x4000_0000 + i * 4096)).collect()
    }

    /// The generation that called `mmap` owns its frames; extent == frame count.
    #[test]
    fn owned_region_extent_matches_frames() {
        let r = MmapRegion::owned(0x2012_0000, frames(3));
        assert_eq!(r.pages, 3);
        assert_eq!(r.len_bytes(), 3 * 4096);
        assert!(r.contains(0x2012_0000));
        assert!(r.contains(0x2012_2fff));
        assert!(!r.contains(0x2012_3000));
        assert_eq!(r.frame_for(0x2012_1000).map(|f| f.addr), Some(0x4000_1000));
    }

    /// An eager region must carry the protection it was created with, and a CoW
    /// child must inherit it.
    ///
    /// This is what lets the EL0 write-permission-fault handler tell a repairable
    /// read-only PTE inside a writable eager mapping from a genuine access
    /// violation. Two-sided on purpose: the writable region grants the upgrade, the
    /// `PROT_READ` one must not, and the unrecorded default must not either —
    /// revert `MmapRegion::owned`'s default to `RW_NO_EXEC` and the third assertion
    /// fails, which is the regression that would silently defeat `mprotect`.
    #[test]
    fn eager_region_records_protection_and_child_inherits_it() {
        let rw = MmapRegion::owned_with_flags(0x2012_0000, frames(2), user_flags::RW_NO_EXEC);
        let ro = MmapRegion::owned_with_flags(0x2013_0000, frames(1), user_flags::RO);
        let unknown = MmapRegion::owned(0x2014_0000, frames(1));

        let writable = |r: &MmapRegion| r.flags & crate::flags::AP_MASK == crate::flags::AP_RW_ALL;
        assert!(writable(&rw), "a PROT_WRITE region must permit the upgrade");
        assert!(!writable(&ro), "mprotect(PROT_READ) must still fault");
        assert!(!writable(&unknown),
            "an unrecorded protection must grant nothing — a permissive default \
             would silently defeat mprotect on every region built this way");

        let child = inherit_mmap_regions_for_cow_child(&[rw, ro]);
        assert_eq!(child[0].flags, user_flags::RW_NO_EXEC, "child loses the repair path otherwise");
        assert_eq!(child[1].flags, user_flags::RO, "child must not gain write on a RO mapping");
    }

    /// A CoW child keeps the extent but owns no frames — the exact state whose
    /// extent used to be lost.
    #[test]
    fn cow_child_inherits_extent_without_owning_frames() {
        let parent = alloc::vec![
            MmapRegion::owned(0x2012_0000, frames(1)),
            MmapRegion::owned(0x2012_1000, frames(1)),
            MmapRegion::owned(0x2012_2000, frames(2)),
            MmapRegion::owned(0x2012_4000, frames(1)),
        ];

        let child = inherit_mmap_regions_for_cow_child(&parent);

        assert_eq!(child.len(), 4);
        for (c, p) in child.iter().zip(parent.iter()) {
            assert_eq!(c.start_va, p.start_va);
            assert_eq!(c.pages, p.pages, "extent must survive the CoW fork");
            assert!(c.frames.is_empty(), "a CoW child owns no per-region frames");
        }
        // Total extent preserved: 1+1+2+1 = 5 pages — the five pages the
        // grandchild used to be missing.
        assert_eq!(child.iter().map(|r| r.pages).sum::<usize>(), 5);
    }

    /// The actual regression: fork the child again. Every region must still
    /// present a non-zero range to share, or the grandchild faults on first touch.
    #[test]
    fn grandchild_still_inherits_full_extent() {
        let parent = alloc::vec![MmapRegion::owned(0x2012_0000, frames(1))];
        let child = inherit_mmap_regions_for_cow_child(&parent);
        let grandchild = inherit_mmap_regions_for_cow_child(&child);

        assert_eq!(grandchild.len(), 1);
        assert_eq!(grandchild[0].start_va, 0x2012_0000);
        assert_eq!(grandchild[0].pages, 1);
        assert!(
            grandchild[0].len_bytes() > 0,
            "a zero-length range is skipped by cow_share_range — this is the bug"
        );
        // The faulting address from the original report lands in this region.
        assert!(grandchild[0].contains(0x2012_0338));
    }

    /// An inherited region has no owned frame to re-map from, so the eager
    /// demand-paging fallback must decline rather than index an empty list.
    #[test]
    fn inherited_region_has_no_frame_to_remap() {
        let r = MmapRegion::inherited(0x2012_0000, 2);
        assert!(r.contains(0x2012_0338));
        assert_eq!(r.frame_for(0x2012_0338), None);
        assert_eq!(r.frame_for(0x9999_0000), None);
    }
    // ── `detach_eager_regions_in_range` — munmap's region bookkeeping ──────────
    //
    // Region splitting is where this code has gone wrong before, so every shape is
    // pinned here rather than inferred: the pieces handed back must account for
    // exactly the pages inside the range, the survivors must account for exactly
    // the pages outside it, and frames must follow their pages in both directions.

    /// `NONE` is two different facts, and `recorded_prot` is what separates them.
    ///
    /// `owned()`/`inherited()` default to `NONE` meaning "protection unrecorded";
    /// `from_prot(PROT_NONE)` yields the identical value meaning "no access". A
    /// *grant* decision may read `flags` directly — `NONE` grants nothing either
    /// way — but a *deny* decision must not, and this is the regression that
    /// proved it: treating unrecorded as read-only refused legitimate CoW breaks
    /// and killed `rustc` mid-build.
    #[test]
    fn unrecorded_none_and_explicit_prot_none_are_distinguishable() {
        let unrecorded = MmapRegion::owned(0x1000_0000, frames(1));
        assert_eq!(unrecorded.flags, user_flags::NONE, "the safe default is still NONE");
        assert_eq!(unrecorded.recorded_prot(), None, "…but it states nothing");

        let explicit = MmapRegion::owned_with_flags(0x1000_0000, frames(1), user_flags::NONE);
        assert_eq!(explicit.flags, unrecorded.flags, "identical in the flags field");
        assert_eq!(
            explicit.recorded_prot(),
            Some(user_flags::NONE),
            "and yet distinguishable — this is the whole point"
        );
    }

    /// Same for the inherited constructors, because a CoW child of an unrecorded
    /// region is itself unrecorded.
    #[test]
    fn inherited_constructors_split_the_same_way() {
        assert_eq!(MmapRegion::inherited(0x1000_0000, 2).recorded_prot(), None);
        assert_eq!(
            MmapRegion::inherited_with_flags(0x1000_0000, 2, user_flags::RW).recorded_prot(),
            Some(user_flags::RW)
        );
    }

    /// A CoW child must carry the bit, or every forked process looks unrecorded
    /// and `mprotect` stops being enforced exactly where the probe exercises it.
    #[test]
    fn cow_inheritance_carries_whether_protection_was_recorded() {
        let parent = alloc::vec![
            MmapRegion::owned_with_flags(0x1000_0000, frames(1), user_flags::RO),
            MmapRegion::owned(0x2000_0000, frames(1)),
        ];
        let child = inherit_mmap_regions_for_cow_child(&parent);
        assert_eq!(child[0].recorded_prot(), Some(user_flags::RO), "statement survives fork");
        assert_eq!(child[1].recorded_prot(), None, "and so does its absence");
    }

    /// Splitting changes extent, not what a region states about itself — both
    /// survivors keep it.
    #[test]
    fn detach_survivors_keep_the_recorded_flag() {
        for (recorded, want) in [(true, Some(user_flags::RW)), (false, None)] {
            let mut r = alloc::vec![if recorded {
                MmapRegion::owned_with_flags(0x1000_0000, frames(6), user_flags::RW)
            } else {
                MmapRegion::owned(0x1000_0000, frames(6))
            }];
            let _ = detach_eager_regions_in_range(&mut r, 0x1000_2000, 0x1000_4000);
            assert_eq!(r.len(), 2, "middle detach leaves a head and a tail");
            for survivor in &r {
                assert_eq!(survivor.recorded_prot().is_some(), recorded, "{want:?}");
            }
        }
    }

    /// Total pages a region list covers, for conservation assertions.
    fn total_pages(r: &[MmapRegion]) -> usize {
        r.iter().map(|x| x.pages).sum()
    }

    /// A range covering the whole region detaches it entirely, leaving nothing.
    #[test]
    fn detach_full_region_removes_it() {
        let mut r = alloc::vec![MmapRegion::owned_with_flags(0x1000_0000, frames(4), user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_4000);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, 0x1000_0000);
        assert_eq!(pieces[0].1, 4);
        assert_eq!(pieces[0].2.len(), 4, "all frames go with the detached pages");
        assert!(r.is_empty(), "nothing should survive a full-cover unmap");
    }

    /// A prefix unmap keeps the suffix, with the suffix's frames and its flags.
    #[test]
    fn detach_prefix_keeps_suffix() {
        let mut r = alloc::vec![MmapRegion::owned_with_flags(0x1000_0000, frames(6), user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_2000);
        assert_eq!(pieces[0].1, 2);
        assert_eq!(pieces[0].2.len(), 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start_va, 0x1000_2000);
        assert_eq!(r[0].pages, 4);
        assert_eq!(r[0].frames.len(), 4);
        assert_eq!(r[0].flags, user_flags::RW, "extent changed, permission did not");
    }

    /// A suffix unmap keeps the head at the original base.
    #[test]
    fn detach_suffix_keeps_head() {
        let mut r = alloc::vec![MmapRegion::owned_with_flags(0x1000_0000, frames(6), user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_4000, 0x1000_6000);
        assert_eq!(pieces[0].0, 0x1000_4000);
        assert_eq!(pieces[0].1, 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start_va, 0x1000_0000);
        assert_eq!(r[0].pages, 4);
    }

    /// A middle unmap leaves TWO survivors, and every frame lands in exactly one
    /// of the three parts.
    #[test]
    fn detach_middle_splits_into_two_survivors() {
        let mut r = alloc::vec![MmapRegion::owned_with_flags(0x1000_0000, frames(6), user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_2000, 0x1000_4000);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].0, 0x1000_2000);
        assert_eq!(pieces[0].1, 2);
        assert_eq!(r.len(), 2, "head and tail both survive a middle unmap");
        assert_eq!(total_pages(&r), 4);
        let frames_kept: usize = r.iter().map(|x| x.frames.len()).sum();
        assert_eq!(frames_kept + pieces[0].2.len(), 6, "frames are conserved across the split");
    }

    /// The defect this function exists to fix: a range spanning several regions
    /// must detach ALL of them, not just the one starting at `addr`.
    #[test]
    fn detach_spans_multiple_regions() {
        let mut r = alloc::vec![
            MmapRegion::owned_with_flags(0x1000_0000, frames(2), user_flags::RW),
            MmapRegion::owned_with_flags(0x1000_2000, frames(2), user_flags::RO),
            MmapRegion::owned_with_flags(0x1000_4000, frames(2), user_flags::RW),
        ];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_6000);
        assert_eq!(pieces.len(), 3, "every overlapped region must be detached");
        let detached_pages: usize = pieces.iter().map(|p| p.1).sum();
        assert_eq!(detached_pages, 6);
        assert!(r.is_empty());
    }

    /// An unmap starting mid-region and running into the next one: the old
    /// exact-`start_va` match found nothing here and silently unmapped nothing.
    #[test]
    fn detach_starting_mid_region_reaches_the_next() {
        let mut r = alloc::vec![
            MmapRegion::owned_with_flags(0x1000_0000, frames(4), user_flags::RW),
            MmapRegion::owned_with_flags(0x1000_4000, frames(4), user_flags::RW),
        ];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_2000, 0x1000_6000);
        let detached_pages: usize = pieces.iter().map(|p| p.1).sum();
        assert_eq!(detached_pages, 4, "2 pages from each region");
        assert_eq!(total_pages(&r), 4, "the untouched halves survive");
    }

    /// A CoW-inherited region owns no frames but still covers pages; its extent
    /// must split without inventing frames for the pieces.
    #[test]
    fn detach_cow_inherited_region_splits_by_pages() {
        let mut r = alloc::vec![MmapRegion::inherited_with_flags(0x1000_0000, 6, user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_2000, 0x1000_4000);
        assert_eq!(pieces[0].1, 2, "pages come from `pages`, not `frames.len()`");
        assert!(pieces[0].2.is_empty(), "an inherited region owns no frames to hand over");
        assert_eq!(total_pages(&r), 4);
        assert!(r.iter().all(|x| x.frames.is_empty()));
    }

    /// A range touching nothing leaves the list alone.
    #[test]
    fn detach_non_overlapping_range_is_a_noop() {
        let mut r = alloc::vec![MmapRegion::owned_with_flags(0x1000_0000, frames(2), user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x2000_0000, 0x2000_2000);
        assert_eq!(pieces.len(), 0);
        assert_eq!(r.len(), 1);
        assert_eq!(total_pages(&r), 2);
    }

    /// An empty range detaches nothing — and, since survivors are re-pushed onto
    /// the same vector the loop is scanning, must not spin.
    #[test]
    fn detach_empty_range_terminates() {
        let mut r = alloc::vec![MmapRegion::owned_with_flags(0x1000_0000, frames(2), user_flags::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_0000);
        assert_eq!(pieces.len(), 0);
        assert_eq!(total_pages(&r), 2);
    }

}
