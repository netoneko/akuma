//! x86_64 4-level page tables: map, unmap, translate.
//!
//! Stage B of the amd64 bring-up. `boot.s` leaves the machine on a fixed
//! identity map of the first 1 GiB built from 2 MiB pages; this is the first
//! code that can *change* a mapping, which is the prerequisite for anything that
//! demand-pages, protects a region, or addresses memory beyond that window.
//!
//! # Two `Prot`s, and why they stay two
//!
//! [`PteProt`] below is the **page-table** vocabulary: what the hardware is
//! told, including a [`COW`] marker bit that is not a permission at all.
//! `akuma_mmap::Prot` is the **region** vocabulary: what a mapping is *supposed*
//! to be. They were both called `Prot` until 2026-09-07 and the rename is what
//! makes the difference visible at every call site.
//!
//! [`encode`] is now `akuma_mmap::Prot`'s x86 backend, reached through
//! [`PteProt::from_region`]. The bits stay here — with the walker that writes
//! them — because the two encodings share **no field**:
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
//! translation loses information in one direction. That asymmetry is why
//! `akuma_mmap::Prot` is an opaque token rather than `{read, write, exec}` —
//! `RO` and `RX` grant EL0 the same thing and differ only in `PXN`, which x86
//! cannot spell, so x86 maps both to the same PTE and says so in
//! [`PteProt::from_region`].
//!
//! Do **not** try to make the region token carry the CoW marker: it is x86 PTE
//! bit 9, meaningless to a region, and `akuma-cow` already takes decoded
//! booleans precisely so it never sees a PTE.
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
/// Available-to-software bit 9. The CPU ignores bits 9..11 in a PTE entirely,
/// which is what makes one usable as the copy-on-write marker.
///
/// **A marker is required, not a convenience.** A CoW-demoted page and a page
/// that is read-only *on purpose* (`mprotect(PROT_READ)`, an ELF `.rodata`
/// segment) are byte-identical in the page table otherwise, and this target has
/// no region table to consult instead — the PTE is the only record there is.
/// Deciding from the share count alone promotes an `mprotect`ed page to
/// writable the moment its frame happens to be shared, which is the trap
/// `docs/archive/GRANT_RECORDS_VS_DENY_RECORDS.md` is about.
const COW: u64 = 1 << 9;

/// No-execute. **Requires `EFER.NXE`**, which `boot.s` sets alongside `LME`;
/// without it this is a reserved bit and setting it faults.
const NX: u64 = 1 << 63;

/// Physical address field of an entry: bits 51:12.
const ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

const PAGE_SIZE: usize = 4096;
/// Entries per table: 4 KiB / 8 bytes. Also the index mask below.
const ENTRIES: usize = 512;

/// The error code the CPU pushes for vector 14, asked as questions.
///
/// # Why this is a type and not five `const`s at the fault site
///
/// It was five `const`s at the fault site — `PF_PRESENT`, `PF_WRITE` and
/// `PF_USER` declared **inside** `idt::page_fault_dispatch`'s body, with
/// `code & 1 == 0` written out separately a few lines above them for the
/// not-present test. Three problems, and none of them is style:
///
/// - The bits had no owner. They are the fault-side counterpart of the PTE bits
///   at the top of this module — `P`, `RW`, `US` — and a second, private
///   spelling of them in another file is exactly the drift this target already
///   paid for once with `PteProt` (see the module header).
/// - Two of the five were missing. Bit 3 (a reserved bit set in a paging
///   structure) and bit 4 (an instruction fetch) were simply not named, so the
///   handler could not distinguish "the walker built a malformed entry" — always
///   a kernel bug — from an ordinary protection fault, and could not tell a
///   `#PF` on a *fetch* from one on a data access. Demand paging needs the
///   second to decide whether a lazy page must be mapped executable.
/// - Nothing pinned the positions. A one-off `1 << 2` typed at a call site is
///   right or wrong with no test either way; [`smoke_test`] now checks each
///   against a decoded value.
///
/// Positions are Intel SDM Vol. 3A §4.7, "Page-Fault Exceptions". Bit 4 is only
/// ever set when `EFER.NXE` is on, which `boot.s` sets alongside `LME`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PageFaultCode(u64);

impl PageFaultCode {
    /// Bit 0. Set: the fault was a **protection violation** on a page that is
    /// present. Clear: the page was not present.
    const P_BIT: u64 = 1 << 0;
    /// Bit 1. Set: the access that faulted was a write.
    const WR_BIT: u64 = 1 << 1;
    /// Bit 2. Set: the access came from ring 3.
    const US_BIT: u64 = 1 << 2;
    /// Bit 3. Set: a reserved bit was set in one of the paging-structure
    /// entries the walk went through.
    const RSVD_BIT: u64 = 1 << 3;
    /// Bit 4. Set: the access was an instruction fetch. Requires `EFER.NXE`.
    const ID_BIT: u64 = 1 << 4;

    /// Wrap the raw code the CPU pushed.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw code, for the diagnostic printers.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// The page was **not present**: nothing was mapped at the faulting address.
    ///
    /// The demand-paging question. Its complement is [`Self::protection`], and
    /// keeping both names is deliberate — servicing a *protection* fault by
    /// mapping a fresh page would silently defeat whatever the protection was
    /// for.
    #[must_use]
    pub const fn not_present(self) -> bool {
        self.0 & Self::P_BIT == 0
    }

    /// A present page was accessed in a way its permissions forbid.
    #[must_use]
    pub const fn protection(self) -> bool {
        !self.not_present()
    }

    /// The faulting access was a write.
    #[must_use]
    pub const fn is_write(self) -> bool {
        self.0 & Self::WR_BIT != 0
    }

    /// The faulting access was a **user-mode** access — it came from ring 3.
    ///
    /// Note this reflects the CPL of the access, not the `U/S` bit of the page:
    /// a `copy_to_user` writing a user page from ring 0 answers `false` here.
    #[must_use]
    pub const fn is_user_mode(self) -> bool {
        self.0 & Self::US_BIT != 0
    }

    /// A paging-structure entry had a reserved bit set.
    ///
    /// Never a userspace fault and never fixable: it means this kernel's own
    /// walker wrote a malformed entry (the usual cause is setting [`NX`] with
    /// `EFER.NXE` clear). Named so the handler can say so rather than reporting
    /// a generic protection fault.
    #[must_use]
    pub const fn reserved_bit(self) -> bool {
        self.0 & Self::RSVD_BIT != 0
    }

    /// The faulting access was an instruction fetch, not a data access.
    #[must_use]
    pub const fn instruction_fetch(self) -> bool {
        self.0 & Self::ID_BIT != 0
    }

    /// The shape a copy-on-write break answers to: a write to a page that **is**
    /// mapped, from either ring.
    ///
    /// One predicate rather than two ANDed at the call site, because both
    /// conditions are load-bearing together and a reader has to be able to see
    /// that. Absence would mean demand paging, not sharing; a read would mean
    /// the page is genuinely unreadable.
    ///
    /// # Why ring 3 is *not* part of it
    ///
    /// It was, until 2026-09-07 — the fault site required `is_user_mode` — and
    /// that was wrong in a way nothing could observe, because `CR0.WP` was also
    /// clear and no ring-0 write ever faulted to begin with. Both halves are
    /// fixed together (see `akuma_user_access`'s `CR0_WP`): `copy_to_user`
    /// writes a user page **from ring 0**, so a `read(2)` into a forked child's
    /// untouched buffer arrives here with `U/S` clear. Requiring ring 3 sends it
    /// to the user-copy fixup instead, which is an unexplained `EFAULT` on a
    /// perfectly ordinary read.
    ///
    /// Letting a supervisor write through costs nothing: the break itself
    /// re-checks that the page is user-accessible (`prot.user`) and refuses a
    /// kernel page, and `akuma_cow` refuses a page that is read-only on purpose
    /// rather than CoW-marked. A ring-0 write to either still falls through to
    /// the fixup, which is the `EFAULT` those cases deserve.
    #[must_use]
    pub const fn is_write_to_present_page(self) -> bool {
        self.protection() && self.is_write()
    }
}

/// Permissions, as permissions — not as an encoding.
///
/// See the module header: this is the vocabulary proposal item 1 wants, and the
/// point of keeping it a struct is that `RO` and `EXEC` cannot accidentally be
/// the same value, which is exactly the defect item 1.1 documents on the
/// AArch64 side.
// Four `bool`s, and clippy would rather they were a bitflag type. They are not:
// the whole point of this struct (see the module header, and the `RO`-vs-`EXEC`
// defect it exists to prevent) is that each permission is a *named field* that
// cannot be confused with another, which a packed encoding is exactly what
// gives up.
#[allow(clippy::struct_excessive_bools)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PteProt {
    pub write: bool,
    pub exec: bool,
    pub user: bool,
    /// Copy-on-write: mapped read-only, but a write fault should break the
    /// sharing rather than kill the process. See [`COW`].
    ///
    /// Always accompanied by `write: false` — a page that is both writable and
    /// CoW would never fault, so the sharing would never break and two address
    /// spaces would diverge silently. [`PteProt::cow`] is the only constructor and
    /// it enforces that.
    pub cow: bool,
}

impl PteProt {
    /// Kernel read-only, no execute.
    pub const KERNEL_RO: Self = Self { write: false, exec: false, user: false, cow: false };
    /// Kernel read/write, no execute. The default for data.
    pub const KERNEL_RW: Self = Self { write: true, exec: false, user: false, cow: false };
    /// Kernel read + execute, not writable. The only executable shape offered:
    /// there is deliberately no writable-and-executable constructor.
    pub const KERNEL_RX: Self = Self { write: false, exec: true, user: false, cow: false };
    /// User read/write, no execute.
    pub const USER_RW: Self = Self { write: true, exec: false, user: true, cow: false };
    /// User read + execute.
    pub const USER_RX: Self = Self { write: false, exec: true, user: true, cow: false };
    /// User read only — no write, no execute. The `PROT_READ` shape
    /// `akuma_mmap::Prot::RO_NO_EXEC` names.
    pub const USER_RO: Self = Self { write: false, exec: false, user: true, cow: false };

    /// The x86 page-table spelling of a region's [`akuma_mmap::Prot`].
    ///
    /// This is the crate's **x86 backend**: `akuma-mmap` records what a mapping
    /// is supposed to be, and this decides what the hardware is told. Total by
    /// construction — a `match` on [`akuma_mmap::Prot::ALL`], pinned page-by-page
    /// by [`region_prot_roundtrip_check`].
    ///
    /// Two arms are **pinned divergences from the AArch64 encoding**, not
    /// oversights, and each is here rather than in a doc because this is where
    /// someone will look:
    ///
    /// * `RO` and `RX` collapse to the same PTE. They differ only in `PXN` —
    ///   whether EL1 may fetch — and x86 has one execute bit, not two. Nothing
    ///   is lost that x86 could have expressed.
    /// * `RW` (writable **and** executable on AArch64) becomes `USER_RW`, i.e.
    ///   non-executable. `sys_mmap` on this target refuses `PROT_WRITE |
    ///   PROT_EXEC` with `EINVAL` and `Prot::from_prot` never yields `RW`, so no
    ///   region here can carry it; if one ever does, dropping execute produces a
    ///   fault at the fetch — visible, addressed, debuggable — where granting it
    ///   would silently hand ring 3 a writable code page.
    ///
    /// `NONE` maps to a **kernel-only** present page ([`Self::KERNEL_RO`]): x86
    /// has no "present and wholly inaccessible" encoding, and clearing `U/S` is
    /// what makes a ring-3 touch fault. Note `mmap(PROT_NONE)` never reaches
    /// this — `akuma_syscalls_mem::mmap::plan` calls it a lazy reservation, so
    /// its pages are never populated at all. The reachable caller is
    /// `mprotect(PROT_NONE)` over pages that are already present, which is the
    /// guard-page idiom every allocator uses.
    #[must_use]
    pub const fn from_region(prot: akuma_mmap::Prot) -> Self {
        match prot.tag() {
            // NONE — present, kernel-only, so ring 3 faults on any access.
            0 => Self::KERNEL_RO,
            // RO / RX — collapse; see the divergence note above.
            1 | 4 => Self::USER_RX,
            // RW / RW_NO_EXEC — both non-executable here; see the note above.
            2 | 3 => Self::USER_RW,
            // RO_NO_EXEC.
            5 => Self::USER_RO,
            // Unreachable over `Prot::ALL` as it stands. A `const fn` cannot
            // `panic!` its way out of a `u8` match, so the fallback is
            // **fail-closed** — a seventh variant would map to a page ring 3
            // cannot touch, which faults visibly instead of over-granting.
            // `region_prot_roundtrip_check` asserts the arity, so the variant
            // fails the boot suite before it can reach here.
            _ => Self::KERNEL_RO,
        }
    }

    /// This protection, demoted to copy-on-write: read-only in the hardware,
    /// marked so the fault handler knows the write is legitimate.
    ///
    /// Clearing `write` is not optional — see the field's own note.
    #[must_use]
    pub const fn cow(self) -> Self {
        Self { write: false, cow: true, ..self }
    }
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

/// Encode a [`PteProt`] and [`MemAttr`] into x86_64 PTE bits.
///
/// The x86 backend of what item 1 calls `encode(prot, attr)`.
const fn encode(prot: PteProt, attr: MemAttr) -> u64 {
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
    if prot.cow {
        bits |= COW;
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
pub fn map_page(va: usize, pa: u64, prot: PteProt, attr: MemAttr) -> bool {
    map_page_in(read_cr3(), va, pa, prot, attr)
}

/// Map `va` to `pa` in the address space rooted at `root`.
///
/// Taking the root as a parameter rather than always reading `CR3` is what lets
/// a process's tables be built *before* they are activated — the alternative is
/// switching to a half-built address space, which cannot be done safely from
/// code that is itself running out of memory those tables describe.
pub fn map_page_in(root: u64, va: usize, pa: u64, prot: PteProt, attr: MemAttr) -> bool {
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
pub fn prot_in(root: u64, va: usize) -> Option<PteProt> {
    let entry = match walk_in(root, va) {
        Walk::Missing => return None,
        Walk::Large(entry, _) | Walk::Leaf(entry) => entry,
    };
    Some(PteProt {
        write: entry & RW != 0,
        exec: entry & NX == 0,
        user: entry & US != 0,
        // Decoded, not dropped. `for_each_user_leaf` reports this and `fork`
        // re-maps from it, so losing the bit here would silently un-mark every
        // page on the *second* fork of a process — the child of a child would
        // share frames with no marker and take a fatal fault on its first write.
        cow: entry & COW != 0,
    })
}

/// Visit every present 4 KiB **user** leaf in the lower half of `root`, with its
/// virtual address, physical frame and permissions.
///
/// The walk `AddressSpace::free` does, one level deeper — down to the leaves
/// rather than stopping at the page tables. `fork` uses it to share a parent's
/// address space copy-on-write: each leaf is re-mapped read-only and marked in
/// **both** spaces, and the write fault breaks the sharing a page at a time.
///
/// The `PteProt` reported includes [`PteProt::cow`], which is what lets a fork of a
/// forked process preserve the marking.
pub fn for_each_user_leaf(root: u64, mut f: impl FnMut(usize, u64, PteProt)) {
    // SAFETY: every table is reached through the physmap; only the private lower
    // half is walked, so the kernel's shared tables are never touched.
    unsafe {
        for l4 in 0..256 {
            let e4 = table_mut(root).add(l4).read_volatile();
            if e4 & P == 0 || e4 & PS != 0 {
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
                    if e2 & P == 0 || e2 & PS != 0 {
                        continue;
                    }
                    let pt = e2 & ADDR_MASK;
                    for l1 in 0..ENTRIES {
                        let e1 = table_mut(pt).add(l1).read_volatile();
                        if e1 & P == 0 {
                            continue;
                        }
                        // Lower half: bit 47 is 0, so no sign extension needed.
                        let va = (l4 << 39) | (l3 << 30) | (l2 << 21) | (l1 << 12);
                        let prot = PteProt {
                            write: e1 & RW != 0,
                            exec: e1 & NX == 0,
                            user: e1 & US != 0,
                            cow: e1 & COW != 0,
                        };
                        f(va, e1 & ADDR_MASK, prot);
                    }
                }
            }
        }
    }
}

/// Visit every **present 4 KiB leaf** in `[start, end)` of `root`, with its
/// virtual address, physical frame and permissions.
///
/// [`for_each_user_leaf`] walks the whole lower half; this walks a range, and
/// the difference is not cosmetic. `munmap` and `mprotect` are given a range by
/// ring 3, and the naive shape — `for va in (start..end).step_by(4096)` with a
/// four-level walk each time — costs a full walk per page whether or not
/// anything is mapped. A lazy `PROT_NONE` reservation of a gigabyte is 262144
/// pages of which zero are present, and `rustc` makes reservations that size.
///
/// This descends once and **skips an absent subtree whole**: a missing PML4
/// entry advances the cursor by 512 GiB, a missing PDPT entry by 1 GiB, a
/// missing PD entry by 2 MiB. So an empty range costs a handful of reads
/// regardless of its length.
///
/// `f` may edit the leaf it is handed — both callers do, one clearing it and one
/// rewriting its permissions. That is safe here because neither ever frees a
/// page **table**: the tables this walk holds pointers into stay live for the
/// whole visit.
///
/// A 2 MiB page (`PS` at PD level) is skipped rather than reported. This kernel
/// never creates one for a user address space — [`map_page_in`] always builds
/// down to a PT — and reporting one as if it were a 4 KiB leaf would hand the
/// caller a frame 512 times the size it thinks.
pub fn for_each_leaf_in_range(
    root: u64,
    start: usize,
    end: usize,
    mut f: impl FnMut(usize, u64, PteProt),
) {
    /// Bytes one entry at `level` spans: 512 GiB, 1 GiB, 2 MiB, 4 KiB.
    const fn span(level: u32) -> usize {
        1usize << (12 + 9 * (level - 1))
    }

    let mut va = start;
    while va < end {
        // SAFETY: every table is reached through the physmap; `root` is a live
        // PML4 and each descent is guarded by the entry's present bit.
        let step = unsafe {
            let mut table = root;
            let mut missing = 0u32;
            for level in (2..=4).rev() {
                let entry = table_mut(table).add(index(va, level)).read_volatile();
                if entry & P == 0 || entry & PS != 0 {
                    missing = level;
                    break;
                }
                table = entry & ADDR_MASK;
            }
            if missing != 0 {
                // Nothing present under this entry: jump to the next one at the
                // level that was missing, so an empty gigabyte costs one read.
                let s = span(missing);
                s - (va & (s - 1))
            } else {
                let leaf = table_mut(table).add(index(va, 1));
                let entry = leaf.read_volatile();
                if entry & P != 0 {
                    f(
                        va,
                        entry & ADDR_MASK,
                        PteProt {
                            write: entry & RW != 0,
                            exec: entry & NX == 0,
                            user: entry & US != 0,
                            cow: entry & COW != 0,
                        },
                    );
                }
                PAGE_SIZE
            }
        };
        // `checked_add` rather than `+`: a range ending at the top of the
        // address space would otherwise wrap the cursor back to 0 and loop
        // forever, and `end` comes from ring 3.
        match va.checked_add(step) {
            Some(next) => va = next,
            None => break,
        }
    }
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
        map_page(TEST_VA, frame as u64, PteProt::KERNEL_RW, MemAttr::WriteBack),
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
    page_fault_code_check(t);
    region_prot_roundtrip_check(t);
}

/// Pin every `akuma_mmap::Prot` variant to the exact PTE bits it encodes to.
///
/// This is the x86 half of the pin `akuma-mmu` put on its own encoder
/// (`prot_roundtrips_to_todays_bits`). `akuma_mmap::Prot` is the vocabulary two
/// kernels' regions now speak; [`PteProt::from_region`] and [`encode`] are the
/// only place a region's protection becomes hardware permission on this target,
/// and a change there is invisible in every test that merely maps a page and
/// reads it back — a page mapped one bit too permissively still works.
///
/// Literal `u64`s rather than expressions built from `P`/`RW`/`US`/`NX`: an
/// expression re-derives the answer from the same constants the code under test
/// uses, so it agrees with a typo. These are the bits, written out.
fn region_prot_roundtrip_check(t: &mut Suite) {
    use akuma_mmap::Prot;

    // The named constants first — these are live today and every mapping in the
    // kernel goes through one of them.
    t.check_eq("prot: KERNEL_RO", encode(PteProt::KERNEL_RO, MemAttr::WriteBack), 0x8000_0000_0000_0001);
    t.check_eq("prot: KERNEL_RW", encode(PteProt::KERNEL_RW, MemAttr::WriteBack), 0x8000_0000_0000_0003);
    t.check_eq("prot: KERNEL_RX", encode(PteProt::KERNEL_RX, MemAttr::WriteBack), 0x0000_0000_0000_0001);
    t.check_eq("prot: USER_RW", encode(PteProt::USER_RW, MemAttr::WriteBack), 0x8000_0000_0000_0007);
    t.check_eq("prot: USER_RX", encode(PteProt::USER_RX, MemAttr::WriteBack), 0x0000_0000_0000_0005);
    t.check_eq("prot: USER_RO", encode(PteProt::USER_RO, MemAttr::WriteBack), 0x8000_0000_0000_0005);
    // The CoW demotion: writable cleared, bit 9 set. Both halves in one value,
    // because a demotion that kept `RW` would never fault and the sharing would
    // never break.
    t.check_eq(
        "prot: USER_RW demoted to CoW",
        encode(PteProt::USER_RW.cow(), MemAttr::WriteBack),
        0x8000_0000_0000_0205,
    );
    // `MemAttr::Device` is the other axis, and the LAPIC depends on it: a
    // writeback-cached MMIO register can be answered from cache and the access
    // never issued.
    t.check_eq("prot: Device adds PCD|PWT", encode(PteProt::KERNEL_RW, MemAttr::Device), 0x8000_0000_0000_001b);

    // Then the region vocabulary, variant by variant.
    t.check_eq("prot: region NONE is kernel-only", encode(PteProt::from_region(Prot::NONE), MemAttr::WriteBack), 0x8000_0000_0000_0001);
    t.check_eq("prot: region RO", encode(PteProt::from_region(Prot::RO), MemAttr::WriteBack), 0x0000_0000_0000_0005);
    t.check_eq("prot: region RW", encode(PteProt::from_region(Prot::RW), MemAttr::WriteBack), 0x8000_0000_0000_0007);
    t.check_eq("prot: region RW_NO_EXEC", encode(PteProt::from_region(Prot::RW_NO_EXEC), MemAttr::WriteBack), 0x8000_0000_0000_0007);
    t.check_eq("prot: region RX", encode(PteProt::from_region(Prot::RX), MemAttr::WriteBack), 0x0000_0000_0000_0005);
    t.check_eq("prot: region RO_NO_EXEC", encode(PteProt::from_region(Prot::RO_NO_EXEC), MemAttr::WriteBack), 0x8000_0000_0000_0005);

    // The two pinned divergences, asserted as *equalities* so that "un-fixing"
    // one shows up here rather than as a permission change nobody notices.
    t.check(
        "prot: RO and RX collapse on x86 (no PXN)",
        PteProt::from_region(Prot::RO) == PteProt::from_region(Prot::RX),
    );
    t.check(
        "prot: region RW is not executable here (W^X)",
        !PteProt::from_region(Prot::RW).exec,
    );

    // Arity. `from_region` matches on the opaque tag and cannot be exhaustive,
    // so this is what stops a seventh variant landing on the fail-closed arm
    // unnoticed: add one to `Prot::ALL` and the boot suite says so.
    t.check_eq("prot: six region variants, all pinned above", Prot::ALL.len() as u64, 6);
}

/// Pin every [`PageFaultCode`] bit position against a decoded value.
///
/// The point of the type is that these five bits stop being one-off `1 << n`
/// literals typed at a fault site, where they are right or wrong with no test
/// either way. Two of them (reserved-bit, instruction-fetch) had never been
/// named at all before this, so nothing had ever checked them.
///
/// Values are Intel SDM Vol. 3A §4.7.
fn page_fault_code_check(t: &mut Suite) {
    // Bit 0 clear is "not present", which is the demand-paging question. The
    // sense is inverted relative to every other bit here, and getting it
    // backwards services a protection fault by mapping a fresh page over it.
    t.check("pf: bit 0 clear is not-present", PageFaultCode::new(0).not_present());
    t.check("pf: bit 0 set is a protection fault", PageFaultCode::new(0b0_0001).protection());
    t.check("pf: bit 1 is write", PageFaultCode::new(0b0_0010).is_write());
    t.check("pf: bit 2 is ring 3", PageFaultCode::new(0b0_0100).is_user_mode());
    t.check("pf: bit 3 is a reserved-bit violation", PageFaultCode::new(0b0_1000).reserved_bit());
    t.check("pf: bit 4 is an instruction fetch", PageFaultCode::new(0b1_0000).instruction_fetch());

    // Each predicate reads only its own bit: a code with everything *else* set
    // must answer `false`. This is what a hand-written `code & 1 == 0` next to
    // a `PF_USER` three lines away could never be checked for.
    t.check("pf: read is not write", !PageFaultCode::new(0b1_1101).is_write());
    t.check("pf: ring 0 is not ring 3", !PageFaultCode::new(0b1_1011).is_user_mode());
    t.check("pf: a data access is not a fetch", !PageFaultCode::new(0b0_1111).instruction_fetch());

    // The composite the copy-on-write break answers to. Both rings qualify: a
    // ring-0 write is `copy_to_user` landing on a forked child's buffer, and
    // requiring ring 3 here is what made that an unexplained `EFAULT`.
    t.check(
        "pf: a ring-3 write to a present page is the CoW shape",
        PageFaultCode::new(0b0_0111).is_write_to_present_page(),
    );
    t.check(
        "pf: a ring-0 write to a present page is the CoW shape too",
        PageFaultCode::new(0b0_0011).is_write_to_present_page(),
    );
    // The near misses: an absent page is demand paging, and a read is a
    // genuinely unreadable page.
    for raw in [0b0_0110u64, 0b0_0101] {
        t.check(
            "pf: a near miss is not the CoW shape",
            !PageFaultCode::new(raw).is_write_to_present_page(),
        );
    }
}

/// Check that the encoder cannot express write+execute, and that NX is set.
///
/// A pure check on [`encode`], not on the hardware: the hardware half needs a
/// `#PF` handler to observe. What it does prove is the property proposal item 1
/// exists to guarantee — that "read-only" and "executable" are distinct values,
/// which on the AArch64 side they currently are not
/// (`user_flags::RO == user_flags::EXEC`).
fn nx_encoding_check(t: &mut Suite) {
    let rw = encode(PteProt::KERNEL_RW, MemAttr::WriteBack);
    let rx = encode(PteProt::KERNEL_RX, MemAttr::WriteBack);
    let ro = encode(PteProt::KERNEL_RO, MemAttr::WriteBack);
    let urw = encode(PteProt::USER_RW, MemAttr::WriteBack);
    let urx = encode(PteProt::USER_RX, MemAttr::WriteBack);
    let dev = encode(PteProt::KERNEL_RW, MemAttr::Device);

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
    pub fn map(&self, va: usize, pa: u64, prot: PteProt, attr: MemAttr) -> bool {
        map_page_in(self.root, va, pa, prot, attr)
    }

    /// Resolve a virtual address in this space, without activating it.
    #[must_use]
    pub fn translate(&self, va: usize) -> Option<u64> {
        translate_in(self.root, va)
    }

    /// The permissions `va` carries in this space, or `None` if unmapped.
    #[must_use]
    pub fn prot(&self, va: usize) -> Option<PteProt> {
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
