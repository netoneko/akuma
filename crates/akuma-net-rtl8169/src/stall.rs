//! When a silent receiver is a stalled one.
//!
//! The receive loop asks this once per frameless lap. It owns no state, reads no
//! register and performs no I/O — it is the *decision*, separated from the poke
//! sequence for the same reason the bring-up is: the answer is four conditions
//! in a particular order, and getting the order wrong is silent.
//!
//! # Why this is a crate module and not four lines in the poll loop
//!
//! Every arm here was added because the one before it could not fire in some
//! real case, and each gap cost a boot on the one machine that has this part:
//!
//! * `RDU` alone — "the ring ran dry with a frame waiting" — is the chip's own
//!   evidence and the cheapest signal there is (the poll loop already harvests
//!   `ISR`). But a chip whose receiver has *stopped* takes nothing off the wire
//!   and therefore has nothing to report, so `RDU` cannot fire for the fault it
//!   most needs to catch.
//! * [`StallArm::Blind`] was added for the receiver that never started. It is
//!   `!rx_seen`, and the first frame to arrive retires it **for the rest of the
//!   boot**.
//! * Which leaves the case with no arm at all, and it is the one observed on
//!   2026-09-20: frames arrived (`rx=35`), the receiver then stopped, and the
//!   chip stayed quiet. `rx_seen` was true so `Blind` was retired; no frame
//!   could arrive to raise `RDU` again, so `Backpressure` never armed. The box
//!   sat deaf for twenty minutes with `kicks=0` — the resync, the kick and the
//!   full re-init all intact and unreachable — until a person walked to it.
//!   [`StallArm::Silent`] is that arm.
//!
//! # The order is the substance
//!
//! `Backpressure` outranks `Silent` because the chip's own report is better
//! evidence than the passage of time, and the two recoveries differ. `Blind`
//! is checked against `rx_seen` rather than against silence, because "nothing
//! has ever arrived" is a stronger statement than any duration. A test that
//! only ever sets one condition at a time passes under every ordering, so
//! [`tests`] pins the conflicts explicitly.

/// What, if anything, says the receiver has stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallArm {
    /// Nothing to do.
    None,
    /// The chip raised `RDU`: the ring ran dry with a frame waiting.
    Backpressure,
    /// Not one frame has arrived since bring-up.
    Blind,
    /// Frames arrived, then stopped, and the chip is not complaining.
    Silent,
}

/// The horizons. The consumer owns these numbers — they are statements about
/// its poll loop and its link, not about the chip — and this module owns only
/// what to do with them.
#[derive(Debug, Clone, Copy)]
pub struct StallPolicy {
    /// Silence that makes a *complaining* chip a stalled one.
    pub quiet_us: u64,
    /// Silence that makes a quiet, previously-working receiver a stalled one.
    ///
    /// Deliberately longer than [`Self::quiet_us`]: this arm has no evidence
    /// beyond the passage of time, and an idle link is a normal state. The
    /// cost of being wrong is one restarted receiver, which loses a frame the
    /// chip has written but the driver has not yet read.
    pub silent_us: u64,
    /// Silence between *subsequent* [`StallArm::Silent`] firings.
    ///
    /// Shorter than [`Self::silent_us`], and that asymmetry is the point: slow
    /// to first accuse a quiet link of being broken, quick to retry once it has
    /// proved itself pathological. Without it the recovery cadence is the
    /// accusation threshold, and a re-init every fourth attempt lands minutes
    /// apart on a box that is off the network the whole time.
    pub silent_retry_us: u64,
    /// Lap fallback before the clock seam exists, for `!rx_seen`.
    pub laps_unstarted: u32,
    /// Lap fallback before the clock seam exists, once frames have arrived.
    pub laps: u32,
}

/// What the loop knows on a lap that produced no frame.
#[derive(Debug, Clone, Copy)]
pub struct StallInputs {
    /// Has any frame come off the ring since bring-up?
    pub rx_seen: bool,
    /// Has the chip raised `RDU` since the last frame?
    pub backpressure: bool,
    /// Microseconds since the last frame, or `None` when there is no clock yet
    /// (early boot) or no frame yet to measure from.
    pub silent_us: Option<u64>,
    /// Consecutive frameless laps, used only while `silent_us` is `None`.
    pub idle_laps: u32,
    /// Has the [`StallArm::Silent`] arm already fired this boot?
    pub silent_fired: bool,
}

/// Decide which arm — if any — fires on this lap.
#[must_use]
pub fn decide(policy: &StallPolicy, i: &StallInputs) -> StallArm {
    // Wall clock first, laps only as the pre-clock fallback. The two must not
    // both be able to fire: a bring-up spin would otherwise kick on laps while
    // the clock says receive is healthy.
    let quiet = match i.silent_us {
        Some(us) => us >= policy.quiet_us,
        None if !i.rx_seen => i.idle_laps >= policy.laps_unstarted,
        None => i.idle_laps >= policy.laps,
    };

    // The chip's own report outranks every inference from duration, and it is
    // checked first for that reason -- including against `Silent`, whose
    // recovery is the more destructive of the two.
    if i.backpressure && quiet {
        return StallArm::Backpressure;
    }

    // "Nothing has ever arrived" is a stronger statement than any duration, so
    // it is a condition rather than a threshold. It cannot coexist with
    // `Silent`, which requires the opposite.
    if !i.rx_seen {
        return if quiet { StallArm::Blind } else { StallArm::None };
    }

    // Started, then went quiet, with the chip saying nothing. Needs a clock:
    // there is no honest way to measure "stopped" in laps once the loop's rate
    // is a scheduling decision, so before the clock seam exists this arm
    // abstains rather than guessing.
    let threshold = if i.silent_fired { policy.silent_retry_us } else { policy.silent_us };
    match i.silent_us {
        Some(us) if us >= threshold => StallArm::Silent,
        _ => StallArm::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: StallPolicy = StallPolicy {
        quiet_us: 5_000_000,
        silent_us: 60_000_000,
        silent_retry_us: 5_000_000,
        laps_unstarted: 20_000,
        laps: 2_000_000,
    };

    fn healthy() -> StallInputs {
        StallInputs {
            rx_seen: true,
            backpressure: false,
            silent_us: Some(0),
            idle_laps: 0,
            silent_fired: false,
        }
    }

    #[test]
    fn a_busy_link_is_never_a_stall() {
        assert_eq!(decide(&P, &healthy()), StallArm::None);
    }

    /// The 2026-09-20 failure, which had no arm at all: 35 frames arrived, the
    /// receiver stopped, the chip stayed quiet, and `kicks` never left zero.
    #[test]
    fn a_receiver_that_started_and_then_went_quiet_is_a_stall() {
        let i = StallInputs { silent_us: Some(60_000_000), ..healthy() };
        assert_eq!(decide(&P, &i), StallArm::Silent);
    }

    /// ...but not before the long horizon: an idle link is a normal state on
    /// this machine, and the recovery costs a frame in flight.
    #[test]
    fn a_quiet_link_below_the_silent_horizon_is_left_alone() {
        let i = StallInputs { silent_us: Some(59_999_999), ..healthy() };
        assert_eq!(decide(&P, &i), StallArm::None);
        // Note it is past `quiet_us` twelve times over and still not a stall:
        // `quiet` alone has never been sufficient and must not become so.
        assert!(59_999_999 > P.quiet_us);
    }

    #[test]
    fn once_silent_has_fired_the_retry_horizon_is_the_short_one() {
        let i = StallInputs {
            silent_us: Some(5_000_000),
            silent_fired: true,
            ..healthy()
        };
        assert_eq!(decide(&P, &i), StallArm::Silent);

        let not_yet = StallInputs { silent_us: Some(4_999_999), ..i };
        assert_eq!(decide(&P, &not_yet), StallArm::None);
    }

    #[test]
    fn rdu_plus_quiet_is_backpressure() {
        let i = StallInputs { backpressure: true, silent_us: Some(5_000_000), ..healthy() };
        assert_eq!(decide(&P, &i), StallArm::Backpressure);
    }

    #[test]
    fn rdu_without_quiet_is_not_a_stall() {
        let i = StallInputs { backpressure: true, silent_us: Some(4_999_999), ..healthy() };
        assert_eq!(decide(&P, &i), StallArm::None);
    }

    #[test]
    fn nothing_since_bringup_is_blind_once_quiet() {
        let i = StallInputs { rx_seen: false, silent_us: Some(5_000_000), ..healthy() };
        assert_eq!(decide(&P, &i), StallArm::Blind);
    }

    #[test]
    fn blind_falls_back_to_the_small_lap_count_before_the_clock_exists() {
        let i = StallInputs {
            rx_seen: false,
            silent_us: None,
            idle_laps: 20_000,
            ..healthy()
        };
        assert_eq!(decide(&P, &i), StallArm::Blind);

        let not_yet = StallInputs { idle_laps: 19_999, ..i };
        assert_eq!(decide(&P, &not_yet), StallArm::None);
    }

    /// Ordering, not a single condition: all three could be read as firing, and
    /// the chip's own evidence has to win because the recoveries differ.
    #[test]
    fn backpressure_outranks_silent_when_both_hold() {
        let i = StallInputs {
            backpressure: true,
            silent_us: Some(600_000_000),
            ..healthy()
        };
        assert_eq!(decide(&P, &i), StallArm::Backpressure);
    }

    /// `Silent` must never fire for a receiver that never started -- that case
    /// has its own arm, its own counter and its own escalation.
    #[test]
    fn silent_never_fires_before_the_first_frame() {
        let i = StallInputs {
            rx_seen: false,
            silent_us: Some(600_000_000),
            ..healthy()
        };
        assert_eq!(decide(&P, &i), StallArm::Blind);
    }

    /// Without a clock there is no honest measure of "stopped" once the loop's
    /// rate is a scheduling decision, so this arm abstains rather than guessing
    /// -- the lap count that `Blind` leans on is two orders of magnitude out
    /// here (2,000,000 laps is five and a half hours at the parked rate).
    #[test]
    fn silent_abstains_when_there_is_no_clock() {
        let i = StallInputs {
            silent_us: None,
            idle_laps: u32::MAX,
            ..healthy()
        };
        assert_eq!(decide(&P, &i), StallArm::None);
    }
}
