//! The AArch64 4-level walker: stage 1, 4 KiB granule, 48-bit VA (L0..L3).
//!
//! The counterpart of [`amd64`](crate::amd64) for a kernel that names an address
//! space by its root's physical address (a tokio kernel's `TTBR0`), over the
//! same [`Table`]/[`Slot`] and the same [`index`] - the crate's level 4..1 are
//! AArch64's L0..L3, and the index bits are identical for this granule.
//!
//! It knows only the *structure* of a descriptor: bit 0 valid, bit 1 table
//! (at L0-L2) or page (at L3) versus block, and the output address in bits
//! 47:12. What a leaf grants (`AP`, `UXN`/`PXN`, `AttrIndx`, `AF`, software
//! bits) is the caller's encoding, handed in whole to [`map_leaf`] and handed
//! back whole by [`lookup`]. Table descriptors are written with no `APTable`/
//! `XNTable` restriction, so the leaf alone decides access.

use crate::{PhysMap, Slot, Table, index};

/// Bit 0: the descriptor is valid.
pub const VALID: u64 = 1 << 0;
/// Bit 1 of a valid descriptor: a table (L0-L2) or a page (L3); clear is a
/// block (L1/L2 only).
pub const TABLE_OR_PAGE: u64 = 1 << 1;
/// The output address, bits 47:12.
pub const ADDR_MASK: u64 = 0x0000_ffff_ffff_f000;

/// The table in the frame at `pa`: **the one place this module claims "this
/// frame is a page table"**, discharged as in the amd64 walker - the root is a
/// promise, every other frame is reached through a valid table descriptor of a
/// live table, and a fresh frame is one the caller's allocator just handed over.
fn frame_table<'a, M: PhysMap>(pa: u64) -> &'a Table {
    // SAFETY: as above.
    unsafe { Table::at(M::table_addr(pa)) }
}

/// Where a [`lookup`] ended.
#[derive(Clone, Copy)]
pub enum Lookup<'a> {
    /// The descriptor at `level` (4 = L0 ... 1 = L3) is invalid.
    Missing { level: u32 },
    /// A block ends the walk at `level` (3 = L1, 1 GiB; 2 = L2, 2 MiB).
    Block { level: u32, entry: u64 },
    /// A valid 4 KiB page descriptor.
    Leaf { slot: Slot<'a>, entry: u64 },
}

/// Bytes a descriptor at `level` covers.
#[must_use]
pub const fn entry_span(level: u32) -> u64 {
    1 << (12 + 9 * (level - 1))
}

/// Walk `root` down to whatever maps `va`. `root` is a promise; see the crate docs.
#[must_use]
pub fn lookup<'a, M: PhysMap>(root: u64, va: usize) -> Lookup<'a> {
    let mut table_pa = root;
    for level in (2..=4).rev() {
        let entry = frame_table::<M>(table_pa).get(index(va, level));
        if entry & VALID == 0 {
            return Lookup::Missing { level };
        }
        if entry & TABLE_OR_PAGE == 0 {
            // A block; at L0 the encoding is reserved, which reads as unmapped.
            return if level == 4 { Lookup::Missing { level } } else { Lookup::Block { level, entry } };
        }
        table_pa = entry & ADDR_MASK;
    }
    let slot = frame_table::<M>(table_pa).slot(index(va, 1));
    let entry = slot.get();
    // At L3, `0b01` is reserved: only `0b11` is a page.
    if entry & (VALID | TABLE_OR_PAGE) != VALID | TABLE_OR_PAGE {
        return Lookup::Missing { level: 1 };
    }
    Lookup::Leaf { slot, entry }
}

/// The physical address `va` resolves to, blocks included.
#[must_use]
pub fn translate<M: PhysMap>(root: u64, va: usize) -> Option<u64> {
    match lookup::<M>(root, va) {
        Lookup::Missing { .. } => None,
        Lookup::Block { level, entry } => {
            let span = entry_span(level);
            Some((entry & ADDR_MASK & !(span - 1)) + (va as u64 & (span - 1)))
        }
        Lookup::Leaf { entry, .. } => Some((entry & ADDR_MASK) + (va as u64 & 0xfff)),
    }
}

/// The L3 slot for `va`, valid or not, without allocating.
///
/// `None` when a table above it is missing or a block covers it. (Unlike [`lookup`], an invalid
/// leaf still has a slot - a demand-paging record lives in one.)
#[must_use]
pub fn leaf_slot<'a, M: PhysMap>(root: u64, va: usize) -> Option<Slot<'a>> {
    let mut table_pa = root;
    for level in (2..=4).rev() {
        let entry = frame_table::<M>(table_pa).get(index(va, level));
        if entry & (VALID | TABLE_OR_PAGE) != VALID | TABLE_OR_PAGE {
            return None;
        }
        table_pa = entry & ADDR_MASK;
    }
    Some(frame_table::<M>(table_pa).slot(index(va, 1)))
}

/// Clear the valid page descriptor for `va`, returning the frame it mapped. The
/// caller owns the TLB maintenance and the frame's fate.
#[must_use = "the frame is the caller's to free, and None means nothing was mapped"]
pub fn unmap<M: PhysMap>(root: u64, va: usize) -> Option<u64> {
    let Lookup::Leaf { slot, entry } = lookup::<M>(root, va) else {
        return None;
    };
    slot.set(0);
    Some(entry & ADDR_MASK)
}

/// Install `entry` as the L3 descriptor for `va`, building the path to it.
///
/// `alloc_table` supplies a frame for each missing table (zeroed here before it
/// is linked); `None` fails the map. Returns `false` on that, or when the path
/// meets a block (splitting one is not implemented). The caller owns the TLB
/// maintenance - and, on a real core, the barrier that makes a new table
/// visible to the walker before a translation can use it.
pub fn map_leaf<M: PhysMap>(root: u64, va: usize, entry: u64, mut alloc_table: impl FnMut() -> Option<u64>) -> bool {
    let mut table_pa = root;
    for level in (2..=4).rev() {
        let slot = frame_table::<M>(table_pa).slot(index(va, level));
        let cur = slot.get();
        if cur & VALID == 0 {
            let Some(frame) = alloc_table() else { return false };
            frame_table::<M>(frame).clear();
            slot.set((frame & ADDR_MASK) | VALID | TABLE_OR_PAGE);
            table_pa = frame;
        } else if cur & TABLE_OR_PAGE == 0 {
            return false;
        } else {
            table_pa = cur & ADDR_MASK;
        }
    }
    frame_table::<M>(table_pa).slot(index(va, 1)).set(entry);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAMES: usize = 32;

    #[repr(C, align(4096))]
    struct Arena([[u64; 512]; FRAMES]);

    static mut ARENA: Arena = Arena([[0; 512]; FRAMES]);
    static NEXT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1);

    struct Fake;
    impl PhysMap for Fake {
        fn table_addr(pa: u64) -> usize {
            assert!(pa < (FRAMES as u64) * 4096, "frame outside the fake physmap");
            (&raw mut ARENA) as usize + pa as usize
        }
    }

    fn alloc() -> Option<u64> {
        let n = NEXT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        (n < FRAMES).then(|| (n * 4096) as u64)
    }

    /// A user RW page: AF, inner-shareable, AttrIndx 0, AP = EL0 RW, PXN.
    const PAGE_ATTRS: u64 = (1 << 10) | (3 << 8) | (1 << 6) | (1 << 53) | VALID | TABLE_OR_PAGE;

    // One test owns the arena (tests run on threads).
    #[test]
    fn map_translate_unmap_blocks_and_records() {
        let root = 0u64;
        let va = 0x7f12_3456_7000usize;
        let leaf = 0xdead_b000 | PAGE_ATTRS;

        assert!(matches!(lookup::<Fake>(root, va), Lookup::Missing { level: 4 }));
        assert!(leaf_slot::<Fake>(root, va).is_none());
        assert!(map_leaf::<Fake>(root, va, leaf, alloc));
        assert_eq!(translate::<Fake>(root, va + 0x123), Some(0xdead_b123));
        let Lookup::Leaf { entry, .. } = lookup::<Fake>(root, va) else { panic!() };
        assert_eq!(entry, leaf, "the caller's attributes come back whole");

        // A sibling in the same L3 table allocates nothing.
        let before = NEXT.load(core::sync::atomic::Ordering::SeqCst);
        assert!(map_leaf::<Fake>(root, va + 0x1000, leaf, alloc));
        assert_eq!(NEXT.load(core::sync::atomic::Ordering::SeqCst), before);

        // An invalid leaf with software bits (a lazy record) is unmapped to every
        // reader, yet its slot is still reachable for the record's owner.
        let record = 0x2 | (7 << 6);
        assert!(map_leaf::<Fake>(root, va + 0x2000, record, alloc));
        assert!(matches!(lookup::<Fake>(root, va + 0x2000), Lookup::Missing { level: 1 }));
        assert_eq!(translate::<Fake>(root, va + 0x2000), None);
        assert_eq!(leaf_slot::<Fake>(root, va + 0x2000).unwrap().get(), record);
        assert_eq!(unmap::<Fake>(root, va + 0x2000), None, "unmap leaves a record standing");

        // L3's reserved `0b01` is not a page.
        leaf_slot::<Fake>(root, va + 0x3000).unwrap().set(0xbeef_0000 | VALID);
        assert_eq!(translate::<Fake>(root, va + 0x3000), None);

        assert_eq!(unmap::<Fake>(root, va), Some(0xdead_b000));
        assert_eq!(unmap::<Fake>(root, va), None);

        // Allocation failure links nothing.
        assert!(!map_leaf::<Fake>(root, 0x40_0000_0000, leaf, || None));
        assert!(matches!(lookup::<Fake>(root, 0x40_0000_0000), Lookup::Missing { level: 4 }));

        // A 2 MiB block at L2: translate honours it, map and leaf_slot refuse it.
        let big = 0x3_0000_0000usize;
        assert!(map_leaf::<Fake>(root, big, leaf, alloc));
        // SAFETY: arena frames.
        let l0 = unsafe { Table::at(Fake::table_addr(root)) };
        let l1 = unsafe { Table::at(Fake::table_addr(l0.get(index(big, 4)) & ADDR_MASK)) };
        let l2 = unsafe { Table::at(Fake::table_addr(l1.get(index(big, 3)) & ADDR_MASK)) };
        l2.set(index(big, 2), 0x20_0000 | (1 << 10) | VALID);
        assert!(matches!(lookup::<Fake>(root, big), Lookup::Block { level: 2, .. }));
        assert_eq!(translate::<Fake>(root, big + 0x1_2345), Some(0x20_0000 + 0x1_2345));
        assert!(!map_leaf::<Fake>(root, big, leaf, alloc), "splitting a block is refused");
        assert!(leaf_slot::<Fake>(root, big).is_none());
        assert_eq!(unmap::<Fake>(root, big), None);
    }

    #[test]
    fn spans() {
        assert_eq!(entry_span(1), 4096);
        assert_eq!(entry_span(2), 2 << 20);
        assert_eq!(entry_span(3), 1 << 30);
    }
}
