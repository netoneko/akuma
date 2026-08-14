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

use akuma_primitives::{GuardToggle, ToggledGuard};
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

/// The `no-bkl-process` carve-out, as a [`GuardToggle`] marker.
///
/// The guard body — latch the toggle at construction, re-acquire the BKL on every
/// return path, never re-read the toggle in `drop` — is
/// [`akuma_primitives::ToggledGuard`]'s, stated once for all five carve-outs. What is
/// specific to *this* carve-out is only the three lines below, plus the correctness
/// argument in the module header: `as_lock` held in bounded chunks over every
/// parent-page-table access, and read+`cow_ref_inc`+demote as one atom.
///
/// [`enter`](GuardToggle::enter)/[`exit`](GuardToggle::exit) go through
/// [`crate::bkl::dropped_window_open`]/[`close`](crate::bkl::dropped_window_close), so
/// the per-thread ledger restores the dropped state after a nested IRQ, fault, or
/// context switch. A bare `leave_kernel`/`enter_kernel` pair would be balanced, but
/// the first timer tick inside the window would silently re-hold the BKL for the
/// remainder — the `[BKL] stuck` regression, `BKL_VFS_CARVE_OUT.md` §8.
pub struct ProcessBkl;

impl GuardToggle for ProcessBkl {
    const COMPILED_IN: bool = cfg!(all(kernel_smp_shared, kernel_no_bkl_process));
    #[inline]
    fn enabled() -> bool {
        process_bkl_drop_enabled()
    }
    #[inline]
    fn enter() {
        crate::bkl::dropped_window_open();
    }
    #[inline]
    fn exit() {
        crate::bkl::dropped_window_close();
    }
}

/// RAII guard that runs `fork_process`'s page-copy window **without** the Big Kernel
/// Lock. The `ProcessBklGuard` counterpart to `VfsBklGuard` / `NetBklGuard` — all five
/// are now the same [`ToggledGuard`] over a different marker.
///
/// `new()` drops the BKL; `drop()` re-acquires it on every return path, including the
/// `?` early-returns inside the copy loop, keeping the syscall wrapper's single
/// `leave_kernel` balanced.
///
/// No-op unless BOTH `kernel_smp_shared` and `kernel_no_bkl_process` are set (or the
/// runtime toggle is off): `COMPILED_IN` is then a constant `false`, so `new`/`drop`
/// fold away and default, `size`, `extreme`, and plain `smp-shared` builds are
/// unchanged.
pub type ProcessBklGuard = ToggledGuard<ProcessBkl>;
