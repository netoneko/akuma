//! The eager-mapping record and the two pure operations over a region list.
//!
//! Both operations take the list itself — a slice or a `&mut Vec` — never a process.
//! That shape predates this crate: `detach_eager_regions_in_range` was already split
//! out of `sys_munmap` so it could be tested without a live process, and the tests at
//! the bottom of this file came with it. The crate boundary is what makes the shape
//! permanent rather than a convention.

use alloc::vec::Vec;

use crate::{PhysFrame, Prot};

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
    /// The protection this mapping is *supposed* to have, as the neutral [`Prot`]
    /// vocabulary — the eager counterpart of `LazyRegion::flags`, which is still a
    /// raw AArch64 `u64` because it lives in `akuma-exec`, above this crate.
    ///
    /// Without it an eager region records extent and frames but no permission, so
    /// the EL0 write-permission-fault handler cannot tell a PTE that is wrongly
    /// read-only (page state lost some other way) from a mapping that is
    /// legitimately read-only (`mprotect(PROT_READ)`). Lazy regions carry flags and
    /// therefore get a permission upgrade; eager regions had no such path and died
    /// with SIGSEGV instead. See
    /// `docs/archive/J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §3.
    pub prot: Prot,

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

    /// Whether [`prot`](Self::prot) is a **statement** about this mapping's
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

    /// Where this region's pages come from when it is **file-backed and
    /// demand-paged**: the file's identity and this region's place in it.
    ///
    /// `None` for anonymous memory and for a file mapping whose pages were
    /// filled at `mmap` time — an eager file region needs no record, because
    /// nothing will ever ask it for bytes again.
    ///
    /// It lives here rather than beside the region list because a region is the
    /// only thing that survives the operations that reshape a mapping.
    /// `mprotect` splits one region into three and `munmap` clips it at either
    /// end; a parallel table keyed by VA would have to be taught each of those
    /// shapes over again, and the first one it was not taught would serve a page
    /// the bytes that belong 64 KiB away. Carried through them here instead, by
    /// [`FileBacking::advance`], with the offsets tested.
    ///
    /// **The identity is integers only, deliberately.** Keeping the file's data
    /// alive across an `unlink` needs an `akuma_primitives::InodePin`, and this
    /// crate has an empty `[dependencies]` table that is load-bearing (see the
    /// crate docs). The pin is the kernel's to hold; this is the record of
    /// *which* file, not a claim on it.
    pub file: Option<FileBacking>,
}

/// The file identity and extent behind a demand-paged file mapping.
///
/// `Copy` and four integers: a region's file backing has to survive every clip
/// and split in this module, and a type that could not be copied would make each
/// of those a decision about ownership instead of arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileBacking {
    /// Which mount `inode` belongs to (`akuma_vfs::ResolvedMount::id`).
    ///
    /// An inode number alone does not name a file — a second `mount(2)` puts
    /// another filesystem's numbers in the same range — and this pair is what a
    /// global page cache must be keyed by.
    pub mount_id: u32,
    /// The inode the mapping was created against. Never `0`: a mapping with no
    /// inode identity has to read by path, and a path is not something this
    /// crate can hold.
    pub inode: u32,
    /// Byte offset in the file of this region's `start_va`.
    pub offset: usize,
    /// Bytes of **file data** reachable from [`offset`](Self::offset).
    ///
    /// Everything past it is zero-fill: a mapping may legitimately extend beyond
    /// EOF, and `mmap(2)` specifies the remainder of the last page as zero. Kept
    /// as a length rather than a file size so a clipped head adjusts it by the
    /// same arithmetic that adjusts the offset.
    pub filesz: usize,
}

impl FileBacking {
    /// Where the page `page_index` pages into this region gets its bytes:
    /// `(file offset, bytes of file data)`.
    ///
    /// The second half is what stops a mapping that extends past EOF showing the
    /// file's neighbours: a page fully inside the data reports a whole page, the
    /// page straddling EOF reports the part that is real, and a page entirely
    /// past it reports `0` — all zero-fill.
    ///
    /// The rule lives here, in one place, because both callers need it and they
    /// are far apart: [`MmapRegion::file_page_source`] answers it for a VA, and
    /// a fault path filling a readahead batch answers it for page after page
    /// without a region in hand.
    #[must_use]
    pub const fn page_source(self, page_index: usize) -> (usize, usize) {
        let delta = page_index.saturating_mul(crate::PAGE_SIZE);
        let from_file = {
            let left = self.filesz.saturating_sub(delta);
            if left > crate::PAGE_SIZE { crate::PAGE_SIZE } else { left }
        };
        (self.offset.saturating_add(delta), from_file)
    }

    /// This backing with its first `pages` pages clipped away — the record a
    /// surviving *tail* piece needs after a split.
    ///
    /// Saturating on `filesz`: a piece that starts past EOF has no file data at
    /// all, which is `0` and not a wrap to `usize::MAX`.
    #[must_use]
    pub const fn advance(self, pages: usize) -> Self {
        let bytes = pages.saturating_mul(crate::PAGE_SIZE);
        Self {
            mount_id: self.mount_id,
            inode: self.inode,
            offset: self.offset.saturating_add(bytes),
            filesz: self.filesz.saturating_sub(bytes),
        }
    }
}

impl MmapRegion {
    /// Region created by this process: it owns every frame, protection unrecorded.
    ///
    /// Defaults to `NONE` **deliberately**. `flags` exists so the fault handler can
    /// grant a write it would otherwise refuse, so an unknown protection has to be
    /// the one that grants nothing: a wrong `RW` default would silently defeat
    /// `mprotect(PROT_READ)` on any region built through this constructor. `NONE`
    /// leaves such a region behaving exactly as it did before `flags` existed.
    /// Callers that know the real protection use [`MmapRegion::owned_with_prot`].
    #[must_use]
    pub fn owned(start_va: usize, frames: Vec<PhysFrame>) -> Self {
        let mut r = Self::owned_with_prot(start_va, frames, crate::Prot::NONE);
        // `NONE` here is the safe default, NOT a statement — see `prot_recorded`.
        r.prot_recorded = false;
        r
    }

    /// Region created by this process, with its real protection recorded.
    #[must_use]
    pub fn owned_with_prot(start_va: usize, frames: Vec<PhysFrame>, prot: Prot) -> Self {
        Self {
            start_va, pages: frames.len(), frames, prot,
            shared_anon: false, prot_recorded: true, file: None,
        }
    }

    /// Region inherited by a CoW-forked child: extent known, no owned frames,
    /// protection unrecorded (`NONE` — see [`MmapRegion::owned`] for why).
    #[must_use]
    pub fn inherited(start_va: usize, pages: usize) -> Self {
        let mut r = Self::inherited_with_prot(start_va, pages, crate::Prot::NONE);
        r.prot_recorded = false;
        r
    }

    /// Region inherited by a CoW-forked child, carrying the parent's protection.
    #[must_use]
    pub fn inherited_with_prot(start_va: usize, pages: usize, prot: Prot) -> Self {
        Self {
            start_va, pages, frames: Vec::new(), prot,
            shared_anon: false, prot_recorded: true, file: None,
        }
    }

    /// The protection this region **states**, or `None` if it never recorded one.
    ///
    /// The accessor a *deny* decision must use. A *grant* decision can read
    /// [`prot`](Self::prot) directly, because the unrecorded default (`NONE`)
    /// grants nothing anyway.
    #[must_use]
    pub const fn recorded_prot(&self) -> Option<Prot> {
        if self.prot_recorded { Some(self.prot) } else { None }
    }

    /// Mark this region `MAP_SHARED | MAP_ANONYMOUS`. See [`MmapRegion::shared_anon`].
    #[must_use]
    pub fn shared_anon(mut self) -> Self {
        self.shared_anon = true;
        self
    }

    /// Mark this region as demand-paged from a file. See [`MmapRegion::file`].
    #[must_use]
    pub fn file_backed(mut self, file: FileBacking) -> Self {
        self.file = Some(file);
        self
    }

    /// Where the page at `va` gets its bytes: `(file offset, bytes of file data)`.
    ///
    /// The second half is what stops a mapping that extends past EOF showing the
    /// file's neighbours: a page fully inside the data reports a whole page, the
    /// page straddling EOF reports the part that is real, and a page entirely
    /// past it reports `0` — all zero-fill. `None` when the region is not
    /// file-backed or `va` is outside it, which the caller must treat as
    /// anonymous rather than as an error.
    #[must_use]
    pub fn file_page_source(&self, va: usize) -> Option<(usize, usize)> {
        let file = self.file?;
        if !self.contains(va) {
            return None;
        }
        let page = va & !(crate::PAGE_SIZE - 1);
        Some(file.page_source((page - self.start_va) / crate::PAGE_SIZE))
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
            let mut inherited = MmapRegion::inherited_with_prot(r.start_va, r.pages, r.prot);
            // Carry `prot_recorded` too: a child of an unrecorded region is itself
            // unrecorded, and a child of an `mprotect`ed one keeps the statement.
            inherited.prot_recorded = r.prot_recorded;
            // Must carry `shared_anon` across, or a grandchild silently stops sharing:
            // the child would CoW-share a mapping its parent shares by identity.
            // And the file backing, for the same reason one step further on: a
            // child of a demand-paged file mapping faults on pages its parent
            // never touched, and a child that forgot where they come from
            // would serve them as anonymous zeros — a program image full of
            // holes, reported as a `SIGSEGV` or worse as silence.
            inherited.file = r.file;
            if r.shared_anon { inherited.shared_anon() } else { inherited }
        })
        .collect()
}

/// Apply `new_prot` to exactly `[range_start, range_end)`, **splitting** any
/// region the range only partly covers.
///
/// # Why this exists
///
/// `mprotect` used to record its new protection against the *whole* overlapping
/// region, and the comment justifying that said the widening was safe "because
/// the fault handler only ever uses these flags to grant a write". That is true
/// for granting and false for refusing — and the moment the EL0 write-fault
/// handler started using the record to *refuse* a CoW break, a guard page
/// `mprotect(PROT_NONE)`-ed inside a larger mapping recorded `NONE` for every page
/// of it and killed `rustc` mid-build (three `[MPROTECT-DENY]` addresses, three
/// `SIGSEGV`s at exactly those addresses).
///
/// The old objection to splitting was that "`MmapRegion` keys its frame list to
/// `start_va`, and splitting one would have to split `frames` in step".
/// [`detach_eager_regions_in_range`] has done exactly that all along; this is the
/// same walk, keeping every piece instead of handing the middle back.
///
/// Every surviving piece — including the untouched head and tail — is a real
/// region with its own frames, so the record is now page-accurate and a *deny*
/// decision can be built on it.
///
/// Returns the number of regions the range touched, for the caller's counter.
pub fn mprotect_eager_regions_in_range(
    regions: &mut alloc::vec::Vec<MmapRegion>,
    range_start: usize,
    range_end: usize,
    new_prot: Prot,
) -> usize {
    if range_end <= range_start {
        return 0;
    }
    // Single pass into a fresh vector. The in-place `remove`/`push` shape that
    // `detach_eager_regions_in_range` uses cannot be borrowed here: the middle
    // piece it pushes is *fully covered* by the range, so the loop re-examines it,
    // takes the fast path and counts it a second time. Draining sidesteps that
    // entirely — every input region is considered exactly once.
    let mut touched = 0usize;
    let mut out: alloc::vec::Vec<MmapRegion> = alloc::vec::Vec::with_capacity(regions.len() + 2);
    for mut reg in regions.drain(..) {
        let reg_start = reg.start_va;
        let reg_end = reg_start + reg.pages * crate::PAGE_SIZE;
        if reg_start >= range_end || reg_end <= range_start {
            out.push(reg);
            continue;
        }
        let clip_start = range_start.max(reg_start);
        let clip_end = range_end.min(reg_end);
        let head_pages = (clip_start - reg_start) / crate::PAGE_SIZE;
        let mid_pages = (clip_end - clip_start) / crate::PAGE_SIZE;
        let tail_pages = (reg_end - clip_end) / crate::PAGE_SIZE;

        // Fully covered: no split, and the common "mprotect a whole mapping" case
        // does not churn the vector.
        if head_pages == 0 && tail_pages == 0 {
            reg.prot = new_prot;
            reg.prot_recorded = true;
            touched += 1;
            out.push(reg);
            continue;
        }

        let old_prot = reg.prot;
        let shared_anon = reg.shared_anon;
        let was_recorded = reg.prot_recorded;
        // Each piece keeps its own place in the file: the head starts where the
        // region did, and the two below it start that many pages further in.
        let file = reg.file;
        // `filter_map(next)` tolerates a CoW-inherited region (`frames` empty):
        // each piece then carries its page count and no frames, which is right.
        let mut it = reg.frames.into_iter();
        let head: alloc::vec::Vec<PhysFrame> = (0..head_pages).filter_map(|_| it.next()).collect();
        let mid: alloc::vec::Vec<PhysFrame> = (0..mid_pages).filter_map(|_| it.next()).collect();
        let tail: alloc::vec::Vec<PhysFrame> = it.collect();

        // Head and tail keep what the ORIGINAL region said — including whether it
        // said anything at all. Only the middle was named by this `mprotect`.
        if head_pages > 0 {
            out.push(MmapRegion {
                start_va: reg_start, pages: head_pages, frames: head,
                prot: old_prot, shared_anon, prot_recorded: was_recorded, file });
        }
        if mid_pages > 0 {
            out.push(MmapRegion {
                start_va: clip_start, pages: mid_pages, frames: mid,
                prot: new_prot, shared_anon, prot_recorded: true,
                file: file.map(|f| f.advance(head_pages)) });
            touched += 1;
        }
        if tail_pages > 0 {
            out.push(MmapRegion {
                start_va: clip_end, pages: tail_pages, frames: tail,
                prot: old_prot, shared_anon, prot_recorded: was_recorded,
                file: file.map(|f| f.advance(head_pages + mid_pages)) });
        }
    }
    *regions = out;
    touched
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
        //
        // Taken out of the slot rather than by removing the region, because the
        // region usually stays: a clip at either edge leaves one survivor, and
        // that survivor can reuse the slot the original occupied.
        let prot = regions[i].prot;
        // A partial unmap changes extent, not identity: both survivors are still the
        // same `MAP_SHARED|MAP_ANONYMOUS` object if the original was.
        let shared_anon = regions[i].shared_anon;
        // A partial unmap changes extent, not what the region states about itself.
        let prot_recorded = regions[i].prot_recorded;
        // Nor where the region sits in its file — but the surviving *tail*
        // starts further in, by everything the head and the clip took.
        let file = regions[i].file;
        let mut it = core::mem::take(&mut regions[i].frames).into_iter();
        let head: alloc::vec::Vec<PhysFrame> = (0..head_pages).filter_map(|_| it.next()).collect();
        let mid: alloc::vec::Vec<PhysFrame> = (0..clip_pages).filter_map(|_| it.next()).collect();
        let tail: alloc::vec::Vec<PhysFrame> = it.collect();

        // Survivors keep the protection of the region they came from: a partial
        // unmap changes extent, not permission.
        //
        // **They stay where the region they came from was**, in address order,
        // rather than being appended. Both are carved out of the extent that was
        // in slot `i`, so that is where they belong — and the list stays sorted,
        // which is the property `amd64`'s first-fit placer needs.
        //
        // That matters because it had none. `find_free_va` is a first-fit scan,
        // so it sorted the list itself on every `mmap`; with the order destroyed
        // by every partial `munmap`, that sort stopped being the adaptive linear
        // re-confirm its comment claimed and became real work. Measured
        // 2026-09-17 in an in-guest `cargo build -p zerocopy`: 4 400 regions,
        // 18 % of calls arriving unsorted, **468 us of a 483 us `mmap`** inside
        // `sort_unstable_by_key` — 74 % of `rustc`'s wall clock.
        // See docs/archive/AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md §8.
        //
        // It is also **strictly less shifting than the `remove`-and-`push` it
        // replaces**, which is what makes it free for the AArch64 kernel — that
        // one calls this too (`akuma-syscalls-glue`'s `munmap`) and gets no
        // benefit from the ordering, because it places from a bump cursor and a
        // free list and never scans this list for a gap. The old shape shifted
        // the tail once unconditionally (`Vec::remove`); this one shifts it only
        // for a middle split, which produces two survivors, or for a whole-region
        // unmap, which produces none. A clip at either edge now moves nothing.
        let mut next = i;
        if head_pages > 0 {
            regions[i].pages = head_pages;
            regions[i].frames = head;
            next += 1;
        }
        if tail_pages > 0 {
            let tail_region = MmapRegion {
                start_va: clip_end, pages: tail_pages, frames: tail, prot,
                shared_anon, prot_recorded,
                file: file.map(|f| f.advance(head_pages + clip_pages)) };
            if next == i {
                regions[i] = tail_region;
            } else {
                regions.insert(next, tail_region);
            }
            next += 1;
        }
        if next == i {
            // Wholly inside the range: no survivor, so the slot goes.
            regions.remove(i);
        }
        if clip_pages > 0 {
            pieces.push((clip_start, clip_pages, mid));
        }
        // Step over the survivors. They lie outside [range_start, range_end) by
        // construction, so re-examining them could only waste overlap tests —
        // and `remove` above already shifted the next candidate into `i`.
        i = next;
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
        let rw = MmapRegion::owned_with_prot(0x2012_0000, frames(2), Prot::RW_NO_EXEC);
        let ro = MmapRegion::owned_with_prot(0x2013_0000, frames(1), Prot::RO);
        let unknown = MmapRegion::owned(0x2014_0000, frames(1));

        let writable = |r: &MmapRegion| r.prot.is_write();
        assert!(writable(&rw), "a PROT_WRITE region must permit the upgrade");
        assert!(!writable(&ro), "mprotect(PROT_READ) must still fault");
        assert!(!writable(&unknown),
            "an unrecorded protection must grant nothing — a permissive default \
             would silently defeat mprotect on every region built this way");

        let child = inherit_mmap_regions_for_cow_child(&[rw, ro]);
        assert_eq!(child[0].prot, Prot::RW_NO_EXEC, "child loses the repair path otherwise");
        assert_eq!(child[1].prot, Prot::RO, "child must not gain write on a RO mapping");
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
        assert_eq!(unrecorded.prot, Prot::NONE, "the safe default is still NONE");
        assert_eq!(unrecorded.recorded_prot(), None, "…but it states nothing");

        let explicit = MmapRegion::owned_with_prot(0x1000_0000, frames(1), Prot::NONE);
        assert_eq!(explicit.prot, unrecorded.prot, "identical in the flags field");
        assert_eq!(
            explicit.recorded_prot(),
            Some(Prot::NONE),
            "and yet distinguishable — this is the whole point"
        );
    }

    /// Same for the inherited constructors, because a CoW child of an unrecorded
    /// region is itself unrecorded.
    #[test]
    fn inherited_constructors_split_the_same_way() {
        assert_eq!(MmapRegion::inherited(0x1000_0000, 2).recorded_prot(), None);
        assert_eq!(
            MmapRegion::inherited_with_prot(0x1000_0000, 2, Prot::RW).recorded_prot(),
            Some(Prot::RW)
        );
    }

    /// A CoW child must carry the bit, or every forked process looks unrecorded
    /// and `mprotect` stops being enforced exactly where the probe exercises it.
    #[test]
    fn cow_inheritance_carries_whether_protection_was_recorded() {
        let parent = alloc::vec![
            MmapRegion::owned_with_prot(0x1000_0000, frames(1), Prot::RO),
            MmapRegion::owned(0x2000_0000, frames(1)),
        ];
        let child = inherit_mmap_regions_for_cow_child(&parent);
        assert_eq!(child[0].recorded_prot(), Some(Prot::RO), "statement survives fork");
        assert_eq!(child[1].recorded_prot(), None, "and so does its absence");
    }

    /// Splitting changes extent, not what a region states about itself — both
    /// survivors keep it.
    #[test]
    fn detach_survivors_keep_the_recorded_flag() {
        for (recorded, want) in [(true, Some(Prot::RW)), (false, None)] {
            let mut r = alloc::vec![if recorded {
                MmapRegion::owned_with_prot(0x1000_0000, frames(6), Prot::RW)
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

    // ── `mprotect_eager_regions_in_range` — the split that makes the record
    // page-accurate. Every shape is pinned: this function's imprecision is what
    // killed `rustc`, so "it works on the whole-region case" is not enough.

    /// The whole-region case takes the fast path: flags updated, no split.
    #[test]
    fn mprotect_fully_covered_region_is_not_split() {
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(4), Prot::RW)];
        let n = mprotect_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_4000, Prot::RO);
        assert_eq!(n, 1);
        assert_eq!(r.len(), 1, "no split needed");
        assert_eq!(r[0].recorded_prot(), Some(Prot::RO));
        assert_eq!(r[0].pages, 4);
    }

    /// **The regression shape.** A guard page in the middle must leave the pages
    /// around it untouched — this is precisely what the old whole-region widening
    /// got wrong, and what refused `rustc`'s legitimate writes.
    #[test]
    fn mprotect_middle_page_leaves_its_neighbours_writable() {
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(8), Prot::RW)];
        let n = mprotect_eager_regions_in_range(&mut r, 0x1000_3000, 0x1000_4000, Prot::NONE);
        assert_eq!(n, 1);
        assert_eq!(r.len(), 3, "head + guard + tail");
        assert_eq!(total_pages(&r), 8, "no page invented or lost");
        let guard = r.iter().find(|x| x.start_va == 0x1000_3000).expect("guard piece");
        assert_eq!(guard.pages, 1);
        assert_eq!(guard.recorded_prot(), Some(Prot::NONE));
        for other in r.iter().filter(|x| x.start_va != 0x1000_3000) {
            assert_eq!(
                other.recorded_prot(),
                Some(Prot::RW),
                "a neighbour of the guard page must stay writable — the rustc bug"
            );
        }
    }

    /// Frames follow their pages across the split, or a survivor points at the
    /// wrong physical memory.
    #[test]
    fn mprotect_split_moves_frames_in_step_with_pages() {
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(6), Prot::RW)];
        mprotect_eager_regions_in_range(&mut r, 0x1000_2000, 0x1000_4000, Prot::RO);
        for piece in &r {
            assert_eq!(piece.frames.len(), piece.pages, "frame count must track extent");
            for i in 0..piece.pages {
                let va = piece.start_va + i * 4096;
                let want = 0x4000_0000 + (va - 0x1000_0000);
                assert_eq!(piece.frame_for(va).map(|f| f.addr), Some(want), "va {va:#x}");
            }
        }
    }

    /// A prefix and a suffix `mprotect` each split into exactly two.
    #[test]
    fn mprotect_prefix_and_suffix_split_into_two() {
        for (start, end, marked_at) in
            [(0x1000_0000usize, 0x1000_2000usize, 0x1000_0000usize),
             (0x1000_4000, 0x1000_6000, 0x1000_4000)]
        {
            let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(6), Prot::RW)];
            mprotect_eager_regions_in_range(&mut r, start, end, Prot::RO);
            assert_eq!(r.len(), 2);
            assert_eq!(total_pages(&r), 6);
            let marked = r.iter().find(|x| x.start_va == marked_at).unwrap();
            assert_eq!(marked.recorded_prot(), Some(Prot::RO));
        }
    }

    /// A CoW-inherited region owns no frames but has every page mapped; splitting
    /// it must produce pieces with correct extents and no frames.
    #[test]
    fn mprotect_splits_a_cow_inherited_region_by_pages() {
        let mut r = alloc::vec![MmapRegion::inherited_with_prot(0x2000_0000, 4, Prot::RW)];
        mprotect_eager_regions_in_range(&mut r, 0x2000_1000, 0x2000_2000, Prot::RO);
        assert_eq!(total_pages(&r), 4);
        for piece in &r {
            assert!(piece.frames.is_empty(), "inherited pieces own no frames");
        }
    }

    /// A range covering several regions marks each of them, and an empty or
    /// non-overlapping range does nothing and terminates.
    #[test]
    fn mprotect_spans_regions_and_ignores_empty_ranges() {
        let mut r = alloc::vec![
            MmapRegion::owned_with_prot(0x1000_0000, frames(2), Prot::RW),
            MmapRegion::owned_with_prot(0x1000_2000, frames(2), Prot::RW),
        ];
        assert_eq!(mprotect_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_4000, Prot::RO), 2);
        assert!(r.iter().all(|x| x.recorded_prot() == Some(Prot::RO)));
        assert_eq!(mprotect_eager_regions_in_range(&mut r, 0x9000_0000, 0x9000_1000, Prot::RW), 0);
        assert_eq!(mprotect_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_0000, Prot::RW), 0);
        assert_eq!(total_pages(&r), 4);
    }

    /// Head and tail keep whether the ORIGINAL region had recorded anything —
    /// an `mprotect` names the middle, and says nothing about the rest.
    #[test]
    fn mprotect_split_preserves_the_neighbours_recorded_state() {
        let mut r = alloc::vec![MmapRegion::owned(0x1000_0000, frames(4))];
        assert_eq!(r[0].recorded_prot(), None);
        mprotect_eager_regions_in_range(&mut r, 0x1000_1000, 0x1000_2000, Prot::RO);
        assert_eq!(r.len(), 3);
        for piece in &r {
            if piece.start_va == 0x1000_1000 {
                assert_eq!(piece.recorded_prot(), Some(Prot::RO));
            } else {
                assert_eq!(piece.recorded_prot(), None, "neighbours were never named");
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
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(4), Prot::RW)];
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
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(6), Prot::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_2000);
        assert_eq!(pieces[0].1, 2);
        assert_eq!(pieces[0].2.len(), 2);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start_va, 0x1000_2000);
        assert_eq!(r[0].pages, 4);
        assert_eq!(r[0].frames.len(), 4);
        assert_eq!(r[0].prot, Prot::RW, "extent changed, permission did not");
    }

    /// A suffix unmap keeps the head at the original base.
    #[test]
    fn detach_suffix_keeps_head() {
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(6), Prot::RW)];
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
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(6), Prot::RW)];
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
            MmapRegion::owned_with_prot(0x1000_0000, frames(2), Prot::RW),
            MmapRegion::owned_with_prot(0x1000_2000, frames(2), Prot::RO),
            MmapRegion::owned_with_prot(0x1000_4000, frames(2), Prot::RW),
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
            MmapRegion::owned_with_prot(0x1000_0000, frames(4), Prot::RW),
            MmapRegion::owned_with_prot(0x1000_4000, frames(4), Prot::RW),
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
        let mut r = alloc::vec![MmapRegion::inherited_with_prot(0x1000_0000, 6, Prot::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_2000, 0x1000_4000);
        assert_eq!(pieces[0].1, 2, "pages come from `pages`, not `frames.len()`");
        assert!(pieces[0].2.is_empty(), "an inherited region owns no frames to hand over");
        assert_eq!(total_pages(&r), 4);
        assert!(r.iter().all(|x| x.frames.is_empty()));
    }

    /// A range touching nothing leaves the list alone.
    #[test]
    fn detach_non_overlapping_range_is_a_noop() {
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(2), Prot::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x2000_0000, 0x2000_2000);
        assert_eq!(pieces.len(), 0);
        assert_eq!(r.len(), 1);
        assert_eq!(total_pages(&r), 2);
    }

    /// An empty range detaches nothing — and, since survivors are re-pushed onto
    /// the same vector the loop is scanning, must not spin.
    #[test]
    fn detach_empty_range_terminates() {
        let mut r = alloc::vec![MmapRegion::owned_with_prot(0x1000_0000, frames(2), Prot::RW)];
        let pieces = detach_eager_regions_in_range(&mut r, 0x1000_0000, 0x1000_0000);
        assert_eq!(pieces.len(), 0);
        assert_eq!(total_pages(&r), 2);
    }

}

#[cfg(test)]
mod file_backing_tests {
    //! Where a demand-paged file mapping's pages come from, through every
    //! operation that reshapes a region.
    //!
    //! These are the tests the amd64 lazy file mapping is built on, and they
    //! exist because the failure they guard is silent. A region that loses its
    //! file backing serves anonymous zeros — a hole in a program image, which
    //! surfaces as a `SIGILL` or a wrong answer, not as an error. A region that
    //! keeps the backing but not the *offset* is worse: every page is real file
    //! data, from the wrong place in the file.
    use super::*;

    const PAGE: usize = 4096;

    fn backing(offset: usize, filesz: usize) -> FileBacking {
        FileBacking { mount_id: 2, inode: 77, offset, filesz }
    }

    fn file_region(start_va: usize, pages: usize, file: FileBacking) -> MmapRegion {
        MmapRegion::inherited_with_prot(start_va, pages, crate::Prot::RO_NO_EXEC)
            .file_backed(file)
    }

    /// A plain anonymous region — the shape the ordering tests below care about,
    /// where only the extent matters.
    fn region(start_va: usize, pages: usize) -> MmapRegion {
        MmapRegion::inherited_with_prot(start_va, pages, crate::Prot::RO_NO_EXEC)
    }

    /// A page wholly inside the file's data reads a whole page from it; the page
    /// straddling EOF reads the part that exists; past EOF reads nothing.
    #[test]
    fn page_source_tracks_eof() {
        // 4 pages mapped, 2.5 pages of file data behind them.
        let r = file_region(0x1_0000_0000, 4, backing(0x2000, 2 * PAGE + 2048));
        assert_eq!(r.file_page_source(0x1_0000_0000), Some((0x2000, PAGE)));
        assert_eq!(r.file_page_source(0x1_0000_1000), Some((0x3000, PAGE)));
        assert_eq!(r.file_page_source(0x1_0000_2000), Some((0x4000, 2048)));
        assert_eq!(r.file_page_source(0x1_0000_3000), Some((0x5000, 0)));
    }

    /// An address inside the page, not just its base, answers for that page.
    #[test]
    fn page_source_rounds_the_fault_address_down() {
        let r = file_region(0x1_0000_0000, 2, backing(0, 2 * PAGE));
        assert_eq!(r.file_page_source(0x1_0000_1abc), Some((PAGE, PAGE)));
    }

    /// Not file-backed, or not in this region: `None`, which the fault path
    /// reads as "anonymous", not as "fail".
    #[test]
    fn page_source_is_none_outside_and_for_anonymous() {
        let r = file_region(0x1_0000_0000, 2, backing(0, 2 * PAGE));
        assert_eq!(r.file_page_source(0x1_0000_2000), None);
        let anon = MmapRegion::inherited(0x1_0000_0000, 2);
        assert_eq!(anon.file_page_source(0x1_0000_0000), None);
    }

    /// A CoW child faults on pages its parent never touched, so it must inherit
    /// where they come from.
    #[test]
    fn cow_child_inherits_the_file_backing() {
        let parent = alloc::vec![file_region(0x1_0000_0000, 4, backing(0x1000, 4 * PAGE))];
        let child = inherit_mmap_regions_for_cow_child(&parent);
        assert_eq!(child[0].file, parent[0].file);
        assert_eq!(child[0].file_page_source(0x1_0000_2000), Some((0x3000, PAGE)));
    }

    /// `mprotect` in the middle of a mapping splits it three ways, and each
    /// piece keeps its own place in the file.
    ///
    /// This is the shape the dynamic linker produces on every shared object it
    /// loads — `mprotect(PROT_READ)` over the relocated middle of a mapping it
    /// made `PROT_READ|PROT_WRITE` — so it is not an exotic case.
    #[test]
    fn mprotect_split_advances_each_piece() {
        let mut regions = alloc::vec![file_region(0x1_0000_0000, 6, backing(0, 6 * PAGE))];
        let touched = mprotect_eager_regions_in_range(
            &mut regions,
            0x1_0000_2000,
            0x1_0000_4000,
            crate::Prot::RO_NO_EXEC,
        );
        assert_eq!(touched, 1);
        regions.sort_by_key(|r| r.start_va);
        assert_eq!(regions.len(), 3);
        assert_eq!(regions[0].file.map(|f| f.offset), Some(0));
        assert_eq!(regions[1].file.map(|f| f.offset), Some(2 * PAGE));
        assert_eq!(regions[2].file.map(|f| f.offset), Some(4 * PAGE));
        // Every piece still answers for its own pages with the offset it had
        // before the split — the property the offsets exist to preserve.
        for i in 0..6 {
            let va = 0x1_0000_0000 + i * PAGE;
            let r = regions.iter().find(|r| r.contains(va)).expect("still mapped");
            assert_eq!(r.file_page_source(va), Some((i * PAGE, PAGE)));
        }
    }

    /// `munmap` of a middle range leaves a head and a tail; the tail starts
    /// further into the file by everything that went away.
    #[test]
    fn munmap_clip_advances_the_tail() {
        let mut regions = alloc::vec![file_region(0x1_0000_0000, 6, backing(0x8000, 6 * PAGE))];
        let pieces = detach_eager_regions_in_range(&mut regions, 0x1_0000_1000, 0x1_0000_3000);
        assert_eq!(pieces.len(), 1);
        regions.sort_by_key(|r| r.start_va);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].file.map(|f| f.offset), Some(0x8000));
        assert_eq!(regions[1].file.map(|f| f.offset), Some(0x8000 + 3 * PAGE));
        assert_eq!(regions[1].file_page_source(0x1_0000_3000), Some((0x8000 + 3 * PAGE, PAGE)));
    }

    /// A split puts its survivors back **where the region was**, so a list that
    /// was in address order still is afterwards.
    ///
    /// This is the property `amd64`'s first-fit placer needs and did not have.
    /// Survivors used to be appended, so every partial `munmap` moved a
    /// low-address region to the end of the list, and the `sort_unstable_by_key`
    /// that `find_free_va` runs per `mmap` stopped being the adaptive linear
    /// re-confirm its comment assumed. Asserted on the *shape* rather than a
    /// time, the same way `va_placement_check` asserts the scan's bound: a
    /// timing test cannot run in a boot suite, and this is what the timing was
    /// a consequence of. See AKUMA_AMD64_SELFHOST_BUILD_SLOWNESS.md §8.
    #[test]
    fn split_keeps_the_list_in_address_order() {
        // Four abutting regions; punch a hole in the middle of the second and
        // of the last, so both the head-and-tail and the tail-only shapes run.
        let mut regions = alloc::vec![
            region(0x1_0000_0000, 4),
            region(0x1_0000_4000, 4),
            region(0x1_0000_8000, 4),
            region(0x1_0000_C000, 4),
        ];
        detach_eager_regions_in_range(&mut regions, 0x1_0000_5000, 0x1_0000_6000);
        assert!(regions.windows(2).all(|w| w[0].start_va < w[1].start_va),
                "head+tail split left the list out of order: {:?}",
                regions.iter().map(|r| r.start_va).collect::<alloc::vec::Vec<_>>());
        detach_eager_regions_in_range(&mut regions, 0x1_0000_C000, 0x1_0000_D000);
        assert!(regions.windows(2).all(|w| w[0].start_va < w[1].start_va),
                "tail-only split left the list out of order: {:?}",
                regions.iter().map(|r| r.start_va).collect::<alloc::vec::Vec<_>>());
        // And the split still did what it is for: 4 + 5 extents, no overlap.
        assert_eq!(regions.len(), 5);
    }

    /// A split still visits every region exactly once.
    ///
    /// The insert above shifts the unexamined tail of the list, so the loop has
    /// to step over what it inserted. Getting that wrong does not crash — it
    /// re-tests two regions that cannot match, or worse skips one that can — so
    /// the observable is that a range spanning *every* region clips all of them.
    #[test]
    fn split_visits_every_region_once() {
        let mut regions: alloc::vec::Vec<MmapRegion> =
            (0..8).map(|i| region(0x1_0000_0000 + i * 4 * PAGE, 4)).collect();
        let pieces = detach_eager_regions_in_range(
            &mut regions, 0x1_0000_0000, 0x1_0000_0000 + 8 * 4 * PAGE);
        assert_eq!(pieces.len(), 8, "every region should have been clipped");
        assert!(regions.is_empty(), "nothing should survive a full-range unmap");
    }

    /// Clipping the head shortens the data behind the survivor as well as
    /// moving its offset — otherwise a region past EOF starts reporting file
    /// bytes that are not there.
    #[test]
    fn advance_shrinks_the_data_length() {
        let f = backing(0, 3 * PAGE);
        assert_eq!(f.advance(2), backing(2 * PAGE, PAGE));
        // Past the end of the data entirely: zero bytes, not a wrap.
        assert_eq!(f.advance(5), backing(5 * PAGE, 0));
    }
}
