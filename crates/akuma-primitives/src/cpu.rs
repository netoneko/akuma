//! Per-CPU identity registers.
//!
//! The read-only half of "which CPU am I?", alongside [`crate::preempt::current_tid`]'s
//! `TPIDRRO_EL0` read. Both are a single `mrs` with no memory effects, and both
//! are needed by crates well below the scheduler — which is why they live in the
//! leaf rather than in whatever crate happened to want them first.

/// This core's identity (MPIDR aff0).
///
/// Matches the `mpidr & 0xff` indexing used by the SMP bringup path and
/// `trigger_sgi_core`. Always `0` on non-`smp-shared` builds and on host tests, so
/// callers (the scheduler's per-core idle, the BKL's owner field, the MMU's
/// per-core TTBR gate) can use it unconditionally.
///
/// **The `cfg` is deliberately `kernel_smp_shared`, not `target_os = "none"`.**
/// A bare-metal single-core build would read aff0 = 0 anyway, so reading for real
/// looks equivalent — but the multikernel build (`smp`, one whole kernel per core)
/// runs on cores with *non-zero* aff0 while `kernel_smp_shared` is off, and there
/// every caller expects the `0` this shim returns. Widening the gate would
/// silently repoint that build's per-core tables.
///
/// Moved here from `akuma_bkl::bkl` on 2026-08-30: it was the crate's last
/// `unsafe` site, and removing it let the BKL protocol carry
/// `#![forbid(unsafe_code)]` (`docs/archive/AKUMA_EXEC_SPLIT_AGAIN.md` §7.9).
#[cfg(all(kernel_smp_shared, target_os = "none"))]
#[inline]
#[must_use]
pub fn current_core_id() -> u32 {
    (akuma_cpu::sysreg::mpidr_el1() & 0xff) as u32
}

/// Non-SMP / host shim: a single-core build is always core 0.
#[cfg(not(all(kernel_smp_shared, target_os = "none")))]
#[inline(always)]
#[must_use]
pub fn current_core_id() -> u32 {
    0
}
