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

use crate::addr_types::PAGE_SIZE;

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
    fn a_length_past_the_address_space_is_refused_not_wrapped() {
        assert!(Pages::from_len_ceil(1usize << 48).is_some());
        assert!(Pages::from_len_ceil((1usize << 48) + 1).is_none());
        // The lengths whose naive `(len + 4095) & !4095` is 0.
        assert!(Pages::from_len_ceil(usize::MAX).is_none());
        assert!(Pages::from_len_ceil(usize::MAX - 4094).is_none());
    }
}
