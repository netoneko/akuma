//! The memory syscall arms a tokio kernel serves itself: `mprotect`, `munmap`,
//! `msync`, `madvise`, and the release and write-back paths under them.
//!
//! Arch-neutral: the page table, the TLB, the copy-on-write break and the W^X
//! record are the machine's ([`Mm`]); the task's layout is `akuma-user-layout`'s
//! (through [`Mm::with_task`]); the decisions shared with the other kernels are
//! `akuma-syscalls-mem`'s.

use akuma_syscalls_linux::flags::prot::{PROT_EXEC, PROT_WRITE};

use crate::{lazy, shmpages};
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
    /// Has user code written the page at `va` (the W^X record)?
    fn page_written(root: u64, va: u64) -> bool;
    /// Record that the page at `va` was written (kept across a remap).
    fn mark_written(root: u64, va: u64);
    /// Give this space a private copy of the page at `va` if it shares it.
    /// `false` when no frame could be had.
    fn cow_privatize(root: u64, va: u64) -> bool;
    /// Is W^X enforced (`wx=0` turns it off)?
    fn wx_enforced() -> bool;
    /// The mmap-slab bisect mask (`mmdbg=N`).
    fn mmdbg() -> u32;
    /// Record a slab event for the fault reports (`trace-err`).
    fn mm_event(kind: u8, start: u64, end: u64);
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
    if M::wx_enforced()
        && let Some(no) = akuma_syscalls_mem::wx::check_range(prot, |i| M::page_written(root, addr + i * 4096), pages).errno()
    {
        return e(no);
    }
    for i in 0..pages {
        let va = (addr + i * 4096) as usize;
        if lazy::reprotect::<M>(root, va as u64, write, exec, none) {
            continue;
        }
        // The remap below builds a fresh PTE; carry "written" across it or the policy forgets.
        let was_written = M::page_written(root, va as u64);
        // A shared mapping's frame is meant to be in every holder.
        if write && !M::with_task(|t| t.layout.in_shared(va as u64)) && !M::cow_privatize(root, va as u64) {
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
            M::mark_written(root, va as u64);
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

