#![cfg_attr(not(test), no_std)]
//! Page tables in memory, and the only place their `unsafe` lives.
//!
//! # What this replaces
//!
//! Every walker in the tree reached a table as `phys_to_virt(pa) as *mut u64`,
//! then `.add(index).read_volatile()` / `.write_volatile(..)` — 14 sites in
//! `amd64/src/paging.rs`, ~30 in the amd64 half of `akuma-mmu`, 80-odd in its
//! AArch64 half — each with a `// SAFETY: table is a live table frame` comment
//! vouching for the same fact. The fact is not about the line; it is about the
//! *frame*, and it is true once.
//!
//! [`Table`] is that frame as a type: 512 [`AtomicU64`]s. A shared reference to
//! an atomic is sound even while the MMU walker reads the same words and another
//! core edits them, so [`Table::get`] / [`Table::set`] are **safe**. The single
//! `unsafe` is [`Table::at`], the claim "this address is a page-table frame".
//!
//! # The one stated contract
//!
//! > An address handed to [`Table::at`], or reached by following a present entry
//! > from a root handed to a walker in [`amd64`], is a 4 KiB, 4 KiB-aligned
//! > frame that is a live page table (or a zeroed frame about to become one), is
//! > mapped in the kernel's physmap, and is not freed or repurposed while a
//! > reference derived from it is alive.
//!
//! The third clause is what a reader must hold to. It is discharged by the
//! *callers' own locking* — an address space's tables are freed in `Drop`, after
//! a per-core check that nothing still runs on them — and nothing in this crate
//! can check it. **A root is a promise**: the walkers take `root: u64`, as the
//! code they replace did, and a bogus root makes a walk read (and a map write)
//! arbitrary physmap memory. What the walkers *do* check: that an intermediate
//! entry's frame is inside the physmap ([`PhysMap::table_addr`]), and every index
//! is masked to `0..512`.
//!
//! # Orderings
//!
//! Relaxed. These replaced `read_volatile`/`write_volatile`, which order nothing
//! either; the callers' TLB maintenance and fences are unchanged and stay at the
//! call site. [`Slot::compare_exchange`] is acquire-release because it publishes
//! a freshly zeroed table.

use core::sync::atomic::{AtomicU64, Ordering};

pub mod amd64;

/// Entries in a 4 KiB table: 4096 / 8.
pub const ENTRIES: usize = 512;

/// Index into the level-`level` table for `va`, 4 KiB granule, 9 bits per level
/// (level 1 = the leaf table, the same on both architectures).
#[must_use]
pub const fn index(va: usize, level: u32) -> usize {
    (va >> (12 + 9 * (level - 1))) & (ENTRIES - 1)
}

/// How a physical frame address becomes a kernel-virtual one.
///
/// Implemented by each kernel for its physmap. **Safe** to implement: the
/// walkers treat the returned address as a claim to be checked against the
/// contract above, and an implementation should `assert!` its own bound the way
/// `amd64/src/phys.rs::phys_to_virt` does, so a frame outside the physmap fails
/// loudly instead of returning a plausible pointer into nothing.
pub trait PhysMap {
    /// Kernel-virtual address of the frame at `pa`.
    fn table_addr(pa: u64) -> usize;
}

/// One 4 KiB page-table frame: 512 entries.
#[repr(C, align(4096))]
pub struct Table([AtomicU64; ENTRIES]);

impl Table {
    /// View the frame at kernel-virtual `addr` as a table.
    ///
    /// # Safety
    /// The crate-level contract: `addr` is a page-aligned page-table frame (or a
    /// zeroed frame about to become one), mapped, and neither freed nor
    /// repurposed for `'a`.
    #[must_use]
    pub unsafe fn at<'a>(addr: usize) -> &'a Self {
        debug_assert!(addr.is_multiple_of(4096), "page-table frame is not page-aligned");
        // SAFETY: the caller's obligation, stated above; `AtomicU64` has the
        // size and alignment of `u64`, so 512 of them are exactly one frame.
        unsafe { &*(addr as *const Self) }
    }

    /// Kernel-virtual address of the frame.
    #[must_use]
    pub fn addr(&self) -> usize {
        core::ptr::from_ref(self) as usize
    }

    /// Entry `i`.
    ///
    /// # Panics
    /// If `i >= 512`.
    #[must_use]
    #[inline]
    #[track_caller]
    pub fn get(&self, i: usize) -> u64 {
        self.0[i].load(Ordering::Relaxed)
    }

    /// Store entry `i`.
    ///
    /// # Panics
    /// If `i >= 512`.
    #[inline]
    #[track_caller]
    pub fn set(&self, i: usize, entry: u64) {
        self.0[i].store(entry, Ordering::Relaxed);
    }

    /// Zero every entry — what makes a fresh frame a valid empty table.
    pub fn clear(&self) {
        for e in &self.0 {
            e.store(0, Ordering::Relaxed);
        }
    }

    /// A handle to entry `i`.
    ///
    /// # Panics
    /// If `i >= 512`.
    #[must_use]
    #[track_caller]
    pub fn slot(&self, i: usize) -> Slot<'_> {
        assert!(i < ENTRIES, "page-table index out of range");
        Slot { table: self, idx: i }
    }
}

/// One entry of one [`Table`]: the shape a walk hands back for an edit.
///
/// It borrows the table, so it cannot outlive the reference the caller derived
/// under the crate contract — and, unlike the raw `*mut u64` it replaces, it
/// cannot be turned into a pointer anywhere else.
#[derive(Clone, Copy)]
pub struct Slot<'a> {
    table: &'a Table,
    idx: usize,
}

impl Slot<'_> {
    /// The entry.
    #[must_use]
    #[inline]
    pub fn get(&self) -> u64 {
        self.table.0[self.idx].load(Ordering::Relaxed)
    }

    /// Store the entry.
    #[inline]
    pub fn set(&self, entry: u64) {
        self.table.0[self.idx].store(entry, Ordering::Relaxed);
    }

    /// Replace `old` with `new` if the entry still holds `old`; `Err` carries
    /// what it held instead. Acquire-release: publishes a table the winner just
    /// zeroed.
    ///
    /// # Errors
    /// The current entry, when it was not `old`.
    pub fn compare_exchange(&self, old: u64, new: u64) -> Result<u64, u64> {
        self.table.0[self.idx].compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
    }

    /// Address of the entry word itself (for a diagnostic print).
    #[must_use]
    pub fn addr(&self) -> usize {
        core::ptr::from_ref(&self.table.0[self.idx]) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::{ENTRIES, Table, index};

    #[repr(C, align(4096))]
    struct Frame([u64; ENTRIES]);

    fn frame() -> Box<Frame> {
        Box::new(Frame([0; ENTRIES]))
    }

    #[test]
    fn get_set_slot_and_clear() {
        let mut f = frame();
        let addr = f.as_mut() as *mut Frame as usize;
        // SAFETY: a live, page-aligned, 4 KiB heap block owned by this test.
        let t = unsafe { Table::at(addr) };
        assert_eq!(t.addr(), addr);
        t.set(7, 0xAB);
        assert_eq!(t.get(7), 0xAB);
        assert_eq!(f.0[7], 0xAB, "the write lands in the frame");
        let s = t.slot(7);
        assert_eq!(s.get(), 0xAB);
        assert_eq!(s.addr(), addr + 7 * 8);
        s.set(5);
        assert_eq!(t.get(7), 5);
        assert_eq!(s.compare_exchange(5, 9), Ok(5));
        assert_eq!(s.compare_exchange(5, 1), Err(9));
        t.clear();
        assert_eq!(t.get(7), 0);
    }

    #[test]
    #[should_panic(expected = "index out of bounds")]
    fn get_past_the_end_panics() {
        let mut f = frame();
        // SAFETY: as above.
        let t = unsafe { Table::at(f.as_mut() as *mut Frame as usize) };
        let _ = t.get(512);
    }

    #[test]
    #[should_panic(expected = "page-table index out of range")]
    fn slot_past_the_end_panics() {
        let mut f = frame();
        // SAFETY: as above.
        let t = unsafe { Table::at(f.as_mut() as *mut Frame as usize) };
        let _ = t.slot(512);
    }

    #[test]
    fn index_math() {
        // 4 KiB granule: level 1 selects bits 20:12, level 4 bits 47:39.
        assert_eq!(index(0x1000, 1), 1);
        assert_eq!(index(0x20_0000, 2), 1);
        assert_eq!(index(0x4000_0000, 3), 1);
        assert_eq!(index(0x80_0000_0000, 4), 1);
        assert_eq!(index(0xffff_ffff_ffff_f000, 1), 511);
        assert_eq!(index(0xffff_ffff_ffff_f000, 4), 511, "masked, so an upper-half VA folds");
    }
}
