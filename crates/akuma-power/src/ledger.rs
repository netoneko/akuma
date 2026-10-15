//! Integrating the battery's instantaneous power into energy over time.
//!
//! [`crate::render`] reports `power_mw`, the pack's *right now* number. What a
//! before/after comparison wants — "what did this boot cost?", "what did the
//! idle change save?" — is the running total, and the meter for it already
//! exists: `current × voltage` is the pack's own power, sampled as often as
//! someone reads `/proc/power` (the rio panel does, every second). This
//! integrates those samples.
//!
//! # The rule
//!
//! **Left rectangle**: a sample's power is taken to have flowed from its own
//! timestamp until the next sample said otherwise. Battery power moves slowly
//! — the measured samples in `docs/archive/AKUMA_AMD64_ON_RYZEN_LAPTOP.md`
//! §5.8 swing between 6.9 W and 47 W across tens of seconds, not between two
//! reads a second apart — so the error is the usual trapezoid-vs-rectangle
//! slop on a smooth signal, not a step artifact. The reader who wants better
//! resolution reads more often; the kernel keeps no timer of its own for this,
//! which is deliberate: `/proc/power` already costs nothing when unread.
//!
//! Units are the battery's own: the total is kept in **micro-watt-hours** so a
//! one-second sample at laptop power (~7 W) keeps nearly all of its 1.9 mWh
//! instead of truncating to nothing, and renders as **mWh** — the same unit
//! `energy_now_mwh` speaks — with only the display truncating.
//! `mW × µs` is nanojoules; `/3.6e6` is micro-watt-hours.
//!
//! No clock, no allocation, no `unsafe` — the caller supplies the timestamp,
//! exactly as [`crate::ec`] supplies the bytes.

/// The running energy total, fed by [`sample`](Self::sample).
#[derive(Clone, Copy, Debug, Default)]
pub struct Ledger {
    last_mw: u32,
    last_us: Option<u64>,
    total_uwh: u64,
}

impl Ledger {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_mw: 0, last_us: None, total_uwh: 0 }
    }

    /// Energy integrated so far, in µWh — the unit the accumulation is exact
    /// in. A reader wanting mWh divides by 1000.
    #[must_use]
    pub const fn total_uwh(&self) -> u64 {
        self.total_uwh
    }

    /// Energy integrated so far, in mWh — the display unit, matching
    /// `energy_now_mwh`.
    #[must_use]
    pub const fn total_mwh(&self) -> u64 {
        self.total_uwh / 1000
    }

    /// Feed one sample of `power_mw` taken at monotonic `now_us`; returns the
    /// running total in mWh.
    ///
    /// The first sample only anchors. A `now_us` at or before the previous one
    /// integrates nothing — a clock that stood still must not pay energy — and
    /// leaves the later of the two timestamps anchored, so a clock that ran
    /// backwards for a read cannot make the next interval pay twice.
    pub fn sample(&mut self, power_mw: u32, now_us: u64) -> u64 {
        if let Some(at) = self.last_us {
            let dt = now_us.saturating_sub(at);
            if dt > 0 {
                let uwh = (u128::from(dt) * u128::from(self.last_mw) / 3_600_000)
                    .min(u128::from(u64::MAX)) as u64;
                self.total_uwh = self.total_uwh.saturating_add(uwh);
            }
            self.last_us = Some(at.max(now_us));
        } else {
            self.last_us = Some(now_us);
        }
        self.last_mw = power_mw;
        self.total_mwh()
    }
}

#[cfg(test)]
mod tests {
    use super::Ledger;

    #[test]
    fn constant_power_integrates_exactly() {
        // 1000 mW for exactly one hour, one sample: 1 mWh, exact.
        let mut l = Ledger::new();
        l.sample(1000, 0);
        assert_eq!(l.total_mwh(), 0, "the anchor pays nothing yet");
        assert_eq!(l.sample(1000, 3_600_000_000), 1000, "1 W × 1 h = 1 mWh");
        assert_eq!(l.total_uwh(), 1_000_000);
    }

    #[test]
    fn one_second_steps_keep_their_energy() {
        // The failure this unit choice exists for: in mWh, a 1 s step at 1 W
        // is 0.28 mWh and truncates to 0 every step — a whole hour integrates
        // to nothing. In µWh it keeps all but 0.7 %.
        let mut l = Ledger::new();
        l.sample(1000, 0);
        for s in 1..=3600u64 {
            l.sample(1000, s * 1_000_000);
        }
        assert_eq!(l.total_mwh(), 997, "3600 µWh-steps of 277 = 997 mWh, not 0");
    }

    #[test]
    fn first_sample_only_anchors() {
        let mut l = Ledger::new();
        assert_eq!(l.sample(7_000, 1_000_000), 0, "no interval yet, nothing integrated");
        assert_eq!(l.sample(7_000, 2_000_000), 7_000 * 1_000_000 / 3_600_000_000);
    }

    #[test]
    fn still_time_and_backwards_time_integrate_nothing() {
        let mut l = Ledger::new();
        l.sample(7_000, 1_000_000);
        assert_eq!(l.sample(7_000, 1_000_000), 0, "clock stood still");
        assert_eq!(l.sample(7_000, 500_000), 0, "clock ran backwards");
        // The backwards read did not re-anchor: the next forward interval is
        // measured from the later timestamp, not from the rewound one.
        let after = l.sample(7_000, 1_500_000);
        assert_eq!(after, 7_000 * 500_000 / 3_600_000_000, "paid once, from the later anchor");
    }

    #[test]
    fn power_changes_take_effect_at_the_next_sample() {
        // Left rectangle: 7 W held until the read that says otherwise.
        let mut l = Ledger::new();
        l.sample(7_000, 0);
        let total = l.sample(1_000, 3_600_000_000); // one hour later
        assert_eq!(total, 7_000, "the 7 W sample's power ran the whole hour");
        let total = l.sample(1_000, 7_200_000_000); // another hour at 1 W
        assert_eq!(total, 7_000 + 1_000);
    }

    #[test]
    fn the_simulated_cycle_charges_more_than_it_discharges() {
        // The sim's own numbers: ~7 W discharging for 60 s, ~31 W charging for
        // 60 s. The ledger must land between them and on the right side of
        // each, which is the property a broken multiply would break first.
        let mut l = Ledger::new();
        let mut prev_mw = None;
        let mut expected_uwh: u64 = 0;
        for s in 0..120u64 {
            let block = crate::sim::block(s);
            let r = crate::ec::Reading::decode(&block);
            let mw = r.power_mw();
            if let Some(prev) = prev_mw {
                // what the previous second's power should have contributed
                expected_uwh += u64::from(prev) * 1_000_000 / 3_600_000;
            }
            prev_mw = Some(mw);
            let _ = l.sample(mw, s * 1_000_000);
            // the ledger and the hand sum agree to the truncation of one step
            let got = l.total_uwh();
            assert!(got >= expected_uwh.saturating_sub(1) && got <= expected_uwh + 1, "s={s}");
        }
        let total_mwh = expected_uwh / 1000;
        assert!(
            (500..=700).contains(&total_mwh),
            "one sim cycle at 7 W then 31 W should land near 630 mWh, got {total_mwh}"
        );
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        let mut l = Ledger::new();
        l.sample(u32::MAX, 0);
        // Two intervals, each far past u64 µWh on its own.
        l.sample(u32::MAX, u64::MAX / 4);
        let total = l.sample(u32::MAX, u64::MAX / 2);
        assert_eq!(l.total_uwh(), u64::MAX, "saturating, not wrapping");
        assert_eq!(total, u64::MAX / 1000);
    }
}
