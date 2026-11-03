//! Demand paging: `mmap` reserves, the first touch fills.
//!
//! Until 2026-11-03 every `mmap` page was allocated, zeroed and (for a file)
//! filled at `mmap` time, so a `rustc` held a private copy of all 311 MB of
//! `librustc_driver` and a 17 MB zeroed stack per codegen thread, touched or
//! not (`docs/archive/TAOXICHUAN_LAZY_MMAP.md`). Now `mmap` writes a lazy leaf
//! per page - `akuma_pte_amd64::lazy`, the record of what the page will be -
//! and [`fill`] makes it present on the first touch, from one of three places:
//!
//! - a ring-3 not-present fault (`fault_trap_locked`);
//! - the kernel's own copies through `TaskWalk`, which walk the page table and
//!   never fault;
//! - `akuma_user_access::validate_user_range`, which glue's arms call before
//!   touching a user buffer, through the hook [`prefault`] registers.
//!
//! The page table is the whole record, so `munmap`, `MAP_FIXED`, `mprotect`,
//! `mremap` and fork edit it page by page with the pages themselves. The two
//! places a walker would get a lazy leaf wrong are handled here: fork copies it
//! ([`is_lazy`] in `clone_user_tree`), and `release_pages` clears it.
//!
//! Every caller holds the kernel lock (a syscall, or `fault_trap`'s hold), so a
//! fill cannot race another fill or an `munmap`; a peer thread that faulted on
//! the same page and waited for the lock finds it present and retries.

use akuma_boot_amd64::paging::{self, MemAttr, PteProt};
use akuma_boot_amd64::phys::FreshFrame;
use akuma_pte_amd64::lazy;

pub use akuma_pte_amd64::lazy::{FilePage, Lazy, is_lazy};

/// Write lazy leaves for `[base, base + len)`, the page at `base` being
/// `first` (a file page's index advances one per page). Whatever the range
/// held must already be released (`release_pages`): a present page under it
/// would leak. `false` when a table frame cannot be had or a file index runs
/// past what an entry holds - [`fits`] answers the second up front.
pub fn reserve(root: u64, base: u64, len: u64, first: Lazy) -> bool {
    let mut i = 0u64;
    while i * 4096 < len {
        let Some(entry) = first.offset_by(i).and_then(Lazy::encode) else {
            return false;
        };
        if !paging::set_leaf_in(root, (base + i * 4096) as usize, entry) {
            return false;
        }
        i += 1;
    }
    true
}

/// Can `pages` pages starting at `first` all be recorded lazily?
pub fn fits(first: Lazy, pages: u64) -> bool {
    pages == 0 || first.offset_by(pages - 1).and_then(Lazy::encode).is_some()
}

/// What [`fill`] did.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fill {
    /// The page is present now - filled here, or already (a peer thread got
    /// the lock first). The faulting access can retry.
    Present,
    /// Nothing lazy at that address: a real fault.
    NotLazy,
    /// Lazy, but no frame, no table, or the file could not be read.
    Failed,
}

/// Make the page holding `va` present if it is lazy. `files == false` leaves
/// a file page alone (`NotLazy`): a ring-0 fault can arrive with filesystem
/// state held, and a fill reads the filesystem.
pub fn fill(root: u64, va: u64, files: bool) -> Fill {
    let va = va & !0xFFF;
    let Some(slot) = crate::leaf_entry(root, va) else {
        return Fill::NotLazy;
    };
    let entry = slot.read();
    if entry & akuma_pte_amd64::P != 0 {
        return Fill::Present;
    }
    let Some(page) = lazy::decode(entry) else {
        return Fill::NotLazy;
    };
    // `PROT_NONE` is never filled: the access that got here is a real fault.
    if page.none || (page.file.is_some() && !files) {
        return Fill::NotLazy;
    }
    let prot = PteProt { write: page.write, exec: page.exec, user: true };
    // A `MAP_SHARED` writable file page: one frame for every mapper (`shmpages`).
    if let Some(f) = page.file.filter(|f| f.shared) {
        return fill_shared(root, va, f, prot);
    }
    // A read-only file page is shared through the file-page cache: one frame
    // per `(inode, offset)` however many processes map it (amd64's rule, and
    // the same eligibility - see `akuma_fpcache`'s module docs).
    let share = page.file.filter(|_| !page.write && akuma_fpcache::cap() > 0);
    if let Some(f) = share
        && let Some((frame, _)) = akuma_fpcache::lookup_and_ref(FPCACHE_MOUNT, f.inode, f.index as usize * 4096, page.exec)
    {
        if paging::map_page_in(root, va as usize, frame.addr as u64, prot, MemAttr::WriteBack) {
            return Fill::Present;
        }
        // The lookup took a count for this mapping; give it back.
        akuma_pmm::free_page(frame.addr, akuma_primitives::preempt::current_tid() as u32);
        return Fill::Failed;
    }
    let mut filled = 0;
    let frame = match page.file {
        None => alloc_frame(true),
        Some(f) => alloc_frame(false).and_then(|mut frame| {
            filled = read_file_page(f, frame.bytes_mut())?;
            Some(frame)
        }),
    };
    let Some(frame) = frame else {
        return Fill::Failed;
    };
    if !paging::map_page_in(root, va as usize, frame.pa() as u64, prot, MemAttr::WriteBack) {
        return Fill::Failed; // the frame drops back to the PMM
    }
    let pa = frame.publish();
    // Published after it is mapped and filled: a peer can map it the moment it
    // lands. A page that runs past EOF is not shared (its zero tail belongs to
    // the mapping, not the file). Losing the insert race leaves this frame
    // private, which is still correct.
    if let Some(f) = share
        && filled == 4096
    {
        akuma_fpcache::insert(FPCACHE_MOUNT, f.inode, f.index as usize * 4096, akuma_exec::PhysFrame::new(pa), true);
    }
    Fill::Present
}

/// Fill a page of a writable `MAP_SHARED` file mapping (its record keeps
/// `shared` across a later `mprotect`, so this may map it read-only): the frame
/// every other mapper of the page maps, or a fresh one filled from the file and
/// published for them. amd64's `mm::shared_write_page`, on this kernel's frames.
fn fill_shared(root: u64, va: u64, f: FilePage, prot: PteProt) -> Fill {
    use crate::shmpages;
    let key = shmpages::key(f.inode, f.index);
    let pa = if let Some(pa) = shmpages::lookup_and_ref(key) {
        pa
    } else {
        let Some(mut frame) = alloc_frame(false) else {
            return Fill::Failed;
        };
        // Every fill and every file write holds the kernel lock, so a write
        // cannot land between the read and the publish; the generation check
        // is the crate's contract all the same, bounded as amd64 bounds it (the
        // last attempt publishes unchecked).
        let mut outcome = shmpages::Fill::Stale;
        for attempt in 0..8 {
            let generation = shmpages::fill_begin(f.inode);
            let read = read_file_page(f, frame.bytes_mut());
            let r = read.map(|_| shmpages::publish(key, frame.pa(), (attempt < 7).then_some(generation)));
            shmpages::fill_end();
            let Some(r) = r else {
                return Fill::Failed;
            };
            outcome = r;
            if !matches!(outcome, shmpages::Fill::Stale) {
                break;
            }
        }
        match outcome {
            // The peer's frame is the page; this one goes back.
            shmpages::Fill::Existing(peer) => peer,
            // Published (the table took its reference) or private.
            shmpages::Fill::Inserted | shmpages::Fill::Refused => frame.publish(),
            shmpages::Fill::Stale => return Fill::Failed,
        }
    };
    if paging::map_page_in(root, va as usize, pa as u64, prot, MemAttr::WriteBack) {
        return Fill::Present;
    }
    // This mapping's reference goes; if it was the last besides the table's,
    // so does the entry.
    akuma_pmm::free_page(pa, akuma_primitives::preempt::current_tid() as u32);
    shmpages::reap(key, pa);
    Fill::Failed
}

/// The file-page cache keys on `(inode, mount id)`. This kernel has one ext2
/// mount, and every invalidation is by inode alone (`invalidate_inode` is
/// id-blind on purpose), so any fixed non-zero id is the whole identity.
const FPCACHE_MOUNT: u32 = 1;

/// A frame for a fill. Out of memory, the file-page cache gives back pages
/// nobody maps (this kernel has no reclaim pass that would ask it) and the
/// allocation is tried once more.
fn alloc_frame(zeroed: bool) -> Option<FreshFrame> {
    let alloc = || if zeroed { FreshFrame::alloc_zeroed() } else { FreshFrame::alloc() };
    alloc().or_else(|| (akuma_fpcache::shrink(64) > 0).then(alloc).flatten())
}

/// Fill `out` with page `f.index` of inode `f.inode`; bytes past the end of
/// the file are zero, as `mmap(2)` specifies. The count of file bytes read, or
/// `None` on a read error.
fn read_file_page(f: lazy::FilePage, out: &mut [u8; 4096]) -> Option<usize> {
    let mut done = 0usize;
    while done < out.len() {
        match crate::root_fs().read_at_by_inode(f.inode, f.index as usize * 4096 + done, &mut out[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) => return None,
        }
    }
    out[done..].fill(0);
    Some(done)
}

/// `akuma_user_access`'s prefault hook: make every lazy page of
/// `[start, start + len)` present before a kernel copy touches it. `false`
/// only when a fill failed; a page that is not lazy is left for
/// `validate_user_range`'s own walk to judge.
pub fn prefault(start: usize, len: usize) -> bool {
    let root = crate::task_root();
    let end = start.saturating_add(len) as u64;
    let mut va = start as u64 & !0xFFF;
    while va < end {
        if fill(root, va, true) == Fill::Failed {
            return false;
        }
        va += 4096;
    }
    true
}

/// `mprotect` on a page nothing has touched: its record takes the new
/// permissions (`none` for `PROT_NONE`). `false` when the page at `va` is not
/// lazy.
pub fn reprotect(root: u64, va: u64, write: bool, exec: bool, none: bool) -> bool {
    let Some(slot) = crate::leaf_entry(root, va & !0xFFF) else {
        return false;
    };
    let Some(page) = lazy::decode(slot.read()) else {
        return false;
    };
    // Same backing, so it encodes wherever the original did.
    let Some(entry) = page.with_prot(write, exec, none).encode() else {
        return false;
    };
    slot.write(entry);
    true
}

/// Remove the lazy record at `va`, if there is one (`munmap`, `MAP_FIXED`,
/// `brk`): `paging::unmap_page_in` only clears a present leaf. A record names
/// no frame and was never in a TLB, so there is nothing else to undo.
pub fn clear(root: u64, va: u64) {
    if let Some(slot) = crate::leaf_entry(root, va & !0xFFF)
        && is_lazy(slot.read())
    {
        slot.write(0);
    }
}

/// Move the leaf at each page of `[from, from + len)` to the same offset
/// from `to`, present or lazy, frame and bits untouched (`mremap`'s move: the
/// pages change address, not owner, so no share count moves). On `false` (no
/// frame for a table) every page already moved is put back. The caller shoots
/// down peers' TLBs after a success.
pub fn move_range(root: u64, from: u64, to: u64, len: u64) -> bool {
    let mut off = 0;
    while off < len {
        if !move_leaf(root, from + off, to + off) {
            let mut back = 0;
            while back < off {
                // The source tables still exist, so this cannot need a frame.
                let _ = move_leaf(root, to + back, from + back);
                back += 4096;
            }
            return false;
        }
        off += 4096;
    }
    true
}

fn move_leaf(root: u64, from: u64, to: u64) -> bool {
    let Some(src) = crate::leaf_entry(root, from) else {
        return true; // no table, so nothing at `from`
    };
    let entry = src.read();
    if entry == 0 {
        return true;
    }
    if !paging::set_leaf_in(root, to as usize, entry) {
        return false;
    }
    src.write(0);
    akuma_cpu::tlb::invlpg(from as usize);
    true
}

/// The lazy leaf for the present page `pte` would be if it were dropped and
/// refaulted as zeros: same permissions, no file (`MADV_DONTNEED`). A page
/// without the user bit is a `PROT_NONE` one, and stays inaccessible.
pub fn anon_like(pte: u64) -> Lazy {
    let write = pte & (akuma_pte_amd64::RW | akuma_pte_amd64::COW) != 0;
    let none = pte & akuma_pte_amd64::US == 0;
    Lazy { none, ..Lazy::anon(write, pte & akuma_pte_amd64::NX == 0) }
}

// ---------------------------------------------------------------------------
// Inode pins: a lazy file page names its inode by number, so the inode must
// outlive every address space that can still fault it in.
// ---------------------------------------------------------------------------

/// Address space `root` may hold lazy pages of `pin`'s inode, all inside
/// `[lo, hi)`. One entry per `(root, inode)`; its range only grows (`pin`,
/// `move_pins`) until a release trims it ([`unpin_released`]). The entry goes
/// when no unfilled record of the inode is left in its range - `munmap` of the
/// mapping, as Linux - or with the root (`free_user_root`). A filled page holds
/// its own frame and no longer needs the inode.
/// Under the kernel lock, as `TASKS` is.
struct Pin {
    root: u64,
    pin: akuma_primitives::InodePin,
    lo: u64,
    hi: u64,
}

static PINS: akuma_primitives::cell::BorrowCell<alloc::vec::Vec<Pin>> =
    akuma_primitives::cell::BorrowCell::new(alloc::vec::Vec::new());

/// Pin `inode` for `root`, for records in `[lo, hi)`. `false` when the table
/// cannot grow - the caller maps the file eagerly instead.
pub fn pin(root: u64, inode: u32, lo: u64, hi: u64) -> bool {
    PINS.with_mut(|p| {
        if let Some(e) = p.iter_mut().find(|e| e.root == root && e.pin.inode() == inode) {
            e.lo = e.lo.min(lo);
            e.hi = e.hi.max(hi);
            return true;
        }
        if p.try_reserve(1).is_err() {
            return false;
        }
        p.push(Pin { root, pin: akuma_primitives::InodePin::new(inode), lo, hi });
        true
    })
}

/// A fork child's address space holds every lazy page its parent did, at the
/// same addresses. `false` when the table cannot grow; whatever was added is
/// kept, and goes with `child`'s root.
pub fn fork_pins(parent: u64, child: u64) -> bool {
    PINS.with_mut(|p| {
        let n = p.iter().filter(|e| e.root == parent).count();
        if p.try_reserve(n).is_err() {
            return false;
        }
        for i in 0..p.len() {
            if p[i].root == parent {
                let e = Pin { root: child, pin: p[i].pin.clone(), lo: p[i].lo, hi: p[i].hi };
                p.push(e);
            }
        }
        true
    })
}

/// `mremap` moved `[from, from + len)` to `to`: a pin whose range overlaps
/// the source now covers where those records went as well.
pub fn move_pins(root: u64, from: u64, to: u64, len: u64) {
    PINS.with_mut(|p| {
        for e in p.iter_mut().filter(|e| e.root == root && e.lo < from + len && from < e.hi) {
            let (s, t) = (e.lo.max(from), e.hi.min(from + len));
            e.lo = e.lo.min(to + (s - from));
            e.hi = e.hi.max(to + (t - from));
        }
    });
}

/// Pages a range check walks before it gives up and keeps the pin: a huge
/// mapping unmapped a slice at a time would otherwise walk its rest each time.
const SCAN_LIMIT: u64 = 1 << 16;

/// Does an unfilled record in `[lo, hi)` of `root` name `inode`? `true` past
/// [`SCAN_LIMIT`] without looking.
fn names_inode(root: u64, lo: u64, hi: u64, inode: u32) -> bool {
    if (hi - lo) / 4096 > SCAN_LIMIT {
        return true;
    }
    let mut va = lo;
    while va < hi {
        if let Some(slot) = crate::leaf_entry(root, va)
            && let Some(page) = lazy::decode(slot.read())
            && page.file.is_some_and(|f| f.inode == inode)
        {
            return true;
        }
        va += 4096;
    }
    false
}

/// `[start, end)` of `root` was just released (`release_pages`): trim every
/// pin of `root` by it, then drop each whose remaining range holds no record
/// of its inode and which `keep(inode, lo, hi)` does not claim (a shared
/// mapping writes back by inode). Returns whether a pin went. Allocates
/// nothing; past the first 16 pins a release touches, the rest only trim.
pub fn unpin_released(root: u64, start: u64, end: u64, keep: impl Fn(u32, u64, u64) -> bool) -> bool {
    let mut cand = [(0u32, 0u64, 0u64); 16];
    let mut n = 0;
    PINS.with_mut(|p| {
        for e in p.iter_mut().filter(|e| e.root == root && e.lo < end && start < e.hi) {
            if start <= e.lo {
                e.lo = end.min(e.hi);
            } else if end >= e.hi {
                e.hi = start;
            }
            if n < cand.len() {
                cand[n] = (e.pin.inode(), e.lo, e.hi);
                n += 1;
            }
        }
    });
    let mut dropped = false;
    // The checks walk page tables and read the task record: no `PINS` borrow.
    for &(inode, lo, hi) in &cand[..n] {
        if lo < hi && (names_inode(root, lo, hi, inode) || keep(inode, lo, hi)) {
            continue;
        }
        PINS.with_mut(|p| p.retain(|e| !(e.root == root && e.pin.inode() == inode)));
        dropped = true;
    }
    dropped
}

/// `root` is gone: release its pins.
pub fn drop_pins(root: u64) {
    // Dropping a pin is a CAS on the pin table and nothing else, so the
    // entries can drop inside the borrow; no allocation on the teardown path.
    PINS.with_mut(|p| p.retain(|e| e.root != root));
}
