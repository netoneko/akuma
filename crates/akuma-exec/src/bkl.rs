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
use core::sync::atomic::{AtomicU32, Ordering};

/// The one Big Kernel Lock. Only meaningful under `cfg(kernel_smp_shared)`.
#[cfg(kernel_smp_shared)]
static KERNEL_LOCK: KernelLock = KernelLock::new();

// --- Deliberately-dropped BKL windows -------------------------------------------------
//
// The BKL-carve-out guards (`VfsBklGuard`, `NetBklGuard`, the execve ELF-read drop, the
// file-fault fill drop) run a bounded stretch of EL1 code with the BKL deliberately
// RELEASED so peer cores can enter the kernel. That violates the reconcile invariant
// "BKL held iff EL1" on purpose — and before this ledger existed, the violation did not
// survive the first interrupt: a timer IRQ landing inside the window would
// `enter_kernel` for the handler and the eret epilogue's `reconcile_for_spsr`, seeing an
// EL1 target frame, would KEEP the lock. From that instant the remainder of the window —
// potentially most of a slow ext2 syscall (~20 ms for a 64 KiB write at the measured
// ~2.5 MB/s) — ran with the BKL silently re-held, serializing the peer for tens of
// milliseconds. That is precisely the `[BKL] stuck` (10M-spin, owner genuinely held)
// regression the `no-bkl-vfs` A/B I/O regimen caught (docs/archive/BKL_VFS_CARVE_OUT.md
// §8): the fair FIFO ticket lock cannot produce such a wait without a genuine long hold.
//
// The ledger records, PER THREAD, how many dropped-BKL windows are currently open, so
// every eret that resumes EL1 code can restore the state that code actually wants:
// dropped. It is thread-scoped (not core-scoped) because a window survives preemption,
// blocking waits, and migration — the thread may resume on another core.

/// Per-thread count of open deliberately-dropped-BKL windows.
///
/// Pure atomics with no target dependencies so the nesting/reset contract is
/// host-testable; the kernel uses the [`DROPPED_WINDOWS`] instance via the
/// `dropped_window_*` free functions below. Out-of-range `tid`s are ignored (reads
/// return "no window") so callers never need a bounds check.
pub struct DroppedWindowLedger<const N: usize> {
    depth: [AtomicU32; N],
}

impl<const N: usize> DroppedWindowLedger<N> {
    /// All threads start with no open window.
    pub const fn new() -> Self {
        Self {
            depth: [const { AtomicU32::new(0) }; N],
        }
    }

    /// Open a window for `tid` (nesting: increments the depth).
    pub fn open(&self, tid: usize) {
        if let Some(d) = self.depth.get(tid) {
            d.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Close the innermost window for `tid`. Returns `true` when this closed the
    /// OUTERMOST window — i.e. the caller should re-acquire the BKL. A nested close
    /// returns `false` so the outer window stays BKL-free.
    pub fn close(&self, tid: usize) -> bool {
        match self.depth.get(tid) {
            Some(d) => {
                // Saturating at 0: an unbalanced close is a caller bug, but wrapping to
                // u32::MAX would make the thread permanently BKL-free — far worse.
                let prev = d.load(Ordering::Relaxed);
                if prev == 0 {
                    return true;
                }
                d.store(prev - 1, Ordering::Relaxed);
                prev == 1
            }
            None => true,
        }
    }

    /// `true` if `tid` has at least one open window (its EL1 code runs BKL-free).
    pub fn is_open(&self, tid: usize) -> bool {
        self.depth
            .get(tid)
            .is_some_and(|d| d.load(Ordering::Relaxed) != 0)
    }

    /// Force-clear `tid`'s windows, returning the prior depth. For abnormal unwinds
    /// (fault-kill paths) that skip guard destructors, and for recycled thread slots.
    pub fn reset(&self, tid: usize) -> u32 {
        self.depth
            .get(tid)
            .map_or(0, |d| d.swap(0, Ordering::Relaxed))
    }
}

impl<const N: usize> Default for DroppedWindowLedger<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The kernel's ledger, indexed by thread id.
///
/// Depth updates need no cross-core atomicity guarantees beyond the store itself: a
/// thread only opens/closes its OWN entry, and it cannot run on two cores at once. The
/// IRQ epilogue on the resuming core reads the entry after the scheduler's `commit_switch`
/// published the thread, so it observes the depth the thread had when it was switched out.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
static DROPPED_WINDOWS: DroppedWindowLedger<{ crate::threading::MAX_THREADS }> =
    DroppedWindowLedger::new();

/// How many times an eret epilogue preserved a dropped window that the pre-ledger code
/// would have converted into a BKL-held run. Diagnostic for the `[BKL] stuck` regression;
/// sampled with decaying frequency into the log by `reconcile_for_spsr`.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
static WINDOWS_PRESERVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Open a deliberately-dropped-BKL window for the current thread and release the BKL.
///
/// Order matters: the depth is published BEFORE the release, so an IRQ that lands in
/// between sees the window and leaves the lock released on its way out (our own
/// `leave_kernel` then idempotently no-ops).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn dropped_window_open() {
    DROPPED_WINDOWS.open(crate::threading::current_thread_id());
    leave_kernel();
}

/// Close the current thread's innermost dropped-BKL window, re-acquiring the BKL when it
/// was the outermost one. A nested close leaves the lock released for the outer window.
///
/// Order matters: the depth is decremented BEFORE the re-acquire, so an IRQ landing in
/// between reconciles to "held" (target EL1, no window) and our `enter_kernel` is the
/// owner-reentrant no-op.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn dropped_window_close() {
    if DROPPED_WINDOWS.close(crate::threading::current_thread_id()) {
        enter_kernel();
    }
}

/// Close the current thread's innermost dropped-BKL window WITHOUT re-acquiring the
/// BKL when it was the outermost one.
///
/// The exit half of the per-syscall BKL opt-out (Phase 7f,
/// docs/archive/BKL_FINE_GRAINED_LOCKING_PLAN.md §7.3): an opted-out syscall's whole
/// EL0→EL1 excursion is one open dropped window that was opened WITHOUT a prior
/// `enter_kernel` (the entry handler skipped the acquire), so its close must not take
/// the lock either — the excursion ends by `eret`ing to EL0, where the BKL is released
/// anyway. Everything in between (IRQ epilogues, preemption, migration, nested
/// carve-out guards at depth ≥ 2) sees a perfectly ordinary dropped window, which is
/// what keeps the mixed converted/unconverted state safe. Pair strictly with a
/// `dropped_window_open()` made on the never-acquired entry path; every other dropper
/// keeps using [`dropped_window_close`].
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn dropped_window_close_no_reacquire() {
    let _ = DROPPED_WINDOWS.close(crate::threading::current_thread_id());
}

/// `true` if the current thread is inside a deliberately-dropped-BKL window, i.e. its
/// EL1 code must be resumed with the BKL RELEASED.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn in_dropped_window() -> bool {
    DROPPED_WINDOWS.is_open(crate::threading::current_thread_id())
}

/// RAII pause of the current thread's dropped-BKL window(s): construction closes every
/// open window and takes the BKL (via [`reset_dropped_windows`]); drop reopens the same
/// number of windows, releasing the lock again.
///
/// For the few cold paths a BKL-opted-out syscall (Phase 7f) shares with everything
/// else that still genuinely need BKL-held execution — plain `Process`-field writes
/// (cross-core exclusion for those is still the BKL, see locking.md's load-bearing
/// table) and the phantom-SVC/QEMU-misroute fallout in the trap prologue. The depth is
/// LATCHED at construction (the guard-latching rule): drop restores exactly what it
/// found, regardless of runtime-toggle changes in between. On a thread with no open
/// window (every non-converted path) both halves are no-ops, and off `smp-shared` the
/// whole type compiles to nothing.
pub struct DroppedWindowPause {
    reopen_depth: u32,
}

impl DroppedWindowPause {
    #[inline]
    pub fn new() -> Self {
        Self {
            reopen_depth: reset_dropped_windows(),
        }
    }
}

impl Default for DroppedWindowPause {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DroppedWindowPause {
    #[inline]
    fn drop(&mut self) {
        for _ in 0..self.reopen_depth {
            dropped_window_open();
        }
    }
}

/// Force-clear the current thread's dropped-BKL windows and restore the "EL1 holds the
/// BKL" invariant. Returns the prior depth (nonzero means windows were actually leaked).
///
/// For paths that bypass the guards' destructors: the EL1 fault-kill teardown
/// (`return_to_kernel_from_fault`) and the syscall-entry tripwire in
/// `rust_sync_el0_handler` (a recycled thread slot must never inherit a stale window —
/// its EL1 excursions would silently run BKL-free).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
pub fn reset_dropped_windows() -> u32 {
    let prior = DROPPED_WINDOWS.reset(crate::threading::current_thread_id());
    if prior != 0 {
        // The thread believed it was BKL-free; the invariant restore needs a real hold.
        enter_kernel();
    }
    prior
}

/// Clear a **foreign, dead** thread slot's dropped-BKL window depth, returning the prior
/// depth. Ledger-only: unlike [`reset_dropped_windows`] it performs no lock operation,
/// because there is no invariant to restore — the thread is TERMINATED and will never
/// resume, so nobody is waiting to be handed a BKL-held execution.
///
/// Called from the thread-slot recycler (`threading::reclaim_terminated_slots`) just
/// before a TERMINATED slot goes FREE. The ledger is indexed by thread id, so without
/// this the *next* occupant of the slot inherits the dead thread's depth and its EL1
/// excursions silently run BKL-free until the syscall-entry tripwire heals them. That
/// heal is a self-repair, not a design: it fires only at the next EL0 entry, and only
/// [`reset_dropped_windows`]'s counter records that it happened.
///
/// A thread can die mid-window whenever a converted syscall parks: the kill path marks
/// it TERMINATED while it sits in `schedule_blocking`, so its excursion never reaches
/// the close. Phase 7f tranche 2b found this the moment `nanosleep` — the first
/// converted syscall that parks a thread for a long time — was listed.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
pub fn clear_dropped_windows_for_dead_thread(tid: usize) -> u32 {
    DROPPED_WINDOWS.reset(tid)
}

/// Open a window on a FOREIGN tid's ledger entry, without touching the BKL. Exists
/// only so `test_syscall_bkl_optout` can stage the "thread died mid-window" shape that
/// [`clear_dropped_windows_for_dead_thread`] exists to clean up, without actually
/// killing a parked thread. Never call this outside tests.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
pub fn dropped_window_open_for_tid_test(tid: usize) {
    DROPPED_WINDOWS.open(tid);
}

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
/// will be restored: `SPSR.M[3:0] == 0` means EL0 (release), otherwise EL1 (acquire) —
/// UNLESS the thread being resumed is inside a deliberately-dropped-BKL window
/// (see [`DroppedWindowLedger`]), in which case an EL1 target is restored to its chosen
/// state: released. Without that exception, the first timer tick inside a dropped
/// window converts the window's remainder into a BKL-held run — the `[BKL] stuck`
/// regression (docs/archive/BKL_VFS_CARVE_OUT.md §8). No-op unless
/// `cfg(kernel_smp_shared)`.
///
/// The current-thread read is authoritative here: both callers are IRQ epilogues that
/// run after the scheduler's `commit_switch` published the incoming thread, so the
/// window check applies to the thread the eret actually resumes.
///
/// Callers are IRQ epilogues (already masked), but mask anyway so the core-id read
/// stays migration-atomic with the lock op no matter the calling context (see
/// [`enter_kernel`]).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn reconcile_for_spsr(spsr: u64) {
    let target_is_el0 = (spsr & 0xf) == 0;
    let release = target_is_el0 || note_preserved_window();
    let daif = crate::sync::irq_save_mask();
    KERNEL_LOCK.reconcile(current_core_id(), release);
    crate::sync::irq_restore(daif);
}

/// Window check for the eret epilogues, with a decaying-frequency diagnostic: counts —
/// and occasionally logs — each time an epilogue PRESERVES a dropped window that the
/// pre-ledger reconcile would have silently converted to a BKL-held run. Log volume is
/// power-of-two sampled (x1, x2, x4, …), so a whole bulk-I/O run costs ~log2(N) lines.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
fn note_preserved_window() -> bool {
    if !in_dropped_window() {
        return false;
    }
    let n = WINDOWS_PRESERVED.fetch_add(1, Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        use core::fmt::Write;
        struct Buf([u8; 96], usize);
        impl Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let b = s.as_bytes();
                let l = b.len().min(96 - self.1);
                self.0[self.1..self.1 + l].copy_from_slice(&b[..l]);
                self.1 += l;
                Ok(())
            }
        }
        let mut buf = Buf([0u8; 96], 0);
        let _ = writeln!(buf, "[BKL] dropped window preserved across IRQ x{n}");
        if let Ok(s) = core::str::from_utf8(&buf.0[..buf.1]) {
            (crate::runtime::runtime().print_str)(s);
        }
    }
    true
}

/// Total eret epilogues that preserved a dropped-BKL window (see [`note_preserved_window`]).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
pub fn dropped_windows_preserved() -> u64 {
    WINDOWS_PRESERVED.load(Ordering::Relaxed)
}

/// Ticket-free variant of reconcile_for_spsr for use after BKL-free scheduler paths.
/// When we run the scheduler BKL-free (M5c step-2), we never called `enter_kernel`,
/// so a reconcile that targets EL1 must acquire without taking a ticket — otherwise
/// we leak a ticket. This variant uses `KernelLock::reconcile_no_ticket`.
///
/// Consults the dropped-window ledger exactly like [`reconcile_for_spsr`]: resuming an
/// EL1 thread inside a dropped window must not (re-)acquire on its behalf.
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
pub fn reconcile_for_spsr_no_ticket(spsr: u64) {
    let target_is_el0 = (spsr & 0xf) == 0;
    let release = target_is_el0 || note_preserved_window();
    let daif = crate::sync::irq_save_mask();
    KERNEL_LOCK.reconcile_no_ticket(current_core_id(), release);
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
pub fn reconcile_for_spsr_no_ticket(_spsr: u64) {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn held_by_current() -> bool {
    false
}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn dropped_window_open() {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn dropped_window_close() {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn dropped_window_close_no_reacquire() {}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn in_dropped_window() -> bool {
    false
}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn reset_dropped_windows() -> u32 {
    0
}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn clear_dropped_windows_for_dead_thread(_tid: usize) -> u32 {
    0
}

#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
pub fn dropped_windows_preserved() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::DroppedWindowLedger;

    /// The nesting contract the eret epilogues and guards rely on: only the OUTERMOST
    /// close asks for a re-acquire; while any window is open the thread reads as
    /// BKL-free; depths are per-thread.
    #[test]
    fn dropped_window_ledger_nesting_and_isolation() {
        let ledger: DroppedWindowLedger<4> = DroppedWindowLedger::new();
        assert!(!ledger.is_open(1));

        ledger.open(1); // outer window (e.g. VfsBklGuard)
        assert!(ledger.is_open(1));
        assert!(!ledger.is_open(2), "windows must be per-thread");

        ledger.open(1); // nested window (e.g. a file-fault fill inside the syscall)
        assert!(ledger.is_open(1));
        assert!(
            !ledger.close(1),
            "closing a NESTED window must not re-acquire — the outer window is still open"
        );
        assert!(ledger.is_open(1));
        assert!(ledger.close(1), "closing the outermost window re-acquires");
        assert!(!ledger.is_open(1));
    }

    /// An unbalanced close saturates at zero instead of wrapping — wrapping would make
    /// the thread permanently BKL-free, which is catastrophic; saturating just makes the
    /// extra close a plain re-acquire.
    #[test]
    fn dropped_window_ledger_unbalanced_close_saturates() {
        let ledger: DroppedWindowLedger<2> = DroppedWindowLedger::new();
        assert!(ledger.close(0), "close with no open window still re-acquires");
        assert!(!ledger.is_open(0), "depth must not wrap to u32::MAX");
    }

    /// Abnormal unwinds (fault-kill) skip guard destructors; `reset` must clear any
    /// depth and report it, and out-of-range tids must be inert.
    #[test]
    fn dropped_window_ledger_reset_and_bounds() {
        let ledger: DroppedWindowLedger<2> = DroppedWindowLedger::new();
        ledger.open(0);
        ledger.open(0);
        assert_eq!(ledger.reset(0), 2);
        assert!(!ledger.is_open(0));
        assert_eq!(ledger.reset(0), 0);

        // Out-of-range tid: no panic, reads as closed, close() falls back to re-acquire.
        ledger.open(99);
        assert!(!ledger.is_open(99));
        assert!(ledger.close(99));
        assert_eq!(ledger.reset(99), 0);
    }
}
