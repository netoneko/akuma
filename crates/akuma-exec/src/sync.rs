//! Synchronization primitives for akuma-exec.
//!
//! Provides `RwSpinlock<T>` — a reader-writer spinlock built on `lock_api`
//! with writer priority to prevent reader starvation — and `KernelLock`, the
//! recursive Big Kernel Lock used by real (shared-kernel) SMP.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Total Big-Kernel-Lock spin iterations across all contended [`KernelLock::acquire`]
/// calls — a cross-core BKL-wait-time proxy for A/B measurement (e.g. does dropping the
/// BKL around a fault's block I/O reduce peer wait). Accumulated once per acquire, so the
/// uncontended fast path is unaffected. Read/reset via [`contention_spins`] /
/// [`reset_contention_spins`].
static CONTENTION_SPINS: AtomicU64 = AtomicU64::new(0);

/// Snapshot the total BKL contention-spin counter (see [`CONTENTION_SPINS`]).
pub fn contention_spins() -> u64 {
    CONTENTION_SPINS.load(Ordering::Relaxed)
}

/// Reset the total BKL contention-spin counter to zero (for A/B measurement windows).
pub fn reset_contention_spins() {
    CONTENTION_SPINS.store(0, Ordering::Relaxed);
}

/// The Big Kernel Lock (BKL) for real (shared-kernel) SMP — an **owner-tracked,
/// idempotent** spinlock that serializes kernel execution across cores.
///
/// **Invariant:** the lock is held by a core **iff that core is executing kernel code
/// (EL1).** It is *reconciled* at every EL transition rather than balanced like an
/// ordinary lock: entry from EL0 acquires it; an `eret` back to EL0 releases it; a
/// nested exception taken while already in EL1, and the `eret` back to EL1 from it,
/// leave it held (the target is still EL1). Because there is exactly one EL1→EL0
/// return per kernel excursion, there is exactly one release per excursion — no
/// per-thread depth needs to travel across context switches. This upgrades the
/// kernel's pervasive single-core `with_irqs_disabled` invariant (mutual exclusion on
/// one core only) into a genuine cross-core one, so the ~218 legacy
/// `lookup_process() -> &'static mut Process` sites become correct without per-site
/// changes (docs/archive/SMP_SHARED.md, M1). Uncontended on a single-core build and in
/// M0/M1 (secondaries parked).
///
/// **Contract:** all operations must run with local IRQs masked (exception entry, the
/// eret epilogue, and `with_irqs_disabled` all guarantee this), so re-entrancy from a
/// *local* interrupt is stack-ordered, never concurrent, on the owning core. Cross-core
/// exclusion is the compare-exchange on `owner`. `acquire`/`release` are **idempotent**
/// for the owner (re-acquiring what you hold, or releasing what you don't, is a no-op),
/// which makes the reconciliation robust against the non-lexical acquire/release that
/// context switches create.
pub struct KernelLock {
    /// `0` = free; otherwise `owner_core_aff0 + 1`. A CAS from `0` transfers ownership
    /// between cores; the owner writes `0` back to release.
    owner: AtomicU32,
}

impl Default for KernelLock {
    fn default() -> Self {
        Self::new()
    }
}

impl KernelLock {
    /// A free lock.
    pub const fn new() -> Self {
        Self {
            owner: AtomicU32::new(0),
        }
    }

    /// Ensure `core_id` (an MPIDR aff0) owns the lock, spinning until free if another
    /// core holds it. Idempotent: a no-op if this core already owns it. Must run with
    /// local IRQs masked (see the type contract).
    #[inline]
    pub fn acquire(&self, core_id: u32) {
        let me = core_id + 1;
        let mut spins: u32 = 0;
        let mut total_spins: u64 = 0;
        loop {
            // Re-check ownership EVERY iteration, not just once up front. The syscall
            // path calls this with IRQs enabled, so a timer IRQ can nest mid-spin and
            // *its* `enter_kernel` may win the lock for THIS core; the outer spin must
            // then observe `owner == me` and return, rather than retrying `CAS(0, me)`
            // forever (which fails once owner is a nonzero `me`). This is the
            // re-entrancy that produced the `owner=N waiter=N` self-deadlock.
            let cur = self.owner.load(Ordering::Acquire);
            if cur == me {
                return; // this core already owns it (possibly via a nested acquire)
            }
            if cur == 0
                && self
                    .owner
                    .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
            {
                // Accumulate this acquire's spin count once (a cross-core BKL-wait-time
                // proxy for measuring contention; see `contention_spins`). One atomic
                // add per acquire — the uncontended fast path adds nothing.
                if total_spins > 0 {
                    CONTENTION_SPINS.fetch_add(total_spins, Ordering::Relaxed);
                }
                return;
            }
            spins = spins.wrapping_add(1);
            total_spins = total_spins.wrapping_add(1);
            if spins == SPIN_WARN_THRESHOLD {
                spins = 0;
                log_kernel_lock_stuck(self.owner.load(Ordering::Relaxed), me);
            }
            core::hint::spin_loop();
        }
    }

    /// Ensure `core_id` does not own the lock, freeing it for a waiting core.
    /// Idempotent: a no-op if this core does not own it. Must run with local IRQs
    /// masked by the current owner.
    #[inline]
    pub fn release(&self, core_id: u32) {
        let me = core_id + 1;
        // Only the owner may free it; releasing what you don't hold is a no-op (the
        // reconciliation path can legitimately call this after a sibling core's
        // excursion already moved the lock).
        let _ = self
            .owner
            .compare_exchange(me, 0, Ordering::Release, Ordering::Relaxed);
    }

    /// Reconcile the lock to the EL this core is about to run in: acquire when
    /// returning to / staying in EL1, release when returning to EL0. This is the
    /// single operation the `eret` epilogues call, keeping the invariant "held iff in
    /// EL1" true across context switches that change the target EL.
    #[inline]
    pub fn reconcile(&self, core_id: u32, target_is_el0: bool) {
        if target_is_el0 {
            self.release(core_id);
        } else {
            self.acquire(core_id);
        }
    }

    /// `true` if `core_id` currently owns the lock.
    #[inline]
    pub fn held_by(&self, core_id: u32) -> bool {
        self.owner.load(Ordering::Relaxed) == core_id + 1
    }

    /// `true` if any core owns the lock.
    #[inline]
    pub fn is_held(&self) -> bool {
        self.owner.load(Ordering::Relaxed) != 0
    }
}

/// Diagnostic: log when the Big Kernel Lock is stuck spinning (a cross-core deadlock
/// canary). Stack-buffered to avoid heap use in an IRQ-masked context.
fn log_kernel_lock_stuck(owner: u32, me: u32) {
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
    let _ = writeln!(
        buf,
        "[BKL] stuck: owner={} waiter={} (core ids are aff0+1)",
        owner, me
    );
    if buf.1 > 0 {
        if let Ok(s) = core::str::from_utf8(&buf.0[..buf.1]) {
            (crate::runtime::runtime().print_str)(s);
        }
    }
}

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

    // --- KernelLock (Big Kernel Lock) ---

    #[test]
    fn kernel_lock_acquire_release_single_core() {
        let bkl = KernelLock::new();
        assert!(!bkl.is_held());
        assert!(!bkl.held_by(0));
        bkl.acquire(0);
        assert!(bkl.is_held());
        assert!(bkl.held_by(0));
        bkl.release(0);
        assert!(!bkl.is_held());
        assert!(!bkl.held_by(0));
    }

    #[test]
    fn kernel_lock_acquire_is_idempotent_for_owner() {
        let bkl = KernelLock::new();
        bkl.acquire(2);
        bkl.acquire(2); // nested (e.g. IRQ/fault while already in a syscall)
        bkl.acquire(2);
        assert!(bkl.held_by(2));
        // A single release frees it — there is one EL1→EL0 return per excursion.
        bkl.release(2);
        assert!(!bkl.is_held());
    }

    #[test]
    fn kernel_lock_release_by_non_owner_is_noop() {
        let bkl = KernelLock::new();
        bkl.acquire(1);
        bkl.release(0); // core 0 doesn't own it
        assert!(bkl.held_by(1), "non-owner release must not free the lock");
        bkl.release(1);
        assert!(!bkl.is_held());
    }

    #[test]
    fn kernel_lock_ownership_transfers_between_cores() {
        let bkl = KernelLock::new();
        bkl.acquire(0);
        assert!(bkl.held_by(0));
        assert!(!bkl.held_by(1));
        bkl.release(0);
        // Now a different core can take it.
        bkl.acquire(1);
        assert!(bkl.held_by(1));
        assert!(!bkl.held_by(0));
        bkl.release(1);
        assert!(!bkl.held_by(1));
    }

    #[test]
    fn kernel_lock_reconcile_matches_target_el() {
        let bkl = KernelLock::new();
        // Entering / staying in EL1 acquires.
        bkl.reconcile(0, /* target_is_el0 */ false);
        assert!(bkl.held_by(0));
        // Re-entering EL1 (nested) is idempotent.
        bkl.reconcile(0, false);
        assert!(bkl.held_by(0));
        // Returning to EL0 releases.
        bkl.reconcile(0, true);
        assert!(!bkl.is_held());
        // Returning to EL0 when already free is a no-op.
        bkl.reconcile(0, true);
        assert!(!bkl.is_held());
    }

    #[test]
    fn kernel_lock_held_by_only_owner() {
        let bkl = KernelLock::new();
        bkl.acquire(3);
        for other in [0u32, 1, 2, 4, 5] {
            assert!(!bkl.held_by(other));
        }
        assert!(bkl.held_by(3));
        bkl.release(3);
    }
}
