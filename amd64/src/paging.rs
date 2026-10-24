//! x86_64 4-level page tables **for the kernel's own mappings**.
//!
//! The walker half — map/unmap/translate, the physmap `PhysMap` impl, the
//! device-window mapper — lives in `akuma-boot-amd64::paging` since the
//! Taoxichuan stage-1 extraction (2026-10-24), and this module re-exports it so
//! every `crate::paging::{MemAttr, PteProt, map_page, …}` call site is
//! unchanged.
//!
//! What stays here is the half that is *this kernel's own*: [`activate`]
//! (every CR3 switch, published to `akuma-mmu`'s per-core live-L0 registry so a
//! dropped `UserAddressSpace` cannot free tables a core still runs),
//! [`activate_unpublished`] (the AP boot path), [`drop_identity_map`] (the
//! trampoline's map retired, with the registry-publishing reload), and the boot
//! self-test suite.

#[cfg(not(feature = "no-tests"))]
use akuma_selftest::Suite;

use crate::smp;

// The walker half: constants, translations, map/unmap — one implementation,
// shared with the composed kernels that pull `akuma-boot-amd64`.
pub use akuma_boot_amd64::paging::{
    MemAttr, PageFaultCode, Physmap, PteProt, active_root, map_device_window, map_page,
    map_page_in, translate, translate_in, unmap_page, unmap_page_in,
};

/// Switch the active address space, publishing the transition to
/// `akuma-mmu`'s per-core live-L0 registry.
///
/// # Safety
/// `root` must be a PML4 that maps every page this kernel is currently
/// executing from and every page it will touch before switching back — the
/// kernel image, its stacks, the heap, the PMM pool and the LAPIC window. An
/// address space missing any of those faults on the instruction after
/// `mov cr3`, with no way to report it.
pub unsafe fn activate(root: u64) {
    // Published on every call, not only when the root changes — which is where
    // the AArch64 caller puts its `if new_ttbr0 != current_ttbr0`. Deliberate,
    // and matching this function's existing "write CR3 unconditionally" rule:
    // the `mov cr3` here is a full TLB flush costing hundreds of cycles, next to
    // which three uncontended `xchg` is a small constant.
    let flags = akuma_cpu::daif::save_and_mask_irq();
    let core = akuma_mmu::publish_l0_begin(root, smp::cpu_index());
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
/// 0 on a core with no IDT.
///
/// # Safety
/// As [`activate`], plus: the caller must be on a path where per-CPU state does
/// not yet exist.
pub unsafe fn activate_unpublished(root: u64) {
    // SAFETY: caller's obligation, stated above.
    unsafe { akuma_pagetable::amd64::load_cr3(root) };
}

/// Retire the trampoline's identity map: clear PML4 slot 0 and reload CR3
/// through [`activate`], so the registry sees the flush and the lower half is
/// free for userspace.
pub fn drop_identity_map() {
    akuma_boot_amd64::paging::clear_identity_slot();
    // SAFETY: the caller must already be running from the kernel window with a
    // physmap stack — which `boot.s`'s `high_entry` arranges before it calls
    // `kmain`. Reloading CR3 flushes the now-stale identity translations.
    unsafe { activate(active_root()) };
}

///
/// Chosen VA is 1 GiB — the first address `boot.s` does *not* map, so the whole
/// path (allocate PDPT entry, PD, PT, leaf) is exercised and a false pass from
/// accidentally hitting the identity map is impossible.
#[cfg(not(feature = "no-tests"))]
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
