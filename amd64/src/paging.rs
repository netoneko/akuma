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

/// The page-permission vocabulary and its encoder — **`akuma-mmu`'s**, since
/// amd64 step 5a.
///
/// This file used to define its own `PteProt`, its own `MemAttr` and its own
/// `encode`, structurally identical to the crate's and held in step with them by
/// a boot self-test that compared the two arm by arm
/// (`x86_prot_matches_amd64_encoding`). That pin existed because there were two
/// x86 page-table walkers on this target; step 5a left one for user address
/// spaces (`akuma_mmu::UserAddressSpace`) and one for the kernel's own
/// mappings (this file), and a shared vocabulary between them is what stops the
/// second pair drifting the way the first would have.
///
/// Re-exported rather than merely used, so the `crate::paging::{MemAttr,
/// PteProt}` call sites in `blk.rs`, `lapic.rs`, `pci.rs`, `idt.rs` and
/// `uaccess.rs` are unchanged: those are kernel mappings, and this is still the
/// module that performs them.
///
/// The copy-on-write marker is **not** a field of [`PteProt`] and never was part
/// of what a kernel mapping says — it is [`akuma_mmu::encode_pte`]'s third
/// argument, and [`COW`] below is still this file's statement of which bit it
/// is.
pub use akuma_mmu::{MemAttr, PteProt};

/// Encode a [`PteProt`] and [`MemAttr`] into x86_64 PTE bits, for a **kernel**
/// mapping — which is to say with the copy-on-write marker clear.
///
/// A thin forward to [`akuma_mmu::encode_pte`], kept as a local name because
/// every walker below reads better for it, and because "kernel mappings never
/// carry the marker" is worth stating once here rather than as a `false`
/// repeated at each call site.
const fn encode(prot: PteProt, attr: MemAttr) -> u64 {
    akuma_mmu::encode_pte(prot, attr, false)
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

/// Switch the active address space, publishing the transition to
/// `akuma-mmu`'s per-core live-L0 registry.
///
/// # The publish is not bookkeeping
///
/// `akuma_mmu::UserAddressSpace` frees its page tables in `Drop`, and that is
/// only safe because `any_core_on_l0` first asks whether any core is still
/// running them — parking the frames if so. That registry is fed by
/// `publish_l0_begin`/`publish_l0_end`, whose only caller was the
/// `#[cfg(target_arch = "aarch64")]` `msr ttbr0_el1` block in `akuma-threading`.
/// This target writes `CR3` here, so until now the gate answered "no core holds
/// this" for **every** table — and step 5a is where address spaces start
/// dropping (`proposals/AMD64_STEP5_PROCESS_TABLE.md`). Publishing before there
/// is a consumer is the right order: the registry has to be correct *for the
/// whole run* before the first `Drop`, not from the moment one is added.
///
/// `smp::cpu_index()`, not `bkl::current_core_id()` — that one is a literal `0`
/// on every build without `kernel_smp_shared`, which this target is while
/// running four cores. See `publish_l0_begin`'s own note.
///
/// IRQs are masked across the pair, as the registry requires: it publishes into
/// *this* core's slot, and a thread that migrated between the two calls would
/// leave a stale ACTIVE entry naming a table this core no longer runs. Save and
/// restore rather than `cli`/`sti`, because most callers already have them off.
///
/// # Safety
/// `root` must be a PML4 that maps every page this kernel is currently
/// executing from and every page it will touch before switching back — the
/// kernel image, its stacks, the heap, the PMM pool and the LAPIC window. An
/// address space missing any of those faults on the instruction after `mov cr3`,
/// with no way to report it.
pub unsafe fn activate(root: u64) {
    // Published on every call, not only when the root changes — which is where
    // the AArch64 caller puts its `if new_ttbr0 != current_ttbr0`. Deliberate,
    // and matching this function's existing "write CR3 unconditionally" rule:
    // the `mov cr3` here is a full TLB flush costing hundreds of cycles, next to
    // which three uncontended `xchg` is a small constant. A publish-on-change
    // refinement is available (compare against `ACTIVE_L0` rather than re-reading
    // CR3) if a real scheduler probe ever shows it matters — the boot suite
    // cannot answer that question in either direction, for the reasons
    // `boot::self_tests`' guest-clock stamp records.
    let flags = irq_save_mask();
    let core = akuma_mmu::publish_l0_begin(root, crate::smp::cpu_index());
    // SAFETY: caller's obligation, stated above. Writing CR3 also flushes the
    // non-global TLB, which is what makes the switch take effect.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
    akuma_mmu::publish_l0_end(core);
    // SAFETY: restores exactly the interrupt-enable state observed above.
    unsafe { irq_restore(flags) };
}


/// [`activate`] **without** the registry publish, for the one caller that runs
/// before this core can say which core it is.
///
/// `smp::ap_entry64`'s very first act — before `install_percpu`, before even
/// `idt::load()` — is to get off the boot tables. `cpu_index()` reads `gs:[0]`
/// and `IA32_GS_BASE` is still 0 there, so publishing would dereference address
/// 0 on a core with no IDT: a triple fault whose symptom is the *BSP* hanging in
/// `start_secondaries` waiting for a core that is already dead. Measured
/// 2026-09-07 — the suite stopped after `preempt: teardown leaks nothing` and
/// produced no tally at all. It is the trap `smp`'s own module header states for
/// `percpu_installed`, arrived at from the other direction.
///
/// **A separate entry point rather than a `percpu_installed()` check inside
/// [`activate`]**, because that check is an `rdmsr` and `activate` is on the
/// context-switch path. One caller knows it is early; every other caller should
/// not pay a serialising MSR read for it.
///
/// Skipping the publish here is not a hole: the only transition before per-CPU
/// state exists is boot tables → kernel root, and neither is ever freed, so the
/// gate has nothing to protect. Every switch that can name a *user* address
/// space happens long after.
///
/// # Safety
/// As [`activate`], plus: the caller must be on a path where per-CPU state does
/// not yet exist. Anywhere else, use [`activate`] so the free gate sees the
/// switch.
pub unsafe fn activate_unpublished(root: u64) {
    // SAFETY: caller's obligation, stated above.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// `RFLAGS.IF` mask — interrupts enabled.
const RFLAGS_IF: u64 = 1 << 9;

/// Read `RFLAGS` and mask interrupts, returning what to hand [`irq_restore`].
///
/// Not `akuma_cpu::daif::mask_irq()`: that is a **silent no-op on x86_64** (its
/// `asm!` is `#[cfg(target_arch = "aarch64")]` and every other arm falls through
/// to an empty body), the same trap `amd64/src/exec_runtime.rs` records against
/// `akuma_primitives::irq::IrqGuard` on this target.
#[inline]
fn irq_save_mask() -> u64 {
    let flags: u64;
    // SAFETY: pushes and pops one word on the current stack, then masks — the
    // conservative direction, with no memory effect.
    unsafe {
        core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem));
    }
    flags
}

/// Re-enable interrupts only if [`irq_save_mask`] found them enabled.
///
/// # Safety
/// `flags` must be a value returned by [`irq_save_mask`] on this core, with no
/// intervening change of interrupt policy the caller meant to keep.
#[inline]
unsafe fn irq_restore(flags: u64) {
    if flags & RFLAGS_IF != 0 {
        // SAFETY: the caller observed interrupts enabled before masking them.
        unsafe {
            core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        }
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
/// # What it pins changed with amd64 step 5a
///
/// Both halves used to be local: this file's `PteProt::from_region` and this
/// file's `encode`, with `akuma-mmu`'s `x86_prot_matches_amd64_encoding` host
/// test asserting the crate's copies produced the same six values. Two
/// implementations, one agreement. There is one implementation now — this
/// function calls straight into it — so what runs here is the same code the
/// user address space maps through, checked against **literals** on real
/// hardware rather than against a second copy.
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
    // The CoW demotion: writable cleared, bit 9 set. Both halves in one value,
    // because a demotion that kept `RW` would never fault and the sharing would
    // never break. The marker is `encode_pte`'s third argument now rather than a
    // `PteProt::cow()` constructor, and the *demotion* — dropping `write` — is
    // the caller's, which is what makes it visible here: `USER_RO` is what
    // `USER_RW` demoted is, spelled out.
    t.check_eq(
        "prot: USER_RW demoted to CoW",
        akuma_mmu::encode_pte(PteProt::USER_RO, MemAttr::WriteBack, true),
        0x8000_0000_0000_0205,
    );
    t.check_eq(
        "prot: the CoW marker is bit 9 and nothing else",
        akuma_mmu::encode_pte(PteProt::USER_RO, MemAttr::WriteBack, true)
            ^ encode(PteProt::USER_RO, MemAttr::WriteBack),
        COW,
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
