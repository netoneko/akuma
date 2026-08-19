//! Observe a measurement, decide a policy, don't flap.
//!
//! # The name
//!
//! *Kachou* (課長) is "section chief" — Aramaki's title in *Ghost in the
//! Shell*, and an accurate job description for this crate. A chief does not go
//! into the field: he takes reports and issues verdicts, and the operatives do
//! the work. Nothing here touches hardware, owns state, allocates, or has a
//! clock. You hand it a measurement, it hands back a decision, and the timer,
//! the page cache and netpoll go and act on it.
//!
//! Three subsystems arrived at this same shape independently, each hand-rolling
//! it, before it was worth naming:
//!
//! - **The timer tick** demotes itself when the host stops honouring `wfi`:
//!   count idle-loop iterations per tick, and if the loop is spinning for
//!   several consecutive windows, lengthen the tick — once, permanently
//!   ([`Latch`]).
//! - **The file-page cache** grows its cap by a percentage when free RAM can
//!   spare it and gives the growth back when it cannot, with the grow and shrink
//!   thresholds deliberately different so a workload parked on the line cannot
//!   toggle the cap on every check ([`hysteresis`]).
//! - **The netpoll wake period** (proposed in
//!   `docs/archive/AKUMA_SCHEDULING_EXTRACTION.md`) backs off toward a long idle
//!   period as measured traffic falls and tightens to the tick when it rises
//!   ([`ramp`]).
//!
//! What they share is not the decision but the *discipline*: a policy driven by
//! a live measurement has to answer "what stops this oscillating?", and every
//! one of them answered it differently and separately. These primitives are
//! deliberately tiny — the value is that the answer is now named, tested once,
//! and hard to forget.
//!
//! Everything here is pure: no clock, no allocation, no interior mutability, no
//! `Ordering` to get wrong. Callers own the state and the sampling cadence,
//! which is what lets the same primitive serve a static in an IRQ handler and a
//! local in a host test.

#![no_std]

// ============================================================================
// Latch — N consecutive confirmations, then a one-way verdict
// ============================================================================

/// A one-way trip switch: fires once, after the signal has stayed over its
/// threshold for `needed` consecutive observations, and never resets.
///
/// The consecutive requirement is the anti-flap mechanism, and the one-way part
/// is a deliberate second one. Its original user demotes the timer tick when it
/// detects the host ignoring `wfi`; re-promoting on a single good window would
/// buy a second burn every time the host wavered, so it does not re-promote at
/// all. Any policy whose *reversal* is more expensive than staying wrong wants
/// this rather than [`Hysteresis`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Latch {
    consecutive: u32,
    latched: bool,
}

impl Latch {
    #[must_use]
    pub const fn new() -> Self {
        Self { consecutive: 0, latched: false }
    }

    /// Rebuild from packed state, for callers that keep this in one atomic.
    #[must_use]
    pub const fn from_parts(consecutive: u32, latched: bool) -> Self {
        Self { consecutive, latched }
    }

    #[must_use]
    pub const fn into_parts(self) -> (u32, bool) {
        (self.consecutive, self.latched)
    }

    #[must_use]
    pub const fn is_latched(&self) -> bool {
        self.latched
    }

    /// Feed one observation. Returns `true` **exactly once**, on the
    /// observation that trips the latch.
    ///
    /// `over` is the caller's own predicate ("was this window bad?"), kept
    /// outside so the primitive never has to know whether the comparison is a
    /// rate, a level, a ratio, or a direction.
    pub const fn observe(&mut self, over: bool, needed: u32) -> bool {
        if self.latched {
            return false;
        }
        if over {
            self.consecutive += 1;
        } else {
            self.consecutive = 0;
        }
        if self.consecutive >= needed {
            self.latched = true;
            return true;
        }
        false
    }
}

// ============================================================================
// Hysteresis — two thresholds, one bit of state
// ============================================================================

/// Should a two-state policy be engaged, given where it is now?
///
/// Engages when `signal >= engage_at`, releases when `signal < release_at`, and
/// **holds its current state in between**. Set `release_at < engage_at` to get
/// a hysteresis band; setting them equal degenerates to a bare threshold and
/// reintroduces exactly the flapping this exists to prevent.
///
/// Pure and stateless — the caller passes in whether the policy is currently
/// engaged. That suits callers whose engaged-ness is already derivable from the
/// thing being governed (the file-page cache knows it is inflated because its
/// cap exceeds its base) and so should not keep a second copy that can disagree.
/// Use [`Hysteresis`] when there is nothing to derive it from.
#[must_use]
pub const fn hysteresis(engaged: bool, signal: u64, engage_at: u64, release_at: u64) -> bool {
    if engaged { signal >= release_at } else { signal >= engage_at }
}

/// Stateful wrapper over [`hysteresis`] for callers with no derivable state.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hysteresis {
    engaged: bool,
}

impl Hysteresis {
    #[must_use]
    pub const fn new() -> Self {
        Self { engaged: false }
    }

    #[must_use]
    pub const fn is_engaged(&self) -> bool {
        self.engaged
    }

    /// Feed one observation; returns the state *after* it.
    pub const fn observe(&mut self, signal: u64, engage_at: u64, release_at: u64) -> bool {
        self.engaged = hysteresis(self.engaged, signal, engage_at, release_at);
        self.engaged
    }
}

// ============================================================================
// Ramp — a continuous knob between two anchors
// ============================================================================

/// Interpolate a knob linearly between its value at zero signal and its value
/// at `full_at`, clamped outside that range.
///
/// Works in either direction: `at_zero > at_full` is the normal case for a
/// *period* that should shorten as load rises (netpoll's wake period), and
/// `at_zero < at_full` for a budget that should grow with it.
///
/// Integer arithmetic throughout, and saturating, because every caller is a
/// kernel path where a panic is not an option and `f64` is not available.
#[must_use]
pub const fn ramp(signal: u64, at_zero: u64, at_full: u64, full_at: u64) -> u64 {
    if full_at == 0 || signal >= full_at {
        return at_full;
    }
    if at_zero >= at_full {
        // Descending: at_zero -> at_full as signal -> full_at.
        let span = at_zero - at_full;
        at_zero - (span.saturating_mul(signal) / full_at)
    } else {
        let span = at_full - at_zero;
        at_zero + (span.saturating_mul(signal) / full_at)
    }
}

/// Events per second, from a count over an elapsed span in microseconds.
///
/// Guards the warm-up case every windowed rate has and most hand-rolled ones
/// get wrong: before the window has filled, dividing by the *nominal* window
/// under-reports the rate and the policy reacts late. Pass the elapsed span,
/// not the window width.
#[must_use]
pub const fn rate_per_sec(events: u64, elapsed_us: u64) -> u64 {
    if elapsed_us == 0 {
        return 0;
    }
    events.saturating_mul(1_000_000) / elapsed_us
}

// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latch_needs_consecutive_confirmations() {
        let mut l = Latch::new();
        assert!(!l.observe(true, 3));
        assert!(!l.observe(true, 3));
        // One good window resets the run.
        assert!(!l.observe(false, 3));
        assert!(!l.observe(true, 3));
        assert!(!l.observe(true, 3));
        assert!(l.observe(true, 3), "third consecutive should trip it");
    }

    #[test]
    fn latch_fires_once_and_stays_latched() {
        let mut l = Latch::new();
        for _ in 0..2 {
            l.observe(true, 2);
        }
        assert!(l.is_latched());
        // Never fires again, and never un-latches however good things get.
        for _ in 0..10 {
            assert!(!l.observe(false, 2));
        }
        assert!(l.is_latched());
    }

    #[test]
    fn latch_round_trips_through_packed_parts() {
        let mut l = Latch::new();
        l.observe(true, 5);
        let (c, latched) = l.into_parts();
        assert_eq!((c, latched), (1, false));
        assert_eq!(Latch::from_parts(c, latched).into_parts(), (c, latched));
    }

    #[test]
    fn hysteresis_holds_state_inside_the_band() {
        // engage at 100, release below 40.
        assert!(!hysteresis(false, 50, 100, 40), "inside band, was off -> stays off");
        assert!(hysteresis(true, 50, 100, 40), "inside band, was on -> stays on");
        assert!(hysteresis(false, 100, 100, 40), "at engage threshold -> on");
        assert!(!hysteresis(true, 39, 100, 40), "below release -> off");
    }

    /// The whole point: a signal oscillating inside the band must not toggle.
    #[test]
    fn hysteresis_does_not_flap() {
        let mut h = Hysteresis::new();
        h.observe(200, 100, 40);
        assert!(h.is_engaged());
        let mut toggles = 0;
        let mut last = h.is_engaged();
        for s in [90, 50, 90, 45, 99, 41, 90] {
            let now = h.observe(s, 100, 40);
            if now != last {
                toggles += 1;
            }
            last = now;
        }
        assert_eq!(toggles, 0, "signal stayed inside the band; policy must not move");
    }

    #[test]
    fn equal_thresholds_degenerate_to_a_bare_threshold() {
        // Documented degenerate case — worth pinning so nobody "simplifies"
        // the two thresholds into one and wonders why a policy oscillates.
        let mut h = Hysteresis::new();
        assert!(h.observe(100, 100, 100));
        assert!(!h.observe(99, 100, 100));
        assert!(h.observe(100, 100, 100));
    }

    #[test]
    fn ramp_descends_and_clamps() {
        // netpoll's shape: 100 ms at idle, 1 ms at 1000 pps.
        assert_eq!(ramp(0, 100_000, 1_000, 1_000), 100_000);
        assert_eq!(ramp(1_000, 100_000, 1_000, 1_000), 1_000);
        assert_eq!(ramp(9_999, 100_000, 1_000, 1_000), 1_000, "clamps above full_at");
        let mid = ramp(500, 100_000, 1_000, 1_000);
        assert!((49_000..=52_000).contains(&mid), "midpoint was {mid}");
    }

    #[test]
    fn ramp_ascends_too() {
        assert_eq!(ramp(0, 10, 100, 50), 10);
        assert_eq!(ramp(50, 10, 100, 50), 100);
        assert_eq!(ramp(25, 10, 100, 50), 55);
    }

    #[test]
    fn ramp_survives_a_zero_span() {
        assert_eq!(ramp(7, 42, 99, 0), 99, "full_at == 0 must not divide by zero");
    }

    #[test]
    fn rate_uses_elapsed_not_nominal_window() {
        // 5 events in the first 100 ms of a 10 s window is 50/s, not 0.5/s.
        assert_eq!(rate_per_sec(5, 100_000), 50);
        assert_eq!(rate_per_sec(5, 10_000_000), 0);
        assert_eq!(rate_per_sec(0, 0), 0);
    }
}
