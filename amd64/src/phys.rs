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
/// `boot.s` builds **four** page directories of 512 x 2 MiB pages each and
/// points the physmap, the identity map and the kernel window at them, so all
/// three describe the same first **4 GiB**. Physical memory beyond this is not
/// addressable by the kernel, which is why `mem::init` clamps the PMM to it.
///
/// **Raised from 1 GiB on 2026-09-05**, when the bare-metal boot arrived. It is
/// not a tuning knob: on real hardware the framebuffer is a PCI BAR at
/// `0xE000_0000` and the LAPIC is at `0xFEE0_0000`, both of them nearly 4 GiB
/// up, and with a 1 GiB physmap neither is reachable at all. The constant and
/// the page tables in `boot.s` describe the same thing and must move together.
pub const PHYSMAP_LIMIT: u64 = 4 << 30;

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
