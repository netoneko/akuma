//! The shared [`akuma_mmu::UserAddressSpace`], exercised on real hardware.
//!
//! `amd64/src/paging.rs` drives this kernel's page tables today; the type this
//! module tests is the *other* x86 walker — the one inside `akuma-mmu` that
//! `akuma-exec`, `akuma-elf` and `akuma-syscalls-glue` reach through, and that
//! item **C1** will fold this kernel onto. Until that fold it has no runtime
//! caller here at all, which is exactly why it needs a test: B3 widened it from
//! 6 methods to 30 (`docs/archive/AKUMA_SELF_HOSTING_AMD64.md`), and a
//! compile-only proof of thirty methods is a proof that they *type-check*.
//!
//! # What this cannot cover, and why
//!
//! The suite never writes `CR3`. Installing a second address space mid-boot to
//! reach the two `map_user_page_tracked*` methods would put the running kernel's
//! stack and code behind a page table this test just built — a mistake there is
//! a triple fault with no console, on a box whose only recovery is a walk to
//! another room (`docs/runbooks/amd64-bare-metal-loop.md`). Those two methods
//! are covered by their refusal path instead: they check `CR3` against their own
//! root and decline, which is the branch that runs here.
//!
//! Everything else is reachable without a switch, because every other method
//! walks `self.root` explicitly rather than the installed tables — that
//! property is itself worth pinning, and `translate`-after-`map` is what pins
//! it.

use akuma_mmu::{user_flags, UserAddressSpace};
use akuma_selftest::Suite;

use crate::phys::phys_ptr;

/// A VA in the lower half, clear of anything the kernel maps. The address space
/// under test is private, so this only has to avoid colliding with itself.
const TEST_VA: usize = 0x4000_0000;
const PATTERN: u64 = 0x0bad_c0de_dead_beef;

/// x86 leaf PTE bits, spelled out here rather than imported: `akuma-mmu` keeps
/// them private, and a test that reads them through the crate's own constants
/// would pass even if those drifted — the same argument
/// `prot_roundtrips_to_todays_bits` makes on the host.
const PTE_P: u64 = 1 << 0;
const PTE_RW: u64 = 1 << 1;
const PTE_US: u64 = 1 << 2;
/// The copy-on-write marker `demote_range_to_ro` sets. Must equal
/// `paging::COW` — that they are the same bit is the point of checking it.
const PTE_COW: u64 = 1 << 9;
const PTE_NX: u64 = 1 << 63;
const PTE_ADDR: u64 = 0x000f_ffff_ffff_f000;

pub fn smoke_test(t: &mut Suite) {
    let Some(mut uas) = UserAddressSpace::new() else {
        t.check("uas: new", false);
        return;
    };
    t.check("uas: new", true);

    // ── identity ───────────────────────────────────────────────────────────
    // `ttbr0` is the packed `(asid << 48) | root` word `ProcAddressSpace`
    // mirrors lock-free on both architectures. With no ASID here it must be the
    // bare root, or that mirror hands out a corrupt `l0_phys` on this target.
    t.check_eq("uas: ttbr0 is the root", uas.ttbr0(), uas.l0_phys() as u64);
    t.check_eq("uas: asid is 0 (no ASID on x86)", u64::from(uas.asid()), 0);
    t.check("uas: a fresh space is not shared", !uas.is_shared());

    // ── the VA starts empty ────────────────────────────────────────────────
    t.check("uas: test VA starts unmapped", !uas.is_mapped(TEST_VA));
    t.check("uas: translate is None before mapping", uas.translate(TEST_VA).is_none());
    t.check("uas: is_range_mapped is false before mapping", !uas.is_range_mapped(TEST_VA, 4096));
    t.check("uas: empty range is trivially mapped", uas.is_range_mapped(TEST_VA, 0));

    // ── map, and prove the ledger saw everything ───────────────────────────
    let Ok(frame) = uas.alloc_and_map(TEST_VA, user_flags::RW_NO_EXEC) else {
        t.check("uas: alloc_and_map", false);
        return;
    };
    t.check("uas: alloc_and_map", true);
    t.check_eq("uas: translate matches the frame", uas.translate(TEST_VA).unwrap_or(0) as u64, frame.addr as u64);
    t.check("uas: is_mapped after mapping", uas.is_mapped(TEST_VA));
    t.check("uas: is_range_mapped after mapping", uas.is_range_mapped(TEST_VA, 4096));
    t.check("uas: the ledger tracks the data frame", uas.tracks_user_frame(frame.addr));
    t.check_eq("uas: one user frame", uas.user_frame_count() as u64, 1);
    // A first mapping into an empty PML4 allocates a PDPT, a PD and a PT. If
    // the walk did not hand those to the ledger they would be unreachable on
    // teardown — the leak `x86_map_page_in_tracked` exists to prevent.
    t.check_eq("uas: three page-table frames tracked", uas.page_table_frame_count() as u64, 3);

    // ── the mapping points where the walk says ─────────────────────────────
    t.check("uas: write_page_bytes", uas.write_page_bytes(TEST_VA, 8, &PATTERN.to_le_bytes()));
    // SAFETY: `frame` came from the PMM, which is inside the physmap.
    let via_phys = unsafe { phys_ptr::<u64>(frame.addr as u64).add(1).read_volatile() };
    t.check_eq("uas: the write is visible at the physical alias", via_phys, PATTERN);
    t.check_eq(
        "uas: phys_addr_for_page_va matches",
        uas.phys_addr_for_page_va(TEST_VA).unwrap_or(0) as u64,
        frame.addr as u64,
    );

    // ── the permission bits are the ones asked for ─────────────────────────
    let pte = uas.read_l3_page_entry(TEST_VA).unwrap_or(0);
    t.check("uas: RW_NO_EXEC is present", pte & PTE_P != 0);
    t.check("uas: RW_NO_EXEC is user-accessible", pte & PTE_US != 0);
    t.check("uas: RW_NO_EXEC is writable", pte & PTE_RW != 0);
    t.check("uas: RW_NO_EXEC is no-execute", pte & PTE_NX != 0);

    uas.update_page_flags(TEST_VA, user_flags::RX);
    let pte = uas.read_l3_page_entry(TEST_VA).unwrap_or(0);
    t.check("uas: RX dropped write", pte & PTE_RW == 0);
    t.check("uas: RX is executable", pte & PTE_NX == 0);
    t.check_eq("uas: update_page_flags kept the frame", pte & PTE_ADDR, frame.addr as u64);

    // ── the CoW marker ─────────────────────────────────────────────────────
    // The one place the two architectures genuinely cannot share a body:
    // AArch64 has no marker bit and is handed `refs > 0`, this target sets
    // PTE bit 9. Demoting without it makes every CoW fault here look like a
    // real protection violation.
    uas.update_page_flags(TEST_VA, user_flags::RW_NO_EXEC);
    t.check_eq("uas: demote_range_to_ro demoted one page", uas.demote_range_to_ro(TEST_VA, 1) as u64, 1);
    let pte = uas.read_l3_page_entry(TEST_VA).unwrap_or(0);
    t.check("uas: demoted page is read-only", pte & PTE_RW == 0);
    t.check("uas: demoted page carries the CoW marker", pte & PTE_COW != 0);
    t.check_eq("uas: demote kept the frame", pte & PTE_ADDR, frame.addr as u64);
    // Nothing to demote a second time — the range walk must not double-count.
    t.check_eq("uas: demoting again finds nothing", uas.demote_range_to_ro(TEST_VA, 1) as u64, 0);

    // ── the refusal path of the two CR3-guarded methods ────────────────────
    // This address space is not installed, so both must decline rather than
    // edit the tables the kernel is running on. In a `debug_assertions` build
    // they would trip a `debug_assert!` first; the boot suite is a release
    // build, which is where this branch is reachable.
    if cfg!(not(debug_assertions)) {
        t.check(
            "uas: map_user_page_tracked refuses an uninstalled space",
            !uas.map_user_page_tracked(TEST_VA, frame, user_flags::RW_NO_EXEC),
        );
    } else {
        t.note("uas: CR3-guard refusal not checked (debug build)", 0);
    }

    // ── eviction ───────────────────────────────────────────────────────────
    // The page is read-only now, so it is a candidate. The frame comes back
    // because this address space held its only reference.
    let evicted = uas.try_evict_ro_page(TEST_VA);
    t.check("uas: try_evict_ro_page returned the frame", evicted.map(|f| f.addr) == Some(frame.addr));
    t.check("uas: the evicted VA is unmapped", !uas.is_mapped(TEST_VA));
    t.check_eq("uas: the ledger dropped it", uas.user_frame_count() as u64, 0);

    // ── zero_mapped_page, then unmap-and-free ──────────────────────────────
    let Ok(frame2) = uas.alloc_and_map(TEST_VA, user_flags::RW_NO_EXEC) else {
        t.check("uas: remap after eviction", false);
        return;
    };
    t.check("uas: remap after eviction", true);
    // The walk should have reused the PDPT/PD/PT it already had.
    t.check_eq("uas: remap allocated no new tables", uas.page_table_frame_count() as u64, 3);
    t.check("uas: write before zeroing", uas.write_page_bytes(TEST_VA, 8, &PATTERN.to_le_bytes()));
    t.check("uas: zero_mapped_page", uas.zero_mapped_page(TEST_VA));
    // SAFETY: as above — a PMM frame inside the physmap.
    let after_zero = unsafe { phys_ptr::<u64>(frame2.addr as u64).add(1).read_volatile() };
    t.check_eq("uas: the page really is zero", after_zero, 0);

    // A read-only page is *not* a `PROT_NONE` one: `user_flags::NONE` must
    // reach ring 3 as a present-but-kernel-only page, which is how x86 spells
    // an inaccessible mapping.
    uas.update_page_flags(TEST_VA, user_flags::NONE);
    let pte = uas.read_l3_page_entry(TEST_VA).unwrap_or(0);
    t.check("uas: PROT_NONE stays present", pte & PTE_P != 0);
    t.check("uas: PROT_NONE is not user-accessible", pte & PTE_US == 0);

    t.check(
        "uas: unmap_and_free_page returned the frame",
        uas.unmap_and_free_page(TEST_VA).map(|f| f.addr) == Some(frame2.addr),
    );
    t.check("uas: unmapped after free", !uas.is_mapped(TEST_VA));

    // No-op on this architecture, and called for the same reason the shared
    // callers call it: to prove it exists and returns.
    uas.invalidate_icache_for_page_va(TEST_VA);

    // ── a shared view owns nothing ─────────────────────────────────────────
    let root = uas.l0_phys();
    let Some(shared) = UserAddressSpace::new_shared(root) else {
        t.check("uas: new_shared", false);
        return;
    };
    t.check("uas: new_shared", true);
    t.check("uas: a shared view says so", shared.is_shared());
    t.check_eq("uas: a shared view has the same root", shared.l0_phys() as u64, root as u64);
    t.check_eq("uas: a shared view owns no frames", shared.user_frame_count() as u64, 0);
    // It reads the owner's tables, though — that is what makes it a view.
    t.check("uas: a shared view sees the owner's unmapped VA", !shared.is_mapped(TEST_VA));

    // Hand the data frames back. The three page-table frames stay: this target
    // has no `Drop` for an address space (see `akuma-mmu`'s note on why), so a
    // boot-time leak of 12 KiB is the cost of running this test at all, and it
    // is bounded at one address space per boot.
    // Boot: no thread owns these yet, so tid 0 — the same value the rest of
    // the pre-`init` allocation paths on this target pass.
    akuma_pmm::free_page(frame.addr, 0);
    akuma_pmm::free_page(frame2.addr, 0);
    t.note("uas: page-table frames deliberately leaked (no Drop on this target)", 3);
}
