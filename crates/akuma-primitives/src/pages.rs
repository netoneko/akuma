//! A page count whose byte length cannot overflow.
//!
//! Page counts and byte lengths were both `usize`, and the conversions between them wrapped in release: `(len + 4095)
//! & !4095` is `0` for a `len` near `usize::MAX`, so `munmap` of such a length unmapped nothing and returned success,
//! `mprotect` changed nothing and returned success, and `mremap`'s `new_pages * 4096` wrapped to `0` and was saved
//! only because the placer refuses a zero length (`docs/archive/TYPE_SYSTEM_SAFETY_REPORT.md` §10). The two that
//! were not benign (`AKUMA_EXTRACT_MMAP.md` §10.1: an unbounded `madvise` loop and a `MAP_FIXED` guard bypass) were
//! the same arithmetic.
//!
//! [`Pages`] is bounded by the 48-bit user address space, checked **once**, where a user length becomes a count.
//! After that [`Pages::bytes`] is infallible.

use crate::addr_types::{PAGE_SIZE, VirtAddr};

/// A number of 4 KiB pages no larger than the 48-bit user space.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Pages(u64);

impl Pages {
    /// The most pages a user address space can hold: `2^48 / 4096`.
    pub const MAX: u64 = (1u64 << 48) / PAGE_SIZE as u64;

    /// The pages covering `len` bytes, rounded up; `None` past [`Pages::MAX`]. This is the boundary check.
    #[must_use]
    pub const fn from_len_ceil(len: usize) -> Option<Pages> {
        let pages = len.div_ceil(PAGE_SIZE) as u64;
        if pages > Self::MAX { None } else { Some(Pages(pages)) }
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// The length in bytes. Cannot overflow: at most `2^48`.
    #[must_use]
    pub const fn bytes(self) -> usize {
        self.0 as usize * PAGE_SIZE
    }
}

/// `addr` rounded up to the next 4 KiB boundary, or `None` if that would wrap past `usize::MAX`.
///
/// `(addr + 0xFFF) & !0xFFF` on an address near the top of the space wraps to a small number, which a caller that then
/// compares it with another page boundary reads as "far below": `brk(usize::MAX - 5)` used to look like a shrink to page 0
/// and unmapped the process's own heap and image (see `Process::set_brk`).
#[must_use]
pub const fn page_ceil(addr: usize) -> Option<usize> {
    match addr.checked_add(PAGE_SIZE - 1) {
        Some(v) => Some(v & !(PAGE_SIZE - 1)),
        None => None,
    }
}

/// A page-aligned range of user addresses: the start is aligned, the page count is a [`Pages`], and the end does not pass
/// the 48-bit space, all checked at construction. `end()` and `bytes()` are then infallible.
///
/// The architecture's own user limit (47 bits on amd64) is a narrower, per-kernel check the caller still makes against
/// `end()`; this type only guarantees that the arithmetic cannot wrap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PageRange {
    start: VirtAddr,
    pages: Pages,
}

impl PageRange {
    /// `pages` pages from `start`; `None` if `start` is not page-aligned or the range would pass `2^48`.
    #[must_use]
    pub const fn new(start: VirtAddr, pages: Pages) -> Option<PageRange> {
        if !start.is_page_aligned() {
            return None;
        }
        match start.get().checked_add(pages.bytes()) {
            Some(end) if end as u64 <= (1u64 << 48) => Some(PageRange { start, pages }),
            _ => None,
        }
    }

    /// The range a syscall's `(addr, len)` names: `addr` aligned, `len` rounded up to whole pages.
    #[must_use]
    pub const fn from_user(start: usize, len: usize) -> Option<PageRange> {
        match Pages::from_len_ceil(len) {
            Some(p) => PageRange::new(VirtAddr::new(start), p),
            None => None,
        }
    }

    #[must_use]
    pub const fn start(self) -> VirtAddr {
        self.start
    }

    #[must_use]
    pub const fn pages(self) -> Pages {
        self.pages
    }

    #[must_use]
    pub const fn bytes(self) -> usize {
        self.pages.bytes()
    }

    /// One past the last byte. Cannot overflow: checked at construction.
    #[must_use]
    pub const fn end(self) -> usize {
        self.start.get() + self.pages.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_up_and_never_wraps() {
        assert_eq!(Pages::from_len_ceil(0).map(Pages::get), Some(0));
        assert_eq!(Pages::from_len_ceil(1).map(Pages::bytes), Some(4096));
        assert_eq!(Pages::from_len_ceil(4096).map(Pages::bytes), Some(4096));
        assert_eq!(Pages::from_len_ceil(4097).map(Pages::bytes), Some(8192));
    }

    #[test]
    fn page_ceil_rounds_up_and_refuses_a_wrap() {
        assert_eq!(page_ceil(0), Some(0));
        assert_eq!(page_ceil(1), Some(4096));
        assert_eq!(page_ceil(4096), Some(4096));
        assert_eq!(page_ceil(4097), Some(8192));
        assert_eq!(page_ceil(usize::MAX), None);
        assert_eq!(page_ceil(usize::MAX - 5), None);
        assert_eq!(page_ceil(usize::MAX - 4095), Some(usize::MAX - 4095), "the last aligned address rounds to itself");
        assert_eq!(page_ceil(usize::MAX - 4094), None, "one byte into the last page cannot round up");
    }

    #[test]
    fn page_range_checks_alignment_and_the_end() {
        assert_eq!(PageRange::from_user(0x1000, 4097).map(PageRange::end), Some(0x3000));
        assert!(PageRange::from_user(0x1001, 4096).is_none(), "unaligned start");
        assert!(PageRange::from_user(usize::MAX - 4095, 4096).is_none(), "start + len wraps");
        assert!(PageRange::from_user((1usize << 48) - 4096, 8192).is_none(), "passes the 48-bit space");
        assert_eq!(PageRange::from_user((1usize << 48) - 4096, 4096).map(PageRange::end), Some(1usize << 48));
        assert!(PageRange::from_user(0x1000, usize::MAX).is_none());
    }

    #[test]
    fn a_length_past_the_address_space_is_refused_not_wrapped() {
        assert!(Pages::from_len_ceil(1usize << 48).is_some());
        assert!(Pages::from_len_ceil((1usize << 48) + 1).is_none());
        // The lengths whose naive `(len + 4095) & !4095` is 0.
        assert!(Pages::from_len_ceil(usize::MAX).is_none());
        assert!(Pages::from_len_ceil(usize::MAX - 4094).is_none());
    }
}
