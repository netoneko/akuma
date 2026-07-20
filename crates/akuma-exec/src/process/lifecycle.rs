//! Process-lifecycle serialization lock.
//!
//! Under shared-kernel SMP, the Big Kernel Lock serializes EL1 *instants* across cores
//! but does **not** make multi-step process-lifecycle operations
//! (`fork_process` / `do_execve`+`replace_image` / `return_to_kernel` / `kill_process`)
//! **atomic across preemption**: a syscall running with IRQs enabled can be timer-preempted
//! mid-`fork`/`execve`/`exit`, the scheduler can switch to another EL1 thread on the same
//! core (or a peer core can enter EL1 once this core reconciles to an EL0 target), and
//! that other EL1 code then observes half-mutated globals — a `Process` mid-`replace_image`
//! with `mmap_regions` already `.clear()`ed, a `THREAD_CONTEXTS[tid]` mid-spawn, a
//! process-table slot mid-`register`/`unregister`. The signatures of the SMP=4 fork-hammer
//! crash (heterogeneous userspace SIGSEGVs, `parent_pid` fields clobbered to 0 on running
//! services, user PC slots holding kernel text addresses) all match this class of bug — see
//! `docs/runbooks/debug-smp-fork-corruption.md`.
//!
//! This module closes that hole by introducing a dedicated **reentrant spinlock** that is
//! held for the entire critical section of every lifecycle op. Because it is distinct from
//! the BKL, it is **not** reconciled (dropped) at EL transitions — the holder keeps it
//! across preemption until the op completes and the RAII guard drops. Contended acquirers
//! spin with IRQs **enabled**, so they in turn can be preempted and the holder always gets
//! rescheduled to finish.
//!
//! ## Reentrancy
//!
//! Lifecycle ops nest: `return_to_kernel` may call `kill_box`, which calls `kill_process`,
//! which acquires this lock. To support that, the lock tracks `(owner_tid, depth)` and
//! reentrant acquires by the same thread simply bump `depth`.
//!
//! ## Zero-cost when off
//!
//! On builds without `cfg(kernel_smp_shared)` (single-core, `size`, `extreme`,
//! `multikernel`) the lock compiles to a no-op: `acquire()` / `release()` / the
//! `LifecycleGuard` are all empty inlines, so the existing single-CPU `with_irqs_disabled`
//! invariant suffices and there is no new lock cost.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Sentinel value stored in [`LifecycleLock::owner`] when no thread holds the lock.
const FREE: usize = usize::MAX;

/// Reentrant spinlock serializing process-lifecycle operations across preemption under
/// shared-kernel SMP. See the module docs for the rationale.
pub struct LifecycleLock {
    /// `FREE` (=`usize::MAX`) when uncontended; otherwise the owning thread's id
    /// (`TPIDRRO_EL0`). Read with `Acquire`; written with `Release` only when publishing
    /// "free" — the depth-balanced hand-off is single-writer (the owner).
    owner: AtomicUsize,
    /// Reentrant depth. Read/written only by the owner thread, so `Relaxed` is sufficient
    /// (the `owner` Release store on last release is what publishes the state change).
    depth: AtomicU32,
}

impl LifecycleLock {
    pub const fn new() -> Self {
        Self {
            owner: AtomicUsize::new(FREE),
            depth: AtomicU32::new(0),
        }
    }

    /// Acquire the lock, blocking (with IRQs enabled) until free or already owned by this
    /// thread (reentrant). Cheap on the uncontended path: one `Acquire` load + one
    /// `compare_exchange`.
    #[inline]
    pub fn acquire(&self) {
        let tid = crate::threading::current_thread_id();
        // Uncontended fast path: claim it.
        if self
            .owner
            .compare_exchange(FREE, tid, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            self.depth.store(1, Ordering::Relaxed);
            return;
        }
        // Reentrant fast path: this thread already owns it.
        if self.owner.load(Ordering::Acquire) == tid {
            self.depth.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // Contended: spin with IRQs enabled. `current_thread_id()` is per-CPU
        // (`TPIDRRO_EL0`) so it stays correct across preemption of this spinner; the timer
        // can preempt us so the holder gets rescheduled to finish and release.
        let mut spins: u32 = 0;
        loop {
            let cur = self.owner.load(Ordering::Acquire);
            if cur == tid {
                // Owner was preempted, switched back in, and re-entered acquire on the
                // same slot before its earlier depth-balanced release: reentrant.
                self.depth.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if cur == FREE {
                if self
                    .owner
                    .compare_exchange_weak(FREE, tid, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    self.depth.store(1, Ordering::Relaxed);
                    return;
                }
                // Lost the CAS to another acquirer; retry.
            }
            spins = spins.wrapping_add(1);
            // Periodic `core::hint::spin_loop` keeps the spin power-friendly without
            // letting LLVM hoist the atomic load out of the loop.
            if spins & 0x3f == 0 {
                core::hint::spin_loop();
            }
        }
    }

    /// Release one level of reentrant depth. The owner-only publication of "free" happens
    /// on the last (depth-1→0) release. Calling this without a matching `acquire` is a
    /// programming error (debug-asserted).
    #[inline]
    pub fn release(&self) {
        let tid = crate::threading::current_thread_id();
        debug_assert_eq!(
            self.owner.load(Ordering::Relaxed),
            tid,
            "LifecycleLock released by non-owner"
        );
        let prev_depth = self.depth.fetch_sub(1, Ordering::Relaxed);
        if prev_depth == 1 {
            // Last release: publish "free" with Release so a subsequent acquirer's
            // Acquire load sees the lock in a consistent state.
            self.owner.store(FREE, Ordering::Release);
        }
    }

    /// `true` iff the calling thread currently holds this lock (any depth). For
    /// assertions / diagnostics.
    #[inline]
    pub fn held_by_current(&self) -> bool {
        self.owner.load(Ordering::Relaxed) == crate::threading::current_thread_id()
    }
}

/// The one process-lifecycle lock. Only meaningful under `cfg(kernel_smp_shared)`.
#[cfg(kernel_smp_shared)]
pub static PROCESS_LIFECYCLE_LOCK: LifecycleLock = LifecycleLock::new();

/// RAII guard: acquires on construction, releases on drop. Compiles to nothing on
/// non-`kernel_smp_shared` builds so callers can use it unconditionally.
pub struct LifecycleGuard {
    /// `!Send` — the lock is per-thread-reentrant; the guard must drop on the same thread.
    _no_send: core::marker::PhantomData<*mut ()>,
}

impl LifecycleGuard {
    /// Acquire the global lifecycle lock and return a guard that releases on drop.
    #[inline]
    pub fn acquire() -> Self {
        #[cfg(kernel_smp_shared)]
        PROCESS_LIFECYCLE_LOCK.acquire();
        Self {
            _no_send: core::marker::PhantomData,
        }
    }

    /// Release the lock eagerly (equivalent to `drop(guard)`, named for symmetry).
    #[inline]
    pub fn release(self) {
        // Drop impl does the work; this just consumes the guard.
        drop(self);
    }
}

impl Drop for LifecycleGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(kernel_smp_shared)]
        PROCESS_LIFECYCLE_LOCK.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_thread_acquire_release() {
        let lock = LifecycleLock::new();
        lock.acquire();
        lock.release();
    }

    #[test]
    fn reentrant_acquire_balanced_release() {
        // Simulate same-thread reentrancy by hand-crafting the depth/owner fields.
        // We model "current_thread_id" by using 0 (host tests return 0).
        let lock = LifecycleLock::new();
        // First acquire: owner FREE → 0, depth 1.
        lock.acquire();
        // Reentrant: depth 2.
        lock.acquire();
        // First release: depth 1 (still owned).
        lock.release();
        // Second release: depth 0 → FREE.
        lock.release();
        // A fresh acquire must succeed (FREE was published).
        lock.acquire();
        lock.release();
    }

    #[test]
    fn guard_drops_on_early_return() {
        // Mimic the `let _g = guard; ... return;` pattern: the guard's Drop runs and
        // releases the lock, so a second acquire succeeds without deadlock.
        {
            let _g = LifecycleGuard::acquire();
            // early scope end
        }
        // Lock should be free now.
        let _g2 = LifecycleGuard::acquire();
        // depth-balanced: dropping _g2 releases.
    }
}
