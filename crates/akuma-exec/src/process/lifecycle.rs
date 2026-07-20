//! Process-lifecycle guard — currently a **no-op**, kept as the anchor for the eventual
//! narrow preemption-disable fix.
//!
//! ## The problem this exists for
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
//! captured trap frame mid-repopulate. That is the SMP=4 fork-hammer corruption. Full
//! dossier + the empirical results below: `docs/runbooks/debug-smp-fork-corruption.md`.
//!
//! ## Two approaches tried and rejected (both empirically, on a real SMP=4 QEMU run)
//!
//! 1. **Reentrant cross-core spinlock held across preemption** (commit 66e09bf). The crash
//!    *persisted* (it only serialized lifecycle-vs-lifecycle, not lifecycle-vs-the
//!    non-lifecycle readers that also touch the half-built state), and it **inverted
//!    against the BKL**: a preempted holder was switched out still owning the lock while a
//!    peer that had entered EL1 (holding the BKL) spun on it — the holder needs the BKL to
//!    be rescheduled and finish, so it stalled until the spinner was itself timer-preempted.
//!    Symptom: `[BKL] stuck` + `[WATCHDOG] Preemption disabled`.
//!
//! 2. **Whole-op per-core preemption disable (mask `DAIF.I` for the entire op).** This
//!    *eliminated the corruption* (0 SIGSEGV across a full hammer run — proving the fault
//!    class really is preemption-mid-op exposure), but it **hard-deadlocked**: these ops
//!    (and the child's first-run demand-paging that reads the ELF page from ext2) can
//!    cooperatively yield / wait on async block-I/O completion that a *different* thread
//!    must pump. With preemption masked that thread can't run, the I/O never completes, and
//!    the BKL holder never releases → all cores wedge on the BKL. It wedged exactly at the
//!    freshly-exec'd child's first `[IA-DP]` code-page fault.
//!
//! ## The validated fix direction (TODO — not yet implemented)
//!
//! The mechanism is right, the *scope* was wrong. Preemption must be disabled only around
//! the **synchronous, non-yielding, non-blocking memory-mutation windows** — never across a
//! lock-wait, a cooperative yield, block I/O, or an `eret` to userspace. Concretely:
//!
//! - `replace_image`: guard just the `mmap_regions.clear()` + `lazy_regions.clear()` + AS
//!   swap + repopulate window (the destructive middle), released before the process-info
//!   page allocation and before returning.
//! - `fork_process`: guard just the child-publish window (write `Process.context`, register
//!   in the table, mark the thread schedulable) so a peer never sees the child half-built.
//! - The `THREAD_CONTEXTS[tid]` writes and the trap-frame capture likewise need their own
//!   narrow guards.
//!
//! Confirming the exact non-yielding boundaries wants an lldb watchpoint on the victim
//! `Process.parent_pid` / `THREAD_CONTEXTS[tid].pc` (see the runbook) so we place each guard
//! tightly. Until then this guard is inert so the tree is free of both the spinlock's
//! BKL-stall regression and the whole-op version's deadlock. The 11 `LifecycleGuard::acquire()`
//! call sites are retained as no-ops: they mark where the narrow guards belong.
//!
//! On non-`kernel_smp_shared` builds this was always a no-op; it now is on every build.

/// RAII guard reserved for the narrow process-lifecycle preemption-disable fix. Currently a
/// **no-op on every build** — see the module docs for why the cross-core-spinlock and
/// whole-op-IRQ-mask approaches were both rejected, and what the eventual narrow fix must do.
///
/// The `acquire()`/`release()` API and the 11 call sites are kept so the fix can be dropped
/// in without re-touching every lifecycle op.
pub struct LifecycleGuard {
    /// `!Send`: a future implementation will restore per-core IRQ state on the same core it
    /// masked it on, so the guard must not cross a thread/core boundary. Enforced now so the
    /// call sites are already correct when the guard gains behavior.
    _no_send: core::marker::PhantomData<*mut ()>,
}

impl LifecycleGuard {
    /// Acquire the (currently inert) lifecycle guard.
    #[inline]
    pub fn acquire() -> Self {
        Self {
            _no_send: core::marker::PhantomData,
        }
    }

    /// Release eagerly (equivalent to `drop(guard)`, named for symmetry).
    #[inline]
    pub fn release(self) {
        drop(self);
    }
}

impl Drop for LifecycleGuard {
    #[inline]
    fn drop(&mut self) {
        // No-op. See module docs.
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
