//! Demand paging on amd64: `akuma_tokio_kernel_core::lazy` over the x86 page table.
//!
//! The policy (what a fill does, the reservation, the inode pins) is the core's;
//! this file is the machine's answers to it ([`Amd64`]: the 4-level walk,
//! `invlpg`, the x86 leaf bits) and the call sites' names, unchanged.

use akuma_boot_amd64::paging::{self, MemAttr, PteProt};
use akuma_boot_amd64::phys::FreshFrame;
use akuma_tokio_kernel_core::lazy as core_lazy;
use akuma_tokio_kernel_core::mmu::{FreshPage, UserPages};

pub use akuma_tokio_kernel_core::lazy::{Fill, Lazy, drop_pins, fits, fork_pins, is_lazy, move_pins, pin};
pub use akuma_tokio_kernel_core::lazy_record::FilePage;

/// The amd64 machine, as the core's demand paging sees it.
pub struct Amd64;

/// `akuma_boot_amd64`'s fresh frame (where the physmap's `unsafe` lives), as the core's.
pub struct Frame(FreshFrame);

impl FreshPage for Frame {
    fn alloc() -> Option<Self> {
        FreshFrame::alloc().map(Self)
    }
    fn alloc_zeroed() -> Option<Self> {
        FreshFrame::alloc_zeroed().map(Self)
    }
    fn pa(&self) -> usize {
        self.0.pa()
    }
    fn bytes_mut(&mut self) -> &mut [u8; 4096] {
        self.0.bytes_mut()
    }
    fn publish(self) -> usize {
        self.0.publish()
    }
}

impl UserPages for Amd64 {
    type Frame = Frame;

    fn leaf(root: u64, va: u64) -> Option<u64> {
        crate::leaf_entry(root, va).map(|slot| slot.read())
    }

    fn write_leaf(root: u64, va: u64, entry: u64) -> bool {
        crate::leaf_entry(root, va).map(|slot| slot.write(entry)).is_some()
    }

    fn set_leaf(root: u64, va: u64, entry: u64) -> bool {
        paging::set_leaf_in(root, va as usize, entry)
    }

    fn map_user(root: u64, va: u64, pa: u64, write: bool, exec: bool) -> bool {
        paging::map_page_in(root, va as usize, pa, PteProt { write, exec, user: true }, MemAttr::WriteBack)
    }

    fn invalidate(va: u64) {
        akuma_cpu::tlb::invlpg(va as usize);
    }

    /// A page without the user bit is a `PROT_NONE` one, and stays inaccessible.
    fn anon_like(pte: u64) -> Lazy {
        let write = pte & (akuma_pte_amd64::RW | akuma_pte_amd64::COW) != 0;
        let none = pte & akuma_pte_amd64::US == 0;
        Lazy { none, ..Lazy::anon(write, pte & akuma_pte_amd64::NX == 0) }
    }
}

impl core_lazy::Kernel for Amd64 {
    fn root_fs() -> &'static dyn akuma_vfs::Filesystem {
        crate::root_fs()
    }
    fn task_root() -> u64 {
        crate::task_root()
    }
}

pub fn reserve(root: u64, base: u64, len: u64, first: Lazy) -> bool {
    core_lazy::reserve::<Amd64>(root, base, len, first)
}

pub fn fill(root: u64, va: u64, files: bool) -> Fill {
    core_lazy::fill::<Amd64>(root, va, files)
}

pub fn prefault(start: usize, len: usize) -> bool {
    core_lazy::prefault::<Amd64>(start, len)
}

pub fn reprotect(root: u64, va: u64, write: bool, exec: bool, none: bool) -> bool {
    core_lazy::reprotect::<Amd64>(root, va, write, exec, none)
}

pub fn clear(root: u64, va: u64) {
    core_lazy::clear::<Amd64>(root, va);
}

pub fn move_range(root: u64, from: u64, to: u64, len: u64) -> bool {
    core_lazy::move_range::<Amd64>(root, from, to, len)
}

pub fn anon_like(pte: u64) -> Lazy {
    Amd64::anon_like(pte)
}

pub fn unpin_released(root: u64, start: u64, end: u64, keep: impl Fn(u32, u64, u64) -> bool) -> bool {
    core_lazy::unpin_released::<Amd64>(root, start, end, keep)
}
