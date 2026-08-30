//! The Big Kernel Lock protocol, and the spinlocks it is built on.
//!
//! # Why this is a crate
//!
//! Extracted from `akuma-exec` on 2026-08-30
//! (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §3.4). The argument is the one that
//! justified `akuma-syscalls-sync`: **extract the body of code whose bug history
//! is a property of pure logic.** The dropped-window ledger, the `tag=511`
//! attribution storms, the ON_CPU scheduler race and the lost-ticket recovery are
//! all properties of the ~1,500 lines here, and every one of them previously cost
//! a devbox boot — often an `SMP=4` boot under host contention — to reproduce.
//!
//! It also already carried its own proof: [`bkl_model`] is a host model checker
//! for deadlock, mutual exclusion and starvation over this protocol. As a
//! `#[cfg(test)] mod` inside a cross-compiled crate it was awkward to run; here it
//! is a plain `cargo test`.
//!
//! # The one hook
//!
//! The plan budgeted a four-item vtable to break the `sync`/`bkl` <-> `threading`
//! cycle. It turned out to need **one**. `MAX_THREADS`, `current_tid`,
//! `disable_preemption`, `enable_preemption`, `irq_save_mask`/`irq_restore`,
//! `PreemptGuard` and `safe_print!` had all already migrated to
//! `akuma-primitives` — so the only genuinely upward call left is
//! [`set_yield_hook`]'s `yield_now`, the scheduler's own entry point, which
//! nothing below the scheduler can provide.
//!
//! The hook **degrades rather than panics** when unregistered: an unhooked
//! `yield_now` spins with a `PreemptGuard`-shaped pause instead of switching
//! threads. That matters because `lock_bounded` is reachable from early boot,
//! before any scheduler exists, and a `.expect()` there would turn "no scheduler
//! yet" into a boot hang with no console.
//!
//! # What did NOT come with it
//!
//! `current_core_id` did (it is an `MPIDR_EL1` read). The *scheduler* did not:
//! thread states, the run queue, the SGI handler and `yield_now` itself all stay
//! in `akuma_exec::threading`. This crate knows how to serialise cores; it does
//! not know what a thread is.

#![cfg_attr(not(test), no_std)]
// Zero `unsafe` as of 2026-08-30. Two things got it here, neither of them a
// rewrite: the hand-rolled `RawRwSpinlock` (3 sites — an `unsafe impl
// lock_api::RawRwLock` and its two `unsafe fn` unlocks) turned out to have no
// consumers at all and was deleted, and `current_core_id`'s `mrs mpidr_el1` (1
// site) moved to `akuma_primitives::cpu`, beside the `TPIDRRO_EL0` read the leaf
// already owned. `forbid`, not `deny`, so no module can opt back in with a local
// `allow`. See `sync.rs`'s header for how the dead lock hid, and
// `docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §7.9.
#![forbid(unsafe_code)]
// Inherited verbatim from `akuma-exec`'s crate-root `allow` list. This code did
// not change when it moved out on 2026-08-30, so its lint posture must not
// either — a split that silently turns 20 warnings on is not behaviour-preserving,
// and fixing them in the same commit would hide the move in the diff. Tighten
// these deliberately, later, one lint at a time.
#![allow(
    clippy::too_long_first_doc_paragraph,
    clippy::must_use_candidate,
    clippy::redundant_pub_crate,
    clippy::unnecessary_cast,
    clippy::ptr_as_ptr,
    clippy::verbose_bit_mask,
    clippy::single_match_else,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::new_without_default,
    clippy::manual_div_ceil,
    clippy::cast_lossless,
    clippy::vec_init_then_push,
    clippy::unused_self,
    clippy::semicolon_if_nothing_returned,
    clippy::needless_continue,
    clippy::manual_is_multiple_of,
    clippy::identity_op,
    clippy::collapsible_if,
    clippy::cast_possible_wrap,
    clippy::inline_always,
    clippy::missing_safety_doc,
    clippy::uninlined_format_args,
    clippy::doc_markdown,
    clippy::declare_interior_mutable_const,
    clippy::missing_const_for_fn,
)]

extern crate alloc;

pub mod bkl;
pub mod sync;

/// Host-only model checker + concurrency stress harness for the Big Kernel Lock
/// protocol (deadlock / mutual-exclusion / starvation checks).
#[cfg(test)]
mod bkl_model;

/// The tree's one heap-free print macro, re-exported so this crate's
/// `crate::safe_print!(…)` call sites resolve unchanged.
pub use akuma_primitives::safe_print;

use akuma_primitives::Registered;

/// The scheduler's yield entry point.
///
/// The single upward dependency this crate has — see the module header. Registered
/// once during boot by `akuma_exec::threading::init`; unregistered, [`yield_now`]
/// degrades to a spin hint rather than panicking, because `sync::lock_bounded` is
/// reachable before any scheduler exists.
static YIELD_HOOK: Registered<fn()> = Registered::new(
    "akuma-bkl: yield hook not registered — call akuma_exec::init() first",
);

/// Register the scheduler's `yield_now`. Idempotent; last registration wins.
pub fn set_yield_hook(f: fn()) {
    YIELD_HOOK.register(f);
}

/// Yield to the scheduler, or spin-hint if none is registered yet.
#[inline]
pub fn yield_now() {
    if let Some(f) = YIELD_HOOK.get() {
        f();
    } else {
        core::hint::spin_loop();
    }
}
