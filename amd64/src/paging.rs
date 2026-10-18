//! x86_64 4-level page tables **for the kernel's own mappings**: map, unmap,
//! translate.
//!
//! Stage B of the amd64 bring-up. `boot.s` leaves the machine on a fixed
//! identity map of the first 1 GiB built from 2 MiB pages; this is the first
//! code that can *change* a mapping, which is the prerequisite for anything that
//! demand-pages, protects a region, or addresses memory beyond that window.
//!
//! # This file used to serve two roles; step 5a took one away
//!
//! Until then it owned **both** the kernel's own tables and every process's:
//! `AddressSpace::{new,map,translate,prot,free}`, `for_each_user_leaf`,
//! `for_each_leaf_in_range` and `prot_in` were a second, complete x86 walker
//! beside `akuma_mmu::UserAddressSpace` — the one `akuma-exec`, `akuma-elf` and
//! `akuma-syscalls-glue` reach through. `mm.rs`, `idt.rs`, `usermode.rs`,
//! `loader.rs` and `fd.rs` all went through this one; they go through the crate
//! now, and the user half of this file is gone
//! (`docs/archive/AKUMA_AMD64_STEP5A_ONE_WALKER.md`).
//!
//! What remains is role 1 and only role 1: the kernel image, the physmap, the
//! device window and the MMIO pages `pci.rs`/`lapic.rs`/`blk.rs` map. `akuma-mmu`
//! does not do kernel mappings on **either** architecture, so this is not a
//! duplicate of anything.
//!
//! [`activate`] is the one function here that both roles need, which is why it
//! publishes to `akuma-mmu`'s per-core live-L0 registry — see its own header.
//!
//! # Two `Prot`s, and why they stay two
//!
//! [`PteProt`] is the **page-table** vocabulary: what the hardware is told.
//! `akuma_mmap::Prot` is the **region** vocabulary: what a mapping is *supposed*
//! to be. They were both called `Prot` until 2026-09-07 and the rename is what
//! makes the difference visible at every call site.
//!
//! Both the type and [`akuma_mmu::encode_pte`] behind [`encode`] are the shared
//! crate's since step 5a; this file re-exports them (see [`PteProt`]'s note).
//! The bits stay named here — with the walker that writes them — because the two
//! architectures' encodings share **no field**:
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
//! bit 9 ([`COW`]), meaningless to a region, and `akuma-cow` already takes
//! decoded booleans precisely so it never sees a PTE. It is not a field of
//! [`PteProt`] either — it is `encode_pte`'s third argument, and the mappings
//! *this* file makes are the kernel's own, which never carry it.
//!
//! # Why the tables can be dereferenced directly
//!
//! Every page-table frame is allocated from `akuma-pmm`, whose pool is inside the
//! region `boot.s` identity-maps, so a physical address *is* a valid pointer.
//! [`table_mut`] asserts that rather than assuming it: the moment the PMM is
//! given memory above 1 GiB, this stops being true, and the failure mode is a
//! page fault with no IDT installed — a triple-fault and a guest that vanishes
//! with no output.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use akuma_pagetable::PhysMap;
use akuma_pagetable::amd64 as walk;

use crate::phys::phys_to_virt;

/// The error code the CPU pushes for vector 14, asked as questions — the crate's
/// (`akuma-pte-amd64`), re-exported so the `crate::paging::PageFaultCode` call
/// sites in `idt.rs` are unchanged.
pub use akuma_pte_amd64::PageFaultCode;

/// The page-permission vocabulary and its encoder — `akuma-pte-amd64`'s.
///
/// Re-exported rather than merely used, so the `crate::paging::{MemAttr,
/// PteProt}` call sites in `blk.rs`, `lapic.rs`, `pci.rs`, `idt.rs` and
/// `uaccess.rs` are unchanged: those are kernel mappings, and this is still the
/// module that performs them. The copy-on-write marker is **not** a field of
/// [`PteProt`] — it is `encode_pte`'s third argument, and the mappings made here
/// are the kernel's own, which never carry it.
pub use akuma_pte_amd64::{MemAttr, PteProt};

const PAGE_SIZE: usize = akuma_pte_amd64::PAGE_SIZE;

/// The physmap, as `akuma-pagetable` asks for it.
///
/// `phys_to_virt` asserts the frame is inside [`crate::phys::PHYSMAP_LIMIT`]: the
/// moment the PMM is given memory above that, a table frame stops being
/// reachable, and the failure mode of *not* asserting is a page fault with no IDT
/// installed — a triple-fault and a guest that vanishes with no output.
pub struct Physmap;

impl PhysMap for Physmap {
    fn table_addr(pa: u64) -> usize {
        phys_to_virt(pa) as usize
    }
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
    let flags = akuma_cpu::daif::save_and_mask_irq();
    let core = akuma_mmu::publish_l0_begin(root, crate::smp::cpu_index());
    // SAFETY: caller's obligation, stated above. Writing CR3 also flushes the
    // non-global TLB, which is what makes the switch take effect.
    unsafe { akuma_pagetable::amd64::load_cr3(root) };
    akuma_mmu::publish_l0_end(core);
    // Restores exactly the interrupt-enable state observed above (`popfq`).
    akuma_cpu::daif::restore(flags);
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
    unsafe { akuma_pagetable::amd64::load_cr3(root) };
}

// `irq_save_mask`/`irq_restore` were here until 2026-09-08, when
// `akuma_cpu::daif` grew real x86 arms (`save_and_mask_irq` = `pushfq`/`cli`,
// `restore` = `popfq`) and this file's private copy became the crate's. The
// comment that used to justify the copy — that `daif` is a silent no-op on
// this target — retired with it.

/// The active top-level table, from `CR3`.
fn read_cr3() -> u64 {
    akuma_cpu::ctlreg::cr3() & akuma_pte_amd64::ADDR_MASK
}

/// Invalidate one page's TLB entry.
///
/// The amd64 counterpart of `tlbi vaae1`, and a good illustration of proposal
/// item 3: `invlpg` is *core-local*, where AArch64's `tlbi ...is` broadcasts to
/// the inner-shareable domain. On amd64 a multi-core kernel must send an IPI to
/// every other core that could hold the translation — there is no broadcast
/// form. This file's own walker edits only the kernel's boot-time tables, so
/// core-local is complete here; user address spaces go through
/// `akuma_mmu::flush_tlb_*`, which owns the shootdown IPI.
fn invlpg(va: usize) {
    akuma_cpu::tlb::invlpg(va);
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
///
/// Intermediate tables come straight from the PMM and are not tracked: these are
/// the kernel's own mappings, which are never torn down.
pub fn map_page_in(root: u64, va: usize, pa: u64, prot: PteProt, attr: MemAttr) -> bool {
    assert_eq!(va % PAGE_SIZE, 0, "va must be page aligned");
    assert_eq!(pa % PAGE_SIZE as u64, 0, "pa must be page aligned");

    let leaf = akuma_pte_amd64::leaf(pa, prot, attr, false);
    let mapped = walk::map_leaf::<Physmap>(root, va, leaf, prot.user, || {
        akuma_pmm::alloc_page().map(|f| f as u64)
    });
    if mapped {
        invlpg(va);
    }
    mapped
}

/// Remove the mapping for `va` in the active address space.
pub fn unmap_page(va: usize) -> Option<u64> {
    unmap_page_in(read_cr3(), va)
}

/// Remove the mapping for `va` in the address space rooted at `root`.
pub fn unmap_page_in(root: u64, va: usize) -> Option<u64> {
    let frame = walk::unmap::<Physmap>(root, va)?;
    invlpg(va);
    Some(frame)
}

/// Resolve `va` in the active address space.
///
/// Walks rather than trusting a shadow structure, so it reports what the
/// *hardware* would do — which is the only useful answer when checking whether a
/// mapping took effect.
pub fn translate(va: usize) -> Option<u64> {
    translate_in(read_cr3(), va)
}

/// Resolve `va` in the address space rooted at `root`.
pub fn translate_in(root: u64, va: usize) -> Option<u64> {
    walk::translate::<Physmap>(root, va)
}

#[cfg(not(feature = "no-tests"))]
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
    // SAFETY: PMM frames are inside the physmap.
    let via_phys = unsafe { crate::phys::phys_ptr::<u64>(frame as u64).read_volatile() };
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

#[cfg(not(feature = "no-tests"))]
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
    use akuma_pte_amd64::encode;

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
        akuma_pte_amd64::encode_pte(PteProt::USER_RO, MemAttr::WriteBack, true),
        0x8000_0000_0000_0205,
    );
    t.check_eq(
        "prot: the CoW marker is bit 9 and nothing else",
        akuma_pte_amd64::encode_pte(PteProt::USER_RO, MemAttr::WriteBack, true)
            ^ encode(PteProt::USER_RO, MemAttr::WriteBack),
        akuma_pte_amd64::COW,
    );
    // `MemAttr::Device` is the other axis, and the LAPIC depends on it: a
    // writeback-cached MMIO register can be answered from cache and the access
    // never issued.
    t.check_eq("prot: Device adds PCD|PWT", encode(PteProt::KERNEL_RW, MemAttr::Device), 0x8000_0000_0000_001b);

    // Then the region vocabulary, variant by variant.
    t.check_eq("prot: region NONE is kernel-only", encode(PteProt::from_region(Prot::NONE), MemAttr::WriteBack), 0x8000_0000_0000_0001);
    t.check_eq("prot: region RO", encode(PteProt::from_region(Prot::RO), MemAttr::WriteBack), 0x0000_0000_0000_0005);
    // RWX since 2026-10-08 (NX clear): V8's code range. Was 0x8000_…_0007.
    t.check_eq("prot: region RW", encode(PteProt::from_region(Prot::RW), MemAttr::WriteBack), 0x0000_0000_0000_0007);
    t.check_eq("prot: region RW_NO_EXEC", encode(PteProt::from_region(Prot::RW_NO_EXEC), MemAttr::WriteBack), 0x8000_0000_0000_0007);
    t.check_eq("prot: region RX", encode(PteProt::from_region(Prot::RX), MemAttr::WriteBack), 0x0000_0000_0000_0005);
    t.check_eq("prot: region RO_NO_EXEC", encode(PteProt::from_region(Prot::RO_NO_EXEC), MemAttr::WriteBack), 0x8000_0000_0000_0005);

    // The pinned divergence, asserted as an *equality* so that "un-fixing"
    // it shows up here rather than as a permission change nobody notices.
    t.check(
        "prot: RO and RX collapse on x86 (no PXN)",
        PteProt::from_region(Prot::RO) == PteProt::from_region(Prot::RX),
    );
    // The former second divergence, now an agreement with AArch64: `RW` is
    // writable and executable on both (2026-10-08, `jitprobe.c`).
    t.check(
        "prot: region RW is executable (RWX granted, as on AArch64 and Linux)",
        PteProt::from_region(Prot::RW).exec && PteProt::from_region(Prot::RW).write,
    );

    // Arity. `from_region` matches on the opaque tag and cannot be exhaustive,
    // so this is what stops a seventh variant landing on the fail-closed arm
    // unnoticed: add one to `Prot::ALL` and the boot suite says so.
    t.check_eq("prot: six region variants, all pinned above", Prot::ALL.len() as u64, 6);
}

#[cfg(not(feature = "no-tests"))]
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

#[cfg(not(feature = "no-tests"))]
/// Check that the encoder cannot express write+execute, and that NX is set.
///
/// A pure check on [`encode`], not on the hardware: the hardware half needs a
/// `#PF` handler to observe. What it does prove is the property proposal item 1
/// exists to guarantee — that "read-only" and "executable" are distinct values,
/// which on the AArch64 side they currently are not
/// (`user_flags::RO == user_flags::EXEC`).
fn nx_encoding_check(t: &mut Suite) {
    use akuma_pte_amd64::{NX, PCD, PWT, RW, US, encode};
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
    let root = read_cr3();
    walk::clear_root_slot::<Physmap>(root, 0);
    // SAFETY: the caller must already be running from the kernel window with a
    // physmap stack — which `boot.s`'s `high_entry` arranges before it calls
    // `kmain`. Reloading CR3 flushes the now-stale identity translations.
    unsafe { activate(root) };
}
