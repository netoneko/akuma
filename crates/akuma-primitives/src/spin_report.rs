//! `spinning_top::Spinlock` with a stuck-report: a `lock()` that spins past
//! `1 << 27` failed try-locks prints the lock's tag ONCE to the console and
//! keeps spinning.
//!
//! Why this exists: a kernel that runs its tasks with IF masked has no
//! preemption, so a spinlock held across a switch (or taken twice by one
//! thread) does not deadlock politely - the acquirer spins forever at 100 %
//! of the vCPU with zero VM exits, no tick ever fires, and nothing times out.
//! The console print is the only witness such a wedge can ever produce. The
//! tag is the constructor's (`Spinlock::reporting("POOL", ..)`); untagged
//! `new` prints `?`.
//!
//! The spin threshold is ~a second at guest speeds: any legitimate hold is
//! thousands of cycles, never hundreds of millions.

use spinning_top::{Spinlock as Inner, SpinlockGuard};

/// A `spinning_top::Spinlock` whose `lock()` reports a stuck spin once per
/// `1 << 27` failed attempts. Same guard type, so call sites do not change.
pub struct Spinlock<T> {
    tag: &'static str,
    inner: Inner<T>,
}

impl<T> Spinlock<T> {
    /// Untagged lock (`?` in a stuck report).
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self { tag: "?", inner: Inner::new(value) }
    }

    /// Tagged lock: `tag` is what a stuck report names.
    #[must_use]
    pub const fn reporting(tag: &'static str, value: T) -> Self {
        Self { tag, inner: Inner::new(value) }
    }

    /// Acquire, reporting a stuck spin (once per `1 << 27` attempts).
    pub fn lock(&self) -> SpinlockGuard<'_, T> {
        let mut n: u32 = 0;
        loop {
            if let Some(g) = self.inner.try_lock() {
                return g;
            }
            n = n.wrapping_add(1);
            if n == 1 << 27 {
                crate::safe_print!(
                    64,
                    "[stuck] spinlock '{}' spun 1<<27 tries - held across a switch or taken twice\n",
                    self.tag
                );
            }
            core::hint::spin_loop();
        }
    }

    /// Try once; `None` when held.
    pub fn try_lock(&self) -> Option<SpinlockGuard<'_, T>> {
        self.inner.try_lock()
    }

    /// Is the lock currently held? Diagnostic use only.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
}
