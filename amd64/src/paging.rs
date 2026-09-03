//! x86_64 4-level page tables: map, unmap, translate.
//!
//! Stage B of the amd64 bring-up. `boot.s` leaves the machine on a fixed
//! identity map of the first 1 GiB built from 2 MiB pages; this is the first
//! code that can *change* a mapping, which is the prerequisite for anything that
//! demand-pages, protects a region, or addresses memory beyond that window.
//!
//! # Relationship to `akuma-mmap` and proposal item 1
//!
//! [`Prot`] below is deliberately the shape
//! `docs/archive/REDUCING_PLATFORM_DEPENDENCY.md` §1 proposes for `akuma-mmap`: a
//! small `Copy` struct of `{read, write, exec, user}` with named constructors,
//! **not** a `u64` of architectural bits. It is defined here rather than reused
//! from `akuma-mmap` because `MmapRegion.flags` is still a raw AArch64 `u64`
//! today, and that `u64` cannot cross to x86 — the encodings share no field:
//!
//! | | AArch64 | x86_64 |
//! |---|---|---|
//! | writable | `AP[7:6]` == `01` (a *field*, not a bit) | bit 1 set |
//! | user | `AP[6]` set | bit 2 set |
//! | no-execute (EL0/user) | `UXN`, bit 54 | `NX`, bit 63 |
//! | no-execute (EL1/kernel) | `PXN`, bit 53 | — (no separate bit) |
//! | access flag | `AF`, bit 10, **must be set by software** | `A`, bit 5, set by hardware |
//!
//! Note the last two rows especially: AArch64 has two execute-permission bits to
//! x86's one, so `PXN` has no x86 counterpart and a straight bit-for-bit
//! translation loses information in one direction. That asymmetry is the concrete
//! argument for item 1 — the neutral vocabulary has to be *permissions*, and each
//! architecture's encoder decides how to spell them.
//!
//! When item 1 lands, this `Prot` should be deleted and `akuma_mmap::Prot`
//! used instead, with [`encode`] becoming that crate's x86 backend.
//!
//! # Why the tables can be dereferenced directly
//!
//! Every page-table frame is allocated from `akuma-pmm`, whose pool is inside the
//! region `boot.s` identity-maps, so a physical address *is* a valid pointer.
//! [`table_mut`] asserts that rather than assuming it: the moment the PMM is
//! given memory above 1 GiB, this stops being true, and the failure mode is a
//! page fault with no IDT installed — a triple-fault and a guest that vanishes
//! with no output.

use akuma_selftest::Suite;

use crate::phys::{PHYSMAP_LIMIT, phys_ptr};

/// Present.
const P: u64 = 1 << 0;
/// Writable.
const RW: u64 = 1 << 1;
/// User-accessible (ring 3).
const US: u64 = 1 << 2;
/// Page size — at PD level this means a 2 MiB page rather than a PT pointer.
const PS: u64 = 1 << 7;
/// Page-level write-through.
const PWT: u64 = 1 << 3;
/// Page-level cache disable.
const PCD: u64 = 1 << 4;
/// No-execute. **Requires `EFER.NXE`**, which `boot.s` sets alongside `LME`;
/// without it this is a reserved bit and setting it faults.
const NX: u64 = 1 << 63;

/// Physical address field of an entry: bits 51:12.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const PAGE_SIZE: usize = 4096;
/// Entries per table: 4 KiB / 8 bytes. Also the index mask below.
const ENTRIES: usize = 512;

/// Permissions, as permissions — not as an encoding.
///
/// See the module header: this is the vocabulary proposal item 1 wants, and the
/// point of keeping it a struct is that `RO` and `EXEC` cannot accidentally be
/// the same value, which is exactly the defect item 1.1 documents on the
/// AArch64 side.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Prot {
    pub write: bool,
    pub exec: bool,
    pub user: bool,
}

impl Prot {
    /// Kernel read-only, no execute.
    pub const KERNEL_RO: Self = Self { write: false, exec: false, user: false };
    /// Kernel read/write, no execute. The default for data.
    pub const KERNEL_RW: Self = Self { write: true, exec: false, user: false };
    /// Kernel read + execute, not writable. The only executable shape offered:
    /// there is deliberately no writable-and-executable constructor.
    pub const KERNEL_RX: Self = Self { write: false, exec: true, user: false };
    /// User read/write, no execute.
    pub const USER_RW: Self = Self { write: true, exec: false, user: true };
    /// User read + execute.
    pub const USER_RX: Self = Self { write: false, exec: true, user: true };
}

/// How a mapping is cached.
///
/// The other half of item 1's `encode(prot, attr)`, and it arrived the moment
/// something needed it rather than being invented up front: the LAPIC is MMIO,
/// and mapping a device register writeback-cached means the CPU can satisfy a
/// read from cache and never issue the access at all. On AArch64 this is an
/// `AttrIndx` into `MAIR_EL1`; here it is two PTE bits. No consumer should care
/// which — that difference is precisely what the neutral vocabulary hides.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemAttr {
    /// Normal RAM: writeback cached.
    WriteBack,
    /// Device MMIO: uncacheable, and never speculatively read.
    Device,
}

/// Encode a [`Prot`] and [`MemAttr`] into x86_64 PTE bits.
///
/// The x86 backend of what item 1 calls `encode(prot, attr)`.
const fn encode(prot: Prot, attr: MemAttr) -> u64 {
    let mut bits = P;
    if prot.write {
        bits |= RW;
    }
    if prot.user {
        bits |= US;
    }
    if !prot.exec {
        bits |= NX;
    }
    match attr {
        MemAttr::WriteBack => {}
        MemAttr::Device => bits |= PCD | PWT,
    }
    bits
}

/// Interpret a physical address as a page table.
///
/// # Safety
/// `pa` must be a page-aligned frame that is either a live page table or a
/// freshly-zeroed frame the caller is about to make one.
unsafe fn table_mut(pa: u64) -> *mut u64 {
    debug_assert!(pa < PHYSMAP_LIMIT, "page table outside the physmap");
    phys_ptr::<u64>(pa)
}

/// The active top-level table, from `CR3`.
#[must_use]
pub fn active_root() -> u64 {
    read_cr3()
}

/// Switch the active address space.
///
/// # Safety
/// `root` must be a PML4 that maps every page this kernel is currently
/// executing from and every page it will touch before switching back — the
/// kernel image, its stacks, the heap, the PMM pool and the LAPIC window. An
/// address space missing any of those faults on the instruction after `mov cr3`,
/// with no way to report it.
pub unsafe fn activate(root: u64) {
    // SAFETY: caller's obligation, stated above. Writing CR3 also flushes the
    // non-global TLB, which is what makes the switch take effect.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// The active top-level table, from `CR3`.
fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: reading CR3 copies a register into a local; it dereferences
    // nothing and has no side effect.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v & ADDR_MASK
}

/// Invalidate one page's TLB entry.
///
/// The x86 counterpart of `tlbi vaae1`, and a good illustration of proposal
/// item 3: `invlpg` is *core-local*, where AArch64's `tlbi ...is` broadcasts to
/// the inner-shareable domain. On x86 a multi-core kernel must send an IPI to
/// every other core that could hold the translation — there is no broadcast
/// form. Single-core here, so this is complete; it will not stay that way, and
/// item 3's `TlbTarget` is the vocabulary that would make the difference sayable.
fn invlpg(va: usize) {
    // SAFETY: invalidation forces a re-walk; it cannot grant access, and it
    // does not dereference `va`.
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
    }
}

/// Index into the level-`n` table for `va`. Level 4 = PML4 … level 1 = PT.
const fn index(va: usize, level: u32) -> usize {
    (va >> (12 + 9 * (level - 1))) & (ENTRIES - 1)
}

/// Fetch the next table down, allocating and zeroing it if absent.
///
/// Returns `None` if a frame could not be allocated, or if the entry is a
/// 2 MiB page rather than a table pointer — this deliberately refuses to split
/// a large page. `boot.s`'s identity map is built from 2 MiB pages, so any
/// attempt to map a 4 KiB page below 1 GiB lands here and is rejected loudly
/// instead of silently corrupting the map.
unsafe fn next_table(entry_ptr: *mut u64, user: bool) -> Option<u64> {
    // SAFETY: caller guarantees `entry_ptr` points into a live table.
    let entry = unsafe { entry_ptr.read_volatile() };

    if entry & P != 0 {
        if entry & PS != 0 {
            return None; // a 2 MiB page; splitting is not implemented
        }
        // Widen permissions on the way down: a parent that is not user-
        // accessible or not writable masks every child on x86, so an
        // intermediate entry has to be at least as permissive as any leaf
        // beneath it. Enforcement lives entirely in the leaf.
        let want = P | RW | if user { US } else { 0 };
        if entry & want != want {
            // SAFETY: as above.
            unsafe { entry_ptr.write_volatile((entry | want) & !NX) };
        }
        return Some(entry & ADDR_MASK);
    }

    let frame = akuma_pmm::alloc_page()? as u64;
    // SAFETY: a fresh PMM frame inside the identity map; zeroing it is what
    // makes it a valid empty table.
    unsafe {
        core::ptr::write_bytes(phys_ptr::<u8>(frame), 0, PAGE_SIZE);
        // Intermediate entries are permissive; the leaf decides. NX is left
        // clear here for the same reason.
        entry_ptr.write_volatile(frame | P | RW | if user { US } else { 0 });
    }
    Some(frame)
}

/// Map `va` to `pa` in the **active** address space.
pub fn map_page(va: usize, pa: u64, prot: Prot, attr: MemAttr) -> bool {
    map_page_in(read_cr3(), va, pa, prot, attr)
}

/// Map `va` to `pa` in the address space rooted at `root`.
///
/// Taking the root as a parameter rather than always reading `CR3` is what lets
/// a process's tables be built *before* they are activated — the alternative is
/// switching to a half-built address space, which cannot be done safely from
/// code that is itself running out of memory those tables describe.
pub fn map_page_in(root: u64, va: usize, pa: u64, prot: Prot, attr: MemAttr) -> bool {
    assert_eq!(va % PAGE_SIZE, 0, "va must be page aligned");
    assert_eq!(pa % PAGE_SIZE as u64, 0, "pa must be page aligned");

    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame inside the identity map.
        let entry_ptr = unsafe { table_mut(table).add(index(va, level)) };
        // SAFETY: as above.
        match unsafe { next_table(entry_ptr, prot.user) } {
            Some(next) => table = next,
            None => return false,
        }
    }

    // SAFETY: `table` is the PT frame; the index is masked to 0..512.
    unsafe {
        table_mut(table)
            .add(index(va, 1))
            .write_volatile(pa | encode(prot, attr));
    }
    invlpg(va);
    true
}

/// Remove the mapping for `va` in the active address space.
pub fn unmap_page(va: usize) -> Option<u64> {
    unmap_page_in(read_cr3(), va)
}

/// Remove the mapping for `va` in the address space rooted at `root`.
pub fn unmap_page_in(root: u64, va: usize) -> Option<u64> {
    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame inside the identity map.
        let entry = unsafe { table_mut(table).add(index(va, level)).read_volatile() };
        if entry & P == 0 || entry & PS != 0 {
            return None;
        }
        table = entry & ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame.
    let leaf = unsafe { table_mut(table).add(index(va, 1)) };
    // SAFETY: as above.
    let entry = unsafe { leaf.read_volatile() };
    if entry & P == 0 {
        return None;
    }
    // SAFETY: as above.
    unsafe { leaf.write_volatile(0) };
    invlpg(va);
    Some(entry & ADDR_MASK)
}

/// Resolve `va` in the active address space.
///
/// Walks rather than trusting a shadow structure, so it reports what the
/// *hardware* would do — which is the only useful answer when checking whether a
/// mapping took effect.
pub fn translate(va: usize) -> Option<u64> {
    translate_in(read_cr3(), va)
}

/// What a page-table walk found.
enum Walk {
    /// Nothing maps `va`.
    Missing,
    /// A 2 MiB page at PD level; the entry, and the size of the page it maps.
    Large(u64, u64),
    /// A 4 KiB leaf entry.
    Leaf(u64),
}

/// Walk `root` down to whatever maps `va`.
///
/// One walker, so that "what does this resolve to" and "what permissions does
/// it carry" can never disagree — they are two readings of the same entry.
fn walk_in(root: u64, va: usize) -> Walk {
    let mut table = root;
    for level in (2..=4).rev() {
        // SAFETY: `table` is a live table frame reached through the physmap.
        let entry = unsafe { table_mut(table).add(index(va, level)).read_volatile() };
        if entry & P == 0 {
            return Walk::Missing;
        }
        if entry & PS != 0 {
            // Only PD level (2) can carry PS here; a PDPT 1 GiB page is never
            // created by this kernel, so the size is fixed.
            return Walk::Large(entry, 1 << 21);
        }
        table = entry & ADDR_MASK;
    }
    // SAFETY: `table` is the PT frame; the index is masked to 0..512.
    let entry = unsafe { table_mut(table).add(index(va, 1)).read_volatile() };
    if entry & P == 0 {
        Walk::Missing
    } else {
        Walk::Leaf(entry)
    }
}

/// Resolve `va` in the address space rooted at `root`.
pub fn translate_in(root: u64, va: usize) -> Option<u64> {
    match walk_in(root, va) {
        Walk::Missing => None,
        Walk::Large(entry, size) => Some((entry & ADDR_MASK) + (va as u64 & (size - 1))),
        Walk::Leaf(entry) => Some((entry & ADDR_MASK) + (va as u64 & (PAGE_SIZE as u64 - 1))),
    }
}

/// The permissions `va` is mapped with in the address space rooted at `root`.
///
/// The inverse of [`encode`], and the reason it exists is the ELF loader: two
/// `PT_LOAD` segments can land in one page, and deciding what that page's
/// permissions must become needs to *read* what they currently are. Reading the
/// hardware's own entry rather than a shadow record is the same discipline
/// [`translate_in`] follows — and it is what lets a self-test assert that a code
/// page really is non-writable rather than that the loader believes it is.
///
/// [`MemAttr`] is deliberately not returned: nothing needs it yet, and a decoder
/// that guesses would have to invent an answer for `PCD` without `PWT`.
#[must_use]
pub fn prot_in(root: u64, va: usize) -> Option<Prot> {
    let entry = match walk_in(root, va) {
        Walk::Missing => return None,
        Walk::Large(entry, _) | Walk::Leaf(entry) => entry,
    };
    Some(Prot {
        write: entry & RW != 0,
        exec: entry & NX == 0,
        user: entry & US != 0,
    })
}

/// Map a frame outside the identity map, write through it, read it back, unmap.
///
/// Chosen VA is 1 GiB — the first address `boot.s` does *not* map, so the whole
/// path (allocate PDPT entry, PD, PT, leaf) is exercised and a false pass from
/// accidentally hitting the identity map is impossible.
pub fn smoke_test(t: &mut Suite) {
    const TEST_VA: usize = 1 << 30;
    const PATTERN: u64 = 0x0bad_c0de_dead_beef;

    if !t.check("paging: test VA starts unmapped", translate(TEST_VA).is_none()) {
        return;
    }
    let Some(frame) = akuma_pmm::alloc_page() else {
        t.check("paging: frame available", false);
        return;
    };

    if !t.check(
        "paging: map_page",
        map_page(TEST_VA, frame as u64, Prot::KERNEL_RW, MemAttr::WriteBack),
    ) {
        return;
    }
    t.check_eq(
        "paging: translate matches",
        translate(TEST_VA).unwrap_or(0),
        frame as u64,
    );

    // SAFETY: the mapping was just installed and verified by a table walk.
    let readback = unsafe {
        let p = TEST_VA as *mut u64;
        p.write_volatile(PATTERN);
        p.read_volatile()
    };
    t.check_eq("paging: readback through mapping", readback, PATTERN);

    // The write must be visible at the *physical* address too — that is what
    // proves the mapping points where the walk said, rather than at some other
    // page that happens to be readable.
    // SAFETY: PMM frames are inside the identity map.
    let via_phys = unsafe { phys_ptr::<u64>(frame as u64).read_volatile() };
    t.check_eq("paging: visible via physical alias", via_phys, PATTERN);

    t.check_eq(
        "paging: unmap returns the frame",
        unmap_page(TEST_VA).unwrap_or(0),
        frame as u64,
    );
    t.check("paging: unmapped after unmap", translate(TEST_VA).is_none());

    akuma_pmm::free_page(frame, 0);
    nx_encoding_check(t);
}

/// Check that the encoder cannot express write+execute, and that NX is set.
///
/// A pure check on [`encode`], not on the hardware: the hardware half needs a
/// `#PF` handler to observe. What it does prove is the property proposal item 1
/// exists to guarantee — that "read-only" and "executable" are distinct values,
/// which on the AArch64 side they currently are not
/// (`user_flags::RO == user_flags::EXEC`).
fn nx_encoding_check(t: &mut Suite) {
    let rw = encode(Prot::KERNEL_RW, MemAttr::WriteBack);
    let rx = encode(Prot::KERNEL_RX, MemAttr::WriteBack);
    let ro = encode(Prot::KERNEL_RO, MemAttr::WriteBack);
    let urw = encode(Prot::USER_RW, MemAttr::WriteBack);
    let urx = encode(Prot::USER_RX, MemAttr::WriteBack);
    let dev = encode(Prot::KERNEL_RW, MemAttr::Device);

    t.check("W^X: kernel data is non-executable", rw & NX != 0 && rw & RW != 0);
    t.check("W^X: kernel code is exec and not writable", rx & NX == 0 && rx & RW == 0);
    t.check("W^X: read-only is neither", ro & NX != 0 && ro & RW == 0);
    // The defect proposal item 1.1 describes, absent here by construction.
    t.check("W^X: read-only and executable differ", ro != rx);
    t.check("W^X: user mappings carry US", urw & US != 0 && urx & US != 0);
    t.check("W^X: user data non-exec, user code non-writable", urw & NX != 0 && urx & RW == 0);
    t.check("W^X: kernel mappings are not user-reachable", (rw | rx | ro) & US == 0);
    t.check("attr: device is uncacheable, normal is not",
            dev & (PCD | PWT) == PCD | PWT && rw & (PCD | PWT) == 0);
}

/// Unmap the lower half of the kernel's own address space.
///
/// `boot.s` builds an identity map so the 32-bit trampoline has somewhere to
/// stand. Once the kernel is executing from its high linked address and reaching
/// physical memory through the physmap, that mapping is not merely unnecessary —
/// it *occupies the lower half*, which belongs to userspace. Dropping it is what
/// lets a process be mapped wherever it is linked.
///
/// Only PML4 slot 0 is cleared: it is the only lower-half slot `boot.s` filled.
pub fn drop_identity_map() {
    // SAFETY: the caller must already be running from the kernel window with a
    // physmap stack — which `boot.s`'s `high_entry` arranges before it calls
    // `kmain`. Reloading CR3 flushes the now-stale identity translations.
    unsafe {
        let root = read_cr3();
        table_mut(root).write_volatile(0);
        activate(root);
    }
}

/// One address space: a PML4 of its own, sharing the kernel's mappings.
///
/// # What is shared and what is not
///
/// The kernel lives entirely in the upper half — its image, the physmap that
/// covers every physical page it touches, and the device window — so every
/// address space must contain those or the first instruction after `mov cr3`
/// faults, and the timer handler could not write EOI while a process runs.
/// Rather than copy them, a new space **shares the entries**: its three
/// [`SHARED_PML4_SLOTS`] point at the very same tables the kernel's own PML4
/// does, so there is one kernel mapping and no possibility of copies drifting.
///
/// The whole lower half — all 256 remaining PML4 slots — is private. Two spaces
/// can map different frames at the same virtual address and neither can see the
/// other's, which is what [`crate::usermode::smoke_test`] checks before it runs
/// anything.
///
/// # What this is not
///
/// There is no per-space kernel *stack* separation, no ASID/PCID tagging (every
/// `mov cr3` flushes the whole non-global TLB), and no reference counting: an
/// address space is freed by [`Self::free`] at a point the caller picks.
pub struct AddressSpace {
    root: u64,
}

/// PML4 slots every address space shares with the kernel.
///
/// Slots 0..255 are userspace and stay private; these three are the kernel's
/// whole world. Sharing at PML4 level rather than PDPT level is what Stage K
/// bought: the lower half is now entirely the process's, so a user program can
/// be mapped wherever it is linked instead of having to dodge the kernel.
const SHARED_PML4_SLOTS: [usize; 3] = [
    256, // physmap    — every physical page the kernel touches
    257, // device map — MMIO, uncached
    511, // the kernel image itself
];

impl AddressSpace {
    /// Build a new address space sharing the kernel's mappings.
    pub fn new() -> Option<Self> {
        let root = akuma_pmm::alloc_page()? as u64;

        // SAFETY: a fresh PMM frame, reached through the physmap. Zeroing is
        // what makes it a valid empty table; the shared entries written below
        // are the only non-zero ones, so the whole lower half starts unmapped.
        unsafe {
            core::ptr::write_bytes(phys_ptr::<u8>(root), 0, PAGE_SIZE);

            // Share, do not copy: alias the kernel's own top-level entries, so
            // there is one kernel mapping and the copies cannot drift.
            let kroot = read_cr3();
            for slot in SHARED_PML4_SLOTS {
                let e = table_mut(kroot).add(slot).read_volatile();
                table_mut(root).add(slot).write_volatile(e);
            }
        }
        Some(Self { root })
    }

    /// The value to load into `CR3`.
    #[must_use]
    pub const fn root(&self) -> u64 {
        self.root
    }

    /// Map a page in this space.
    pub fn map(&self, va: usize, pa: u64, prot: Prot, attr: MemAttr) -> bool {
        map_page_in(self.root, va, pa, prot, attr)
    }

    /// Resolve a virtual address in this space, without activating it.
    #[must_use]
    pub fn translate(&self, va: usize) -> Option<u64> {
        translate_in(self.root, va)
    }

    /// The permissions `va` carries in this space, or `None` if unmapped.
    #[must_use]
    pub fn prot(&self, va: usize) -> Option<Prot> {
        prot_in(self.root, va)
    }

    /// Release this space's own tables.
    ///
    /// Frees the PML4, the PDPT, and every table below a **non-shared** PDPT
    /// slot. Deliberately not a `Drop` impl: freeing an address space that is
    /// still in `CR3` unmaps the code doing the freeing, and a destructor that
    /// can be triggered by falling out of scope makes that too easy. It has to
    /// be asked for.
    ///
    /// Leaf frames are the caller's — this frees page *tables*, not the pages
    /// they point at.
    pub fn free(self) {
        // SAFETY: every table was allocated by this space's own `map` calls and
        // is reached through the physmap. The shared PML4 slots are skipped, so
        // the kernel's own tables survive; only the lower half is walked.
        unsafe {
            for l4 in 0..ENTRIES {
                if SHARED_PML4_SLOTS.contains(&l4) {
                    continue;
                }
                let e4 = table_mut(self.root).add(l4).read_volatile();
                if e4 & P == 0 {
                    continue;
                }
                let pdpt = e4 & ADDR_MASK;
                for l3 in 0..ENTRIES {
                    let e3 = table_mut(pdpt).add(l3).read_volatile();
                    if e3 & P == 0 || e3 & PS != 0 {
                        continue;
                    }
                    let pd = e3 & ADDR_MASK;
                    for l2 in 0..ENTRIES {
                        let e2 = table_mut(pd).add(l2).read_volatile();
                        if e2 & P != 0 && e2 & PS == 0 {
                            akuma_pmm::free_page((e2 & ADDR_MASK) as usize, 0);
                        }
                    }
                    akuma_pmm::free_page(pd as usize, 0);
                }
                akuma_pmm::free_page(pdpt as usize, 0);
            }
        }
        akuma_pmm::free_page(self.root as usize, 0);
    }
}
