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
    if page.file.is_some() && !files {
        return Fill::NotLazy;
    }
    let frame = match page.file {
        None => FreshFrame::alloc_zeroed(),
        Some(f) => FreshFrame::alloc().and_then(|mut frame| read_file_page(f, frame.bytes_mut()).then_some(frame)),
    };
    let Some(frame) = frame else {
        return Fill::Failed;
    };
    let prot = PteProt { write: page.write, exec: page.exec, user: true };
    if !paging::map_page_in(root, va as usize, frame.pa() as u64, prot, MemAttr::WriteBack) {
        return Fill::Failed; // the frame drops back to the PMM
    }
    frame.publish();
    Fill::Present
}

/// Fill `out` with page `f.index` of inode `f.inode`; bytes past the end of
/// the file are zero, as `mmap(2)` specifies. `false` on a read error.
fn read_file_page(f: lazy::FilePage, out: &mut [u8; 4096]) -> bool {
    let mut done = 0usize;
    while done < out.len() {
        match crate::root_fs().read_at_by_inode(f.inode, f.index as usize * 4096 + done, &mut out[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) => return false,
        }
    }
    out[done..].fill(0);
    true
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
/// permissions. `false` when the page at `va` is not lazy.
pub fn reprotect(root: u64, va: u64, write: bool, exec: bool) -> bool {
    let Some(slot) = crate::leaf_entry(root, va & !0xFFF) else {
        return false;
    };
    let Some(page) = lazy::decode(slot.read()) else {
        return false;
    };
    // Same backing, so it encodes wherever the original did.
    let Some(entry) = page.with_prot(write, exec).encode() else {
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
/// refaulted as zeros: same permissions, no file (`MADV_DONTNEED`).
pub fn anon_like(pte: u64) -> Lazy {
    let write = pte & (akuma_pte_amd64::RW | akuma_pte_amd64::COW) != 0;
    Lazy::anon(write, pte & akuma_pte_amd64::NX == 0)
}

// ---------------------------------------------------------------------------
// Inode pins: a lazy file page names its inode by number, so the inode must
// outlive every address space that can still fault it in.
// ---------------------------------------------------------------------------

/// `(root, pin)`: address space `root` holds lazy pages of `pin`'s inode. One
/// entry per pair. Dropped with the root (`free_user_root`), so an unlinked
/// file's blocks are freed when the last process that mapped it is reaped,
/// not when its last page is unmapped - coarser than Linux, never earlier.
/// Under the kernel lock, as `TASKS` is.
static PINS: akuma_primitives::cell::BorrowCell<alloc::vec::Vec<(u64, akuma_primitives::InodePin)>> =
    akuma_primitives::cell::BorrowCell::new(alloc::vec::Vec::new());

/// Pin `inode` for `root`. `false` when the table cannot grow - the caller
/// maps the file eagerly instead.
pub fn pin(root: u64, inode: u32) -> bool {
    PINS.with_mut(|p| {
        if p.iter().any(|(r, pin)| *r == root && pin.inode() == inode) {
            return true;
        }
        if p.try_reserve(1).is_err() {
            return false;
        }
        p.push((root, akuma_primitives::InodePin::new(inode)));
        true
    })
}

/// A fork child's address space holds every lazy page its parent did.
/// `false` when the table cannot grow; whatever was added is kept, and goes
/// with `child`'s root.
pub fn fork_pins(parent: u64, child: u64) -> bool {
    PINS.with_mut(|p| {
        let n = p.iter().filter(|(r, _)| *r == parent).count();
        if p.try_reserve(n).is_err() {
            return false;
        }
        for i in 0..p.len() {
            if p[i].0 == parent {
                let pin = p[i].1.clone();
                p.push((child, pin));
            }
        }
        true
    })
}

/// `root` is gone: release its pins.
pub fn drop_pins(root: u64) {
    // Dropping a pin is a CAS on the pin table and nothing else, so the
    // entries can drop inside the borrow; no allocation on the teardown path.
    PINS.with_mut(|p| p.retain(|(r, _)| *r != root));
}
