//! Synchronization primitives for akuma-exec.
//!
//! Provides `RwSpinlock<T>` — a reader-writer spinlock built on `lock_api`
//! with writer priority to prevent reader starvation.

use core::sync::atomic::{AtomicU32, Ordering};

/// Raw reader-writer spinlock with writer priority.
///
/// State encoding in a single `AtomicU32`:
/// - Bit 31 (`WRITER_BIT`): set when a writer is pending or active
/// - Bits 0-30: reader count (up to ~2 billion, more than enough)
///
/// Transitions:
/// - `0x0000_0000` = unlocked (no readers, no writer)
/// - `0x0000_000N` = N readers active, no writer pending
/// - `0x8000_000N` = N readers active, writer pending (draining readers)
/// - `0x8000_0000` = write-locked (writer active, no readers)
///
/// Writer priority: once `WRITER_BIT` is set, new `lock_shared` calls spin
/// until the writer finishes, preventing reader starvation of writers.
pub struct RawRwSpinlock(AtomicU32);

const WRITER_BIT: u32 = 0x8000_0000;
const READER_MASK: u32 = 0x7FFF_FFFF;
const UNLOCKED: u32 = 0;

/// Spin iteration limit before logging a diagnostic (helps debug deadlocks).
const SPIN_WARN_THRESHOLD: u32 = 10_000_000;

unsafe impl lock_api::RawRwLock for RawRwSpinlock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self(AtomicU32::new(UNLOCKED));

    type GuardMarker = lock_api::GuardSend;

    fn lock_shared(&self) {
        loop {
            let state = self.0.load(Ordering::Relaxed);
            // If a writer is pending/active, spin (writer priority)
            if state & WRITER_BIT != 0 {
                core::hint::spin_loop();
                continue;
            }
            // Try to increment reader count
            if self.0.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ).is_ok() {
                return;
            }
            core::hint::spin_loop();
        }
    }

    fn try_lock_shared(&self) -> bool {
        let state = self.0.load(Ordering::Relaxed);
        if state & WRITER_BIT != 0 {
            return false;
        }
        self.0.compare_exchange(
            state,
            state + 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ).is_ok()
    }

    unsafe fn unlock_shared(&self) {
        self.0.fetch_sub(1, Ordering::Release);
    }

    fn lock_exclusive(&self) {
        // Phase 1: Set WRITER_BIT to block new readers.
        // fetch_or is atomic — even if readers are active, this succeeds.
        let prev = self.0.fetch_or(WRITER_BIT, Ordering::Acquire);

        // If another writer already has the bit, we must wait for it to finish
        // and then retry (only one writer at a time).
        if prev & WRITER_BIT != 0 {
            // Another writer is active/pending. Spin until state == UNLOCKED,
            // then try the whole sequence again.
            loop {
                let state = self.0.load(Ordering::Relaxed);
                if state == UNLOCKED {
                    if self.0.compare_exchange_weak(
                        UNLOCKED,
                        WRITER_BIT,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    ).is_ok() {
                        break; // We now own the writer bit
                    }
                } else if state & WRITER_BIT == 0 {
                    // Previous writer finished but readers jumped in.
                    // Set writer bit again.
                    let prev2 = self.0.fetch_or(WRITER_BIT, Ordering::Acquire);
                    if prev2 & WRITER_BIT == 0 {
                        break; // We now own the writer bit
                    }
                }
                core::hint::spin_loop();
            }
        }

        // Phase 2: Wait for existing readers to drain.
        // WRITER_BIT is set, so no new readers can enter.
        let mut spins: u32 = 0;
        while self.0.load(Ordering::Acquire) != WRITER_BIT {
            spins = spins.wrapping_add(1);
            if spins == SPIN_WARN_THRESHOLD {
                // Diagnostic: log the stuck state for debugging deadlocks
                log_write_lock_stuck(self.0.load(Ordering::Relaxed));
            }
            core::hint::spin_loop();
        }
        // State is now WRITER_BIT (= write-locked, no readers)
    }

    fn try_lock_exclusive(&self) -> bool {
        self.0.compare_exchange(
            UNLOCKED,
            WRITER_BIT,
            Ordering::Acquire,
            Ordering::Relaxed,
        ).is_ok()
    }

    unsafe fn unlock_exclusive(&self) {
        self.0.store(UNLOCKED, Ordering::Release);
    }
}

/// Diagnostic: log when write lock is stuck spinning.
fn log_write_lock_stuck(state: u32) {
    // Use a stack buffer to avoid heap allocation (might be in IRQ-disabled context).
    // Only print once per stuck episode (caller checks threshold).
    let readers = state & READER_MASK;
    let writer_bit = (state & WRITER_BIT) != 0;

    // Minimal stack-based print to avoid any lock contention
    use core::fmt::Write;
    struct Buf([u8; 96], usize);
    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let b = s.as_bytes();
            let n = b.len().min(96 - self.1);
            self.0[self.1..self.1 + n].copy_from_slice(&b[..n]);
            self.1 += n;
            Ok(())
        }
    }
    let mut buf = Buf([0u8; 96], 0);
    let _ = writeln!(buf, "[RWLOCK] write lock stuck: state={:#x} readers={} writer_bit={}",
        state, readers, writer_bit);
    if buf.1 > 0 {
        if let Ok(s) = core::str::from_utf8(&buf.0[..buf.1]) {
            (crate::runtime::runtime().print_str)(s);
        }
    }
}

impl RawRwSpinlock {
    /// Read the raw lock state for diagnostics.
    pub fn raw_state(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Reader-writer spinlock.
pub type RwSpinlock<T> = lock_api::RwLock<RawRwSpinlock, T>;

/// Read guard for `RwSpinlock`.
pub type RwSpinlockReadGuard<'a, T> = lock_api::RwLockReadGuard<'a, RawRwSpinlock, T>;

/// Write guard for `RwSpinlock`.
pub type RwSpinlockWriteGuard<'a, T> = lock_api::RwLockWriteGuard<'a, RawRwSpinlock, T>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rwspinlock_read_then_write() {
        let lock = RwSpinlock::new(42u32);
        {
            let r = lock.read();
            assert_eq!(*r, 42);
        }
        {
            let mut w = lock.write();
            *w = 99;
        }
        assert_eq!(*lock.read(), 99);
    }

    #[test]
    fn rwspinlock_multiple_readers() {
        let lock = RwSpinlock::new(7u32);
        let r1 = lock.read();
        let r2 = lock.read();
        let r3 = lock.read();
        assert_eq!(*r1, 7);
        assert_eq!(*r2, 7);
        assert_eq!(*r3, 7);
    }

    #[test]
    fn rwspinlock_try_write_fails_while_read_held() {
        let lock = RwSpinlock::new(0u32);
        let _r = lock.read();
        assert!(lock.try_write().is_none());
    }

    #[test]
    fn rwspinlock_try_read_fails_while_write_held() {
        let lock = RwSpinlock::new(0u32);
        let _w = lock.write();
        assert!(lock.try_read().is_none());
    }

    #[test]
    fn rwspinlock_try_write_fails_while_write_held() {
        let lock = RwSpinlock::new(0u32);
        let _w = lock.write();
        assert!(lock.try_write().is_none());
    }

    #[test]
    fn rwspinlock_write_after_readers_drop() {
        let lock = RwSpinlock::new(1u32);
        {
            let _r1 = lock.read();
            let _r2 = lock.read();
            assert!(lock.try_write().is_none());
        }
        let mut w = lock.write();
        *w = 2;
        drop(w);
        assert_eq!(*lock.read(), 2);
    }

    #[test]
    fn rwspinlock_read_after_write_drops() {
        let lock = RwSpinlock::new(10u32);
        {
            let mut w = lock.write();
            *w = 20;
            assert!(lock.try_read().is_none());
        }
        assert_eq!(*lock.read(), 20);
    }

    #[test]
    fn rwspinlock_with_btreemap() {
        use alloc::collections::BTreeMap;
        let lock = RwSpinlock::new(BTreeMap::<u32, u32>::new());
        {
            let mut w = lock.write();
            w.insert(1, 10);
            w.insert(2, 20);
        }
        {
            let r = lock.read();
            assert_eq!(r.get(&1), Some(&10));
            assert_eq!(r.get(&2), Some(&20));
            assert_eq!(r.len(), 2);
        }
    }

    #[test]
    fn rwspinlock_state_encoding_writer_priority() {
        use lock_api::RawRwLock;
        let raw = RawRwSpinlock::INIT;
        assert_eq!(raw.0.load(Ordering::Relaxed), UNLOCKED);

        // Shared locks increment reader count (bits 0-30)
        raw.lock_shared();
        assert_eq!(raw.0.load(Ordering::Relaxed), 1);
        raw.lock_shared();
        assert_eq!(raw.0.load(Ordering::Relaxed), 2);

        unsafe { raw.unlock_shared(); }
        assert_eq!(raw.0.load(Ordering::Relaxed), 1);
        unsafe { raw.unlock_shared(); }
        assert_eq!(raw.0.load(Ordering::Relaxed), UNLOCKED);

        // Exclusive lock sets WRITER_BIT
        raw.lock_exclusive();
        assert_eq!(raw.0.load(Ordering::Relaxed), WRITER_BIT);
        unsafe { raw.unlock_exclusive(); }
        assert_eq!(raw.0.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn rwspinlock_try_read_blocked_by_pending_writer() {
        use lock_api::RawRwLock;
        let raw = RawRwSpinlock::INIT;

        // Simulate a pending writer by setting WRITER_BIT with readers active
        raw.0.store(WRITER_BIT | 1, Ordering::Relaxed); // 1 reader + writer pending

        // try_lock_shared should fail (writer priority)
        assert!(!raw.try_lock_shared());

        // Clean up
        raw.0.store(UNLOCKED, Ordering::Relaxed);
    }

    #[test]
    fn rwspinlock_writer_priority_blocks_new_readers() {
        let lock = RwSpinlock::new(0u32);

        // Take a write lock
        let w = lock.write();

        // While write-locked, try_read should fail
        assert!(lock.try_read().is_none());

        drop(w);

        // After write releases, read should succeed
        assert!(lock.try_read().is_some());
    }
}
