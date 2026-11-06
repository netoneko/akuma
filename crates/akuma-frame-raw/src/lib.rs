//! Raw access to physical frames the caller owns. Three functions; the only `unsafe` `akuma-pmm` ever needed.
//!
//! # The contract
//!
//! Zeroing or poisoning an arbitrary physical address is memory corruption, so these are **not** safe in
//! general. They are safe `fn`s because of a *crate-level* invariant the type system cannot express: the only
//! caller is `akuma-pmm`, which passes a frame it has just taken from its own bitmap allocator (exclusive) or
//! one it has already reclaimed into quarantine. What *is* checked here is the part a function can: on the
//! bare-metal target the whole `[pa, pa + len)` must lie inside the RAM physmap window. The functions are
//! `#[doc(hidden)]`; a second caller must justify the same invariant or make these `unsafe fn`.

#![no_std]
#![allow(clippy::cast_ptr_alignment)] // frames are page-aligned

use akuma_primitives::phys_to_virt;

/// ARM64/amd64 base page size, as in `akuma-pmm`.
pub const PAGE_SIZE: usize = 4096;

/// Refuse a range outside RAM on the bare-metal target. Host tests run on real host addresses (identity
/// `phys_to_virt`), where the physmap window does not apply.
fn check_range(pa: usize, pages: usize) {
    #[cfg(target_os = "none")]
    {
        let len = pages.checked_mul(PAGE_SIZE).expect("frame range overflows");
        let end = pa.checked_add(len).expect("frame range overflows");
        assert!(
            pages != 0 && akuma_primitives::addr::physmap_contains(pa) && akuma_primitives::addr::physmap_contains(end - 1),
            "frame range {pa:#x}+{pages} pages outside RAM"
        );
    }
    #[cfg(not(target_os = "none"))]
    let _ = (pa, pages);
}

/// Clean `pages` frames' worth of data cache starting at `pa`, then `dsb`.
///
/// Steps by the **runtime** `CTR_EL0` line size rather than a hardcoded 64: a hardcoded 64 silently
/// under-cleans on a core whose `DminLine` is smaller; stepping by the architectural minimum can only
/// over-clean.
fn clean_range(pa: usize, pages: usize) {
    let step = akuma_cpu::cache::line_size();
    let start = phys_to_virt(pa) as usize;
    let end = start + pages * PAGE_SIZE;
    let mut addr = start;
    while addr < end {
        akuma_cpu::cache::dc_cvac(addr);
        addr += step;
    }
    akuma_cpu::barrier::dsb_ish();
}

/// Zero `pages` frames starting at `pa` and clean them to the point of coherency, so a device or another core
/// reading the frame without going through this core's cache sees the zeros.
#[doc(hidden)]
pub fn zero_and_clean(pa: usize, pages: usize) {
    check_range(pa, pages);
    // SAFETY: the crate contract - `pa` was just returned by the PMM's allocator, so the `pages` frames from it
    // are contiguous, mapped through the kernel's physical window and owned exclusively by this caller; the
    // write is exactly the allocated extent, and `check_range` confirmed it lies in RAM.
    unsafe {
        core::ptr::write_bytes(phys_to_virt(pa), 0, pages * PAGE_SIZE);
    }
    clean_range(pa, pages);
}

/// Fill one frame with `word`, for the UAF quarantine.
///
/// **Volatile, and it must stay volatile.** Nothing in the PMM reads the poison back in the same compilation
/// unit as the write - `poison_check` runs on a later drain - so a plain store to a frame the compiler believes
/// dead is exactly what LLVM may remove, which would silently stop the quarantine detecting use-after-free.
#[doc(hidden)]
pub fn poison_fill(pa: usize, word: u64) {
    check_range(pa, 1);
    // SAFETY: the crate contract - a frame the allocator has reclaimed and parked in the quarantine ring, so
    // no live mapping should reference it; the loop covers exactly one page.
    unsafe {
        let p = phys_to_virt(pa).cast::<u64>();
        for i in 0..(PAGE_SIZE / 8) {
            p.add(i).write_volatile(word);
        }
    }
}

/// Check one frame's poison. Returns `(byte offset, value found)` at the first word that is not `want`, or
/// `None` if the whole frame is intact.
#[doc(hidden)]
#[must_use]
pub fn poison_check(pa: usize, want: u64) -> Option<(usize, u64)> {
    check_range(pa, 1);
    // SAFETY: as `poison_fill` - a quarantined frame, read back over exactly the page that was written.
    unsafe {
        let p = phys_to_virt(pa).cast::<u64>();
        for i in 0..(PAGE_SIZE / 8) {
            let got = p.add(i).read_volatile();
            if got != want {
                return Some((i * 8, got));
            }
        }
    }
    None
}
