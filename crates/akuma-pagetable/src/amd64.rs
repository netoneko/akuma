//! The amd64 4-level walker, over [`akuma_pte_amd64`]'s entry format.
//!
//! One descent, [`lookup`], reads the path from a PML4 root to whatever maps a
//! virtual address and reports it in the shape every caller wants: where the walk
//! stopped, the entry, a [`Slot`] to edit it, and the permissions **accumulated
//! across every level**. `translate`, `unmap`, "is this reachable from ring 3",
//! and the range walk that skips an absent subtree whole are all readings of that
//! one result, which is why they cannot disagree.
//!
//! Mapping is [`map_leaf`], which allocates missing intermediate tables through a
//! caller-supplied closure — the kernel's own mappings allocate straight from the
//! PMM, a process's also record each frame in its ledger, and that difference is
//! the closure's, not a flag.

use akuma_pte_amd64::{self as pte, Step};

use crate::{PhysMap, Slot, Table, index};

/// The table in the frame at `pa`.
///
/// **The one place this module claims "this frame is a page table".** Every
/// walker reaches memory through here, so the crate contract's three clauses are
/// stated once, on [`Table::at`], and discharged once, below: the caller's root
/// is a PML4 (a promise), every other frame is reached through a present,
/// non-large entry of a live table, and a fresh frame is one the caller's
/// allocator just handed over.
fn frame_table<'a, M: PhysMap>(pa: u64) -> &'a Table {
    // SAFETY: as above.
    unsafe { Table::at(M::table_addr(pa)) }
}

/// Where a [`lookup`] ended.
#[derive(Clone, Copy)]
pub enum Lookup<'a> {
    /// The entry at `level` (4 = PML4 … 1 = PT) is not present: nothing maps the
    /// address, and nothing does for the whole `pte::entry_span(level)`
    /// around it.
    Missing { level: u32 },
    /// A large page ends the walk at `level` (3 = 1 GiB, 2 = 2 MiB).
    Large { level: u32, entry: u64, us_all: bool, rw_all: bool },
    /// A present 4 KiB leaf.
    Leaf { slot: Slot<'a>, entry: u64, us_all: bool, rw_all: bool },
}

/// Walk `root` down to whatever maps `va`.
///
/// `us_all` / `rw_all` are the AND of `U/S` and `R/W` over every entry the walk
/// passed through, **including the final one** — on amd64 the effective
/// permission is the AND across the whole path, so a PML4E with `U/S` clear makes
/// the 512 GiB beneath it supervisor-only however the leaf is marked.
///
/// `root` is a promise; see the crate docs.
#[must_use]
pub fn lookup<'a, M: PhysMap>(root: u64, va: usize) -> Lookup<'a> {
    let mut table_pa = root;
    let mut us_all = true;
    let mut rw_all = true;
    for level in (2..=4).rev() {
        let table = frame_table::<M>(table_pa);
        let entry = table.get(index(va, level));
        if !pte::is_present(entry) {
            return Lookup::Missing { level };
        }
        us_all &= entry & pte::US != 0;
        rw_all &= entry & pte::RW != 0;
        if pte::is_large(entry) {
            return Lookup::Large { level, entry, us_all, rw_all };
        }
        table_pa = pte::frame(entry);
    }
    let table = frame_table::<M>(table_pa);
    let slot = table.slot(index(va, 1));
    let entry = slot.get();
    if !pte::is_present(entry) {
        return Lookup::Missing { level: 1 };
    }
    us_all &= entry & pte::US != 0;
    rw_all &= entry & pte::RW != 0;
    Lookup::Leaf { slot, entry, us_all, rw_all }
}

/// The physical address `va` resolves to, large pages included.
///
/// Walks rather than trusting a shadow structure, so it reports what the
/// *hardware* would do — the only useful answer when checking whether a mapping
/// took effect.
#[must_use]
pub fn translate<M: PhysMap>(root: u64, va: usize) -> Option<u64> {
    match lookup::<M>(root, va) {
        Lookup::Missing { .. } => None,
        // The base clears the PAT bit (bit 12), which is not an address bit in a
        // large-page entry — the framebuffer's PDEs carry it.
        Lookup::Large { level, entry, .. } => {
            Some(pte::large_frame(entry, level) + (va as u64 & (pte::entry_span(level) as u64 - 1)))
        }
        Lookup::Leaf { entry, .. } => Some(pte::frame(entry) + (va as u64 & (pte::PAGE_SIZE as u64 - 1))),
    }
}

/// The leaf slot for `va`, without allocating.
///
/// `None` if any level is absent or a large page — this walker never creates one,
/// so a `PS` entry on the way down means the mapping is not a 4 KiB page and is
/// not ours to edit.
#[must_use]
pub fn leaf_slot<'a, M: PhysMap>(root: u64, va: usize) -> Option<Slot<'a>> {
    match lookup::<M>(root, va) {
        Lookup::Leaf { slot, .. } => Some(slot),
        _ => None,
    }
}

/// Clear the leaf for `va`, returning the frame it mapped. The caller owns the
/// TLB invalidation and the frame's fate.
#[must_use = "the frame is the caller's to free, and None means nothing was mapped"]
pub fn unmap<M: PhysMap>(root: u64, va: usize) -> Option<u64> {
    let Lookup::Leaf { slot, entry, .. } = lookup::<M>(root, va) else {
        return None;
    };
    slot.set(0);
    Some(pte::frame(entry))
}

/// Install `entry` as the 4 KiB leaf for `va`, building the path to it.
///
/// `user` says whether the intermediate entries must be user-accessible (they are
/// widened on the way down if not — a parent masks every child). `alloc_table`
/// supplies a frame for each missing table; it is called **before** the frame is
/// linked, may record it wherever the caller keeps such things, and a `None`
/// fails the map. Returns `false` on that, or when the path meets a large page
/// (splitting one is not implemented — `boot.s`'s identity map is 2 MiB pages,
/// and mapping a 4 KiB page below 1 GiB lands here instead of silently
/// corrupting it).
///
/// The caller owns the TLB invalidation.
pub fn map_leaf<M: PhysMap>(
    root: u64,
    va: usize,
    entry: u64,
    user: bool,
    mut alloc_table: impl FnMut() -> Option<u64>,
) -> bool {
    let mut table_pa = root;
    for level in (2..=4).rev() {
        let table = frame_table::<M>(table_pa);
        let slot = table.slot(index(va, level));
        match pte::step_down(slot.get(), user) {
            Step::Large => return false,
            Step::Table { frame, widened } => {
                if let Some(wide) = widened {
                    slot.set(wide);
                }
                table_pa = frame;
            }
            Step::Absent => {
                let Some(frame) = alloc_table() else { return false };
                // Zeroing the fresh frame is what makes it a valid empty table.
                frame_table::<M>(frame).clear();
                slot.set(pte::table_entry(frame, user));
                table_pa = frame;
            }
        }
    }
    frame_table::<M>(table_pa).slot(index(va, 1)).set(entry);
    true
}

/// Install `root` as the active PML4.
///
/// Here rather than in `akuma-cpu` for the reason the AArch64 `msr ttbr0_el1` is
/// not there: a safe wrapper would let any safe code repoint what the MMU
/// dereferences. Here rather than at each caller because there were three
/// byte-identical copies (`amd64/src/paging.rs` ×2, `akuma-mmu`).
///
/// # Safety
/// `root` must be a PML4 that maps every page the kernel is currently executing
/// from and every page it will touch before switching back — the kernel image,
/// its stacks, the heap, the PMM pool and the LAPIC window. A root missing any of
/// those faults on the instruction after `mov cr3`, with no way to report it.
#[cfg(target_arch = "x86_64")]
pub unsafe fn load_cr3(root: u64) {
    // SAFETY: the caller's obligation, stated above. Writing CR3 also flushes
    // the non-global TLB, which is what makes the switch take effect.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// Make `root_frame` a fresh PML4 that shares the kernel's upper half.
///
/// The `shared` slots alias the live `kernel_root`'s — aliased, not copied, so one
/// kernel mapping cannot drift. Every other slot is zero, so the whole lower half
/// starts unmapped.
///
/// `root_frame` is a frame the caller just allocated; it is zeroed.
pub fn init_root<M: PhysMap>(root_frame: u64, kernel_root: u64, shared: &[usize]) {
    let new = frame_table::<M>(root_frame);
    let kernel = frame_table::<M>(kernel_root);
    new.clear();
    for &slot in shared {
        new.set(slot, kernel.get(slot));
    }
}

/// Zero one entry of the root table. For `drop_identity_map`: PML4 slot 0 is the
/// only lower-half slot `boot.s` filled.
pub fn clear_root_slot<M: PhysMap>(root: u64, i: usize) {
    frame_table::<M>(root).set(i, 0);
}

#[cfg(test)]
mod tests {
    use akuma_pte_amd64::{self as pte, MemAttr, PteProt};

    use super::*;

    /// A fake physmap: "physical" addresses are offsets into one leaked arena,
    /// so a whole PML4 -> PT tree can be built and walked on the host.
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

    // One test owns the arena: the walkers are exercised in sequence on a single
    // tree, and `cargo test` runs tests on threads.
    #[test]
    fn map_translate_unmap_and_the_and_of_permissions() {
        let root = 0u64; // frame 0
        let va = 0x7f12_3456_7000usize;
        let leaf = pte::leaf(0xdead_b000, PteProt::USER_RW, MemAttr::WriteBack, false);

        assert!(matches!(lookup::<Fake>(root, va), Lookup::Missing { level: 4 }));
        assert_eq!(translate::<Fake>(root, va), None);

        assert!(map_leaf::<Fake>(root, va, leaf, true, alloc));
        assert_eq!(translate::<Fake>(root, va), Some(0xdead_b000));
        assert_eq!(translate::<Fake>(root, va + 0x123), Some(0xdead_b123));
        let Lookup::Leaf { entry, us_all, rw_all, .. } = lookup::<Fake>(root, va) else { panic!() };
        assert_eq!(entry, leaf);
        assert!(us_all && rw_all, "a user RW leaf under permissive parents is user-writable");

        // A second mapping in the same PT allocates nothing.
        let before = NEXT.load(core::sync::atomic::Ordering::SeqCst);
        assert!(map_leaf::<Fake>(root, va + 0x1000, leaf, true, alloc));
        assert_eq!(NEXT.load(core::sync::atomic::Ordering::SeqCst), before);

        // A kernel-only leaf: the path is not user-reachable.
        let kleaf = pte::leaf(0xbeef_0000, PteProt::KERNEL_RW, MemAttr::WriteBack, false);
        assert!(map_leaf::<Fake>(root, va + 0x2000, kleaf, false, alloc));
        let Lookup::Leaf { us_all, .. } = lookup::<Fake>(root, va + 0x2000) else { panic!() };
        assert!(!us_all, "the leaf's own U/S counts");

        // leaf_slot edits in place; unmap clears and returns the frame.
        let slot = leaf_slot::<Fake>(root, va).unwrap();
        slot.set(slot.get() | pte::COW);
        assert!(pte::is_cow(leaf_slot::<Fake>(root, va).unwrap().get()));
        assert_eq!(unmap::<Fake>(root, va), Some(0xdead_b000));
        assert_eq!(unmap::<Fake>(root, va), None);
        assert!(matches!(lookup::<Fake>(root, va), Lookup::Missing { level: 1 }));

        // A mapping that must be user-reachable widens a kernel-only path.
        assert!(map_leaf::<Fake>(root, va + 0x2000, leaf, true, alloc));
        let Lookup::Leaf { us_all, .. } = lookup::<Fake>(root, va + 0x2000) else { panic!() };
        assert!(us_all);

        // Allocation failure fails the map and links nothing.
        assert!(!map_leaf::<Fake>(root, 0x40_0000_0000, leaf, true, || None));
        assert!(matches!(lookup::<Fake>(root, 0x40_0000_0000), Lookup::Missing { level: 4 }));

        // Large pages: translate honours them, map refuses to split them.
        let big = 0x3_0000_0000usize;
        assert!(map_leaf::<Fake>(root, big, leaf, true, alloc)); // builds PML4->PDPT->PD->PT
        let pd = {
            let Lookup::Leaf { .. } = lookup::<Fake>(root, big) else { panic!() };
            // Re-walk by hand to the PD entry and make it a 2 MiB page.
            // SAFETY: arena frames.
            let pml4 = unsafe { Table::at(Fake::table_addr(root)) };
            let pdpt_pa = pte::frame(pml4.get(index(big, 4)));
            let pdpt = unsafe { Table::at(Fake::table_addr(pdpt_pa)) };
            let pd_pa = pte::frame(pdpt.get(index(big, 3)));
            unsafe { Table::at(Fake::table_addr(pd_pa)) }
        };
        pd.set(index(big, 2), 0x20_0000 | pte::P | pte::RW | pte::PS | pte::PAT_4K);
        assert!(matches!(lookup::<Fake>(root, big), Lookup::Large { level: 2, .. }));
        assert_eq!(translate::<Fake>(root, big + 0x1_2345), Some(0x20_0000 + 0x1_2345));
        assert!(!map_leaf::<Fake>(root, big, leaf, true, alloc), "splitting a large page is refused");
        assert_eq!(unmap::<Fake>(root, big), None);

        clear_root_slot::<Fake>(root, index(big, 4));
        assert!(matches!(lookup::<Fake>(root, big), Lookup::Missing { level: 4 }));
    }
}
