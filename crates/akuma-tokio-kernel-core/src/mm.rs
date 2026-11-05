//! The memory syscall arms a tokio kernel serves itself: `mprotect`, `munmap`,
//! `msync`, `madvise`, and the release and write-back paths under them.
//!
//! Arch-neutral: the page table and the TLB are the machine's ([`Mm`]), the
//! copy-on-write break and the W^X record [`cow`]'s; the task's layout is `akuma-user-layout`'s
//! (through [`Mm::with_task`]); the decisions shared with the other kernels are
//! `akuma-syscalls-mem`'s.

use akuma_syscalls_linux::flags::prot::{PROT_EXEC, PROT_WRITE};

use crate::{cow, lazy, shmpages};
use crate::mmu::FreshPage;
use crate::task::{MMAP_BASE, Task, USER_TOP};

const EINVAL: u64 = 22;
const ENOMEM: u64 = 12;

/// A negative errno as a syscall return.
const fn e(code: u64) -> u64 {
    0u64.wrapping_sub(code)
}

/// What the memory arms ask of the machine beyond demand paging.
pub trait Mm: lazy::Kernel {
    /// Run `f` on the task record the syscall path acts on now.
    fn with_task<R>(f: impl FnOnce(&mut Task) -> R) -> R;
    /// The physical address `va` maps to in `root`, if a valid leaf maps it.
    fn translate(root: u64, va: u64) -> Option<u64>;
    /// Clear the valid leaf at `va` and return its frame (`None`: nothing valid
    /// there, a lazy record included).
    fn unmap(root: u64, va: u64) -> Option<u64>;
    /// Map `pa` at `va`, write-back memory; `user == false` is `PROT_NONE` (the
    /// frame is kept, every user access faults).
    fn map_page(root: u64, va: u64, pa: u64, write: bool, exec: bool, user: bool) -> bool;
    /// The frame a valid leaf entry maps (`None` for an invalid one).
    fn frame_of(pte: u64) -> Option<u64>;
    /// Every core has dropped its translations of what this core just changed.
    fn shootdown();
    /// The mmap-slab bisect mask (`mmdbg=N`).
    fn mmdbg() -> u32;
    /// Record a slab event for the fault reports (`trace-err`).
    fn mm_event(kind: u8, start: u64, end: u64);
    /// Copy `src` to the running task's `dst_va`; `false` when it does not translate.
    fn copy_to_user(dst_va: u64, src: &[u8]) -> bool;
    /// Copy the running task's `src_va` into `dst`; `false` when it does not translate.
    fn copy_from_user(dst: &mut [u8], src_va: u64) -> bool;
}

/// `mprotect(addr, len, prot)`.
///
/// Re-maps each present page of the range with the requested `R/W/X`, and gives each lazy page ([`lazy`]) the same
/// permissions for when it is first touched. This is what makes
/// `tcc -run` and every JIT work: the code is written RW, then flipped to RX,
/// and a page mapped RW-non-exec faults on the fetch.
///
/// `PROT_NONE` keeps a present page's frame and clears its user bit: ring 3
/// faults on any access, the kernel's copies refuse it (`EFAULT`), and fork,
/// teardown and the share counts see an ordinary present page, so a later
/// `mprotect(RW)` gives the contents back. A lazy page's record is marked
/// `none` and is never filled.
#[must_use]
pub fn mprotect_arm<M: Mm>(addr: u64, len: u64, prot: u64) -> u64 {
    if addr & 0xFFF != 0 {
        return e(EINVAL);
    }
    if len == 0 {
        return 0;
    }
    let pages = len.div_ceil(4096);
    if len > u64::MAX / 2 || !M::with_task(|t| t.layout.user_ok(addr, pages * 4096)) {
        return e(ENOMEM);
    }
    #[allow(clippy::verbose_bit_mask)] // PROT_NONE is spelled as the bits it lacks
    let none = prot & 7 == 0;
    let (write, exec) = (prot & u64::from(PROT_WRITE) != 0, prot & u64::from(PROT_EXEC) != 0);
    let root = M::task_root();
    // W^X: decided for the whole range before any page changes, so a refusal leaves it as it was.
    if let Some(no) = akuma_syscalls_mem::wx::check_range(prot, |i| cow::page_written::<M>(root, addr + i * 4096), pages).errno() {
        return e(no);
    }
    for i in 0..pages {
        let va = (addr + i * 4096) as usize;
        if lazy::reprotect::<M>(root, va as u64, write, exec, none) {
            continue;
        }
        // The remap below builds a fresh PTE; carry "written" across it or the policy forgets.
        let was_written = cow::page_written::<M>(root, va as u64);
        // A shared mapping's frame is meant to be in every holder.
        if write && !M::with_task(|t| t.layout.in_shared(va as u64)) && !cow::privatize::<M>(root, va as u64) {
            return e(ENOMEM);
        }
        let Some(pa) = M::translate(root, va as u64) else {
            continue;
        };
        let _ = M::unmap(root, va as u64);
        if !M::map_page(root, va as u64, pa, write, exec, !none) {
            return e(ENOMEM);
        }
        if was_written {
            cow::mark_written::<M>(root, va as u64);
        }
    }
    // A tightened protection must reach peers' TLBs before the call returns.
    M::shootdown();
    0
}

/// Unmap every present page of `[start, end)` in `root` and give its frame
/// back (`free_page` honours a CoW count), and clear every lazy record in it.
///
/// A writable `MAP_SHARED` file page's frame is in `shmpages`' table too: when
/// the range overlaps such a mapping, the table gives back whatever nobody maps
/// now. The caller has written those pages back (`shared_writeback`).
///
/// A file whose last unfilled record in this space just went is unpinned
/// (`lazy::unpin_released`), so an `unlink` after `munmap` frees its blocks at
/// once, as on Linux, instead of when the process is reaped.
pub fn release_pages<M: Mm>(root: u64, start: u64, end: u64) {
    let tid = akuma_primitives::preempt::current_tid() as u32;
    let mut va = start;
    while va < end {
        if let Some(pa) = M::unmap(root, va) {
            akuma_pmm::free_page(pa as usize, tid);
        } else {
            lazy::clear::<M>(root, va);
        }
        va += 4096;
    }
    if M::with_task(|t| t.layout.shared().iter().any(|f| !f.is_anon() && f.overlaps(start, end))) {
        shmpages::sweep();
    }
    // A shared file mapping left in the pin's range writes back by inode.
    lazy::unpin_released::<M>(root, start, end, |inode, lo, hi| {
        M::with_task(|t| t.layout.shared().iter().any(|f| !f.is_anon() && f.inode == inode && f.overlaps(lo, hi)))
    });
    // Peers may still hold translations for the frames just freed: no frame is
    // reused before they have dropped them (the kernel lock is held throughout).
    M::shootdown();
}

/// Copy the pages of every writable `MAP_SHARED` file mapping that overlap `[lo, hi)` back to their files.
///
/// Clamped to the file's size at map time.
/// A frame several mappers share is written by each of them; the bytes are the
/// same. Anonymous shared mappings have no file and are skipped.
#[allow(clippy::many_single_char_names)] // s/e are the clipped range
pub fn shared_writeback<M: Mm>(lo: u64, hi: u64) {
    // The records and the root are copied out and the task borrow is closed for the whole walk: the loop does file I/O
    // and `read_bytes`, and nothing in it may find the record borrowed. The (possibly shortened) table goes back at the end.
    let (root_pa, layout) = M::with_task(|t| (t.root_pa, t.layout));
    for f in layout.shared().iter().filter(|f| !f.is_anon()) {
        let (base, len, ino, off, fsize) = (f.base, f.len, f.inode, f.offset, f.size);
        let (s, e) = (base.max(lo), (base + len).min(hi));
        if s < e {
            let mut va = s & !0xFFF;
            while va < e {
                if let Some(frame) = M::leaf(root_pa, va).and_then(M::frame_of) {
                    let file_off = off + (va - base);
                    if file_off < fsize {
                        let n = (fsize - file_off).min(4096) as usize;
                        let mut page = [0u8; 4096];
                        let _ = akuma_mmu::copy_from_phys(frame as usize, &mut page[..n]);
                        let _ = M::root_fs().write_at_by_inode(ino, file_off as usize, &page[..n]);
                        // A cached read-only page of this file now holds the old bytes.
                        akuma_fpcache::invalidate_inode(ino);
                    }
                }
                va += 4096;
            }
        }
    }
}

/// Write back every shared file mapping the running task still holds (exit, exec).
///
/// The mapping dies with the address space. Its pages stay mapped until
/// the root is freed (`free_user_root`), whose sweep then gives `shmpages` the
/// entries nobody maps any more.
pub fn shared_flush_all<M: Mm>() {
    if M::with_task(|t| !t.layout.shared().is_empty()) {
        shared_writeback::<M>(0, u64::MAX);
        M::with_task(|t| t.layout.forget_shared_within(0, u64::MAX));
    }
}

/// `msync(addr, len, flags)`: the POSIX flush point lld uses.
#[must_use]
pub fn msync_arm<M: Mm>(addr: u64, len: u64) -> u64 {
    if addr & 0xFFF != 0 {
        return e(EINVAL);
    }
    shared_writeback::<M>(addr, addr.saturating_add(len));
    0
}

/// `madvise(addr, len, advice)`.
///
/// The advice decode is `akuma-syscalls-mem`'s, shared with the other kernels (`MADV_FREE` is `EINVAL`, which musl's allocator reads as "not supported"; the hints are no-ops, and so is
/// `MADV_WILLNEED`: a lazy page fills on its first touch either way).
///
/// `MADV_DONTNEED` makes the next read of each page in the mmap slab return zero, per
/// [`akuma_syscalls_mem::madvise::dontneed_page_action`]: a present page's leaf becomes a zero-fill lazy
/// record with the page's permissions (`lazy.rs`), and the frame goes back - or, if a fork peer also maps it,
/// only this space's count on it. A page not yet touched keeps its record, so a private file page nobody
/// wrote still reads the file, as on Linux. A writable `MAP_SHARED` file page is written back and kept, which
/// is what Linux's drop and refault reads. Pinned divergences: a present private file page refaults as
/// zeros, where Linux rereads the file (the record of which file it came from went when it was filled);
/// pages outside the slab (the image, `brk`, the main stack) are left as they are.
#[must_use]
pub fn madvise_arm<M: Mm>(addr: u64, len: u64, advice: u64) -> u64 {
    use akuma_syscalls_mem::madvise::{self, Action, PageAction};
    if !madvise::range_fits_user_va(addr as usize, len as usize, USER_TOP as usize) {
        return e(EINVAL);
    }
    match madvise::action(advice as i32) {
        Action::Fail(errno) => errno,
        Action::Ignore | Action::Willneed => 0,
        Action::Dontneed => {
            let (start, pages) = madvise::dontneed_zero_range(addr as usize, len as usize);
            let (start, end) = (start as u64, (start + pages * 4096) as u64);
            if pages == 0 {
                return 0;
            }
            if M::with_task(|t| t.layout.shared().iter().any(|f| f.base < end && start < f.base + f.len)) {
                shared_writeback::<M>(start, end);
            }
            // Only the used part of the slab: nothing above the cursor has a leaf.
            let (lo, hi) = (start.max(MMAP_BASE), end.min(M::with_task(|t| t.layout.mmap_cur())));
            let root = M::task_root();
            // Frames whose leaves were just replaced, given back in batches once peers' TLBs have dropped them.
            let mut dropped = [0u64; 64];
            let mut n = 0;
            for va in (lo..hi).step_by(4096) {
                if let (false, Some(pte)) = (M::with_task(|t| t.layout.in_shared(va)), M::leaf(root, va)) {
                    let frame = M::frame_of(pte);
                    match madvise::dontneed_page_action(frame.is_some(), frame.map_or(0, |pa| akuma_pmm::cow_ref_get(pa as usize))) {
                        PageAction::Nothing => {}
                        // Shared or not, the answer is the same here: this space lets go of the frame (a peer
                        // keeps its count) and the page refaults as zeros.
                        PageAction::ZeroInPlace | PageAction::BreakSharing => {
                            let (Some(record), Some(pa)) = (M::anon_like(pte).encode(), frame) else {
                                continue;
                            };
                            M::write_leaf(root, va, record);
                            M::invalidate(va);
                            dropped[n] = pa;
                            n += 1;
                            if n == dropped.len() {
                                free_dropped::<M>(&dropped[..n]);
                                n = 0;
                            }
                        }
                    }
                }
            }
            free_dropped::<M>(&dropped[..n]);
            0
        }
    }
}

/// Give back frames whose leaves this core just rewrote, after every peer has dropped its translations of
/// them (as `cow_resolve`'s copy: the shootdown comes before the count drops).
fn free_dropped<M: Mm>(frames: &[u64]) {
    if frames.is_empty() {
        return;
    }
    M::shootdown();
    let tid = akuma_primitives::preempt::current_tid() as u32;
    for &f in frames {
        akuma_pmm::free_page(f as usize, tid);
    }
}

/// `munmap(addr, len)` for the mmap slab: free the frames, record the hole.
/// Ranges outside the slab (the brk area, a stack) are accepted and ignored.
#[must_use]
pub fn munmap_arm<M: Mm>(addr: u64, len: u64) -> u64 {
    let (addr, end) = match M::with_task(|t| t.layout.munmap(addr, len)) {
        Err(_) => return e(EINVAL),
        Ok(None) => return 0,
        Ok(Some(r)) => r,
    };
    if M::mmdbg() & 1 == 0 {
        return 0;
    }
    shared_writeback::<M>(addr, end);
    M::mm_event(1, addr, end);
    release_pages::<M>(M::task_root(), addr, end);
    M::with_task(|t| {
        t.layout.forget_shared_within(addr, end);
        t.layout.release(addr, end);
    });
    0
}


/// `mremap(old, old_sz, new_sz, flags)`: musl's `realloc` on an mmapped chunk.
///
/// A shrink frees the tail. A grow extends in place when the range ends at the
/// slab cursor, else moves (`MREMAP_MAYMOVE`, which the caller accepts by using
/// the returned address): a slab mapping by moving its leaves, anything else by
/// copying the overlap. `MREMAP_FIXED` is refused (no fixed targets), and so is
/// a range overlapping a `MAP_SHARED` mapping, whose record names its pages (a
/// pinned divergence: Linux allows it).
#[must_use]
pub fn mremap_arm<M: Mm>(old: u64, old_sz: u64, new_sz: u64, flags: u64) -> u64 {
    const EFAULT: u64 = 14;
    if new_sz == 0 || flags & 2 != 0 || old & 0xFFF != 0 {
        return e(EINVAL);
    }
    if old_sz == 0 || !M::with_task(|t| t.layout.user_ok(old, old_sz)) {
        return e(EFAULT);
    }
    let end = old.saturating_add(old_sz);
    if M::with_task(|t| t.layout.shared().iter().any(|f| f.overlaps(old, end))) {
        return e(EINVAL);
    }
    let root = M::task_root();
    let Ok(plan) = M::with_task(|t| t.layout.mremap(old, old_sz, new_sz)) else {
        return e(EINVAL);
    };
    // Both page-rounded; `mremap` above refused a size that does not round.
    let (old_pages, new_pages) = (old_sz.div_ceil(4096) * 4096, new_sz.div_ceil(4096) * 4096);
    match plan {
        akuma_user_layout::Mremap::Shrink { release } => {
            if let Some((lo, hi)) = release
                && M::mmdbg() & 4 != 0
            {
                M::mm_event(3, lo, hi);
                release_pages::<M>(root, lo, hi);
                M::with_task(|t| t.layout.release(lo, hi));
            }
            return old;
        }
        // In place: the range is the topmost thing in the slab.
        akuma_user_layout::Mremap::InPlace { from, to } => {
            if !lazy::reserve::<M>(root, from, to - from, lazy::Lazy::anon(true, false)) {
                release_pages::<M>(root, from, to);
                return e(ENOMEM);
            }
            M::with_task(|t| t.layout.note_mapped(to));
            return old;
        }
        akuma_user_layout::Mremap::Move { .. } => {}
    }
    let release_old = matches!(plan, akuma_user_layout::Mremap::Move { release_old: true });
    let Some(base) = M::with_task(|t| t.layout.alloc(new_pages)) else {
        return e(ENOMEM);
    };
    // A slab mapping moves by its leaves: present or lazy, each page keeps its
    // frame and its bits at the new address, so nothing is copied and nothing
    // untouched gets filled. The tail past the old length is fresh zero-fill.
    if release_old && M::mmdbg() & 4 != 0 {
        if !lazy::reserve::<M>(root, base + old_pages, new_pages - old_pages, lazy::Lazy::anon(true, false))
            || !lazy::move_range::<M>(root, old, base, old_pages)
        {
            release_pages::<M>(root, base, base + new_pages);
            return e(ENOMEM);
        }
        lazy::move_pins(root, old, base, old_pages);
        // Peers may still translate the old addresses to the moved frames.
        M::shootdown();
        M::mm_event(4, old, old + old_pages);
        M::with_task(|t| t.layout.release(old, old + old_pages));
        return base;
    }
    if !lazy::reserve::<M>(root, base, new_pages, lazy::Lazy::anon(true, false)) {
        release_pages::<M>(root, base, base + new_pages);
        return e(ENOMEM);
    }
    let copy = old_sz.min(new_sz);
    let mut chunk = [0u8; 1024];
    let mut done = 0u64;
    while done < copy {
        let n = ((copy - done) as usize).min(chunk.len());
        if !M::copy_from_user(&mut chunk[..n], old + done) || !M::copy_to_user(base + done, &chunk[..n]) {
            return e(EFAULT);
        }
        done += n as u64;
    }
    base
}

/// `brk(want)`: grow the break lazily, shrink it as bookkeeping only.
///
/// Linux's failure value is the *old* break. A grow first releases whatever a
/// shrink left mapped (a regrow must read zeros), then reserves the range lazily.
#[must_use]
pub fn brk_arm<M: Mm>(want: u64) -> u64 {
    let (plan, brk_cur, root) = M::with_task(|t| (t.layout.brk_plan(want), t.layout.brk(), t.root_pa));
    if plan == akuma_user_layout::Brk::Refuse {
        return brk_cur;
    }
    if let akuma_user_layout::Brk::Grow { from, to: end } = plan {
        release_pages::<M>(root, from, end);
        if !lazy::reserve::<M>(root, from, end - from, lazy::Lazy::anon(true, false)) {
            release_pages::<M>(root, from, end);
            return brk_cur;
        }
    }
    M::with_task(|t| t.layout.set_brk(want));
    want
}

/// `mmap(addr, len, prot, flags, fd, off)`, the descriptor already resolved.
///
/// `file` is the regular file's inode on the root, `None`
/// for `MAP_ANONYMOUS`. A non-anonymous mapping of anything else is the caller's
/// `ENODEV`.
///
/// Two kinds of mapping: anonymous, and `MAP_PRIVATE`/`MAP_SHARED` of a file
/// (musl's ldso reserves a library's span anonymous, then maps each `PT_LOAD`
/// over it `MAP_FIXED` from the library's fd). Lazy where a record can say it
/// ([`lazy`]): one record per page, a frame on the first touch; a writable shared
/// file page's record says `shared`, and its fill maps the one frame every mapper
/// maps ([`shmpages`]). An anonymous shared mapping is eager (two lazy records
/// would fill two frames), and so is a file mapping a record cannot hold or whose
/// inode cannot be pinned. Writable only when asked; with W^X on (`cow`) a
/// `PROT_EXEC` mapping is RX and `W|X` is refused.
#[must_use]
#[allow(clippy::too_many_lines)] // one syscall's whole decision, in Linux's order
pub fn mmap_arm<M: Mm>(addr: u64, len: u64, prot: u64, flags: u64, file: Option<u32>, off: u64) -> u64 {
    use akuma_syscalls_linux::flags::map::{MAP_ANONYMOUS, MAP_FIXED, MAP_SHARED};
    const ENODEV: u64 = 19;
    const EIO: u64 = 5;
    // Linux refuses an offset that is not a page multiple, mapping or not.
    if len == 0 || off & 0xFFF != 0 {
        return e(EINVAL);
    }
    if let Some(no) = akuma_syscalls_mem::wx::check_prot(prot).errno() {
        return e(no);
    }
    let anon = flags & u64::from(MAP_ANONYMOUS) != 0;
    let fnode = match (anon, file) {
        (true, _) => None,
        (false, Some(node)) => Some(node),
        (false, None) => return e(ENODEV),
    };
    let fs = M::root_fs();
    let fsize = fnode.map_or(0, |node| fs.metadata_by_inode(node).map_or(0, |md| md.size));
    let root = M::task_root();
    let Some(pages) = akuma_user_layout::page_span(len) else {
        return e(ENOMEM);
    };
    let base = if flags & u64::from(MAP_FIXED) != 0 && M::with_task(|t| t.layout.in_slab(addr, pages)) {
        // Replacing whatever was there: the old frames go back, and the range
        // leaves the hole table. A shared file mapping under it keeps what was
        // written to it.
        if M::mmdbg() & 2 != 0 {
            M::mm_event(2, addr, addr + pages);
            if M::with_task(|t| t.layout.shared().iter().any(|f| f.overlaps(addr, addr + pages))) {
                shared_writeback::<M>(addr, addr + pages);
            }
            release_pages::<M>(root, addr, addr + pages);
            M::with_task(|t| {
                t.layout.forget_shared_within(addr, addr + pages);
                t.layout.carve(addr, addr + pages);
            });
        }
        addr
    } else {
        match M::with_task(|t| t.layout.alloc(pages)) {
            Some(b) => b,
            None => return e(ENOMEM),
        }
    };
    let (write, exec) = if prot & u64::from(PROT_EXEC) == 0 {
        (prot & u64::from(PROT_WRITE) != 0, false)
    } else {
        // W^X refused W+X above, so an executable mapping is never writable.
        (false, true)
    };
    // `PROT_NONE`: a record nothing fills (or, eagerly, a page without the user bit).
    #[allow(clippy::verbose_bit_mask)] // PROT_NONE is spelled as the bits it lacks
    let none = prot & 7 == 0;
    let shared_write = fnode.is_some() && flags & u64::from(MAP_SHARED) != 0 && prot & u64::from(PROT_WRITE) != 0;
    let shared_anon = anon && flags & u64::from(MAP_SHARED) != 0;
    let first = lazy::Lazy {
        write,
        exec,
        none,
        file: fnode.map(|inode| lazy::FilePage { inode, index: off / 4096, shared: shared_write }),
    };
    let lazy_ok = !shared_anon
        && lazy::fits(first, pages / 4096)
        && fnode.is_none_or(|node| lazy::pin(root, node, base, base + pages));
    if lazy_ok {
        if !lazy::reserve::<M>(root, base, pages, first) {
            release_pages::<M>(root, base, base + pages);
            return e(ENOMEM);
        }
    } else {
        let mut page = base & !0xFFF;
        while page < base + pages {
            let Some(mut frame) = <M::Frame as FreshPage>::alloc_zeroed() else {
                return e(ENOMEM);
            };
            if let Some(node) = fnode {
                // File-backed: fill this page straight from the inode. A short
                // read (past EOF) leaves the zeroed tail.
                let foff = off + (page - base);
                let mut done = 0usize;
                while done < 4096 {
                    let got = match fs.read_at_by_inode(node, (foff + done as u64) as usize, &mut frame.bytes_mut()[done..]) {
                        Ok(g) => g,
                        Err(_) => return e(EIO),
                    };
                    if got == 0 {
                        break;
                    }
                    done += got;
                }
            }
            if !M::map_page(root, page, frame.pa() as u64, write && !none, exec && !none, !none) {
                return e(ENOMEM);
            }
            let _ = frame.publish();
            page += 4096;
        }
    }
    // A MAP_FIXED mapping lands inside an earlier reservation, below the
    // cursor: only a mapping above it moves it.
    M::with_task(|t| t.layout.note_mapped(base + pages));
    if let Some(node) = fnode
        && shared_write
    {
        let f = akuma_user_layout::SharedFile { base, len: pages, inode: node, offset: off, size: fsize };
        if !M::with_task(|t| t.layout.add_shared(f)) {
            akuma_primitives::safe_print!(96, "[mmap] shared-file table full: writes to this mapping are lost\n");
        }
    }
    if shared_anon && !M::with_task(|t| t.layout.add_shared(akuma_user_layout::SharedFile::anon(base, pages))) {
        akuma_primitives::safe_print!(96, "[mmap] shared-file table full: a fork copies this mapping\n");
    }
    base
}
