//! When a `FUTEX_WAIT` should give up — the arithmetic only.
//!
//! Small, and the source of one of the more expensive bugs in this tree.

use akuma_syscalls_linux::flags::futex as f;

/// A parked waiter is re-floated this often even when it has no deadline.
///
/// The scheduler's wake/schedule handshake has residual wake-loss windows under
/// heavy SMP preemption (4/4 untimed `Barrier`/`Condvar` hangs under CPU-hog
/// pressure, `J4_WRITE_PERM_FAULT_AND_HALF_WRITTEN_LINKER_OUTPUT.md` §7.9).
/// A *timed* wait survives a lost wake because its deadline forces
/// `schedule_blocking` to return, after which re-reading the futex word reports
/// `EAGAIN` and the caller re-evaluates. An untimed wait has nothing to force
/// that return, so one lost wake strands the thread forever.
///
/// So an untimed wait parks on a rolling deadline instead: when it expires the
/// waiter self-removes, re-reads the word, and re-parks. Spurious wakes are
/// always permitted by the futex contract, so this is free of user-visible
/// consequence — it costs one wakeup per interval per untimed waiter.
pub const REVALIDATE_US: u64 = 200_000;

/// No deadline.
pub const NEVER: u64 = u64::MAX;

/// Convert a user timeout into an absolute uptime deadline.
///
/// `timeout_us` is the user's `timespec` already flattened to microseconds;
/// `now_us` is `uptime_us()`; `utc_now_us` is the wall clock, or `None` on a
/// platform with no RTC yet.
///
/// # Why the op decides whether the timeout is relative
///
/// Linux is not op-agnostic here, and treating it as such is a real bug with a
/// name:
///
/// - plain `FUTEX_WAIT` — the timeout is **relative** to now;
/// - `FUTEX_WAIT_BITSET` — the timeout is **absolute**, against
///   `CLOCK_MONOTONIC` unless `FUTEX_CLOCK_REALTIME` selects the wall clock.
///
/// Rust's std emits `FUTEX_WAIT_BITSET` *without* `FUTEX_CLOCK_REALTIME` for
/// every timed wait — `Condvar::wait_timeout`, `park_timeout`, `Mutex` and
/// `Once` contention — computing `CLOCK_MONOTONIC::now() + dur` itself. Adding
/// uptime to that already-absolute value made every std timed wait sleep about
/// twice the current uptime, growing the longer the VM had been up. That was
/// the rustc "futex deadlock" (`docs/AKUMA_SELF_HOSTING.md` §7d) — a bug whose
/// symptom scaled with uptime, which is exactly the kind that reads as
/// nondeterministic.
///
/// Akuma's `CLOCK_MONOTONIC` *is* `uptime_us`, so an absolute monotonic
/// deadline needs no conversion at all.
#[must_use]
pub fn deadline_us(op: i32, timeout_us: u64, now_us: u64, utc_now_us: Option<u64>) -> u64 {
    if f::cmd_of(op) != f::FUTEX_WAIT_BITSET {
        // Plain FUTEX_WAIT: relative.
        return now_us.saturating_add(timeout_us);
    }
    if !f::is_realtime(op) {
        // Absolute CLOCK_MONOTONIC == absolute uptime.
        return timeout_us;
    }
    match utc_now_us {
        // Absolute wall-clock: re-express as uptime + remaining.
        Some(utc) if timeout_us > utc => now_us.saturating_add(timeout_us - utc),
        // Already in the past: expire immediately.
        Some(_) => now_us,
        // No wall clock yet. Treating the value as uptime microseconds is
        // imprecise but bounded — unlike ignoring the timeout, which converts a
        // timed wait into an untimed one and hides a lost wake forever.
        None => timeout_us,
    }
}

/// The deadline to actually park on, which is not always the user's.
///
/// An untimed wait parks on [`REVALIDATE_US`] from now; a timed one parks on
/// its real deadline. The user-visible timeout check still compares against the
/// original, so the substitution is invisible from userspace.
#[must_use]
pub fn park_deadline_us(deadline: u64, now_us: u64) -> u64 {
    if deadline == NEVER { now_us.saturating_add(REVALIDATE_US) } else { deadline }
}

/// Whether a wait with this deadline has run out of time.
#[must_use]
pub const fn expired(deadline: u64, now_us: u64) -> bool {
    deadline != NEVER && now_us >= deadline
}
