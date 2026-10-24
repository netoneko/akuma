//! Translating physical addresses into ones the kernel can dereference.
//!
//! Until Stage K the kernel ran identity-mapped, so a physical address *was* a
//! valid pointer and this module did not need to exist. It does now: the lower
//! half belongs to userspace, so the kernel reaches physical memory through a
//! window in the upper half instead.
//!
//! ```text
//!   0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF   userspace (PML4 0..255)
//!   0xFFFF_8000_0000_0000 + pa                       physmap   (PML4 256)
//!   0xFFFF_8080_0000_0000 + pa                       device    (PML4 257)
//!   0xFFFF_FFFF_8000_0000 + pa                       kernel image (PML4 511)
//! ```
//!
//! # Why devices need a second window
//!
//! **Corrected 2026-09-04.** This note used to say the LAPIC at `0xFEE0_0000`
//! was "inside the first GiB", already covered by the physmap at a cached
//! address, and that the device window existed to avoid splitting a 2 MiB page.
//! That arithmetic is wrong twice over: `0xFEE0_0000` is **3.98 GiB**, and
//! [`PHYSMAP_LIMIT`] is 1 GiB — so the physmap does not reach the LAPIC at all,
//! and no cached alias of it has ever existed. QEMU `microvm` puts virtio-MMIO
//! at `0xFEB0_0000`, in the same hole, for the same reason: this is the standard
//! x86 MMIO region just below 4 GiB, which is deliberately *not* RAM.
//!
//! The real reasons for a second window are both still good, and neither is the
//! one that was written down:
//!
//! 1. **Reach.** The physmap covers only what `boot.s` maps — one page directory,
//!    512 x 2 MiB, the first GiB. Device MMIO is above it, so it needs a mapping
//!    the physmap does not provide.
//! 2. **Cacheability.** RAM is writeback; MMIO must be uncached, or the CPU can
//!    satisfy a register read from cache and never issue the access. That is what
//!    [`crate::paging::MemAttr`] is for, and it is the half that would still
//!    matter if the physmap were ever grown past 4 GiB — at which point the
//!    "two windows onto one physical page" story becomes true rather than
//!    aspirational, and the physmap would have to skip the MMIO hole or leave a
//!    cached alias of every device register in place.

/// Base of the physmap window (PML4 slot 256).
pub const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;
/// Base of the device window (PML4 slot 257).
pub const DEVMAP_BASE: u64 = 0xFFFF_8080_0000_0000;
/// Base the kernel image is linked at (PML4 slot 511); see `amd64/linker.ld`.
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;

/// How much physical memory the physmap covers.
///
/// `boot.s` builds [`PHYSMAP_PDS`] page directories of 512 x 2 MiB pages each
/// and points the physmap, the identity map and the kernel window at them, so
/// all three describe the same first **64 GiB**. Physical memory beyond this is
/// not addressable by the kernel, which is why `mem::init` clamps the PMM to it
/// — and says so in the boot log rather than dropping it in silence.
///
/// **Raised from 1 GiB to 4 GiB on 2026-09-05**, when the bare-metal boot
/// arrived: on real hardware the framebuffer is a PCI BAR at `0xE000_0000` and
/// the LAPIC is at `0xFEE0_0000`, both of them nearly 4 GiB up, and with a
/// 1 GiB physmap neither is reachable at all.
///
/// **Raised from 4 GiB to 64 GiB on 2026-09-12**, for the opposite half of the
/// address space. A PC displaces the RAM sitting behind the MMIO hole to just
/// above 4 GiB, so on the 16 GiB reference machine the memory map reported
/// 16321 MiB across eight regions and every one of them past the first began at
/// `0x1_0000_0000`. `mem::init`'s `end.min(PHYSMAP_LIMIT)` turned each into an
/// empty range and skipped it, leaving 3275 MiB reachable and 2504 MiB
/// allocatable — 15% of the machine, with nothing in the log saying so.
///
/// 64 rather than 16 (which would have fitted that machine exactly) because
/// unbacked entries cost one PDE apiece and buy the next machine: the whole
/// class of "the box has more RAM than the kernel can see" closes at a price of
/// 256 KiB of NOLOAD `.bss`.
///
/// The constant and the page tables in `boot.s` describe the same thing, which
/// is why [`PHYSMAP_PDS`] is *passed into* that file as a `global_asm!` operand
/// instead of being written down twice. `boot::physmap_reaches_limit` then
/// walks the live page tables for the last page of this window on every boot,
/// so a drift is a failed self-test rather than a `phys_to_virt` that returns a
/// plausible pointer into nothing.
pub const PHYSMAP_LIMIT: u64 = akuma_primitives::addr::PHYSMAP_LIMIT as u64;

/// Page directories `boot.s` builds, one per GiB of [`PHYSMAP_LIMIT`].
///
/// Handed to `boot.s` through `global_asm!` (see `main.rs`), so the count and
/// the limit cannot disagree.
pub const PHYSMAP_PDS: usize = (PHYSMAP_LIMIT >> 30) as usize;

/// The kernel-virtual address of a physical address.
///
/// # Panics
/// If `pa` is outside the physmap. That is a hard error rather than a wrapping
/// return: the alternative is a pointer that looks plausible, faults on first
/// use, and — before the IDT exists — takes the machine down with no output.
#[must_use]
#[inline]
pub fn phys_to_virt(pa: u64) -> u64 {
    assert!(pa < PHYSMAP_LIMIT, "physical address outside the physmap");
    PHYSMAP_BASE + pa
}

/// As [`phys_to_virt`], as a mutable pointer.
#[must_use]
#[inline]
pub fn phys_ptr<T>(pa: u64) -> *mut T {
    phys_to_virt(pa) as *mut T
}

/// Bytes in a frame.
const PAGE_SIZE: usize = 4096;

/// A page frame this code owns **exclusively** and has not yet published to any
/// page table, ledger or other owner.
///
/// That is the one situation where a `&mut` view of a frame through the physmap is
/// sound, so this type is where the physmap's `unsafe` lives for "stage a page
/// before it is mapped": a demand-zero fault, a CoW copy's destination, a loaded
/// segment, a test program's code and stack. A `fn zero_page(pa: u64)` would be
/// unsound — any caller could pass a frame somebody else is using.
///
/// Dropping it returns the frame to the PMM, so an early return between
/// allocation and mapping no longer leaks. **Hand the frame on with
/// [`FreshFrame::publish`] the moment it is mapped or tracked**: dropping a frame
/// that a page table already names frees it out from under that mapping.
pub struct FreshFrame {
    pa: usize,
}

impl FreshFrame {
    /// Allocate a frame with whatever its previous owner left in it. Use
    /// [`FreshFrame::alloc_zeroed`] unless every byte is about to be overwritten.
    #[must_use]
    pub fn alloc() -> Option<Self> {
        akuma_pmm::alloc_page().map(|pa| Self { pa })
    }

    /// Allocate a zeroed frame. Zero *before* mapping: a recycled frame otherwise
    /// leaks the previous owner's bytes to whoever reads it next.
    #[must_use]
    pub fn alloc_zeroed() -> Option<Self> {
        let mut frame = Self::alloc()?;
        frame.bytes_mut().fill(0);
        Some(frame)
    }

    /// The frame's physical address, for mapping it. Does not give it up.
    #[must_use]
    pub fn pa(&self) -> usize {
        self.pa
    }

    /// The frame's bytes.
    pub fn bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        // SAFETY: a PMM frame this value owns alone (nothing maps or tracks it
        // yet, and `publish` consumes the only handle), reached through the
        // physmap, which covers every PMM frame; `[u8; N]` has no alignment or
        // validity requirement, and `&mut self` makes this the only live view.
        unsafe { &mut *phys_ptr::<[u8; PAGE_SIZE]>(self.pa as u64) }
    }

    /// Overwrite this frame with the contents of the live frame at `src_pa`.
    ///
    /// `src_pa` must be a frame nothing is writing while this runs — the CoW
    /// source, which every mapper holds read-only. The copy is one page of
    /// physmap-reachable RAM; nothing here can fault or alias `self`, but a
    /// concurrent writer to `src_pa` would tear the copy.
    ///
    /// # Panics
    /// If `src_pa` is not page-aligned or is outside the physmap.
    pub fn copy_from_frame(&mut self, src_pa: usize) {
        assert!(src_pa.is_multiple_of(PAGE_SIZE), "copy_from_frame: unaligned source");
        assert_ne!(src_pa, self.pa, "copy_from_frame: a frame cannot be its own source");
        // SAFETY: `phys_ptr` asserts `src_pa` is inside the physmap; the frame is
        // a whole page of RAM; it is not `self.pa` (checked), so the shared view
        // cannot alias the `&mut` that `bytes_mut` hands out; and the caller's
        // contract above rules out a concurrent writer.
        let src = unsafe { &*phys_ptr::<[u8; PAGE_SIZE]>(src_pa as u64) };
        self.bytes_mut().copy_from_slice(src);
    }

    /// Give the frame up: it is now mapped or tracked by whoever called this, and
    /// is theirs to free. Returns its physical address.
    pub fn publish(self) -> usize {
        let pa = self.pa;
        core::mem::forget(self);
        pa
    }
}

impl Drop for FreshFrame {
    fn drop(&mut self) {
        akuma_pmm::free_page(self.pa, 0);
    }
}
