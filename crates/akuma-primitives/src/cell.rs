//! A `Sync` cell whose borrow rules are checked at run time.
//!
//! # Why this exists
//!
//! A per-slot record that is too big or too hardware-facing to be a bag of
//! atomics — a saved register file the syscall assembly writes by offset, an
//! `xsave` area a context switch hands to the CPU — used to live in a
//! `static mut` array reached as `(*(&raw mut TABLE))[slot].field`, with an
//! `unsafe` block per access vouching for "under the BKL, in range, no other
//! `&mut`". Those three facts are not properties of the line.
//!
//! [`BorrowCell`] is `RefCell` made `Sync`: the same one-writer-or-many-readers
//! rule, enforced by an atomic word instead of a `Cell<isize>`, so a violation is
//! a panic naming the bug rather than two live `&mut` to one struct. In-range is
//! the array's bounds check; the scheduler's lock stays what it always was —
//! the reason two cores do not *race* — while this catches the thing a lock
//! cannot: one core re-entering a record it already holds.
//!
//! # The pointer escape hatch
//!
//! [`as_ptr`](BorrowCell::as_ptr) forms a raw pointer without borrowing, for the
//! two consumers that are not Rust: assembly that addresses the record by
//! offset, and `xsave`/`xrstor`. Forming it is safe; dereferencing it is the
//! consumer's, and **the cell cannot see it** — a borrow open on the Rust side
//! while the assembly writes is not detected. That is the boundary, stated.
//!
//! # Layout
//!
//! `repr(C)` with the value first, so the value sits at offset 0 of the cell and
//! keeps the alignment of `T` (an `xsave` area needs 64).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

/// `0` free, `WRITER` exclusively held, anything between a reader count.
const WRITER: u32 = u32::MAX;

/// A value reachable from a `static`, borrowed at run time.
#[repr(C)]
pub struct BorrowCell<T> {
    value: UnsafeCell<T>,
    state: AtomicU32,
}

// SAFETY: every access to `value` goes through `with`/`with_mut`/`get`, which
// admit one writer or any number of readers by CAS on `state`, across cores.
// `as_ptr` only forms a pointer. `T: Send` because a value written on one core
// is read and dropped-over on another.
unsafe impl<T: Send> Sync for BorrowCell<T> {}

impl<T> BorrowCell<T> {
    /// A cell holding `value`. `const`, for `static`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self { value: UnsafeCell::new(value), state: AtomicU32::new(0) }
    }

    fn acquire_read(&self) -> bool {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            if s >= WRITER - 1 {
                return false;
            }
            match self.state.compare_exchange_weak(s, s + 1, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(now) => s = now,
            }
        }
    }

    fn acquire_write(&self) -> bool {
        self.state.compare_exchange(0, WRITER, Ordering::Acquire, Ordering::Relaxed).is_ok()
    }

    /// Run `f` on a shared borrow.
    ///
    /// # Panics
    /// If the cell is mutably borrowed.
    #[track_caller]
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        assert!(self.acquire_read(), "BorrowCell read while mutably borrowed");
        // Release on every exit, including an unwind: a panic inside `f` must not
        // leave the cell locked.
        struct Release<'a>(&'a AtomicU32);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Release);
            }
        }
        let _release = Release(&self.state);
        // SAFETY: the read lock excludes any `&mut`.
        f(unsafe { &*self.value.get() })
    }

    /// Run `f` on an exclusive borrow.
    ///
    /// # Panics
    /// If the cell is borrowed in any way.
    #[track_caller]
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        assert!(self.acquire_write(), "BorrowCell mutably borrowed while already borrowed");
        struct Release<'a>(&'a AtomicU32);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(0, Ordering::Release);
            }
        }
        let _release = Release(&self.state);
        // SAFETY: the write lock makes this the only access.
        f(unsafe { &mut *self.value.get() })
    }

    /// A copy of the value.
    ///
    /// # Panics
    /// If the cell is mutably borrowed.
    #[must_use]
    #[track_caller]
    pub fn get(&self) -> T
    where
        T: Copy,
    {
        self.with(|v| *v)
    }

    /// Overwrite the value.
    ///
    /// # Panics
    /// If the cell is borrowed.
    #[track_caller]
    pub fn set(&self, value: T) {
        self.with_mut(|v| *v = value);
    }

    /// A raw pointer to the value, **without borrowing**: for assembly and
    /// `xsave`. Forming it is safe; the consumer owns the dereference and this
    /// cell cannot see it — see the module docs.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut T {
        self.value.get()
    }
}

#[cfg(test)]
mod tests {
    use super::BorrowCell;

    #[test]
    fn with_with_mut_get_set() {
        let c = BorrowCell::new([1u32, 2, 3]);
        assert_eq!(c.with(|a| a[1]), 2);
        c.with_mut(|a| a[1] = 9);
        assert_eq!(c.get(), [1, 9, 3]);
        c.set([0; 3]);
        assert_eq!(c.get(), [0; 3]);
    }

    #[test]
    fn readers_nest_and_writers_do_not() {
        let c = BorrowCell::new(5u64);
        let sum = c.with(|a| c.with(|b| *a + *b));
        assert_eq!(sum, 10);
    }

    #[test]
    #[should_panic(expected = "while already borrowed")]
    fn writer_inside_a_reader_panics() {
        let c = BorrowCell::new(0u8);
        c.with(|_| c.with_mut(|v| *v = 1));
    }

    #[test]
    #[should_panic(expected = "while mutably borrowed")]
    fn reader_inside_a_writer_panics() {
        let c = BorrowCell::new(0u8);
        c.with_mut(|_| c.get());
    }

    #[test]
    fn a_panic_inside_the_closure_releases_the_borrow() {
        let c = BorrowCell::new(0u8);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.with_mut(|_| panic!("boom"))));
        assert!(r.is_err());
        c.set(7); // would panic "already borrowed" if the writer leaked
        assert_eq!(c.get(), 7);
    }

    #[test]
    fn as_ptr_is_the_value_and_keeps_alignment() {
        #[repr(align(64))]
        struct A([u8; 8]);
        let c = BorrowCell::new(A([0; 8]));
        assert_eq!(c.as_ptr() as usize % 64, 0);
        // SAFETY: the test owns the cell and holds no borrow.
        unsafe { c.as_ptr().cast::<u8>().write(5) };
        assert_eq!(c.with(|a| a.0[0]), 5);
    }
}
