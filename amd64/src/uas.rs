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

    // ── the range walks ────────────────────────────────────────────────────
    range_walk_test(t, &mut uas);

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

/// The three range walks C1 step 5a needs, on a private address space.
///
/// These are the one capability `akuma-mmu` did not have and `amd64/src/mm.rs`
/// cannot be ported without: `munmap`, `mprotect`, `mremap`, `madvise`, `fork`'s
/// CoW demote and `/proc/self/maps` are all written against a *table* walk here,
/// where the AArch64 kernel iterates VAs from its region list
/// (`proposals/AMD64_STEP5_PROCESS_TABLE.md` § 5a).
///
/// The walk cannot be host-tested: it dereferences page tables through the
/// physmap and is `#[cfg(target_arch = "x86_64")]`, so on this repo's Apple
/// Silicon host it does not exist. That is the same reason the rest of this
/// module is a boot suite rather than a `#[test]`.
///
/// Three pages, placed to exercise the three things that distinguish this walk
/// from `for va in range { translate(va) }`: a **hole** between two leaves in
/// one page table, a third leaf under a *different* PD entry 2 MiB away, and a
/// wholly empty range that must cost nothing and report nothing.
fn range_walk_test(t: &mut Suite, uas: &mut UserAddressSpace) {
    use akuma_mmu::{Leaf, LeafAction, PteProt};

    /// Far enough from [`TEST_VA`] to land under a different PD entry, so the
    /// walk has to descend twice rather than run along one page table.
    const FAR: usize = TEST_VA + (2 << 20);
    /// A gap of one page between the first two, so "visits present leaves"
    /// is distinguishable from "visits every page in the range".
    const MID: usize = TEST_VA + 2 * 4096;

    let mut frames = [0usize; 3];
    for (i, va) in [TEST_VA, MID, FAR].into_iter().enumerate() {
        let Ok(f) = uas.alloc_and_map(va, user_flags::RW_NO_EXEC) else {
            t.check("uas: range walk setup mapped three pages", false);
            return;
        };
        frames[i] = f.addr;
    }
    t.check("uas: range walk setup mapped three pages", true);

    // Everything, in ascending VA order, with the frames it was given.
    let mut seen: [usize; 4] = [0; 4];
    let mut pas: [usize; 4] = [0; 4];
    let mut n = 0usize;
    let mut ordered = true;
    let mut all_rw_nx = true;
    uas.for_each_leaf_in_range(TEST_VA, FAR + 4096, |leaf: Leaf| {
        if n < seen.len() {
            if n > 0 && leaf.va <= seen[n - 1] {
                ordered = false;
            }
            seen[n] = leaf.va;
            pas[n] = leaf.pa;
        }
        if !(leaf.prot.write && leaf.prot.user && !leaf.prot.exec) || leaf.cow {
            all_rw_nx = false;
        }
        n += 1;
    });
    t.check_eq("uas: for_each_leaf_in_range visits every present leaf", n as u64, 3);
    t.check("uas: leaves arrive in ascending VA order", ordered);
    t.check("uas: leaves report the VAs that were mapped",
        seen[0] == TEST_VA && seen[1] == MID && seen[2] == FAR);
    t.check("uas: leaves report the frames that were mapped",
        pas[0] == frames[0] && pas[1] == frames[1] && pas[2] == frames[2]);
    t.check("uas: leaves decode RW_NO_EXEC and no CoW marker", all_rw_nx);

    // A range that stops before the far page. The bound is exclusive: a leaf
    // exactly at `end` must not be reported, which is the off-by-one that makes
    // `munmap(addr, len)` free one page too many.
    let mut n = 0usize;
    uas.for_each_leaf_in_range(TEST_VA, MID + 4096, |_| n += 1);
    t.check_eq("uas: the range end is exclusive", n as u64, 2);
    let mut n = 0usize;
    uas.for_each_leaf_in_range(TEST_VA, MID, |_| n += 1);
    t.check_eq("uas: a range ending at a leaf excludes it", n as u64, 1);

    // An empty range, and a range over an unmapped gigabyte. The second is the
    // case the subtree skipping exists for — `rustc` reserves address space this
    // size — and getting it wrong is a hang, not a wrong answer.
    let mut n = 0usize;
    uas.for_each_leaf_in_range(TEST_VA, TEST_VA, |_| n += 1);
    t.check_eq("uas: an empty range reports nothing", n as u64, 0);
    let mut n = 0usize;
    uas.for_each_leaf_in_range(TEST_VA + (4 << 30), TEST_VA + (5 << 30), |_| n += 1);
    t.check_eq("uas: an unmapped gigabyte reports nothing", n as u64, 0);

    // The whole user half finds the same three and nothing else — in particular
    // it must not wander into the kernel's shared upper half.
    let mut n = 0usize;
    let mut above_user = 0usize;
    uas.for_each_user_leaf(|leaf: Leaf| {
        n += 1;
        if leaf.va >= akuma_mmu::USER_HALF_END {
            above_user += 1;
        }
    });
    t.check_eq("uas: for_each_user_leaf finds every mapping", n as u64, 3);
    t.check_eq("uas: for_each_user_leaf stays in the user half", above_user as u64, 0);

    // ── the mutating walk ──────────────────────────────────────────────────
    // Reprotect: same frame, new permissions, CoW marker set. This is the
    // `mprotect` shape and the `fork` demote shape at once.
    let ro = PteProt { write: false, exec: false, user: true };
    let rewritten = uas.rewrite_leaves_in_range(TEST_VA, FAR + 4096, |_| LeafAction::Reprotect(ro, true));
    t.check_eq("uas: rewrite_leaves_in_range visited three", rewritten as u64, 3);
    let mut demoted = 0usize;
    let mut kept_frames = true;
    let mut i = 0usize;
    uas.for_each_leaf_in_range(TEST_VA, FAR + 4096, |leaf: Leaf| {
        if !leaf.prot.write && leaf.cow && leaf.prot.user {
            demoted += 1;
        }
        if i < frames.len() && leaf.pa != frames[i] {
            kept_frames = false;
        }
        i += 1;
    });
    t.check_eq("uas: Reprotect demoted every leaf", demoted as u64, 3);
    t.check("uas: Reprotect kept each frame", kept_frames);
    // Read back through the crate's own accessor too, so this is not just the
    // walk agreeing with itself.
    let pte = uas.read_l3_page_entry(TEST_VA).unwrap_or(0);
    t.check("uas: Reprotect is visible to read_l3_page_entry", pte & PTE_RW == 0 && pte & PTE_COW != 0);

    // Unmap: clears the entries and does **not** free the frames or touch the
    // ledger — the caller was handed each `pa` and owns that decision. Checking
    // the ledger is what makes that contract testable rather than a comment.
    let before = uas.user_frame_count();
    let mut reported = [0usize; 3];
    let mut i = 0usize;
    let unmapped = uas.rewrite_leaves_in_range(TEST_VA, MID + 4096, |leaf: Leaf| {
        if i < reported.len() {
            reported[i] = leaf.pa;
        }
        i += 1;
        LeafAction::Unmap
    });
    t.check_eq("uas: Unmap visited the two leaves in range", unmapped as u64, 2);
    t.check("uas: Unmap reported each frame to the caller",
        reported[0] == frames[0] && reported[1] == frames[1]);
    t.check("uas: the unmapped VAs are gone", !uas.is_mapped(TEST_VA) && !uas.is_mapped(MID));
    t.check("uas: the out-of-range leaf survived", uas.is_mapped(FAR));
    t.check_eq("uas: Unmap left the ledger alone", uas.user_frame_count() as u64, before as u64);

    // Hand the three frames back by hand, since `Unmap` deliberately did not.
    for f in frames {
        uas.remove_user_frame(akuma_mmap::PhysFrame::new(f));
    }
    let _ = uas.unmap_and_free_page(FAR);
    for f in [frames[0], frames[1]] {
        akuma_pmm::free_page(f, 0);
    }
}
