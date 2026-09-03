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
//! The physmap maps RAM with 2 MiB pages, writeback-cached — which is right for
//! RAM and wrong for MMIO. The LAPIC sits at `0xFEE0_0000`, inside the first
//! GiB, so it is *already* covered by the physmap at a cached address. Rather
//! than split that 2 MiB page, device registers get their own window mapped 4 KiB
//! at a time with [`crate::paging::MemAttr::Device`]. Two windows onto the same
//! physical address, with different cacheability, is the point.

/// Base of the physmap window (PML4 slot 256).
pub const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;
/// Base of the device window (PML4 slot 257).
pub const DEVMAP_BASE: u64 = 0xFFFF_8080_0000_0000;
/// Base the kernel image is linked at (PML4 slot 511); see `amd64/linker.ld`.
pub const KERNEL_VMA: u64 = 0xFFFF_FFFF_8000_0000;

/// How much physical memory the physmap covers.
///
/// `boot.s` builds one page directory of 512 x 2 MiB pages and points the
/// physmap, the identity map and the kernel window at it, so all three describe
/// the same first gigabyte. Physical memory beyond this is not addressable by
/// the kernel, which is why `mem::init` clamps the PMM to it.
pub const PHYSMAP_LIMIT: u64 = 1 << 30;

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
