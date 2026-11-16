#![cfg_attr(not(test), no_std)]
//! A spinlock that masks local IRQs as part of acquiring.
//!
//! Masking was a separate call at each site (`with_irqs_disabled(|| TABLE.lock()...)`, 272 of them), so a lock that an
//! interrupt handler also takes was only safe where the author remembered the pair. The incidents are the ones in
//! `IRQ_HANDLER_TABLE_DEADLOCK.md` (`IRQ_HANDLERS` taken with IRQs on while `dispatch_irq` takes it from the vector),
//! `STABILITY_URGENT_ISSUES.md` (`RUNTIME` self-deadlocking from an IRQ handler) and `THREADING_RACE_CONDITIONS.md`
//! (`POOL` against the allocator). [`IrqSpinlock`] has no unmasked `lock()`: any lock an IRQ handler or the scheduler
//! takes should be one of these, and the lock/mask pair cannot be split.
//!
//! The guard drops the lock first and restores the interrupt state second (field order), the order
//! `AddressSpaceGuard` already uses. It is `!Send` because [`IrqGuard`] is. This does **not** cover lock *ordering*
//! (AB-BA): see "not worth doing" in `TYPE_SYSTEM_SAFETY_REPORT.md`.

use core::ops::{Deref, DerefMut};

use akuma_primitives::irq::IrqGuard;
use spinning_top::Spinlock;
use spinning_top::guard::SpinlockGuard;

/// A [`Spinlock`] that is only ever taken with local IRQs masked.
pub struct IrqSpinlock<T> {
    inner: Spinlock<T>,
}

/// Holds the lock and the IRQ mask. Dropping it unlocks, then restores `DAIF`.
pub struct IrqSpinlockGuard<'a, T> {
    // Field order is the drop order: unlock before the mask is restored.
    guard: SpinlockGuard<'a, T>,
    _irq: IrqGuard,
}

impl<T> IrqSpinlock<T> {
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self { inner: Spinlock::new(value) }
    }

    /// Mask local IRQs, then acquire. There is no way to acquire without masking.
    pub fn lock(&self) -> IrqSpinlockGuard<'_, T> {
        let irq = IrqGuard::new();
        let guard = self.inner.lock();
        IrqSpinlockGuard { guard, _irq: irq }
    }

    /// Mask local IRQs and try once; the mask is restored if the lock was held.
    pub fn try_lock(&self) -> Option<IrqSpinlockGuard<'_, T>> {
        let irq = IrqGuard::new();
        let guard = self.inner.try_lock()?;
        Some(IrqSpinlockGuard { guard, _irq: irq })
    }

    /// Whether the lock is held right now. Diagnostic use only.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
}

impl<T> Deref for IrqSpinlockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for IrqSpinlockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_mutates_and_releases() {
        let l = IrqSpinlock::new(5u32);
        {
            let mut g = l.lock();
            *g += 1;
            assert!(l.is_locked());
            assert!(l.try_lock().is_none(), "a held lock refuses a second taker");
        }
        assert!(!l.is_locked());
        assert_eq!(*l.lock(), 6);
    }
}
