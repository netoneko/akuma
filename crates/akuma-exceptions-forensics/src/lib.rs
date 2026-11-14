//! Debug-only fault forensics for `akuma-exceptions`: what the kernel prints, after the fact, when a user fault
//! has the shape of a known memory-corruption class. They read other processes' frames through the physmap and
//! walk the PMM's free ledger, which is why they live outside the fault handler and behind its `forensics`
//! feature: a production kernel can drop them without touching the handler's logic.
//!
//! Nothing here changes state. Both run only on anomaly paths (`EAGER-UPGRADE`, `WILD-DA`, an `N x INTERP_BASE`
//! instruction abort), never per fault.

#![no_std]
// `phys_to_virt` returns a pointer to a page-aligned table frame as `*mut u8`; the cast only restores the type.
#![allow(clippy::cast_ptr_alignment)]

extern crate alloc;

use akuma_phys::{Frame, PhysAddr};
use akuma_primitives::safe_print;

/// Forensics for the `N × INTERP_BASE + offset` instruction-abort class
/// (`docs/archive/THREAD_SPAWN_SEGV_INVESTIGATION.md`, class 2).
///
/// The branch target is read from a slot in the interpreter's writable data that
/// musl's **RELR** apply loop mutates with `*slot += base`. A slot that already
/// carried a relocated value gets `base` added a second time, so `FAR` reads
/// `N*base + link_time_offset` with `N` = how many times the slot was relocated.
/// That only happens if two processes' startup code ran over the **same physical
/// frame**, so the one fact worth capturing is the slot's **PA**: two faults
/// reporting the same PA means a shared frame, different PAs mean the poisoned
/// value travelled by copy (a CoW/page-cache fill from an already-bumped source).
///
/// Fires only on a `FAR` of that exact shape, and only walks the interpreter's
/// own VA window, so it costs nothing on any other abort.
pub fn interp_relr_forensics(far: usize, pid: u32) {
    const BASE: usize = akuma_exec::elf_loader::INTERP_BASE;
    // N >= 2 (N == 1 is a correctly relocated pointer) and a plausible in-image offset.
    let n = far / BASE;
    let off = far % BASE;
    if n < 2 || off >= 0x0010_0000 {
        return;
    }
    let ttbr0 = akuma_exec::mmu::get_current_ttbr0();
    if ttbr0 == 0 {
        return;
    }
    let l0_addr = ttbr0 & 0x0000_FFFF_FFFF_F000;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr).cast::<u64>().cast_const();
    let tid = akuma_exec::threading::current_thread_id();
    let (cur_pid, ttbr0_proc, ppid) = akuma_exec::process::current_process_shared()
        .map_or((0, 0, 0), |p| (p.pid, p.address_space.ttbr0(), p.parent_pid));
    const L0_MASK: u64 = 0x0000_FFFF_FFFF_F000;
    let base_differs = ttbr0_proc != 0 && (ttbr0 as u64 & L0_MASK) != (ttbr0_proc & L0_MASK);
    safe_print!(224,
        "[RELR] fault_pid={} cur_pid={} ppid={} tid={} N={} off={:#x}\n",
        pid, cur_pid, ppid, tid, n, off);
    safe_print!(224,
        "[RELR] ttbr0_live={:#x} ttbr0_proc={:#x}{} expected_l0={:#x} switch_ins={} gen={}\n",
        ttbr0, ttbr0_proc,
        if base_differs { "  *** AS MISMATCH (foreign page tables) ***" } else { "" },
        akuma_exec::threading::thread_expected_l0(tid),
        akuma_exec::threading::thread_switch_ins(tid),
        akuma_exec::threading::thread_generation(tid));

    // The writable PT_LOAD of a musl interpreter sits at the top of its image;
    // 1 MB of VA covers it with room to spare and bounds the scan at 256 pages.
    let mut found = 0usize;
    for page in 0..256usize {
        let page_va = BASE + 0x0010_0000 - (page + 1) * 0x1000;
        let Some(pa) = akuma_exec::mmu::translate_user_va(l0_ptr, page_va) else {
            continue;
        };
        // SAFETY: a mapped user page of the interpreter's window: RAM no Rust object lives in.
        let frame = unsafe { Frame::from_pa(PhysAddr::from_usize(pa & !0xFFF)) };
        for i in 0..512usize {
            let v = frame.entry(i) as usize;
            if v == far {
                safe_print!(160,
                    "[RELR] slot va={:#x} pa={:#x} val={:#x}\n",
                    page_va + i * 8, (pa & !0xFFF) + i * 8, v);
                found += 1;
                if found >= 4 {
                    return;
                }
            }
        }
    }
    if found == 0 {
        safe_print!(128, "[RELR] no slot in interp window holds {:#x}\n", far);
    }
}

/// Runs only on anomaly paths (`EAGER-UPGRADE`, `WILD-DA`), never per-fault.
pub fn print_page_forensics(tag: &str, pid: u32, va: usize) {
    let page_va = va & !0xFFF;
    let ttbr0 = akuma_cpu::sysreg::ttbr0_el1();
    let l0_addr = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    let l0_ptr = akuma_exec::mmu::phys_to_virt(l0_addr).cast::<u64>().cast_const();
    let Some(pa) = akuma_exec::mmu::translate_user_va(l0_ptr, page_va).map(|p| p & !0xFFF) else {
        safe_print!(128, "[{}] pid={} va={:#x} UNMAPPED\n", tag, pid, page_va);
        return;
    };

    // Read through the kernel identity map, not the user VA: the point is to
    // inspect the frame regardless of what the user PTE currently permits.
    // SAFETY: `pa` is a page-aligned user frame (possibly already freed): RAM no Rust object lives in.
    let frame = unsafe { Frame::from_pa(PhysAddr::from_usize(pa)) };
    let words = [frame.entry(0), frame.entry(1), frame.entry(2), frame.entry(3)];
    // Distance, not the raw seq: "freed 40 frees ago" and "freed 300 000 frees
    // ago" are different findings, and only the first implicates this frame. With
    // no record at all there IS no distance — printing one computed against a
    // default seq of 0 yields a large number that reads exactly like a real
    // (innocent) age, so report the absence instead.
    let (tid_freed, free_age) = match akuma_pmm::last_free_record(pa) {
        Some((tid, seq)) => (i64::from(tid), i64::from(akuma_pmm::free_ledger_seq().wrapping_sub(seq))),
        None => (-1, -1),
    };
    let tracked = akuma_exec::process::lookup_process_shared(pid)
        .is_some_and(|p| p.address_space.tracks_user_frame(pa));
    safe_print!(255,
        "[{}] pid={} va={:#x} pa={:#x} FREE={} cow_ref={} tracked={} \
         last_free=(tid={} age={}) [-1 = never freed] head={:#x},{:#x},{:#x},{:#x}\n",
        tag, pid, page_va, pa,
        akuma_pmm::is_page_free(pa),
        akuma_pmm::cow_ref_get(pa),
        tracked, tid_freed, free_age,
        words[0], words[1], words[2], words[3]);
    // The reference history is what separates "this page's count was legitimately
    // 0" from "someone decremented it once too often".
    akuma_pmm::print_cow_history(pa);

    // How many eager regions claim this VA? The fault handler answers from the
    // first `Vec` match, so two overlapping regions mean the record it consulted
    // may describe a mapping that no longer exists — the current leading theory
    // for this anomaly. One region refutes it; more than one convicts.
    let regions = akuma_exec::process::eager_regions_containing(pid, page_va);
    safe_print!(96, "  [REGIONS] va={:#x} claimed_by={}\n", page_va, regions.len());
    for (start, pages, flags) in regions.iter().take(4) {
        safe_print!(128, "    start={:#x} pages={} flags={:#x}\n", start, pages, flags);
    }

    // What the PTE actually says. A write can permission-fault on a read-only page,
    // on a `PROT_NONE` guard page, and on a kernel-only page — three different
    // defects that a PA alone cannot tell apart, and the repair above grants the
    // write in all three cases.
    if let Some(pte) = akuma_exec::mmu::user_pte_raw(l0_ptr, page_va) {
        safe_print!(160, "  [PTE] va={:#x} raw={:#x} ap={}\n",
            page_va, pte, akuma_exec::mmu::ap_name(pte));
    } else {
        safe_print!(96, "  [PTE] va={:#x} no valid descriptor\n", page_va);
    }

    // A lazy region covering the same VA as an eager one is invisible to
    // `[REGIONS]` but decisive here: a `PROT_NONE` lazy region makes the fault
    // handler skip its own lazy arm and fall through to the eager repair, which
    // then promotes a page userspace asked to be unmapped.
    if let Some((flags, _src, start, size)) =
        akuma_exec::process::lazy_region_lookup_for_page_fault(pid, page_va)
    {
        safe_print!(192,
            "  [LAZY] va={:#x} ALSO covered by lazy region start={:#x} size={:#x} flags={:#x} prot_none={}\n",
            page_va, start, size, flags, akuma_exec::mmu::user_flags::is_none(flags));
    } else {
        safe_print!(96, "  [LAZY] va={:#x} no lazy region\n", page_va);
    }
}
