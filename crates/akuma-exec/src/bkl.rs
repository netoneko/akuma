//! The global Big Kernel Lock (BKL) for real (shared-kernel) SMP.
//!
//! Wraps the single process-wide [`KernelLock`] plus the core-identity helper, and
//! exposes the acquire/release/reconcile entry points the kernel's exception path
//! calls. See [`crate::sync::KernelLock`] for the lock semantics and
//! docs/reference/subsystems/smp-shared.md for the design.
//!
//! **Zero-cost when off:** every function here compiles to an empty inline body
//! unless `cfg(kernel_smp_shared)` is set (the `smp-shared` feature, forwarded from
//! the bin crate). The default, `size`, `extreme`, and multikernel (`smp`) builds are
//! byte-for-byte unaffected.
//!
//! **M1 wiring:** the syscall path (`rust_sync_el0_handler` entry/exit) and the idle
//! loop take/drop the BKL, establishing "held while a core executes kernel code" on
//! the hot path. The IRQ/scheduler context-switch reconciliation (releasing to an
//! incoming EL0 thread, re-acquiring for an incoming EL1 thread) lands in M2 alongside
//! the cross-core scheduler, where it can be exercised under real contention.

#[cfg(kernel_smp_shared)]
use crate::sync::KernelLock;

/// The one Big Kernel Lock. Only meaningful under `cfg(kernel_smp_shared)`.
#[cfg(kernel_smp_shared)]
static KERNEL_LOCK: KernelLock = KernelLock::new();

/// This core's identity (MPIDR aff0). Matches the `mpidr & 0xff` indexing used by the
/// SMP bringup path and `trigger_sgi_core`. Always `0` on non-SMP builds and on host
/// tests, so callers (e.g. the scheduler's per-core idle) can use it unconditionally.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn current_core_id() -> u32 {
    let mpidr: u64;
    // SAFETY: reading the affinity register has no side effects.
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack)) };
    (mpidr & 0xff) as u32
}

/// Non-SMP / host shim: a single-core build is always core 0.
#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn current_core_id() -> u32 {
    0
}

/// Acquire the BKL for this core — call on entering kernel code from EL0. Spins if
/// another core holds it; idempotent if this core already does. No-op unless
/// `cfg(kernel_smp_shared)`.
///
/// IRQs are masked around the `current_core_id()` read AND the lock operation. The
/// syscall path calls this with IRQs enabled, so without the mask a preemption between
/// reading MPIDR and acting on the lock can MIGRATE this thread to another core — the
/// lock op then runs with a stale core identity. That breaks both directions: a stale
/// `me` skips the reentrant fast path (the thread takes a ticket and spins IRQ-masked
/// on a core whose own hold it can never see — the SMP=4 hard wedge with `owner`
/// frozen nonzero, all cores parked in `acquire`, 0 RECOVERED; lldb-confirmed
/// 2026-07-22: a spinner on CPU3 with `me=2` in its registers), and a stale-`me`
/// `release` CAS silently no-ops (the long-standing ticket-leak family — `owner`/
/// `now_serving` drift the self-heal papers over).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn enter_kernel() {
    let daif = crate::sync::irq_save_mask();
    KERNEL_LOCK.acquire(current_core_id());
    crate::sync::irq_restore(daif);
}

/// Release the BKL for this core — call on returning to EL0. Idempotent if this core
/// does not hold it. No-op unless `cfg(kernel_smp_shared)`.
///
/// Masked for the same migration-atomicity reason as [`enter_kernel`]: releasing with
/// a stale core id is a silent CAS no-op that leaks the hold.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn leave_kernel() {
    let daif = crate::sync::irq_save_mask();
    KERNEL_LOCK.release(current_core_id());
    crate::sync::irq_restore(daif);
}

/// Reconcile the BKL to the EL this core is about to `eret` into, given the SPSR that
/// will be restored: `SPSR.M[3:0] == 0` means EL0 (release), otherwise EL1 (acquire).
/// This is the operation the context-switch path uses in M2. No-op unless
/// `cfg(kernel_smp_shared)`.
///
/// Callers are IRQ epilogues (already masked), but mask anyway so the core-id read
/// stays migration-atomic with the lock op no matter the calling context (see
/// [`enter_kernel`]).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn reconcile_for_spsr(spsr: u64) {
    let target_is_el0 = (spsr & 0xf) == 0;
    let daif = crate::sync::irq_save_mask();
    KERNEL_LOCK.reconcile(current_core_id(), target_is_el0);
    crate::sync::irq_restore(daif);
}

/// `true` if this core currently holds the BKL. For assertions / diagnostics.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn held_by_current() -> bool {
    KERNEL_LOCK.held_by(current_core_id())
}

// ---- No-op shims: everything above collapses to nothing unless smp-shared is on and
// we're building for the bare-metal target. Keeps call sites unconditional. ----

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn enter_kernel() {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn leave_kernel() {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn reconcile_for_spsr(_spsr: u64) {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn held_by_current() -> bool {
    false
}
