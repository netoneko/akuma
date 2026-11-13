//! A fixed-size overwrite ring for the `trace-err` evidence tables (the syscall
//! ring, the futex ring, the mmap-slab events), and the per-slot table beside it.
//!
//! Each used to be a `static mut` array plus a `static mut HEAD`, reached through
//! `&mut *addr_of_mut!` by the hot path and by the idle thread's dump. Here the head
//! and the buffer share one [`BorrowCell`], so a dump that runs while a push is open
//! is a panic naming the bug, and none of the accesses is `unsafe`.

use akuma_primitives::cell::BorrowCell;

struct Inner<T, const N: usize> {
    head: usize,
    buf: [T; N],
}

/// The last `N` entries pushed. `const`, for `static`.
pub struct Ring<T: Copy, const N: usize>(BorrowCell<Inner<T, N>>);

impl<T: Copy, const N: usize> Ring<T, N> {
    /// An empty ring: every entry `zero`.
    pub const fn new(zero: T) -> Self {
        Self(BorrowCell::new(Inner { head: 0, buf: [zero; N] }))
    }

    /// Record `v`, overwriting the oldest entry.
    pub fn push(&self, v: T) {
        self.0.with_mut(|r| {
            r.buf[r.head % N] = v;
            r.head += 1;
        });
    }

    /// Read the ring: `f(head, buf)`. The oldest entry is `buf[head % N]`, so
    /// `buf[(head + i) % N]` for `i` in `0..N` runs oldest to newest.
    pub fn view<R>(&self, f: impl FnOnce(usize, &[T; N]) -> R) -> R {
        self.0.with(|r| f(r.head, &r.buf))
    }
}
