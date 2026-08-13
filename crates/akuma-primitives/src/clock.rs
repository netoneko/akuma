//! The monotonic uptime clock, as a boot-registered hook.
//!
//! A leaf crate has no timer. This exists because the preemption bookkeeping in
//! [`crate::preempt`] wants a timestamp for one diagnostic — the `0 → 1`
//! transition of a thread's preemption-disable count, so the watchdog can say
//! *how long* a thread has been non-preemptible — and that was the **entire**
//! reason `disable_preemption` reached into `akuma_exec::runtime()`
//! (`threading/mod.rs:1856`).
//!
//! # This is not the callback that was deliberately removed
//!
//! `akuma-exec`'s `sync.rs` records why `PreemptGuard` stopped dispatching
//! through a registered function pointer: a direct call works during early boot
//! and in host tests, a registered callback does not. That reasoning applies to
//! *the guard's own operation* — mask IRQs, bump the counter — which must be
//! correct before anything is registered.
//!
//! A clock read is the opposite case. It is not part of the operation, it feeds a
//! log line, and the code it replaces **already** degraded: the original was
//! `if runtime::is_registered() { (runtime().uptime_us)() } else { 0 }`. So
//! [`uptime_us`] returns `0` when unregistered and the watchdog reads that as
//! "no timestamp", exactly as before.

use crate::OnceCopy;

/// Monotonic microseconds since boot, installed once at boot.
static CLOCK_HOOK: OnceCopy<fn() -> u64> = OnceCopy::new();

/// Install the uptime clock. Called from `akuma_exec::runtime::register`.
///
/// Idempotent by `OnceCopy`'s contract: a second call is ignored.
pub fn set_clock_hook(f: fn() -> u64) {
    CLOCK_HOOK.set(f);
}

/// Whether a clock has been registered.
#[must_use]
pub fn is_clock_registered() -> bool {
    CLOCK_HOOK.is_set()
}

/// Monotonic microseconds since boot, or **`0` if no clock is registered yet**.
///
/// Callers must treat `0` as "unknown", not as "time zero" — every consumer in
/// this crate compares against `0` to mean "no timestamp recorded". Never
/// panics, so it is safe from IRQ context and before `register`.
#[must_use]
pub fn uptime_us() -> u64 {
    match CLOCK_HOOK.get() {
        Some(f) => f(),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unregistered_clock_reads_zero_and_does_not_panic() {
        // The degradation contract the preemption watchdog depends on.
        assert_eq!(super::uptime_us(), 0);
        assert!(!super::is_clock_registered());
    }
}
