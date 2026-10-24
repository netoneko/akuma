//! x86_64 4-level page tables **for the kernel's own mappings**: map, unmap,
//! translate — the machine half of `amd64/src/paging.rs`, extracted so a
//! composed kernel can make kernel mappings without inheriting the module.
//!
//! `boot.s` (in this crate) leaves the machine on a fixed identity map of the
//! first 64 GiB built from 2 MiB pages; this is the first code that can
//! *change* a mapping, which is the prerequisite for anything that maps a
//! device window, protects a region, or addresses memory beyond what the boot
//! tables cover.
//!
//! What deliberately stays in each kernel's own `paging` module:
//!
//! * **`activate`** — switching CR3. The full kernel publishes every switch to
//!   `akuma-mmu`'s per-core live-L0 registry (so a dropped `UserAddressSpace`
//!   cannot free tables a core still runs); a single-vCPU kernel with no user
//!   address spaces has nothing to publish. Same reason `drop_identity_map`
//!   here is split into [`clear_identity_slot`] (pure table edit) and the
//!   caller's own CR3 reload.
//! * **the self-test suite** — `akuma-selftest` is a whole-kernel affair.
//!
//! Two `Prot`s, and why they stay two (kept from the original module's docs):
//! [`PteProt`] is the **page-table** vocabulary — what the hardware is told;
//! `akuma_mmap::Prot` is the **region** vocabulary — what a mapping is
//! *supposed* to be. The type and the encoder are `akuma-pte-amd64`'s,
//! re-exported so `crate::paging::{MemAttr, PteProt}` call sites are unchanged.

use akuma_pagetable::PhysMap;
use akuma_pagetable::amd64 as walk;

use crate::phys::phys_to_virt;

/// The error code the CPU pushes for vector 14, asked as questions — the crate's
/// (`akuma-pte-amd64`), re-exported for the fault paths of whichever kernel
/// pulls this.
pub use akuma_pte_amd64::PageFaultCode;

/// The page-permission vocabulary and its encoder — `akuma-pte-amd64`'s,
/// re-exported.
pub use akuma_pte_amd64::{MemAttr, PteProt};

pub(crate) const PAGE_SIZE: usize = akuma_pte_amd64::PAGE_SIZE;

/// The physmap, as `akuma-pagetable` asks for it.
///
/// `phys_to_virt` asserts the frame is inside [`crate::phys::PHYSMAP_LIMIT`]:
/// the moment the PMM is given memory above that, a table frame stops being
/// reachable, and the failure mode of *not* asserting is a page fault with no
/// IDT installed — a triple-fault and a guest that vanishes with no output.
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

fn read_cr3() -> u64 {
    akuma_cpu::ctlreg::cr3() & akuma_pte_amd64::ADDR_MASK
}

/// Map `[pa, pa + len)` as device memory in the device window and return the
/// `len` bytes at `pa` as a register window.
///
/// The one place that vouches "these bytes are device registers" for a physical
/// range: every page is mapped [`MemAttr::Device`] for the kernel's lifetime
/// (the device window is never torn down), so a safe
/// [`akuma_primitives::mmio::MmioWindow`] over it cannot name memory the CPU may
/// serve from cache. Mapping allocates page-table frames, so it must run after
/// `mem`'s bring-up.
#[must_use]
pub fn map_device_window(pa: u64, len: u64) -> Option<akuma_primitives::mmio::MmioWindow> {
    use crate::phys::DEVMAP_BASE;
    let first = pa & !(PAGE_SIZE as u64 - 1);
    let last = (pa + len).div_ceil(PAGE_SIZE as u64) * PAGE_SIZE as u64;
    let mut p = first;
    while p < last {
        if !map_page((DEVMAP_BASE + p) as usize, p, PteProt::KERNEL_RW, MemAttr::Device) {
            return None;
        }
        p += PAGE_SIZE as u64;
    }
    // SAFETY: `[pa, pa + len)` is covered by the pages just mapped Device above,
    // for the kernel's lifetime. Callers name the access widths and offsets.
    Some(unsafe { akuma_primitives::mmio::MmioWindow::new((DEVMAP_BASE + pa) as usize, len as usize) })
}

/// Invalidate one page's TLB entry.
///
/// Core-local (`invlpg`); on amd64 a multi-core kernel must send an IPI to
/// every other core that could hold the translation — there is no broadcast
/// form. This file edits only the boot-time kernel tables, where core-local is
/// complete.
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
/// a process's tables be built *before* they are activated. Intermediate tables
/// come straight from the PMM and are not tracked: these are the kernel's own
/// mappings, which are never torn down.
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
/// *hardware* would do.
pub fn translate(va: usize) -> Option<u64> {
    translate_in(read_cr3(), va)
}

/// Resolve `va` in the address space rooted at `root`.
pub fn translate_in(root: u64, va: usize) -> Option<u64> {
    walk::translate::<Physmap>(root, va)
}

/// Clear PML4 slot 0 — the trampoline's identity map — **without** touching CR3.
///
/// Split out of the full kernel's `drop_identity_map` because the CR3 reload
/// that makes the edit take effect is each kernel's own business (the full
/// kernel republishes to `akuma-mmu`'s live-L0 registry around it; a kernel
/// with one core and no user tables can plain `load_cr3`). The caller **must**
/// reload CR3 (or otherwise flush) after this, or the stale low translations
/// stay live.
pub fn clear_identity_slot() {
    let root = read_cr3();
    walk::clear_root_slot::<Physmap>(root, 0);
}
