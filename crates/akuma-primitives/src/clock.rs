//! The kernel's two clocks: the monotonic uptime, as a boot-registered hook,
//! and the UTC offset anchored against it.
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
//!
//! # Why the time syscalls read this and not the timer crate
//!
//! `akuma-syscalls-time` called `akuma_timer::uptime_us()` from its extraction
//! (2026-08-25) until 2026-09-12. On AArch64 that is the same number this hook
//! returns — the kernel registers exactly that function, so nothing changed
//! there. On amd64 it is `0` forever: `akuma_timer::uptime_us` is
//! CNTVCT/CNTFRQ, and `akuma-cpu`'s x86_64 arms for those two reads are stubs
//! that return `0`, so the whole time family — `clock_gettime`, `nanosleep`,
//! itimers — would have come out frozen at zero the moment it was folded in,
//! which is a wrong answer rather than a missing one. Reading the registered
//! hook is what makes the crate mean "the clock this kernel installed" rather
//! than "the ARM counter".

use core::sync::atomic::{AtomicU64, Ordering};

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

// ============================================================================
// The wall clock
// ============================================================================
//
// The UTC anchor lives here, beside the monotonic clock it is expressed
// against, and **not** in `akuma-timer` where it sat until 2026-09-12.
//
// It moved for a layering reason with a measurable symptom. `akuma-timer` is
// the AArch64 generic-timer crate — CNTVCT, CNTV_CVAL, the PL031, the tick
// policy — and the amd64 kernel must not depend on it to answer "what time is
// it": its clock is a PIT-calibrated LAPIC and its RTC is an SNTP packet. So
// that kernel kept its own `(anchor_unix, anchor_uptime)` pair in
// `amd64/src/clock.rs`, and `akuma-syscalls-time` — shared by both kernels —
// read `akuma_timer`'s. Two anchors, one of which every amd64 `clock_settime`
// wrote and every amd64 `clock_gettime` did not read. Here there is one, and
// the two kernels install the *uptime* under it rather than the wall clock
// over it.

/// The `None` of [`UTC_OFFSET_US`]. A real offset is a Unix-epoch microsecond
/// count (~1.7e15), four orders of magnitude short of this, so the sentinel
/// cannot collide with a reading.
const UTC_OFFSET_UNSET: u64 = u64::MAX;

/// UTC offset in microseconds since the Unix epoch, or [`UTC_OFFSET_UNSET`].
///
/// A lock-free atomic rather than a `Spinlock<Option<u64>>`: the value is one
/// scalar with no other state published alongside it, and the read path is
/// reachable from a BKL-free syscall window (`futex(FUTEX_WAIT_BITSET|
/// CLOCK_REALTIME)` converts its absolute wall-clock deadline through this).
///
/// An **offset** rather than the `(unix, uptime)` pair the amd64 kernel used
/// to keep: one atomic cannot be read half-updated, where two can, and a
/// reader that sampled the new epoch against the old anchor would report a
/// time wrong by however long the machine had been up.
static UTC_OFFSET_US: AtomicU64 = AtomicU64::new(UTC_OFFSET_UNSET);

/// Record that Unix epoch `unix_epoch_us` was the wall-clock time at
/// `boot_uptime_us` on the monotonic clock.
///
/// The anchor uptime is the caller's to supply, and that is deliberate: SNTP
/// samples it at *packet receipt* and hands it over after the parse, so a
/// function reading `uptime_us()` itself would silently add the parse to the
/// clock. A caller anchoring at "now" reads [`uptime_us`] **first** and passes
/// it, for the same reason.
#[inline]
pub fn set_utc_time_us(unix_epoch_us: u64, boot_uptime_us: u64) {
    UTC_OFFSET_US.store(unix_epoch_us.saturating_sub(boot_uptime_us), Ordering::Release);
}

/// Current UTC in microseconds since the epoch, or `None` if never set.
///
/// `None` and not a plausible-looking `0`: at epoch 0 every TLS certificate on
/// earth is not-yet-valid, and a caller that cannot tell "no clock" from
/// "1970" reports that as a certificate error and sends its reader to look at
/// the CA bundle (`docs/archive/AKUMA_FIRECRACKER_AMD64.md` §3.29.5).
#[inline]
#[must_use]
pub fn utc_time_us(boot_uptime_us: u64) -> Option<u64> {
    match UTC_OFFSET_US.load(Ordering::Acquire) {
        UTC_OFFSET_UNSET => None,
        off => Some(off.wrapping_add(boot_uptime_us)),
    }
}

/// Whether the wall clock has ever been set.
///
/// `utc_time_us(..).is_some()` without needing an uptime to pass it — the
/// question "does this machine know what time it is" is asked on paths that
/// have no timestamp in hand (a console line, an SNTP retry gate).
#[must_use]
pub fn is_utc_set() -> bool {
    UTC_OFFSET_US.load(Ordering::Acquire) != UTC_OFFSET_UNSET
}


#[cfg(test)]
mod tests {
    #[test]
    fn unregistered_clock_reads_zero_and_does_not_panic() {
        // The degradation contract the preemption watchdog depends on.
        assert_eq!(super::uptime_us(), 0);
        assert!(!super::is_clock_registered());
    }

    /// The wall clock's own degradation contract, and the one the amd64
    /// kernel's `is_synced()` is built on: never set reads `None`, not `0`.
    ///
    /// These four run against a process-wide `static` under a threaded test
    /// runner, so they are one test: split up, the "unset" assertion would
    /// race whichever of its siblings set the anchor first.
    #[test]
    fn the_utc_anchor_is_unset_then_tracks_uptime() {
        assert_eq!(super::utc_time_us(0), None);
        assert!(!super::is_utc_set());

        // Anchored at uptime 5 s, so reading it back at uptime 5 s is the
        // instant that was anchored and 7 s later is 7 s later.
        const EPOCH_US: u64 = 1_757_000_000_000_000;
        super::set_utc_time_us(EPOCH_US, 5_000_000);
        assert!(super::is_utc_set());
        assert_eq!(super::utc_time_us(5_000_000), Some(EPOCH_US));
        assert_eq!(super::utc_time_us(12_000_000), Some(EPOCH_US + 7_000_000));

        // A machine whose clock is set before its counter has moved far: the
        // offset saturates rather than wrapping to ~1.8e19, which would read
        // as a date ~584 000 years out and pass every "is it set" check.
        super::set_utc_time_us(1_000, 9_999_999);
        assert_eq!(super::utc_time_us(9_999_999), Some(9_999_999));

        super::set_utc_time_us(EPOCH_US, 0);
        assert_eq!(super::utc_time_us(0), Some(EPOCH_US));
    }
}
