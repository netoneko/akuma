//! Process-lifecycle guard — defers **involuntary preemption** for the duration of a
//! lifecycle operation under shared-kernel SMP.
//!
//! ## The problem this solves
//!
//! Under shared-kernel SMP the Big Kernel Lock serializes EL1 *instants* across cores but
//! does **not** make multi-step process-lifecycle operations (`fork_process` /
//! `do_execve`+`replace_image` / `return_to_kernel` / `kill_process` / `spawn_*`) **atomic
//! across preemption**: the syscall/fault handler runs with IRQs enabled
//! (`src/exceptions.rs`, `msr daifclr, #2`), so a thread can be timer-preempted
//! mid-`fork`/`execve`/`exit`. On that preemption the BKL is reconciled away and other EL1
//! code (a peer core, or the next thread on this core — crucially including **non-lifecycle**
//! readers: the page-fault handler, signal delivery, a `for_each_process` sweep) observes
//! half-mutated state: a `Process` mid-`replace_image` with `mmap_regions` already
//! `.clear()`ed, a `THREAD_CONTEXTS[tid]` mid-spawn, a process-table slot mid-register, a
//! captured trap frame mid-repopulate. That was the SMP=4 fork-hammer corruption. Full
//! dossier + the empirical results: `docs/runbooks/debug-smp-fork-corruption.md`.
//!
//! ## Why `disable_preemption()` and not a lock or an IRQ mask
//!
//! Two earlier approaches failed empirically (real SMP=4 QEMU + fork-hammer):
//!
//! 1. **Reentrant cross-core spinlock held across preemption** (commit 66e09bf). The crash
//!    persisted (it only serialized lifecycle-vs-lifecycle, not lifecycle-vs-the
//!    non-lifecycle readers) and it inverted against the BKL (`[BKL] stuck` stalls).
//!
//! 2. **Whole-op IRQ mask (`DAIF.I`)**. This *eliminated the corruption* — 0 SIGSEGV
//!    across a full hammer run, proving these op boundaries are the right scope — but it
//!    **hard-deadlocked**: some ops cooperatively yield / wait on async block-I/O whose
//!    completion another thread must pump (the exec ELF read, spawn's file load, the
//!    child's first demand-paging fault). With IRQs masked that thread never runs.
//!
//! [`crate::threading::disable_preemption`] keeps exactly the property that killed the
//! corruption (no *involuntary* switch can expose the half-mutated state mid-op) while
//! avoiding both failure modes:
//!
//! - **IRQs stay enabled** — timer ticks, device IRQs and the scheduler wake-pass all
//!   still run; block I/O completes. Only the context *switch* is deferred
//!   (`ThreadPool::schedule_indices` returns `None` for involuntary entries).
//! - **Voluntary switches still work** — an op that yields or blocks (`yield_now`,
//!   `blocking_relax`, a file read) switches away normally. While it waits, the op is
//!   not mid-mutation of shared state (the yield points in these ops all sit outside
//!   the destructive windows), so this is safe — and it is what makes the guard
//!   deadlock-free by construction.
//! - **Per-thread counter** — the disable rides the acquiring tid only. It cannot leak
//!   into the freshly-published child thread (new tid, count 0), unlike a masked `SPSR`.
//! - **Watchdog-monitored** — `check_preemption_watchdog` flags a guard held > 100 ms,
//!   so a mis-scoped window is loud, not silent.
//!
//! With the guard held, this core cannot be involuntarily switched away mid-op, and the
//! BKL (held for the whole EL1 excursion, never dropped inside the guarded windows'
//! non-yielding sections) keeps every other core out of EL1 — so no EL1 reader anywhere
//! can observe the half-built state.
//!
//! ## No-return callers
//!
//! `return_to_kernel` / `return_to_kernel_from_fault` never return, so RAII would leak
//! the disable count on their tid forever (and the slot-recycle reset would be the only
//! recovery). They call [`LifecycleGuard::release`] explicitly before parking — keep it
//! that way when touching teardown. As defense-in-depth, thread-slot recycling resets
//! the per-tid preemption counter (see `threading::cleanup_terminated_threads`).
//!
//! On non-`kernel_smp_shared` builds this is a no-op and compiles to nothing.

/// RAII guard that defers involuntary preemption of the current thread for the span of a
/// process-lifecycle operation under `cfg(kernel_smp_shared)`. No-op on all other builds.
///
/// See the module docs for why this is a per-thread preemption disable and not a lock or
/// IRQ mask. Ops that never return must call [`release`](Self::release) explicitly.
pub struct LifecycleGuard {
    /// `!Send`: the disable/enable pair must run on the same thread — the counter is
    /// indexed by the current tid at each call.
    _no_send: core::marker::PhantomData<*mut ()>,
}

impl LifecycleGuard {
    /// Acquire the lifecycle guard: defer involuntary preemption of this thread until
    /// the guard drops. Nests (per-thread counter).
    ///
    /// `#[track_caller]` so the preemption watchdog's culprit line names the lifecycle
    /// *op* that acquired the guard, not this wrapper.
    #[inline]
    #[track_caller]
    pub fn acquire() -> Self {
        #[cfg(kernel_smp_shared)]
        crate::threading::disable_preemption();
        Self {
            _no_send: core::marker::PhantomData,
        }
    }

    /// Release eagerly (equivalent to `drop(guard)`, named for symmetry). Required in
    /// functions that never return (`return_to_kernel*`), where RAII drop never runs.
    #[inline]
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for LifecycleGuard {
    #[inline]
    fn drop(&mut self) {
        #[cfg(kernel_smp_shared)]
        crate::threading::enable_preemption();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_release() {
        let g = LifecycleGuard::acquire();
        g.release();
    }

    #[test]
    fn nested_guards() {
        let _outer = LifecycleGuard::acquire();
        {
            let _inner = LifecycleGuard::acquire();
        }
    }
}
