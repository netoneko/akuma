//! A thread's kernel stack as a region: `[base, base + size)`.
//!
//! The scheduler keeps three kinds of bookkeeping *inside* a stack's own bytes: a **canary** at the base
//! (overrun detection), a **sentinel** painted over the unused part (so a scan can find how deep the stack
//! ever went), and nothing else. All three used to be open-coded raw-pointer loops in `akuma-threading`;
//! here they are methods whose every access is range-checked against the region, so the one contract is
//! stated once, at construction.
//!
//! The contract ([`StackRegion::new`]): the region is mapped, writable RAM that nothing else is *using as
//! data*. Painting overwrites bytes, so the caller must only paint the part of a live stack that is below
//! its stack pointer; that is a call-site fact, noted at each call.

#![no_std]

/// The word the unused part of a stack is painted with.
pub const SENTINEL: u64 = 0xABAB_ABAB_ABAB_ABAB;

/// A stack's address range. Cheap to copy; owns nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackRegion {
    base: usize,
    size: usize,
}

impl StackRegion {
    /// # Safety
    /// `[base, base + size)` must be mapped, writable memory that stays mapped for as long as this value is
    /// used, and no other reference to it may be live while a method here runs (the scheduler's stacks are
    /// owned by their slot; the boot stack by thread 0).
    #[must_use]
    pub const unsafe fn new(base: usize, size: usize) -> Self {
        Self { base, size }
    }

    /// Lowest address.
    #[must_use]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// One past the highest address.
    #[must_use]
    pub const fn top(&self) -> usize {
        self.base.saturating_add(self.size)
    }

    /// First 8-aligned address at least `skip_bytes` above the base (clamped to the top).
    fn scan_start(&self, skip_bytes: usize) -> usize {
        let start = self.base.saturating_add(skip_bytes).saturating_add(7) & !7;
        start.min(self.top())
    }

    /// Paint `[base + skip_bytes, end)` with [`SENTINEL`], 8 bytes at a time. `end` is clamped to the top and
    /// the range is empty if it does not lie above `skip_bytes`.
    ///
    /// The caller must only pass an `end` at or below the live stack pointer's red zone.
    pub fn paint_sentinel(&self, skip_bytes: usize, end: usize) {
        let mut addr = self.scan_start(skip_bytes);
        let end = end.min(self.top());
        while addr + 8 <= end {
            // SAFETY: `addr..addr+8` is inside `[start, end) ⊆ [base, top)`, 8-aligned, in the region
            // `new` vouched for.
            unsafe { (addr as *mut u64).write_volatile(SENTINEL) };
            addr += 8;
        }
    }

    /// Bytes of the stack that have ever been used, below the top: scans up from `base + skip_bytes` for the
    /// first word that is not [`SENTINEL`] and returns `top - that address` (0 if the whole range is intact).
    #[must_use]
    pub fn high_water(&self, skip_bytes: usize) -> usize {
        let top = self.top();
        let mut addr = self.scan_start(skip_bytes);
        while addr + 8 <= top {
            // SAFETY: `addr..addr+8` is inside `[base, top)`, 8-aligned.
            if unsafe { (addr as *const u64).read_volatile() } != SENTINEL {
                return top - addr;
            }
            addr += 8;
        }
        0
    }

    /// Write `words` copies of `value` at the base. Writes nothing for a region too small to hold them.
    pub fn write_canary(&self, words: usize, value: u64) {
        if words.saturating_mul(8) > self.size {
            return;
        }
        for i in 0..words {
            // SAFETY: `base + 8*i + 8 <= base + 8*words <= top`; the base is 8-aligned by contract of a stack.
            unsafe { ((self.base + i * 8) as *mut u64).write_volatile(value) };
        }
    }

    /// Whether the base canary of `words` copies of `value` is intact. A region too small to hold it is
    /// reported as corrupt.
    #[must_use]
    pub fn canary_intact(&self, words: usize, value: u64) -> bool {
        if words.saturating_mul(8) > self.size {
            return false;
        }
        (0..words).all(|i| {
            // SAFETY: as in `write_canary`.
            unsafe { ((self.base + i * 8) as *const u64).read_volatile() == value }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec;

    fn region(buf: &mut [u64]) -> StackRegion {
        // SAFETY: the test owns `buf` for the whole test.
        unsafe { StackRegion::new(buf.as_mut_ptr() as usize, buf.len() * 8) }
    }

    #[test]
    fn paint_then_scan_sees_nothing_used() {
        let mut b = vec![0u64; 64];
        let r = region(&mut b);
        r.paint_sentinel(16, r.top());
        assert_eq!(r.high_water(16), 0);
    }

    #[test]
    fn high_water_is_the_lowest_disturbed_word() {
        let mut b = vec![0u64; 64];
        let r = region(&mut b);
        r.paint_sentinel(16, r.top());
        b[40] = 1; // the stack reached word 40 (24 words below top)
        let r = region(&mut b);
        assert_eq!(r.high_water(16), (64 - 40) * 8);
    }

    #[test]
    fn paint_respects_end_and_skip() {
        let mut b = vec![0u64; 16];
        let r = region(&mut b);
        r.paint_sentinel(16, r.base() + 64);
        assert_eq!((b[0], b[1], b[2], b[7], b[8]), (0, 0, SENTINEL, SENTINEL, 0));
    }

    #[test]
    fn canary_round_trip_and_break() {
        let mut b = vec![0u64; 8];
        let r = region(&mut b);
        r.write_canary(2, 0xDEAD);
        assert!(r.canary_intact(2, 0xDEAD));
        b[1] = 0;
        let r = region(&mut b);
        assert!(!r.canary_intact(2, 0xDEAD));
    }

    #[test]
    fn oversized_requests_do_not_touch_memory() {
        let mut b = vec![7u64; 2];
        let r = region(&mut b);
        r.write_canary(100, 1);
        assert_eq!(b, [7, 7]);
        assert!(!r.canary_intact(100, 7));
        let r = region(&mut b);
        r.paint_sentinel(0, usize::MAX); // clamped to top
        assert_eq!(b, [SENTINEL, SENTINEL]);
    }
}
