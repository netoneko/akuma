//! Pure session-lifetime policy: the pre-auth grace deadline and the
//! established-session idle deadline, as functions over timestamps. No I/O.
//!
//! Split out of `protocol.rs` so the *decisions* are unit-testable on the
//! host, the same way `wire.rs` owns pure byte layout. This module exists
//! because of the 2026-10-15 ryzen boot-120 lockout (`proposals/
//! NEXT_AGENT_SSHD_SESSION_LEAK.md`): clients whose TCP connection died
//! mid-key-exchange left half-open sessions that nothing ever retired, and
//! ~24 of them pinned `max_sessions` for hours — sshd kept answering TCP and
//! serving its banner but refused every new session. Tracing the lifecycle,
//! **two transitions had no timer at all**:
//!
//! 1. **pre-auth stall** — a session sits anywhere between the version
//!    string and a successful `USERAUTH_REQUEST` forever;
//! 2. **vanished client, post-auth** — a session with an open channel whose
//!    peer silently went away (dormant radio, no FIN ever arrives) waits on
//!    its socket forever; writes into the void may never error.
//!
//! Every other retirement path (peer EOF/RST, `DISCONNECT`, channel close,
//! shell exit, write error) already existed; these two were the leak.
//!
//! OpenSSH's answers are [`crate`] equivalents of `LoginGraceTime` (close
//! sessions that never complete auth) and `ClientAliveInterval` +
//! `ClientAliveCountMax` (probe an established session and cut it off when
//! the probes go unanswered — *not* a bare idle-kill, which would murder a
//! healthy session whose user simply walked away). The daemon in
//! `protocol.rs` carries the effects; this module owns the deadlines.
//!
//! Host-test gate (same as `wire.rs`):
//!
//! ```text
//! cargo test -p sshd --lib --no-default-features --target $(rustc -vV | grep '^host:' | cut -d' ' -f2)
//! ```

/// Default pre-auth grace, in milliseconds — OpenSSH's `LoginGraceTime`
/// default (120 s). A connection that has not authenticated within this
/// window of *connecting* is torn down; see [`grace_expired`].
pub const DEFAULT_LOGIN_GRACE_MS: u64 = 120_000;

/// Default established-session silence budget, in milliseconds, before sshd
/// sends the first client-alive probe — the `ClientAliveInterval` analog.
/// Deliberately on by default (OpenSSH ships this off): a machine whose only
/// remote way back is ssh must not let vanished peers occupy session slots
/// forever. See [`idle_probe_due`] / [`idle_dead`].
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 600_000;

/// How long an unanswered client-alive probe is tolerated before the session
/// is declared dead — the `ClientAliveCountMax` analog, expressed as one
/// probe plus a grace window (90 s ≈ OpenSSH's default 3 × 30 s cadence,
/// collapsed to a single probe to keep the per-session state at one bool).
pub const DEFAULT_IDLE_PROBE_GRACE_MS: u64 = 90_000;

/// Elapsed time from `stamp_ms` to `now_ms`, saturating at zero.
///
/// `saturating_sub`, not `-`: a stamp can land after `now` if callers sample
/// different clocks (or a hostile `now`), and the plain subtraction wraps to
/// `u64::MAX` under `overflow-checks = false`, which would read as "idle
/// forever" and tear down every healthy session in one tick.
#[must_use]
pub fn elapsed_since(stamp_ms: u64, now_ms: u64) -> u64 {
    now_ms.saturating_sub(stamp_ms)
}

/// Decision: is this session past its pre-auth grace deadline?
///
/// True only when **all three** hold:
///
/// - the session has **not** completed auth yet (the instant
///   `USERAUTH_REQUEST` succeeds, this timer stops applying — an
///   authenticated session is idle-managed, not grace-managed);
/// - grace is enabled at all (`grace_ms > 0`; 0 means "off", mirroring
///   OpenSSH's `LoginGraceTime 0`, and off must mean *off* — this is also
///   how a test disables the timer without a way to stop the clock);
/// - at least `grace_ms` has passed since `connected_ms`.
///
/// This is the fix for the boot-120 lockout's primary mass: every one of the
/// ~24 leaked sessions was pre-auth, dead mid-key-exchange, holding a slot
/// no timer would ever reclaim.
#[must_use]
pub fn grace_expired(authenticated: bool, connected_ms: u64, now_ms: u64, grace_ms: u64) -> bool {
    !authenticated && grace_ms > 0 && elapsed_since(connected_ms, now_ms) >= grace_ms
}

/// Decision: should this session's client-alive probe go out now?
///
/// True when the session has been silent (no inbound bytes at all — SSH
/// transport bytes of any kind count, which is what makes a client's
/// `REQUEST_FAILURE` reply count as life) for `idle_timeout_ms`, grace is
/// enabled (`idle_timeout_ms > 0`), and no probe is already outstanding.
///
/// A *decision*, not an effect: the caller sends the probe and records it in
/// [`SessionWatch::note_probe_sent`]. The probe — not the timeout — is what
/// keeps a healthy session alive: any RFC 4254 §4 client (OpenSSH's, and
/// this repo's own `ssh`) answers an unrecognized global request with
/// `REQUEST_FAILURE`, that reply is inbound bytes, [`SessionWatch::
/// note_activity`] resets the clock, and the session survives. A peer whose
/// radio is gone never answers.
#[must_use]
pub fn idle_probe_due(
    last_activity_ms: u64,
    now_ms: u64,
    idle_timeout_ms: u64,
    probe_outstanding: bool,
) -> bool {
    idle_timeout_ms > 0 && !probe_outstanding && elapsed_since(last_activity_ms, now_ms) >= idle_timeout_ms
}

/// Decision: is this session idle-dead?
///
/// True when grace is enabled (`idle_timeout_ms > 0`) and the session has
/// now been silent for the full budget — the initial [`idle_probe_due`]
/// window **plus** the unanswered-probe grace. Total silence for
/// `idle_timeout_ms + probe_grace_ms` means the probe (if one was ever sent)
/// went unanswered: on a machine whose radio is gone, this is the only
/// signature a vanished peer produces, because no FIN ever arrives and a
/// write into the void may never error.
#[must_use]
pub fn idle_dead(
    last_activity_ms: u64,
    now_ms: u64,
    idle_timeout_ms: u64,
    probe_grace_ms: u64,
) -> bool {
    idle_timeout_ms > 0 && elapsed_since(last_activity_ms, now_ms) >= idle_timeout_ms.saturating_add(probe_grace_ms)
}

/// Per-session timer state — the stamps the two deadlines are measured from,
/// plus the one-bit probe tracker. Sized for the 4 MB extreme-size image:
/// two `u64`s and a `bool` in the existing session struct, nothing on the
/// heap, nothing allocated per tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionWatch {
    /// When the connection was accepted. The grace deadline is measured
    /// from here, not from the first byte: a client that connects and then
    /// sends nothing is precisely the leak shape.
    pub connected_ms: u64,
    /// Last time this session produced an inbound byte, of any kind, at any
    /// protocol stage. Reset by [`Self::note_activity`].
    pub last_activity_ms: u64,
    /// A client-alive probe has been sent and not yet answered. Cleared by
    /// [`Self::note_activity`] — the answer *is* activity.
    pub probe_outstanding: bool,
}

impl SessionWatch {
    /// Watch for a session accepted at `now_ms`.
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self {
            connected_ms: now_ms,
            last_activity_ms: now_ms,
            probe_outstanding: false,
        }
    }

    /// The session produced an inbound byte (any byte, any stage — version
    /// line, KEX, a `REQUEST_FAILURE` answering our probe). Resets the idle
    /// clock and retracts an outstanding probe.
    pub fn note_activity(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
        self.probe_outstanding = false;
    }

    /// A probe went out. Takes no stamp deliberately: the death deadline is
    /// measured from `last_activity_ms` (total silence), not from the probe
    /// send time, so a reaper fires even if the probe write itself jammed.
    pub fn note_probe_sent(&mut self) {
        self.probe_outstanding = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // grace_expired
    // ------------------------------------------------------------------

    #[test]
    fn grace_does_not_fire_before_the_deadline() {
        assert!(!grace_expired(false, 1_000, 1_000 + 119_999, DEFAULT_LOGIN_GRACE_MS));
    }

    #[test]
    fn grace_fires_at_and_after_the_deadline() {
        assert!(grace_expired(false, 1_000, 1_000 + 120_000, DEFAULT_LOGIN_GRACE_MS));
        assert!(grace_expired(false, 1_000, 1_000 + 500_000, DEFAULT_LOGIN_GRACE_MS));
    }

    #[test]
    fn grace_never_fires_once_authenticated() {
        // Even hours past the deadline: auth completed, so this session is
        // idle-managed, not grace-managed.
        assert!(!grace_expired(true, 1_000, 1_000 + 3_600_000, DEFAULT_LOGIN_GRACE_MS));
    }

    #[test]
    fn grace_zero_disables_the_timer_entirely() {
        assert!(!grace_expired(false, 1_000, u64::MAX / 2, 0));
    }

    #[test]
    fn grace_measurements_are_relative_not_absolute() {
        // Late-accepted connection, tiny uptime: the deadline rides on
        // `connected_ms`, so this must stay false.
        assert!(!grace_expired(false, 90_000, 100_000, DEFAULT_LOGIN_GRACE_MS));
    }

    // ------------------------------------------------------------------
    // idle_probe_due
    // ------------------------------------------------------------------

    #[test]
    fn probe_not_due_inside_the_idle_window() {
        assert!(!idle_probe_due(0, DEFAULT_IDLE_TIMEOUT_MS - 1, DEFAULT_IDLE_TIMEOUT_MS, false));
    }

    #[test]
    fn probe_due_at_the_idle_boundary_when_none_outstanding() {
        assert!(idle_probe_due(0, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_IDLE_TIMEOUT_MS, false));
    }

    #[test]
    fn probe_not_due_twice_while_one_is_outstanding() {
        // The caller must not spray probes every tick; the outstanding bit
        // is part of the decision.
        assert!(!idle_probe_due(0, DEFAULT_IDLE_TIMEOUT_MS * 10, DEFAULT_IDLE_TIMEOUT_MS, true));
    }

    #[test]
    fn probe_rearm_after_activity_reset() {
        let mut watch = SessionWatch::new(0);
        watch.note_probe_sent();
        assert!(!idle_probe_due(watch.last_activity_ms, 700_000, DEFAULT_IDLE_TIMEOUT_MS, watch.probe_outstanding));
        // Client answered (the answer is activity) — probe bit clears, and a
        // fresh window starts from the new stamp.
        watch.note_activity(700_000);
        assert!(!idle_probe_due(watch.last_activity_ms, 700_000 + DEFAULT_IDLE_TIMEOUT_MS - 1, DEFAULT_IDLE_TIMEOUT_MS, watch.probe_outstanding));
        assert!(idle_probe_due(watch.last_activity_ms, 700_000 + DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_IDLE_TIMEOUT_MS, watch.probe_outstanding));
    }

    // ------------------------------------------------------------------
    // idle_dead
    // ------------------------------------------------------------------

    #[test]
    fn not_dead_merely_because_the_probe_window_opened() {
        // Silence for the full initial window but not past the probe grace:
        // a healthy-but-slow client may still answer. Killing here would be
        // a bare idle-kill, not the probe protocol.
        assert!(!idle_dead(0, DEFAULT_IDLE_TIMEOUT_MS + DEFAULT_IDLE_PROBE_GRACE_MS - 1, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_IDLE_PROBE_GRACE_MS));
    }

    #[test]
    fn dead_at_the_end_of_the_unanswered_probe_grace() {
        assert!(idle_dead(0, DEFAULT_IDLE_TIMEOUT_MS + DEFAULT_IDLE_PROBE_GRACE_MS, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_IDLE_PROBE_GRACE_MS));
    }

    #[test]
    fn dead_is_about_total_silence_not_the_probe_bit() {
        // `probe_outstanding` is deliberately not an input: even a caller
        // that never managed to send the probe (write path jammed) must
        // reap. The stuck /bin/cat from boot 120 is the case this covers.
        let watch = SessionWatch::new(0);
        assert!(!watch.probe_outstanding);
        assert!(idle_dead(watch.last_activity_ms, DEFAULT_IDLE_TIMEOUT_MS + DEFAULT_IDLE_PROBE_GRACE_MS, DEFAULT_IDLE_TIMEOUT_MS, DEFAULT_IDLE_PROBE_GRACE_MS));
    }

    #[test]
    fn idle_zero_disables_the_reaper() {
        assert!(!idle_dead(0, u64::MAX / 2, 0, DEFAULT_IDLE_PROBE_GRACE_MS));
        assert!(!idle_probe_due(0, u64::MAX / 2, 0, false));
    }

    // ------------------------------------------------------------------
    // elapsed_since / SessionWatch
    // ------------------------------------------------------------------

    #[test]
    fn elapsed_saturates_against_a_stamp_in_the_future() {
        assert_eq!(elapsed_since(5_000, 4_999), 0);
        assert_eq!(elapsed_since(5_000, 5_000), 0);
        assert_eq!(elapsed_since(5_000, 5_001), 1);
    }

    #[test]
    fn activity_clears_the_outstanding_probe() {
        let mut watch = SessionWatch::new(10);
        watch.note_probe_sent();
        assert!(watch.probe_outstanding);
        watch.note_activity(11);
        assert!(!watch.probe_outstanding);
        assert_eq!(watch.last_activity_ms, 11);
        assert_eq!(watch.connected_ms, 10, "grace origin is never moved by activity");
    }

    #[test]
    fn watch_starts_with_both_stamps_at_accept_time() {
        let watch = SessionWatch::new(42);
        assert_eq!(watch.connected_ms, 42);
        assert_eq!(watch.last_activity_ms, 42);
        assert!(!watch.probe_outstanding);
    }
}
