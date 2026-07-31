//! `no-bkl-process` — the Phase 3 BKL carve-out for `fork_process`'s page-copy window.
//!
//! Third carve-out after `no-bkl-network` and `no-bkl-vfs`; see
//! `docs/reference/subsystems/locking.md` for the playbook and
//! `docs/archive/BKL_PROCESS_CARVE_OUT.md` §9 for this one's design record.
//!
//! ## Why fork's page copy is carvable when the audit said it wasn't
//!
//! The original audit treated `fork_process` step 4 as one monolithic
//! BKL-dependent block, because it writes the **parent's live L0 page table**
//! (`demote_range_to_ro`) and no lock covered those PTE edits. That is true of the
//! code as it was written, but it is not a property of the operation: the fault
//! handler already edits the very same PTEs *without* the BKL, serialized by the
//! address space's own [`Process::as_lock`] (`exceptions.rs`, the
//! `AsLockHold::new(&owner.as_lock)` sites). `as_lock` is the inner lock the VFS
//! playbook asks for — fork simply wasn't taking it.
//!
//! So the carve-out is: take `as_lock` over every parent-page-table access, drop
//! the BKL for the surrounding copy, and leave steps 5–8 (ProcessInfo,
//! `THREAD_CONTEXTS` capture, thread spawn, register+READY) fully BKL-held. Those
//! really are BKL-dependent — the audit's finding there stands unchanged.
//!
//! ## Three constraints that shaped the implementation
//!
//! 1. **It must be the thread-group LEADER's `as_lock`, not the forking thread's.**
//!    `CLONE_THREAD` siblings each get their own `Process` with their own
//!    `Spinlock` (`fork_process`'s struct literal constructs a fresh `as_lock`) but
//!    *share* one address space. The fault handler resolves its owner with
//!    [`crate::process::address_space_owner_pid_for_fault`] — TTBR0 → the
//!    non-shared process that owns that L0 — so fork must resolve the same way, or
//!    a worker-thread fork would take a lock nothing else in the system holds.
//!
//! 2. **The hold must be chunked, never spanning the whole copy.** [`AsLockHold`]
//!    masks IRQs for its duration, and it has to: without the mask, a timer IRQ
//!    inside the BKL-free window does `enter_kernel()` and hard-spins for the BKL
//!    while this core holds `as_lock`, against a peer that holds the BKL and wants
//!    `as_lock` in `munmap`/`mprotect` — the AB-BA wedge the network Phase 2
//!    `PreemptGuard` fix exists to prevent. Masking IRQs across a milliseconds-long
//!    page copy is equally unacceptable (it starves this core's tick, and the
//!    playbook's "mask per attempt, never across an unbounded wait" rule says so).
//!    The resolution is [`FORK_AS_CHUNK_PAGES`]: bounded holds, with the child-side
//!    page-table construction — the allocating, expensive part — outside them.
//!
//! 3. **The PTE read, the `cow_ref_inc`, and the demote must be in ONE hold.**
//!    Split them and this race is live: fork reads a PTE naming frame X, a peer's
//!    CoW fault breaks X (`cow_ref_dec` → refcount 0 → frame freed, VA remapped to
//!    Y), and fork then `cow_ref_inc`s the freed X and maps it into the child.
//!    Holding `as_lock` across read+inc+demote makes fork's per-page transition
//!    atomic against the fault handler's, which does its own break under the same
//!    lock. This is why the carve-out *merges* the demote pass into the share pass
//!    rather than leaving it as the separate second walk it used to be.
//!
//! [`AsLockHold`]: crate::process::AsLockHold
//! [`Process::as_lock`]: crate::process::Process::as_lock
//! [`FORK_AS_CHUNK_PAGES`]: crate::process::FORK_AS_CHUNK_PAGES

use core::sync::atomic::{AtomicBool, Ordering};

/// Runtime toggle (default **on**) for the `no-bkl-process` fork page-copy BKL-drop.
///
/// Mirrors `VFS_BKL_DROP_ENABLED` / `EXEC_BKL_DROP_ENABLED` in the bin crate's
/// `smp_shared.rs` (which re-exports the accessors below so every BKL toggle is
/// reachable from one place). It lets a boot image with `no-bkl-process` compiled in
/// A/B against the BKL-held path without a rebuild, and doubles as a kill switch.
///
/// The toggle lives here rather than in the bin crate because [`ProcessBklGuard`] is
/// constructed inside `akuma-exec`, which cannot name bin-crate items.
static PROCESS_BKL_DROP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Whether the fork page-copy BKL-drop (`no-bkl-process`) is currently enabled.
#[inline]
pub fn process_bkl_drop_enabled() -> bool {
    PROCESS_BKL_DROP_ENABLED.load(Ordering::Relaxed)
}

/// Enable/disable the fork page-copy BKL-drop at runtime (A/B measurement, kill switch).
pub fn set_process_bkl_drop_enabled(on: bool) {
    PROCESS_BKL_DROP_ENABLED.store(on, Ordering::Relaxed);
}

/// RAII guard that runs `fork_process`'s page-copy window **without** the Big Kernel
/// Lock. The `ProcessBklGuard` counterpart to `VfsBklGuard` / `NetBklGuard`.
///
/// `new()` drops the BKL; `drop()` re-acquires it on every return path (including the
/// `?` early-returns inside the copy loop), keeping the syscall wrapper's single
/// `leave_kernel` balanced. Both go through [`crate::bkl::dropped_window_open`] /
/// [`close`](crate::bkl::dropped_window_close) so the per-thread ledger restores the
/// dropped state after a nested IRQ, fault, or context switch — a bare
/// `leave_kernel`/`enter_kernel` pair would be balanced but the first timer tick inside
/// the window would silently re-hold the BKL for the remainder (the `[BKL] stuck`
/// regression, `BKL_VFS_CARVE_OUT.md` §8).
///
/// Zero-cost no-op unless BOTH `kernel_smp_shared` and `kernel_no_bkl_process` are set
/// (or the runtime toggle is off): the struct is empty and `new`/`drop` compile to
/// nothing, so default, `size`, `extreme`, multikernel, and plain `smp-shared` builds
/// are byte-for-byte unchanged.
pub struct ProcessBklGuard {
    /// Whether `new()` actually dropped the BKL, **latched at construction**.
    ///
    /// `drop()` must not re-read [`process_bkl_drop_enabled`]. The toggle is genuinely
    /// flipped while guards are live (the boot self-test flips it between phases), and a
    /// guard that asked twice would, on an ON→OFF flip mid-fork, leave the BKL released
    /// for the rest of the syscall — the wrapper's `leave_kernel` would then advance
    /// `now_serving` for a ticket nobody owns and corrupt the FIFO for every core.
    /// Latching makes the guard balanced by construction. (`BKL_VFS_CARVE_OUT.md` §2.4.)
    #[cfg(all(kernel_smp_shared, kernel_no_bkl_process))]
    dropped_bkl: bool,
}

impl ProcessBklGuard {
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_process))]
        let dropped_bkl = process_bkl_drop_enabled();
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_process))]
        if dropped_bkl {
            crate::bkl::dropped_window_open();
        }
        Self {
            #[cfg(all(kernel_smp_shared, kernel_no_bkl_process))]
            dropped_bkl,
        }
    }
}

impl Default for ProcessBklGuard {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ProcessBklGuard {
    #[inline]
    fn drop(&mut self) {
        // Latched in `new()` — deliberately NOT a fresh `process_bkl_drop_enabled()` read.
        #[cfg(all(kernel_smp_shared, kernel_no_bkl_process))]
        if self.dropped_bkl {
            crate::bkl::dropped_window_close();
        }
    }
}
