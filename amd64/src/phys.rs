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

/// How much physical memory the PMM is given, and the bound page tables are
/// checked against.
///
/// One gigabyte, which is what `boot.s` mapped until it grew a fourth page
/// directory for the bare-metal framebuffer; the PMM's range was never widened
/// with it, and stays here on purpose — `paging::table_mut` and every frame
/// this kernel hands out are checked against this. Growing it means auditing
/// what treats a frame address as small. What is *mapped* is
/// [`PHYSMAP_MAPPED`], which is larger.
pub const PHYSMAP_LIMIT: u64 = 1 << 30;

/// How much physical memory the physmap actually covers.
///
/// `boot.s` builds **four** page directories of 512 x 2 MiB pages — 4 GiB — and
/// points the physmap and the identity map at all four (the kernel window at the
/// first). This is the bound for *reading* through the physmap: ACPI tables in
/// particular live wherever the VMM put them, and QEMU puts them at the top of
/// RAM — `0x7fff_ffaf` on a 2 GiB guest, above [`PHYSMAP_LIMIT`] — where a
/// 1 GiB bound made the MADT invisible and the machine look single-core.
/// (Measured 2026-09-05; Firecracker's tables are at `0xA00xx` and never hit
/// this.) The MMIO hole below 4 GiB is inside this range as a *cached* alias;
/// nothing reads devices through it — the device window exists for that.
pub const PHYSMAP_MAPPED: u64 = 4 << 30;

/// The kernel-virtual address of a physical address.
///
/// # Panics
/// If `pa` is outside the physmap. That is a hard error rather than a wrapping
/// return: the alternative is a pointer that looks plausible, faults on first
/// use, and — before the IDT exists — takes the machine down with no output.
#[must_use]
#[inline]
pub fn phys_to_virt(pa: u64) -> u64 {
    assert!(pa < PHYSMAP_MAPPED, "physical address outside the physmap");
    PHYSMAP_BASE + pa
}

/// As [`phys_to_virt`], as a mutable pointer.
#[must_use]
#[inline]
pub fn phys_ptr<T>(pa: u64) -> *mut T {
    phys_to_virt(pa) as *mut T
}
