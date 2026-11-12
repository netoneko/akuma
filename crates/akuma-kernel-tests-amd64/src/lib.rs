//! The amd64 kernel's boot self-test suite.
//!
//! Moved out of `crates/akuma-kernel-amd64/src` (2026-11-11, `docs/archive/AKUMA_MULTIARCH_ATTEMPT_0.md`) so the
//! machine half carries no test driver: the suite's list and order (`suite`, was
//! `boot::self_tests`), the ring-3 tests (`usermode_tests`, was `usermode/tests.rs`) and the
//! `akuma-mmu` walker's (`uas_tests`, was `uas.rs`). Both entry
//! points run inside `akuma-kernel-amd64`, which cannot depend on this crate, so the suite is
//! registered as a hook: the bin is `akuma_kernel::entry!(akuma_kernel_tests_amd64::register)`,
//! and `register` runs first thing in `kmain`/`kmain_mb2`.
//!
//! The inline `smoke_test`s other kernel modules carry, and every kernel item the tests reach,
//! come through `akuma_kernel_amd64::testing`, the kernel's one export surface for this crate.
//!
//! With `no-tests` the crate is empty (the bin names it only as `use .. as _`, for cargo's
//! unused-dependency lint), so nothing of it is linked. Kernel-only: on any other
//! target the crate is empty too.

#![no_std]
#![cfg(all(target_arch = "x86_64", target_os = "none", not(feature = "no-tests")))]

extern crate alloc;

mod suite;
mod uas_tests;
mod usermode_tests;

/// Install the boot suite into the kernel. The bin's `before_main`.
pub fn register() {
    akuma_kernel_amd64::testing::boot::register_self_tests(suite::self_tests);
}
