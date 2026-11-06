//! Demand paging on amd64: `akuma_tokio_kernel_core::lazy` over the x86 page table.
//!
//! The policy (what a fill does, the reservation, the inode pins) is the core's;
//! this file is the machine's answers to it ([`Amd64`]: the 4-level walk,
//! `invlpg`, the x86 leaf bits) and the call sites' names, unchanged.

use akuma_boot_amd64::paging::{self, MemAttr, PteProt};
use akuma_primitives::addr::PhysAddr;
use akuma_boot_amd64::phys::FreshFrame;
use akuma_tokio_kernel_core::lazy as core_lazy;
use akuma_tokio_kernel_core::mmu::{FreshPage, UserPages};

pub use akuma_tokio_kernel_core::lazy::{Fill, Lazy};

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
        self.0.pa().as_usize()
    }
    fn bytes_mut(&mut self) -> &mut [u8; 4096] {
        self.0.bytes_mut()
    }
    fn copy_from(&mut self, src_pa: usize) {
        self.0.copy_from_frame(PhysAddr::from_usize(src_pa));
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

    fn writable(pte: u64) -> bool {
        pte & akuma_pte_amd64::RW != 0
    }

    /// Bit 9, which the CPU ignores: amd64's CoW mark (`akuma_cow::CowFault::marked`).
    fn cow_marked(pte: u64) -> bool {
        pte & akuma_pte_amd64::COW != 0
    }

    /// The dirty bit: the CPU sets it on a user store, `with_written` on a kernel one.
    fn written(pte: u64) -> bool {
        pte & akuma_pte_amd64::D != 0
    }

    fn with_written(pte: u64) -> u64 {
        pte | akuma_pte_amd64::D
    }

    fn take(pte: u64) -> u64 {
        (pte | akuma_pte_amd64::RW) & !akuma_pte_amd64::COW
    }

    fn take_at(pte: u64, pa: u64) -> u64 {
        ((pte & !akuma_pte_amd64::ADDR_MASK) | akuma_pte_amd64::RW | pa) & !akuma_pte_amd64::COW
    }

    fn share_marked(pte: u64) -> u64 {
        (pte & !akuma_pte_amd64::RW) | akuma_pte_amd64::COW
    }

    /// A page without the user bit is a `PROT_NONE` one, and stays inaccessible.
    fn anon_like(pte: u64) -> Lazy {
        let write = pte & (akuma_pte_amd64::RW | akuma_pte_amd64::COW) != 0;
        let none = pte & akuma_pte_amd64::US == 0;
        Lazy { none, ..Lazy::anon(write, pte & akuma_pte_amd64::NX == 0) }
    }
}

impl akuma_tokio_kernel_core::mm::Mm for Amd64 {
    fn with_task<R>(f: impl FnOnce(&mut akuma_tokio_kernel_core::task::Task) -> R) -> R {
        crate::with_task(f)
    }
    fn translate(root: u64, va: u64) -> Option<u64> {
        paging::translate_in(root, va as usize)
    }
    fn unmap(root: u64, va: u64) -> Option<u64> {
        paging::unmap_page_in(root, va as usize)
    }
    fn map_page(root: u64, va: u64, pa: u64, write: bool, exec: bool, user: bool) -> bool {
        paging::map_page_in(root, va as usize, pa, PteProt { write, exec, user }, MemAttr::WriteBack)
    }
    fn frame_of(pte: u64) -> Option<u64> {
        akuma_pte_amd64::is_present(pte).then(|| akuma_pte_amd64::frame(pte))
    }
    fn shootdown() {
        crate::smp::shootdown();
    }
    fn mmdbg() -> u32 {
        crate::mmdbg()
    }
    fn mm_event(kind: u8, start: u64, end: u64) {
        crate::mm_event(kind, start, end);
    }
    fn copy_to_user(dst_va: u64, src: &[u8]) -> bool {
        crate::copy_to_user(dst_va, src)
    }
    fn copy_from_user(dst: &mut [u8], src_va: u64) -> bool {
        crate::copy_from_user(dst, src_va)
    }
}

impl akuma_tokio_kernel_core::tree::UserTree for Amd64 {
    const LAYOUT: akuma_user_tree::PteLayout = akuma_user_tree::X86_64;
    fn entry(table: u64, i: usize) -> u64 {
        // SAFETY: the tree walkers name only a live root or a frame a present
        // table entry of one names: a PMM page-table frame.
        unsafe { akuma_boot_amd64::phys::Frame::from_pa(PhysAddr::new(table)) }.entry(i)
    }
    fn set_entry(table: u64, i: usize, value: u64) {
        // SAFETY: as `entry`.
        unsafe { akuma_boot_amd64::phys::Frame::from_pa(PhysAddr::new(table)) }.set_entry(i, value);
    }
    /// The lower half; the upper half is the kernel's, shared by every root.
    fn user_root_slot(i: usize) -> bool {
        i < 256
    }
    /// Nothing inside a lower-half table is the kernel's.
    fn kernel_entry(_depth: u32, _va: u64) -> bool {
        false
    }
    fn install_kernel(dst: u64) -> bool {
        // SAFETY: `dst` is the fresh root frame the caller allocated.
        crate::copy_kernel_half(unsafe { akuma_boot_amd64::phys::Frame::from_pa(PhysAddr::new(dst)) });
        true
    }
    fn live_root() -> u64 {
        akuma_cpu::ctlreg::cr3() & akuma_pte_amd64::ADDR_MASK
    }
}

impl akuma_tokio_kernel_core::loader::Loader for Amd64 {}

impl core_lazy::Kernel for Amd64 {
    fn root_fs() -> &'static dyn akuma_vfs::Filesystem {
        crate::root_fs()
    }
    fn task_root() -> u64 {
        crate::task_root()
    }
}

pub fn fill(root: u64, va: u64, files: bool) -> Fill {
    core_lazy::fill::<Amd64>(root, va, files)
}

pub fn prefault(start: usize, len: usize) -> bool {
    core_lazy::prefault::<Amd64>(start, len)
}


impl akuma_tokio_kernel_core::proc::Proc for Amd64 {
    fn current_slot() -> usize {
        // `NO_SLOT` (the boot task before init) is `usize::MAX`: outside the ring.
        crate::exec_twin::current_slot()
    }
    fn prepare_slot(slot: usize) -> bool {
        akuma_sched_amd64::prepare_task_slot::<crate::sched::Tx>(slot).is_some()
    }
    fn unlocked(f: impl FnOnce()) {
        crate::smp::bkl_unlocked(f);
    }
    fn uptime_us() -> u64 {
        crate::net::uptime_us()
    }
    fn task_slot_index() -> usize {
        crate::task_slot_index()
    }
    fn with_task_slot<R>(i: usize, f: impl FnOnce(&mut akuma_tokio_kernel_core::task::Task) -> R) -> R {
        crate::with_task_slot(i, f)
    }
    fn record_root(i: usize) -> u64 {
        crate::task_root_of(i)
    }
}
