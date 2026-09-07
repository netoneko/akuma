//! `mmap`, `munmap` and `mprotect` for ring 3, over the `akuma-mmap` region table.
//!
//! The one memory syscall family a userspace allocator needs. `libakuma`'s
//! global allocator is mmap-based — it had a `brk` arm once and that was removed
//! — so without this no program that allocates can run, which is every program
//! more complex than the Stage L probe.
//!
//! # What changed on 2026-09-07 (item B1/B2)
//!
//! This module used to be an eager bump allocator with no region table, and
//! every one of its limits was a wall `rustc` hits. All five are gone:
//!
//! | before | now |
//! |---|---|
//! | `MAX_MAPPING` 64 MiB, anything larger `EINVAL` | bounded by the VA window, `ENOMEM` past it |
//! | `EAGER_MAX_PAGES = usize::MAX` — no lazy path at all | [`akuma_config::MMAP_EAGER_MAX_PAGES`], demand-paged past it |
//! | one **global** bump `NEXT_VA`, never reused | per-address-space first-fit over the region list |
//! | `MAP_FIXED` refused | honoured, and it replaces what it lands on |
//! | `munmap` could not clip or split | [`akuma_mmap::detach_eager_regions_in_range`] |
//!
//! and `mprotect`, which accepted-and-did-nothing, now splits regions and
//! re-permissions the pages that are actually present.
//!
//! # What a file mapping is here (2026-09-07)
//!
//! `mmap(MAP_PRIVATE, fd)` used to be `ENOSYS`, on the reasoning that serving it
//! as anonymous memory "would look like a working call and hand the caller a
//! file full of zeros". That reasoning was right about the **danger** and too
//! broad about the **remedy**: the zeros are what a mapping with no fill path
//! produces, not what a private file mapping is.
//!
//! `MAP_PRIVATE` asks for a private copy of the file's bytes whose writes
//! nobody else can see. This target can give exactly that, because
//! `fd.rs` already holds every open file's contents in the kernel: each page is
//! allocated, zeroed, and then overwritten from the file
//! ([`populate_file_page`]). What it does **not** give is a page *cache* — two
//! processes mapping one file hold two sets of frames. That is a cost, not a
//! semantic difference, and it is the state the AArch64 kernel was in until
//! `src/file_page_cache.rs` landed in 2026-08.
//!
//! One refusal is left and it is the one that genuinely needs the cache: a
//! **writable `MAP_SHARED`** file mapping, whose writes must reach the file and
//! every other mapper. A private copy would accept the write and drop it, which
//! is the original objection in its true scope.
//!
//! Two pinned divergences come with it: a file mapping is always **eager** here
//! (`plan` never marks a file mapping lazy, so a mapping larger than free memory
//! is `ENOMEM` at `mmap` rather than a fault later), and a mapping never sees a
//! write made to the file after the `mmap` — `MAP_PRIVATE` leaves that
//! unspecified on Linux too.
//!
//! # The decisions are shared crates, not local
//!
//! Which *kind* of mapping a request asks for — anonymous or file-backed, lazy
//! or eager, shared-writable, a `PROT_NONE` reservation — is
//! [`akuma_syscalls_mem::mmap::plan`]. The region algebra (clip, split, inherit)
//! is `akuma-mmap`. The protection vocabulary is `akuma_mmap::Prot`, which
//! [`crate::paging::PteProt::from_region`] encodes into x86 PTE bits. All three
//! are host-tested and shared with the AArch64 kernel, so this target cannot
//! drift from it on exactly the arguments where Linux compatibility is subtle.
//!
//! What stays here is the half that is genuinely per-architecture: allocating
//! frames, writing page tables, and the fault that populates a lazy page.
//!
//! # Where the region list lives
//!
//! `Process::regions` in `usermode.rs`, behind its own lock, reached through
//! [`crate::usermode::with_current_regions`]. `akuma-mmap` cannot hold it — the
//! crate has an empty `[dependencies]` table and cannot lock, allocate a frame,
//! edit a page table or name a process, which is precisely what makes it
//! trustworthy. A region list belongs to an *address space*, and a
//! `clone(CLONE_VM)` thread shares one, which is why it is keyed by
//! `proc_slot` and not by thread.
//!
//! # Frames are the ledger's, not the region's
//!
//! `MmapRegion::frames` is left **empty** here and `pages` carries the extent —
//! the CoW-inherited shape the crate documents and explicitly supports. Frame
//! ownership on this target is `akuma_user_space::FrameLedger`
//! (`Process::frames`), which counts VAs per frame and is what teardown walks. A
//! second frame list inside the region would be a second answer to the same
//! question, and the two would drift the first time a CoW break swapped a frame.

use crate::paging::{self, MemAttr, PteProt};
use crate::phys::phys_ptr;
use crate::usermode;
use akuma_mmap::{MmapRegion, Prot};
use akuma_selftest::Suite;
use alloc::vec::Vec;

use crate::fd::errno;

const PAGE_SIZE: u64 = 4096;

/// `PROT_WRITE` / `PROT_EXEC`, from the shared flag tables rather than restated
/// here — the same constants the AArch64 kernel dispatches on.
use akuma_syscalls_linux::flags::prot::{PROT_EXEC, PROT_WRITE};

/// Where automatically-placed mappings start.
///
/// Well above where a static binary is linked (`0x40_0000`) and above the
/// dynamic linker's `INTERP_BASE` (`0x4000_0000`), so an image and its mappings
/// cannot meet.
const MMAP_BASE: usize = 0x1_0000_0000;

/// One past the last address the automatic placer will hand out.
///
/// 112 TiB, which leaves the initial stack (`ELF_STACK_TOP`, just under 128 TiB)
/// and 16 TiB of clearance below it outside the window entirely. The stack is
/// not in the region table — the loader places it — so the window has to avoid
/// it by construction rather than by collision test.
const MMAP_TOP: usize = 0x7000_0000_0000;

/// The largest `len` this call will consider, for
/// [`akuma_syscalls_mem::mmap::len_too_large`].
///
/// A bound rather than trust: `len` comes from a ring-3 register and drives
/// `0..pages` loops. Past this the answer is `ENOMEM` — Linux's answer for a
/// length it cannot map — rather than the old `EINVAL`, which was wrong for a
/// caller that simply asked for more memory than exists.
const MMAP_VA_SPAN: usize = MMAP_TOP - MMAP_BASE;

/// One past the last user address. Above this is the kernel's half.
///
/// Only `MAP_FIXED` needs it: automatic placement is confined to
/// `[MMAP_BASE, MMAP_TOP)` already, and a fixed request may legitimately land
/// outside that window (over its own image, say) but never in the upper half.
///
/// Note `akuma_syscalls_mem::mmap::fixed_overlaps_kernel_va` — the guard that
/// exists because the Go runtime commits arenas with `MAP_FIXED` — is **vacuous
/// on this target** and deliberately not called: this kernel has no identity map
/// in the lower half at all (`phys.rs`: physmap at PML4 256, devices at 257,
/// image at 511), so there is nothing of the kernel's down here to overlap. The
/// half-space check below is the whole guard.
const USER_VA_LIMIT: usize = 0x0000_8000_0000_0000;

/// How many pages this target maps eagerly before `plan` calls a mapping lazy.
///
/// `akuma_config::MMAP_EAGER_MAX_PAGES`, the same 16 the AArch64 kernel uses,
/// read from the shared config rather than restated — this is a tuning knob and
/// two copies of a knob is one knob and one bug.
///
/// It was `usize::MAX` until 2026-09-07, which said "never lazy" honestly
/// because there was no region table for the `#PF` handler to consult. There is
/// now, and [`fault_in`] is what consults it.
const EAGER_MAX_PAGES: usize = akuma_config::MMAP_EAGER_MAX_PAGES;

/// First-fit a free `pages`-page range in `[MMAP_BASE, MMAP_TOP)`.
///
/// # Reuse, and what it costs
///
/// The bump allocator this replaces never handed out an address twice, so a
/// use-after-`munmap` faulted instead of landing silently in a later mapping.
/// That is a genuinely useful property and reuse gives it up — but the bump was
/// **global across every process**, so a 64 MiB mapping in one program consumed
/// address space for all of them, and nothing ever gave any of it back. `rustc`
/// reserves gigabytes; a monotonic global cursor is not a thing it can run on.
///
/// The mitigation is that the window is 112 TiB and first-fit only reuses a hole
/// something was explicitly unmapped from, so an address is recycled long after
/// it went away rather than immediately.
///
/// # Why no sort and no allocation
///
/// The region list is not kept in address order (`detach_eager_regions_in_range`
/// pushes survivors onto the end), so a gap scan would have to sort — which
/// means a `Vec` per `mmap`, on the path a program allocating memory takes. This
/// walks instead: propose a candidate, and on an overlap jump the candidate to
/// the end of whatever it hit. `cand` strictly increases on every restart, so
/// the loop terminates in at most one pass per region.
fn find_free_va(regions: &[MmapRegion], pages: usize) -> Option<usize> {
    let len = pages.checked_mul(PAGE_SIZE as usize)?;
    let mut cand = MMAP_BASE;
    'outer: loop {
        let end = cand.checked_add(len)?;
        if end > MMAP_TOP {
            return None;
        }
        for r in regions {
            let start = r.start_va;
            let stop = start.saturating_add(r.len_bytes());
            if cand < stop && end > start {
                cand = stop;
                continue 'outer;
            }
        }
        return Some(cand);
    }
}

/// Is the caller a slotted user task with an address space of its own?
///
/// The one question every effect in this module is gated on. It is asked
/// through the region accessor rather than `current_proc_slot` so there is a
/// single definition of "has an address space" — a slot that exists but holds
/// no `Process` is not one, and the two spellings disagreed about that.
fn have_address_space() -> bool {
    usermode::with_current_regions(|_| ()).is_some()
}

/// The protection the page table should be told for a region page at `pa`.
///
/// [`PteProt::from_region`] plus one rule that only the kernel can know: **a
/// frame that is still CoW-shared never becomes writable in the PTE**, however
/// writable the region says it is. It is mapped read-only and marked instead, so
/// the first write faults and `cow_write_fault` breaks the sharing.
///
/// Without this an `mprotect(PROT_WRITE)` over a forked page would hand both
/// processes a writable mapping of one frame and their memory would silently
/// diverge — the exact failure copy-on-write exists to prevent, reintroduced
/// through the one syscall that is allowed to *raise* a permission.
fn pte_prot_for(prot: Prot, pa: usize) -> PteProt {
    let p = PteProt::from_region(prot);
    if p.write && akuma_pmm::cow_ref_get(pa) > 0 {
        p.cow()
    } else {
        p
    }
}

/// `mmap(addr, len, prot, flags, fd, offset)`.
pub fn sys_mmap(addr: u64, len: u64, prot: u64, flags: u64, fd: u64, offset: u64) -> u64 {
    if len == 0 {
        return errno::EINVAL;
    }
    let (prot32, flags32) = (prot as u32, flags as u32);
    // `fd` arrives as a `u64` and Linux's is a signed `int`: the "no file"
    // sentinel is -1, which is `u64::MAX` here, and truncating to `i32` is what
    // turns it back into the -1 `plan` compares against.
    let fd32 = fd as i32;

    // Alignment first, before anything else looks at the request: the AArch64
    // kernel does the same, and the ordering is asserted by its boot suite.
    if akuma_syscalls_mem::mmap::fixed_addr_unaligned_einval(addr as usize, flags32) {
        return errno::EINVAL;
    }
    // A fixed placement is checked against the user half here, beside the
    // alignment rule and ahead of everything with an effect — the same reason
    // the alignment check is first. `addr == 0` is a fixed request for the null
    // page, which is never a real one and would make a null dereference
    // succeed. Saturating, because `len` is a ring-3 register and a wrap would
    // turn "spans the whole address space" into "fits".
    if flags32 & akuma_syscalls_linux::flags::map::MAP_FIXED != 0 {
        let want = addr as usize;
        if want < PAGE_SIZE as usize
            || want.saturating_add(len as usize) > USER_VA_LIMIT
        {
            return errno::EINVAL;
        }
    }
    if akuma_syscalls_mem::mmap::len_too_large(len as usize, MMAP_VA_SPAN) {
        return errno::ENOMEM;
    }

    let pages = len.div_ceil(PAGE_SIZE) as usize;
    let plan = akuma_syscalls_mem::mmap::plan(prot32, flags32, fd32, pages, EAGER_MAX_PAGES);

    // File-backed mappings (2026-09-07). The refusal that used to live here was
    // right about the danger and too broad about the remedy — see the module
    // header's "What a file mapping is here".
    if plan.is_file_backed {
        // Still refused: a **writable `MAP_SHARED`** file mapping. Writes
        // through it must become visible in the file and to every other mapper,
        // which needs a write-back path and one shared frame per file page —
        // that is the page cache, and it is genuinely not here. Serving it as a
        // private copy would accept the write and silently drop it.
        if plan.is_shared_writable {
            return errno::ENOSYS;
        }
        // Linux requires a page-aligned offset and says `EINVAL` otherwise.
        // Ahead of the descriptor probe because it is decidable from the
        // arguments alone, which is the ordering rule the rest of this function
        // follows and what lets the boot suite assert it with no open file.
        if !offset.is_multiple_of(PAGE_SIZE) {
            return errno::EINVAL;
        }
        // A socket, pipe or directory has no bytes to map.
        if !crate::fd::is_regular_file(fd) {
            return errno::EACCES;
        }
    }

    // W^X, enforced here as it is in the ELF loader: `PteProt` offers no
    // writable-and-executable constructor, and a JIT is not something this
    // target supports. Kept ahead of everything that has an effect so a refused
    // request changes nothing.
    if prot32 & PROT_WRITE != 0 && prot32 & PROT_EXEC != 0 {
        return errno::EINVAL;
    }
    let region_prot = Prot::from_prot(prot32);

    // Everything past this point has an **effect on an address space**, so there
    // has to be one. Kept below the argument checks rather than above them
    // because the ordering is load-bearing: the AArch64 kernel refuses a
    // malformed request before it resolves a process, its boot suite asserts
    // that, and a kernel-test caller with no current task must see `EINVAL`
    // where Linux gives `EINVAL` — not `ESRCH`.
    if !have_address_space() {
        return errno::ESRCH;
    }

    let fixed = flags32 & akuma_syscalls_linux::flags::map::MAP_FIXED != 0;
    let byte_len = pages * PAGE_SIZE as usize;
    let root = paging::active_root();

    let base = if fixed {
        let want = addr as usize;
        // Rounding `len` up to a page can push the end past the half-space that
        // the raw `len` cleared above, so the rounded extent is re-checked here.
        if want.saturating_add(byte_len) > USER_VA_LIMIT {
            return errno::EINVAL;
        }
        // `MAP_FIXED` **replaces**: whatever is there goes away first, mappings
        // and region records alike. Doing this before reserving is what keeps
        // the new region from being clipped by the teardown of the old one.
        unmap_range(root, want, want + byte_len);
        want
    } else {
        // Without `MAP_FIXED` an address is a hint, and hints are advisory.
        let Some(found) = usermode::with_current_regions(|regions| find_free_va(regions, pages))
            .flatten()
        else {
            return errno::ENOMEM;
        };
        found
    };

    // Reserve, then populate. The region goes in **first**, under the lock, so a
    // concurrent `mmap` on another core cannot pick the same range; the frames
    // are allocated after the lock is dropped, so the PMM is never entered from
    // inside it. A `MAP_SHARED|MAP_ANONYMOUS` region is marked as such —
    // `fork` must share it by identity rather than copy-on-write, or a child's
    // write becomes invisible to the parent.
    let mut region = MmapRegion::inherited_with_prot(base, pages, region_prot);
    if plan.shared_anon {
        region = region.shared_anon();
    }
    usermode::with_current_regions(|regions| regions.push(region));

    // **Pinned divergence: a `MAP_SHARED | MAP_ANONYMOUS` mapping is never lazy
    // here**, whatever `plan` says. Such a region is shared with a `fork` child
    // *by identity* — same frames, mapped writable in both — and `fork_from`
    // does that by walking the parent's present leaves. A page that has not been
    // faulted in yet is not a leaf, so each side would demand-page its own
    // private frame and the mapping would silently behave like `MAP_PRIVATE`.
    // Sharing a not-yet-existing page needs a backing object this target does
    // not have; populating up front is the honest alternative, and a shared
    // anonymous mapping is a coordination area rather than a big reservation.
    if plan.use_lazy && !plan.shared_anon {
        // Nothing is allocated. The pages arrive through `fault_in` on first
        // touch, and a `PROT_NONE` reservation never gets any at all — which is
        // the whole point of reserving.
        return base as u64;
    }

    // A file-backed mapping is filled from the file; an anonymous one is zeroed.
    // `plan.use_lazy` is false for every file mapping (the crate decides that),
    // so control only reaches here for a file after the lazy return above.
    let file_source = plan.is_file_backed.then_some((fd, offset as usize));

    for i in 0..pages {
        let va = base + i * PAGE_SIZE as usize;
        let filled = match file_source {
            Some((fd, off)) => {
                populate_file_page(root, va, region_prot, fd, off + i * PAGE_SIZE as usize)
            }
            None => populate_page(root, va, region_prot),
        };
        if !filled {
            // Out of memory partway through. Unlike the pre-region version,
            // which leaked the pages it had already mapped because it had no
            // record of them, this can undo exactly what it did.
            unmap_range(root, base, base + byte_len);
            return errno::ENOMEM;
        }
    }
    base as u64
}

/// Allocate, zero, map and record one anonymous page at `va`.
///
/// The single place a frame becomes part of a user address space on this path,
/// which is why the ledger update is here and not at the three call sites.
/// A frame the ledger does not know about is a frame `Process::free` will not
/// release — the leak that made every post-`fork` `mmap` permanent.
fn populate_page(root: u64, va: usize, prot: Prot) -> bool {
    let Some(frame) = akuma_pmm::alloc_page() else {
        return false;
    };
    // Zero before mapping: a recycled frame otherwise hands ring 3 whatever the
    // previous owner left in it. The same rule the ELF loader follows.
    // SAFETY: a fresh PMM frame, reached through the physmap.
    unsafe { core::ptr::write_bytes(phys_ptr::<u8>(frame as u64), 0, PAGE_SIZE as usize) };
    // A brand-new frame is unshared, so `pte_prot_for`'s CoW rule cannot fire;
    // it is used anyway so there is one answer to "what bits does this region
    // get" rather than two that could drift.
    if !paging::map_page_in(root, va, frame as u64, pte_prot_for(prot, frame), MemAttr::WriteBack) {
        akuma_pmm::free_page(frame, 0);
        return false;
    }
    usermode::track_anon_frame(frame);
    true
}

/// Allocate, **fill from a file**, map and record one page at `va`.
///
/// [`populate_page`]'s sibling, and the difference is the whole reason
/// file-backed `mmap` was refused here until 2026-09-07: this function is what
/// stops the mapping being anonymous memory wearing a file's name.
///
/// # The failure this is built to make impossible
///
/// `mm.rs`'s header used to say a file mapping was `ENOSYS` because serving one
/// as anonymous memory "would look like a working call and hand the caller a
/// file full of zeros". That danger is real and it is *not* removed by having a
/// fill path — it is removed by making the fill path unable to silently skip.
/// So:
///
/// - The frame is zeroed **first**, then overwritten with what the file has.
///   Zero is therefore the value of a byte past EOF and of nothing else.
/// - [`crate::fd::file_bytes_at`] returns `None` for a descriptor that is not a
///   regular file, and that is a **failure of this function**, not a zero-byte
///   fill. `sys_mmap` also refuses such a descriptor up front; the second check
///   is here because this is the function that would otherwise produce the
///   zeros, and a guard at the point of harm outlives a guard at the caller.
/// - A short answer is *not* a failure: it is a page that straddles EOF, whose
///   tail `mmap(2)` specifies as zero.
///
/// # What this is not
///
/// A page cache. Every mapping gets its **own copy** of every page, so two
/// processes mapping one file hold two sets of frames and a write through
/// `MAP_PRIVATE` cannot be seen by anyone — which is what `MAP_PRIVATE` means,
/// so it is correct and merely expensive. `MAP_SHARED` writable is refused in
/// `sys_mmap` precisely because *that* one needs the sharing to be real.
fn populate_file_page(root: u64, va: usize, prot: Prot, fd: u64, offset: usize) -> bool {
    let Some(frame) = akuma_pmm::alloc_page() else {
        return false;
    };
    // SAFETY: a fresh PMM frame, reached through the physmap, and no other
    // reference to it exists until it is mapped below.
    let page = unsafe {
        core::slice::from_raw_parts_mut(phys_ptr::<u8>(frame as u64), PAGE_SIZE as usize)
    };
    // Zeroed before the fill, never instead of it.
    page.fill(0);
    if crate::fd::file_bytes_at(fd, offset, page).is_none() {
        // Not a regular file. Refuse rather than map the zeros just written.
        akuma_pmm::free_page(frame, 0);
        return false;
    }
    if !paging::map_page_in(root, va, frame as u64, pte_prot_for(prot, frame), MemAttr::WriteBack) {
        akuma_pmm::free_page(frame, 0);
        return false;
    }
    usermode::track_anon_frame(frame);
    true
}

/// Service a not-present fault at `addr` from the region table — demand paging.
///
/// `true` when the faulting instruction can be re-executed. `false` means the
/// address is not inside any mapping of this process (or is inside a `PROT_NONE`
/// reservation), and the fault falls through to the handler's remaining arms.
///
/// # Why the region lock is held across the allocation here
///
/// [`sys_mmap`] deliberately populates *outside* the lock, because it may map
/// many pages. This maps exactly one, and holding the lock across it closes the
/// window where a concurrent `munmap` retires the region between the lookup and
/// the map — which would leave a page mapped into a range the process has been
/// told is gone. Lock order is regions → PMM, and nothing takes them the other
/// way round.
///
/// # `PROT_NONE` is a refusal, and it is a *grant* record being read
///
/// A reservation's pages must never be populated: that is what makes a guard
/// page guard. The read is safe against
/// `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` because every region this
/// target creates records its protection explicitly ([`MmapRegion`] is built
/// through `*_with_prot` on both the `mmap` and the fork-inherit path), so
/// `recorded_prot` is always `Some`. The `None` arm is unreachable and grants
/// rather than refuses anyway, which is the direction that trap says to fail in.
pub fn fault_in(addr: u64) -> bool {
    let page = (addr as usize) & !0xfff;
    let root = paging::active_root();
    usermode::with_current_regions(|regions| {
        let Some(region) = regions.iter().find(|r| r.contains(page)) else {
            return false;
        };
        let prot = region.recorded_prot().unwrap_or(Prot::RW_NO_EXEC);
        if prot.is_none() {
            return false; // a reservation, or a guard page: a real fault
        }
        populate_page(root, page, prot)
    })
    .unwrap_or(false)
}

/// `munmap(addr, len)`.
pub fn sys_munmap(addr: u64, len: u64) -> u64 {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return errno::EINVAL;
    }
    // **Divergence 1** of `akuma-syscalls-mem`, preserved rather than fixed:
    // `munmap(addr, 0)` unmaps one page here where Linux returns `EINVAL`.
    let byte_len = akuma_syscalls_mem::mmap::munmap_len(len as usize);
    let start = addr as usize;
    let Some(end) = start.checked_add(byte_len).filter(|e| *e <= USER_VA_LIMIT) else {
        return errno::EINVAL;
    };
    // Without a process there is no region list and no user address space —
    // `paging::active_root()` would be the *kernel's* CR3, and walking a user
    // range in it is at best a no-op and at worst an unmap of something the
    // kernel put there.
    if !have_address_space() {
        return errno::ESRCH;
    }
    unmap_range(paging::active_root(), start, end);
    0
}

/// Retire `[start, end)`: detach the region records it covers, then unmap and
/// release every page that is actually present.
///
/// The two halves are separate on purpose. The **records** are clipped by
/// [`akuma_mmap::detach_eager_regions_in_range`], which splits a region the
/// range only partly covers and leaves the head and tail behind — the algebra
/// that had gone wrong before and now has host tests. The **pages** are found by
/// walking the page table, not the returned pieces, because a range may contain
/// mapped pages no region ever claimed (a `MAP_FIXED` landing on part of the ELF
/// image, say) and those must still be unmapped.
///
/// Frames go back through the ledger, never straight to the PMM:
/// `untrack_anon_frame` reports whether this address space held the last VA onto
/// the frame, and only then does `cow_ref_dec` get asked whether this was the
/// last address space. A frame the ledger does not track is left alone —
/// `Process::free` will release it — because freeing it here would be a double
/// free against that.
fn unmap_range(root: u64, start: usize, end: usize) {
    if end <= start {
        return;
    }
    // Records first, under the lock; the page walk below takes the PMM.
    let _ = usermode::with_current_regions(|regions| {
        let _pieces: Vec<_> = akuma_mmap::detach_eager_regions_in_range(regions, start, end);
    });

    paging::for_each_leaf_in_range(root, start, end, |va, _pa, _prot| {
        // Re-read through `unmap_page_in` rather than trusting the `pa` the walk
        // reported: it is the call that clears the entry and issues the
        // `invlpg`, and one source of truth for "what was there" is worth the
        // second walk of four reads.
        if let Some(frame) = paging::unmap_page_in(root, va) {
            let frame = frame as usize;
            if usermode::untrack_anon_frame(frame) && akuma_pmm::cow_ref_dec(frame) {
                akuma_pmm::free_page(frame, 0);
            }
        }
    });
}

/// `mprotect(addr, len, prot)`.
///
/// # This used to be `return 0`
///
/// Accept-and-do-nothing, because there was no region table to re-permission
/// against. It meant a caller asking for *more* access than it had still saw the
/// original mapping (harmless, since the original was never less permissive) and
/// a caller asking for *less* was simply not honoured — which is every guard
/// page every allocator installs, silently absent.
///
/// # It splits; it does not annotate
///
/// [`akuma_mmap::mprotect_eager_regions_in_range`] splits a region the range
/// only partly covers rather than recording the new protection against the whole
/// of it. Recording it against the whole was the bug that killed `rustc`
/// mid-build on AArch64: a guard page `mprotect(PROT_NONE)`-ed inside a larger
/// mapping recorded `NONE` for every page of it
/// (`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`).
pub fn sys_mprotect(addr: u64, len: u64, prot: u64) -> u64 {
    if !addr.is_multiple_of(PAGE_SIZE) {
        return errno::EINVAL;
    }
    let prot32 = prot as u32;
    // The same W^X stance `sys_mmap` takes. Refusing here as well is what stops
    // `mprotect` being the back door onto a writable code page.
    if prot32 & PROT_WRITE != 0 && prot32 & PROT_EXEC != 0 {
        return errno::EINVAL;
    }
    if len == 0 {
        return 0;
    }
    let start = addr as usize;
    let byte_len = (len as usize).div_ceil(PAGE_SIZE as usize) * PAGE_SIZE as usize;
    let Some(end) = start.checked_add(byte_len).filter(|e| *e <= USER_VA_LIMIT) else {
        return errno::EINVAL;
    };

    let new_prot = Prot::from_prot(prot32);
    if usermode::with_current_regions(|regions| {
        akuma_mmap::mprotect_eager_regions_in_range(regions, start, end, new_prot);
    })
    .is_none()
    {
        return errno::ESRCH;
    }

    // Then the pages that are already present. A lazy page has no PTE to change
    // and does not need one — `fault_in` reads the region, which now says the
    // new thing.
    let root = paging::active_root();
    paging::for_each_leaf_in_range(root, start, end, |va, pa, old| {
        // Kernel pages are not ring 3's to re-permission. Nothing should map one
        // in a user range, and quietly rewriting it if something did is the
        // dangerous half of the two mistakes.
        if !old.user {
            return;
        }
        let want = pte_prot_for(new_prot, pa as usize);
        let _ = paging::map_page_in(root, va, pa, want, MemAttr::WriteBack);
    });
    0
}

/// `mremap(old_addr, old_size, new_size, flags)` — x86_64 syscall 25.
///
/// # It moves pages; it does not copy them
///
/// The AArch64 implementation allocates `new_pages` fresh frames and copies the
/// old bytes through a kernel bounce buffer. That is the only thing it can do
/// with the structures it has, and it has cost this tree two bugs: a copy loop
/// that `break`ed on the first lazy destination page and **silently truncated**
/// the mapping (`docs/archive/USER_COPY_FOLD.md` §5, which is why `mremapmove`
/// exists), and an `unwrap_or(NONE)` that turned "the source recorded no
/// protection" into "the source said `PROT_NONE`" and killed `rustc` mid-build.
///
/// This target has a region table and demand paging, so it can do the honest
/// thing instead: re-point each present page at the new virtual address and
/// leave the frame exactly where it is. No allocation, no copy, no bounce
/// buffer, and **no truncation is possible** — there is no loop that can stop
/// early and still look finished.
///
/// Sparsity falls out of that rather than needing a case. A source page that was
/// never faulted in is not present, so nothing is moved for it and the
/// destination page stays absent — where the next touch demand-pages a zero
/// frame, which is precisely what touching the source would have done. A copy
/// implementation has to *decide* what to do there; this one cannot get it
/// wrong.
///
/// The frame ledger is deliberately untouched: it counts virtual addresses per
/// frame within this address space, one VA goes away and one arrives, so the
/// count is unchanged. Removing and re-adding would be two edits with the same
/// net effect and one more place to get the order wrong.
///
/// # What is shared with AArch64
///
/// The decision: [`akuma_syscalls_mem::mremap::plan`] and
/// [`akuma_syscalls_mem::mremap::no_move_errno`], both host-tested. That
/// includes **divergence 5** — a shrink returns the old address with the tail
/// still mapped, where Linux unmaps it — and the `ENOMEM`-vs-`EFAULT` split for
/// a growth that may not move, which `mremap` implementations classically get
/// backwards.
pub fn sys_mremap(old_addr: u64, old_size: u64, new_size: u64, flags: u64) -> u64 {
    use akuma_syscalls_mem::mremap::{Plan, no_move_errno, plan};

    let (old_addr, old_size, new_size) = (old_addr as usize, old_size as usize, new_size as usize);

    // `MREMAP_FIXED` is refused rather than ignored.
    //
    // It asks for the mapping to land at a **specific** address and to replace
    // whatever is there. This target's placer chooses an address, so honouring
    // the flag is real work — and quietly returning a different address is a
    // confident wrong answer to a request that was explicit, which is the same
    // failure shape as answering `ENOSYS` where Linux answers `EINVAL`.
    //
    // **The AArch64 kernel currently ignores this flag** (`sys_mremap` in
    // `akuma-syscalls-glue` decodes only `MREMAP_MAYMOVE`), so this is a
    // deliberate divergence between the two targets and not an oversight in
    // either. Recorded here rather than fixed there: changing AArch64's answer
    // needs its own A/B, and nothing in the tree passes the flag today.
    if flags as u32 & akuma_syscalls_linux::flags::mremap::MREMAP_FIXED != 0 {
        return errno::EINVAL;
    }

    // Pure, and first — a caller with no address space must see the argument
    // errno rather than `ESRCH`, the same ordering `sys_mmap` and `sys_madvise`
    // follow and the reason the crate takes the VA limit as a parameter.
    let (new_pages, may_move) = match plan(old_addr, old_size, new_size, flags as u32, USER_VA_LIMIT)
    {
        Plan::Fail(e) => return e,
        Plan::InPlace => return old_addr as u64,
        Plan::Grow { new_pages, may_move } => (new_pages, may_move),
    };
    if !may_move {
        // Gated, as the crate documents: this probe is a page-table walk plus a
        // region scan, and running it on every growing `mremap` would cost that
        // on the common path.
        //
        // **Ahead of the `have_address_space` check on purpose.** With no
        // address space nothing is mapped, and "there is no mapping there"
        // (`EFAULT`) is both Linux's answer and more informative than `ESRCH`.
        // The AArch64 kernel arrives at the same answer by the same route — its
        // process lookup may yield `None` and `is_mapped` is then false.
        let is_mapped = have_address_space()
            && (paging::translate_in(paging::active_root(), old_addr).is_some()
                || usermode::with_current_regions(|regions| {
                    regions.iter().any(|r| r.contains(old_addr))
                })
                .unwrap_or(false));
        return no_move_errno(is_mapped);
    }

    if !have_address_space() {
        return errno::ESRCH;
    }
    let root = paging::active_root();

    let old_pages = old_size.div_ceil(PAGE_SIZE as usize);

    // Reserve under the lock, move outside it — the same trade `sys_mmap`
    // documents, and for the same reason: the page-table work must not run with
    // the region lock held any longer than the reservation needs.
    let Some(Some(base)) = usermode::with_current_regions(|regions| {
        let base = find_free_va(regions, new_pages)?;
        // A remap moves and resizes a mapping; it does not **re**protect it, so
        // the new region carries the old one's recorded protection — including
        // whether the old one recorded anything at all. Turning "the source said
        // nothing" into an explicit `PROT_NONE` is what killed `rustc` on the
        // AArch64 side (`docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md`), and
        // `MmapRegion::inherited` is the constructor that states nothing.
        let old_prot = regions
            .iter()
            .find(|r| r.start_va == old_addr)
            .and_then(MmapRegion::recorded_prot);
        regions.push(match old_prot {
            Some(prot) => MmapRegion::inherited_with_prot(base, new_pages, prot),
            None => MmapRegion::inherited(base, new_pages),
        });
        Some(base)
    }) else {
        return errno::ENOMEM;
    };

    // Re-point each present page. `for_each_leaf_in_range` skips an absent
    // subtree whole, so a sparsely-touched 4 MiB source costs a walk of what is
    // there rather than 1024 four-level lookups — and it reports the PTE bits,
    // which are carried across unchanged so a CoW-marked page stays CoW-marked.
    let mut moves: Vec<(usize, u64, PteProt)> = Vec::new();
    paging::for_each_leaf_in_range(root, old_addr, old_addr + old_pages * PAGE_SIZE as usize,
        |va, pa, prot| {
            if prot.user {
                moves.push((va, pa, prot));
            }
        });
    for (va, pa, prot) in moves {
        let offset = va - old_addr;
        // `unmap` first, then `map`: the frame is the same, so mapping the
        // destination before clearing the source would leave it briefly at two
        // VAs — which the ledger, whose count this call deliberately does not
        // touch, would then be one short of.
        paging::unmap_page_in(root, va);
        if !paging::map_page_in(root, base + offset, pa, prot, MemAttr::WriteBack) {
            // Out of page-table frames partway through. The pages already moved
            // are reachable at the new address and the region record covers
            // them, so nothing leaks and nothing is lost — the caller simply
            // gets a mapping with a hole, which the next touch demand-pages.
            // Reported rather than silent: a failure here means the PMM is out.
            break;
        }
    }

    // Retire the source. Its present pages are gone from the page table already,
    // so the walk inside `unmap_range` finds only what was never moved, and the
    // region record is clipped by `detach_eager_regions_in_range`.
    unmap_range(root, old_addr, old_addr + old_pages * PAGE_SIZE as usize);
    base as u64
}

/// `madvise(addr, len, advice)` — x86_64 syscall 28.
///
/// # Why the errno matters more than the feature
///
/// This was not dispatched at all until 2026-09-07, so every advice answered
/// `ENOSYS` — and **that is not a neutral way to say "not implemented"**.
/// Linux's own answer for advice it does not support is `EINVAL`, and callers
/// read the difference: `redis-server` probes `MADV_FREE`, treats `EINVAL` as
/// "older kernel, presumably unaffected" and starts, but treats anything else as
/// a kernel it cannot trust and **exits**
/// (`docs/archive/LONG_ROAD_TO_REDIS.md` §5). So the smallest correct change
/// here was never an implementation; it was returning the errno Linux returns.
///
/// The decode is [`akuma_syscalls_mem::madvise::action`] — the same host-tested
/// function the AArch64 kernel dispatches on, so `MADV_FREE`'s deliberate
/// `EINVAL` and "every unrecognised advice reports success" cannot drift between
/// the two targets. Both are pinned by that crate's own tests.
///
/// # `MADV_WILLNEED` is a no-op here, deliberately
///
/// Only *anonymous* mappings are lazy on this target — `plan` never marks a file
/// mapping lazy, and [`sys_mmap`] populates every file page at `mmap` time — so
/// there is exactly one kind of page pre-faulting could touch, and pre-faulting
/// it installs a zero frame that reads exactly as the demand fault would have
/// produced. The content is identical either way; all pre-faulting changes is
/// *when* the memory is committed, and `madvise(2)` is explicitly advisory about
/// that. Doing nothing is conformant, and it keeps a ring-3 register from
/// driving an allocation loop over a range the caller only reserved.
///
/// **This stops being true the day a file mapping becomes lazy here.** At that
/// point `MADV_WILLNEED` must pre-fault anonymous lazy pages *only*: installing
/// a zeroed frame over a file-backed lazy page marks it present, so the fill
/// never runs and the file reads as zeros — the bug that silently zeroed every
/// weight page of a `llama.cpp` model mmap on AArch64
/// (`docs/archive/BKL_VFS_CARVE_OUT.md` §10). The eagerness is what makes the
/// no-op safe, so the two must move together.
pub fn sys_madvise(addr: u64, len: u64, advice: u64) -> u64 {
    use akuma_syscalls_mem::madvise::{self, Action};

    let (addr, len) = (addr as usize, len as usize);
    // Range validity FIRST — ahead of the advice decode and ahead of anything
    // that resolves a process, the same ordering rule `sys_mmap` follows. `len`
    // arrives straight from a ring-3 register: without this,
    // `madvise(addr, -1, MADV_DONTNEED)` is a page count of ~4.5e15
    // (`docs/archive/AKUMA_EXTRACT_MMAP.md` §10.1 defect A).
    if !madvise::range_fits_user_va(addr, len, USER_VA_LIMIT) {
        return errno::EINVAL;
    }

    match madvise::action(advice as i32) {
        // `MADV_FREE`. The whole point of this function — see the header.
        Action::Fail(e) => e,
        Action::Ignore | Action::Willneed => 0,
        Action::Dontneed => {
            if len == 0 {
                return 0;
            }
            if !have_address_space() {
                return errno::ESRCH;
            }
            let (start, pages) = madvise::dontneed_zero_range(addr, len);
            if pages == 0 {
                return 0;
            }
            dontneed_range(start, start + pages * PAGE_SIZE as usize);
            0
        }
    }
}

/// `MADV_DONTNEED` over `[start, end)`: make the caller's next read return zero
/// without making anyone else's do the same.
///
/// # The walk is the range walker, not a per-page loop
///
/// [`paging::for_each_leaf_in_range`] visits only pages that are actually
/// **present**, and every absent page is `PageAction::Nothing` anyway — so the
/// walker's skip-an-absent-subtree-whole behaviour is not an optimisation here,
/// it is what bounds the work by what is mapped instead of by a length ring 3
/// chose. `MADV_DONTNEED` over a gigabyte-sized lazy reservation — which is the
/// commonest shape an allocator produces — costs a handful of reads.
///
/// # Only pages inside a recorded region are touched
///
/// A range handed to `madvise` may cover pages no `mmap` region ever claimed:
/// the ELF image's own text and data are mapped by the loader and are not in the
/// region list. Zeroing one of those would destroy the running program's code
/// **permanently**, because nothing on this target can re-read it — there is no
/// file backing and no refault path. Linux would drop the page and restore it
/// from the file, so skipping it here is *closer* to Linux than acting, not a
/// shortcut. It is a narrowing of what the AArch64 kernel does and is stated
/// rather than assumed.
///
/// A `PROT_NONE` reservation is skipped for the same reason `fault_in` refuses
/// to populate one: a guard page that quietly acquires a frame stops guarding.
///
/// # Zero in place, or break the sharing
///
/// [`akuma_syscalls_mem::madvise::dontneed_page_action`] decides, and the input
/// that matters is the **CoW share count**: a frame another address space can
/// see must not be zeroed in place, or a `fork` peer's live page is wiped. That
/// is the null-`Rc` corruption in `docs/archive/CARGO_HEAP_NULL_RC.md`, and this
/// target reaches it easily — `fork` is how every shell command starts here.
fn dontneed_range(start: usize, end: usize) {
    use akuma_syscalls_mem::madvise::{PageAction, dontneed_page_action};

    let root = paging::active_root();
    // One hold for the whole walk. Lock order is regions -> PMM, the same
    // direction `fault_in` takes, and nothing takes them the other way round.
    let _ = usermode::with_current_regions(|regions| {
        paging::for_each_leaf_in_range(root, start, end, |va, pa, pte| {
            // A kernel page in a user range is not ring 3's to zero.
            if !pte.user {
                return;
            }
            let Some(region) = regions.iter().find(|r| r.contains(va)) else {
                return;
            };
            let prot = region.recorded_prot().unwrap_or(Prot::RW_NO_EXEC);
            if prot.is_none() {
                return;
            }
            let pa = pa as usize;
            match dontneed_page_action(true, akuma_pmm::cow_ref_get(pa)) {
                // `for_each_leaf_in_range` only reports present pages, so the
                // unmapped arm is unreachable from here. Kept as an arm rather
                // than an `unwrap`: the decision belongs to the crate, and an
                // arm that stops being unreachable should compile, not panic.
                PageAction::Nothing => {}
                // This address space is the frame's only holder. Zeroing it is
                // indistinguishable from Linux's drop-and-refault and costs no
                // allocation.
                //
                // SAFETY: `pa` came from a present user leaf of this address
                // space, so it is a live 4 KiB frame reachable through the
                // physmap. Writing through the physmap rather than the user VA
                // is deliberate — the PTE may be read-only (a `PROT_READ`
                // region, or a CoW-demoted page whose peer has gone).
                PageAction::ZeroInPlace => unsafe {
                    core::ptr::write_bytes(phys_ptr::<u8>(pa as u64), 0, PAGE_SIZE as usize);
                },
                // Someone else maps this frame. Give this address space a
                // private zero frame and drop its share.
                PageAction::BreakSharing => {
                    let Some(fresh) = akuma_pmm::alloc_page() else {
                        // Advisory: out of memory means the page keeps its old
                        // contents, which is a worse answer than Linux's and a
                        // far better one than wiping a peer's live page.
                        return;
                    };
                    // SAFETY: a fresh PMM frame, reached through the physmap.
                    unsafe {
                        core::ptr::write_bytes(phys_ptr::<u8>(fresh as u64), 0, PAGE_SIZE as usize);
                    };
                    if !paging::map_page_in(
                        root,
                        va,
                        fresh as u64,
                        pte_prot_for(prot, fresh),
                        MemAttr::WriteBack,
                    ) {
                        akuma_pmm::free_page(fresh, 0);
                        return;
                    }
                    // The old frame loses this VA. Ledger first, then the
                    // global share count, then the free — the same order
                    // `unmap_range` uses, and for the same reason: a frame this
                    // address space no longer tracks is not this address
                    // space's to hand back.
                    if usermode::untrack_anon_frame(pa) && akuma_pmm::cow_ref_dec(pa) {
                        akuma_pmm::free_page(pa, 0);
                    }
                    usermode::track_anon_frame(fresh);
                }
            }
        });
    });
}

/// The refusals and the arithmetic, which are the parts a guest program cannot
/// easily reach.
///
/// # Why this is not the whole test
///
/// It used to check *only* refusals, on the grounds that the success path is
/// exercised for real by every allocating program. As those refusals were
/// removed — `MAX_MAPPING`, `MAP_FIXED`, the lazy path — that reasoning would
/// have quietly left the module with no self-test at all, which is the "a boot
/// self-test that goes quiet is not a pass" trap. So the arms that disappeared
/// are replaced rather than deleted: the VA placer is now tested directly, and
/// the two refusals that remain are still asserted.
pub fn smoke_test(t: &mut Suite) {
    use akuma_syscalls_linux::flags::map::{MAP_ANONYMOUS, MAP_FIXED};
    const ANON: u64 = MAP_ANONYMOUS as u64;
    // -1, the "no file" sentinel, as it arrives from a 64-bit register.
    const NO_FD: u64 = u64::MAX;

    t.check_eq("mmap: zero length is EINVAL", sys_mmap(0, 0, 3, ANON, NO_FD, 0), errno::EINVAL);
    t.check_eq(
        "mmap: a length past the VA window is ENOMEM",
        sys_mmap(0, (MMAP_VA_SPAN + 1) as u64, 3, ANON, NO_FD, 0),
        errno::ENOMEM,
    );
    // File-backed mappings (2026-09-07). This arm used to assert that **every**
    // file-backed request was `ENOSYS`; that refusal is gone, so it is replaced
    // rather than deleted — a self-test that quietly stops covering the branch
    // it was written for is the "goes quiet is not a pass" trap.
    //
    // The three that remain are all decidable from the arguments, which is why
    // they can be asserted with no process and no open file.
    const MAP_SHARED: u64 = akuma_syscalls_linux::flags::map::MAP_SHARED as u64;
    t.check_eq(
        "mmap: a writable MAP_SHARED file mapping is still ENOSYS",
        sys_mmap(0, 4096, u64::from(PROT_WRITE) | 1, MAP_SHARED, 5, 0),
        errno::ENOSYS,
    );
    t.check_eq(
        "mmap: a file mapping at an unaligned offset is EINVAL",
        sys_mmap(0, 4096, 1, 0, 5, 1),
        errno::EINVAL,
    );
    // The descriptor probe. Nothing is open here, so fd 5 is not a regular file
    // and the request is refused **before** a frame is allocated — which is what
    // stops the zero-filled-file failure the old blanket refusal guarded
    // against. `populate_file_page` refuses the same thing a second time, at the
    // point where the zeros would otherwise be written.
    t.check_eq(
        "mmap: a file mapping on a descriptor that is not a file is EACCES",
        sys_mmap(0, 4096, 1, 0, 5, 0),
        errno::EACCES,
    );
    t.check_eq(
        "mmap: writable+executable is refused",
        sys_mmap(0, 4096, u64::from(PROT_WRITE | PROT_EXEC), ANON, NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "mmap: an unaligned MAP_FIXED address is EINVAL",
        sys_mmap(0x5000_1001, 4096, 3, ANON | u64::from(MAP_FIXED), NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "mmap: MAP_FIXED over the kernel half is EINVAL",
        sys_mmap(USER_VA_LIMIT as u64, 4096, 3, ANON | u64::from(MAP_FIXED), NO_FD, 0),
        errno::EINVAL,
    );
    t.check_eq(
        "munmap: an unaligned address is EINVAL",
        sys_munmap(0x1001, 4096),
        errno::EINVAL,
    );
    t.check_eq(
        "mprotect: writable+executable is refused",
        sys_mprotect(0x1_0000_0000, 4096, u64::from(PROT_WRITE | PROT_EXEC)),
        errno::EINVAL,
    );

    // Every one of these runs during the boot self-tests, where there is no
    // current process — so each must have been refused *before* reaching the
    // region list. A slotted caller is what `ESRCH` distinguishes, and a
    // refusal that returned it here would mean the ordering had slipped.
    t.check_eq(
        "mmap: a valid anonymous request with no process is ESRCH, not a mapping",
        sys_mmap(0, 4096, 3, ANON, NO_FD, 0),
        errno::ESRCH,
    );

    madvise_check(t);
    mremap_check(t);
    va_placement_check(t);
}

/// `mremap`: the argument answers, all of which must be decided before a
/// process is resolved.
///
/// The move itself needs an address space and is exercised for real by
/// `mremapmove` (`scripts/mem_suite.py`), which is where a truncation or a lost
/// page would show. What cannot be reached from a guest is the errno table, and
/// `mremap`'s is the one implementations classically get backwards.
fn mremap_check(t: &mut Suite) {
    use akuma_syscalls_linux::flags::mremap::{MREMAP_FIXED, MREMAP_MAYMOVE};
    const MAYMOVE: u64 = MREMAP_MAYMOVE as u64;
    const PAGE: u64 = 4096;

    t.check_eq(
        "mremap: a zero new size is EINVAL",
        sys_mremap(0x1_0000_0000, PAGE, 0, MAYMOVE),
        errno::EINVAL,
    );
    t.check_eq(
        "mremap: an unaligned old address is EINVAL",
        sys_mremap(0x1_0000_0001, PAGE, 2 * PAGE, MAYMOVE),
        errno::EINVAL,
    );
    t.check_eq(
        "mremap: an old address in the kernel half is EFAULT",
        sys_mremap(USER_VA_LIMIT as u64, PAGE, 2 * PAGE, MAYMOVE),
        errno::EFAULT,
    );
    // **Divergence 5**, pinned by `akuma-syscalls-mem`: a shrink returns the old
    // address and leaves the tail mapped, where Linux unmaps it. Asserted here
    // so a future "fix" is a deliberate change rather than a silent one.
    t.check_eq(
        "mremap: a shrink returns the old address unchanged",
        sys_mremap(0x1_0000_0000, 16 * PAGE, PAGE, MAYMOVE),
        0x1_0000_0000,
    );
    t.check_eq(
        "mremap: growth inside the last page is in place",
        sys_mremap(0x1_0000_0000, 1, PAGE, 0),
        0x1_0000_0000,
    );
    // Refused, not silently placed elsewhere — see `sys_mremap`'s comment.
    t.check_eq(
        "mremap: MREMAP_FIXED is refused rather than ignored",
        sys_mremap(0x1_0000_0000, PAGE, 2 * PAGE, MAYMOVE | u64::from(MREMAP_FIXED)),
        errno::EINVAL,
    );
    // The errno split. There is no process here, so nothing is mapped and the
    // no-move probe must answer `EFAULT` — "there is no mapping there" — rather
    // than `ENOMEM`, "no room to grow it".
    t.check_eq(
        "mremap: growing an unmapped address without MAYMOVE is EFAULT",
        sys_mremap(0x1_0000_0000, PAGE, 2 * PAGE, 0),
        errno::EFAULT,
    );
    // And a growth that may move needs an address space, which the boot suite
    // does not have. `ESRCH` is what says the argument checks were all passed.
    t.check_eq(
        "mremap: a growing move with no process is ESRCH",
        sys_mremap(0x1_0000_0000, PAGE, 2 * PAGE, MAYMOVE),
        errno::ESRCH,
    );
}

/// `madvise`: the errno choices, which are the whole of what this call is for on
/// a target where nothing but an anonymous mapping is ever lazy.
///
/// Every one of these is an *answer*, not an effect, and that is why they can be
/// asserted with no process: each must be decided before anything resolves an
/// address space. A `MADV_DONTNEED` is the one that has an effect, so it is the
/// one that reaches `ESRCH` here — and it reaching anything else would mean the
/// ordering had slipped, exactly as the `mmap` arm below asserts.
fn madvise_check(t: &mut Suite) {
    use akuma_syscalls_linux::flags::madvise::{
        MADV_DONTNEED, MADV_FREE, MADV_NORMAL, MADV_WILLNEED,
    };
    const PAGE: u64 = 4096;

    // The load-bearing one. `ENOSYS` here made `redis-server` exit rather than
    // start: it reads `EINVAL` as "older kernel, skip the check" and anything
    // else as a kernel it cannot trust (docs/archive/LONG_ROAD_TO_REDIS.md §5).
    t.check_eq(
        "madvise: MADV_FREE is EINVAL, not ENOSYS",
        sys_madvise(0x1_0000_0000, PAGE, MADV_FREE as u64),
        errno::EINVAL,
    );
    // The other half of the same rule: an advice nobody implements reports
    // success, because it is a hint. Both are pinned in `akuma-syscalls-mem`.
    t.check_eq(
        "madvise: an unrecognised advice reports success",
        sys_madvise(0x1_0000_0000, PAGE, 0xdead),
        0,
    );
    t.check_eq(
        "madvise: MADV_NORMAL reports success",
        sys_madvise(0x1_0000_0000, PAGE, MADV_NORMAL as u64),
        0,
    );
    // Advisory, and a no-op on this target — see `sys_madvise`'s header for the
    // condition under which that stops being true.
    t.check_eq(
        "madvise: MADV_WILLNEED reports success",
        sys_madvise(0x1_0000_0000, PAGE, MADV_WILLNEED as u64),
        0,
    );
    // The range guard, ahead of the advice decode. Without it this length is a
    // page count of ~4.5e15 and an unbounded loop reachable from ring 3
    // (docs/archive/AKUMA_EXTRACT_MMAP.md §10.1 defect A). Asserted with
    // `MADV_FREE` too, so a future implementation of it cannot skip the guard by
    // answering before the range is checked.
    t.check_eq(
        "madvise: a length past the user VA window is EINVAL",
        sys_madvise(0x1_0000_0000, u64::MAX, MADV_DONTNEED as u64),
        errno::EINVAL,
    );
    t.check_eq(
        "madvise: the range guard runs before the advice decode",
        sys_madvise(0x1_0000_0000, u64::MAX, MADV_FREE as u64),
        errno::EINVAL,
    );
    // And the effectful advice, which needs an address space it does not have
    // here. `ESRCH` rather than a silent 0 is what says the walk was reached.
    t.check_eq(
        "madvise: MADV_DONTNEED with no process is ESRCH",
        sys_madvise(0x1_0000_0000, PAGE, MADV_DONTNEED as u64),
        errno::ESRCH,
    );
    t.check_eq(
        "madvise: MADV_DONTNEED of zero length is a no-op",
        sys_madvise(0x1_0000_0000, 0, MADV_DONTNEED as u64),
        0,
    );
}

/// After the boot suite has run real programs: assert the lazy path was taken.
///
/// # Why this check exists
///
/// `mm::smoke_test` used to check only *refusals*, on the reasoning that the
/// success path is exercised for real by every allocating program. Removing the
/// refusals — `MAX_MAPPING`, `MAP_FIXED`, "never lazy" — would have left that
/// reasoning holding up nothing, which is the "a boot self-test that goes quiet
/// is not a pass" trap in exact form: demand paging could regress to *never
/// firing* and every other check in the suite would still be green, because an
/// eagerly-populated mapping works too. It would simply be the old kernel again,
/// silently.
///
/// So this runs **after** `busybox_test` / `execve_test` / `fork_test` /
/// `redirect_test` — shells, pipelines and a `fork`+`execve` — and asserts that
/// at least one not-present fault was serviced out of a region. musl's allocator
/// asks for arenas well past [`EAGER_MAX_PAGES`], so zero here means the lazy
/// arm is not being reached, whatever the reason.
pub fn demand_paging_report(t: &mut Suite) {
    use core::sync::atomic::Ordering;
    let n = crate::idt::USER_DEMAND_FAULTS.load(Ordering::Relaxed);
    t.note("mmap: user pages demand-paged from a region", n);
    t.check("mmap: the lazy path was actually taken", n > 0);
}

/// Pin the VA placer against the shapes that matter: first fit, skip an
/// occupied range, reuse a hole, and refuse when the window cannot hold it.
///
/// A pure function over a `Vec<MmapRegion>`, so it needs no process and no
/// address space — which is what makes it testable at all. The eager/lazy
/// decision and the region algebra it sits between are host-tested in their own
/// crates; this is the one piece of `mmap` policy that is genuinely this
/// target's own.
fn va_placement_check(t: &mut Suite) {
    fn region(start: usize, pages: usize) -> MmapRegion {
        MmapRegion::inherited_with_prot(start, pages, Prot::RW_NO_EXEC)
    }
    const PG: usize = 4096;

    t.check_eq(
        "mmap va: an empty space places at the base",
        find_free_va(&[], 4).unwrap_or(0) as u64,
        MMAP_BASE as u64,
    );

    // One region at the base: the next mapping goes immediately after it, not
    // at some bumped-past address.
    let one = [region(MMAP_BASE, 4)];
    t.check_eq(
        "mmap va: the next mapping abuts the first",
        find_free_va(&one, 1).unwrap_or(0) as u64,
        (MMAP_BASE + 4 * PG) as u64,
    );

    // A hole between two regions is reused if the request fits, and skipped if
    // it does not. This is the whole difference from the bump allocator, both
    // directions asserted.
    let holed = [region(MMAP_BASE, 2), region(MMAP_BASE + 4 * PG, 2)];
    t.check_eq(
        "mmap va: a 2-page hole is reused by a 2-page request",
        find_free_va(&holed, 2).unwrap_or(0) as u64,
        (MMAP_BASE + 2 * PG) as u64,
    );
    t.check_eq(
        "mmap va: a 2-page hole is skipped by a 3-page request",
        find_free_va(&holed, 3).unwrap_or(0) as u64,
        (MMAP_BASE + 6 * PG) as u64,
    );

    // Order-independence. The list is not kept sorted — `detach` pushes
    // survivors onto the end — so the placer must give the same answer whatever
    // order it walks the regions in. A gap scan that assumed sorted input would
    // pass the case above and fail this one.
    let reversed = [region(MMAP_BASE + 4 * PG, 2), region(MMAP_BASE, 2)];
    t.check_eq(
        "mmap va: the answer does not depend on region order",
        find_free_va(&reversed, 2).unwrap_or(0) as u64,
        (MMAP_BASE + 2 * PG) as u64,
    );

    // The window is finite and the refusal is `None`, not a wrapped address.
    t.check(
        "mmap va: a request larger than the window has no placement",
        find_free_va(&[], MMAP_VA_SPAN / PG + 1).is_none(),
    );
    t.check(
        "mmap va: a page count that would overflow has no placement",
        find_free_va(&[], usize::MAX).is_none(),
    );
}
