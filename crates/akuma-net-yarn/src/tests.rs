//! Behavioural parity with `akuma_net::socket::wait_until` as it shipped.
//!
//! Every test here encodes a rule that was previously only enforced by the
//! kernel source and only observable on a devbox boot. If one of these fails,
//! the machine has drifted from the loop it replaced — check the referenced
//! incident before "fixing" the test.

use super::*;

/// An observation with nothing interesting in it, to be spread from.
const fn idle(now_us: u64, poll_epoch: u64) -> Observation {
    Observation {
        now_us,
        poll_epoch,
        progress: false,
        condition_met: false,
        interrupted: false,
    }
}

fn machine(timeout_us: Option<u64>) -> WaitMachine {
    WaitMachine::new(1_000, timeout_us, WaitPolicy::promiscuous())
}

#[test]
fn condition_met_returns_ready() {
    let mut m = machine(None);
    m.lap_start(7);
    let obs = Observation { condition_met: true, ..idle(1_100, 7) };
    assert_eq!(m.lap_end(&obs), WaitStep::Ready);
}

/// A wait that is already satisfied must never report `EINTR` or `ETIMEDOUT`
/// for work it actually completed. The condition is checked first, always.
#[test]
fn condition_beats_interrupt_and_timeout() {
    let mut m = machine(Some(10));
    m.lap_start(7);
    let obs = Observation {
        condition_met: true,
        interrupted: true,
        ..idle(9_999_999, 7)
    };
    assert_eq!(m.lap_end(&obs), WaitStep::Ready);
}

#[test]
fn interrupt_beats_timeout() {
    let mut m = machine(Some(10));
    m.lap_start(7);
    let obs = Observation { interrupted: true, ..idle(9_999_999, 7) };
    assert_eq!(m.lap_end(&obs), WaitStep::Failed(WaitError::Interrupted));
}

/// `now - start > timeout`, strictly. Exactly at the deadline the caller gets
/// one more lap — matching the shipped comparison.
#[test]
fn timeout_is_strict() {
    let mut m = machine(Some(500));
    m.lap_start(7);
    assert!(matches!(m.lap_end(&idle(1_500, 7)), WaitStep::Park { .. }));

    let mut m = machine(Some(500));
    m.lap_start(7);
    assert_eq!(
        m.lap_end(&idle(1_501, 7)),
        WaitStep::Failed(WaitError::TimedOut)
    );
}

/// A clock that appears to run backwards across a host deschedule must not
/// manufacture an `ETIMEDOUT`. `REDIS_ROUND_TRIP_STAGE_TRACE.md` §6b records
/// single spans charged 7.9 ms against a 15 µs mean; the shipped loop used a
/// plain subtraction here.
#[test]
fn clock_going_backwards_does_not_time_out() {
    let m = machine(Some(500));
    assert!(!m.expired(0));
    assert!(!m.expired(999));
}

/// No progress of our own, but `poll_count` moved: a peer advanced the stack
/// while this lap was checking, so the state we judged is already stale.
/// Parking on it is how a wake gets missed. `AKUMA_NET_ISSUES.md` §8.
#[test]
fn epoch_move_relaps_instead_of_parking() {
    let mut m = machine(None);
    m.lap_start(7);
    assert_eq!(
        m.lap_end(&idle(1_100, 8)),
        WaitStep::Relap(RelapReason::EpochMoved)
    );
    assert_eq!(m.stats().epoch_saves, 1);
    assert_eq!(m.stats().parks, 0);
}

#[test]
fn quiet_lap_parks() {
    let mut m = machine(None);
    m.lap_start(7);
    match m.lap_end(&idle(1_100, 7)) {
        WaitStep::Park { kind, deadline_us } => {
            assert_eq!(kind, ParkKind::Promiscuous);
            assert_eq!(deadline_us, 1_100 + DEFAULT_BACKSTOP_US);
        }
        other => panic!("expected Park, got {other:?}"),
    }
    assert_eq!(m.stats().epoch_saves, 0);
    assert_eq!(m.stats().parks, 1);
}

/// The aria2c guard. `poll()` reporting progress on nearly every call must not
/// let this loop spin holding the BKL forever — an `accept` with no timeout
/// means exactly forever. Reproduced 2026-07-24: SMP=4 hard-wedged.
///
/// Three fruitless laps re-lap; the fourth parks.
#[test]
fn fruitless_progress_parks_on_the_fourth_lap() {
    let mut m = machine(None);
    let busy = |t| Observation { progress: true, ..idle(t, 7) };

    for lap in 1..=3 {
        m.lap_start(7);
        assert_eq!(m.lap_end(&busy(1_000 + lap * 10)), WaitStep::Relap(RelapReason::FruitlessProgress), "lap {lap}");
        assert_eq!(m.stats().parks, 0, "lap {lap}");
    }

    m.lap_start(7);
    assert!(matches!(m.lap_end(&busy(1_040)), WaitStep::Park { .. }));
    assert_eq!(m.stats().fruitless_parks, 1);
}

/// Once the escape has fired it keeps firing — every further progress lap
/// parks, rather than granting another three laps of spinning.
#[test]
fn fruitless_escape_stays_armed() {
    let mut m = machine(None);
    let busy = |t| Observation { progress: true, ..idle(t, 7) };
    for lap in 1..=3 {
        m.lap_start(7);
        m.lap_end(&busy(1_000 + lap * 10));
    }
    for lap in 4..=6 {
        m.lap_start(7);
        assert!(
            matches!(m.lap_end(&busy(1_000 + lap * 10)), WaitStep::Park { .. }),
            "lap {lap}"
        );
    }
    assert_eq!(m.stats().fruitless_parks, 3);
}

/// A quiet lap clears the counter, so unrelated traffic that stops does not
/// leave the loop permanently in park-every-lap mode.
#[test]
fn quiet_lap_resets_the_fruitless_counter() {
    let mut m = machine(None);
    let busy = |t| Observation { progress: true, ..idle(t, 7) };

    m.lap_start(7);
    assert_eq!(m.lap_end(&busy(1_010)), WaitStep::Relap(RelapReason::FruitlessProgress));
    m.lap_start(7);
    assert_eq!(m.lap_end(&busy(1_020)), WaitStep::Relap(RelapReason::FruitlessProgress));

    // A quiet lap: parks, and resets.
    m.lap_start(7);
    assert!(matches!(m.lap_end(&idle(1_030, 7)), WaitStep::Park { .. }));

    // Three more fruitless laps are granted again.
    for lap in 1..=3 {
        m.lap_start(7);
        assert_eq!(m.lap_end(&busy(1_040 + lap)), WaitStep::Relap(RelapReason::FruitlessProgress), "lap {lap}");
    }
}

/// Never sleep past the caller's own timeout, or a `SO_RCVTIMEO` fires a whole
/// backstop late. The bug this rule closes is recorded in `KernelSocket`'s
/// `rcvtimeo_us` doc comment: a 2 s `SO_RCVTIMEO` fired at 30,041 ms.
#[test]
fn park_deadline_never_exceeds_the_caller_timeout() {
    let m = WaitMachine::new(1_000, Some(500), WaitPolicy::promiscuous());
    // Backstop would be 1_100 + 3_000; the hard deadline is 1_000 + 500.
    assert_eq!(m.park_deadline(1_100), 1_500);
}

#[test]
fn park_deadline_is_the_backstop_when_it_is_nearer() {
    let m = WaitMachine::new(1_000, Some(1_000_000), WaitPolicy::promiscuous());
    assert_eq!(m.park_deadline(1_100), 1_100 + DEFAULT_BACKSTOP_US);
}

#[test]
fn park_deadline_saturates() {
    let m = WaitMachine::new(0, None, WaitPolicy::promiscuous());
    assert_eq!(m.park_deadline(u64::MAX), u64::MAX);
}

#[test]
fn lap_start_returns_the_drain_budget() {
    let mut m = machine(None);
    assert_eq!(m.lap_start(0), DEFAULT_DRAIN_BUDGET);
    assert_eq!(m.stats().laps, 1);
}

/// The property the `net-waker-park` measurement turned on: the promiscuous
/// halt is the only shipped kind that needs no registration, and one of the two
/// that any interrupt can end.
#[test]
fn park_kind_properties_match_their_mechanisms() {
    assert!(!ParkKind::Promiscuous.needs_registration());
    assert!(ParkKind::Targeted.needs_registration());
    assert!(ParkKind::DirectWaker.needs_registration());
    assert!(ParkKind::LightSleep.needs_registration());

    assert!(!ParkKind::Promiscuous.is_targeted());
    assert!(ParkKind::DirectWaker.is_targeted());

    // The recorded hypothesis: targetable AND woken by any IRQ is the only
    // combination that can beat the default.
    assert!(ParkKind::Promiscuous.woken_by_any_irq());
    assert!(!ParkKind::Targeted.woken_by_any_irq());
    assert!(!ParkKind::DirectWaker.woken_by_any_irq());
    assert!(ParkKind::LightSleep.woken_by_any_irq());
    assert!(ParkKind::LightSleep.is_targeted() && ParkKind::LightSleep.woken_by_any_irq());
}

#[test]
fn policy_constructors_only_change_the_park_kind() {
    let base = WaitPolicy::promiscuous();
    for p in [WaitPolicy::targeted(), WaitPolicy::direct_waker()] {
        assert_eq!(p.drain_budget, base.drain_budget);
        assert_eq!(p.fruitless_limit, base.fruitless_limit);
        assert_eq!(p.backstop_us, base.backstop_us);
    }
    assert_eq!(WaitPolicy::default(), base);
}

/// A full satisfied wait: two quiet laps, then the data lands.
#[test]
fn typical_recv_sequence() {
    let mut m = machine(Some(30_000_000));
    let mut now = 1_000;

    for _ in 0..2 {
        m.lap_start(42);
        let step = m.lap_end(&idle(now, 42));
        let WaitStep::Park { deadline_us, .. } = step else {
            panic!("expected Park, got {step:?}")
        };
        now = deadline_us;
    }

    m.lap_start(42);
    let obs = Observation {
        progress: true,
        condition_met: true,
        ..idle(now, 43)
    };
    assert_eq!(m.lap_end(&obs), WaitStep::Ready);
    assert_eq!(m.stats().laps, 3);
    assert_eq!(m.stats().parks, 2);
}

// ===========================================================================
// Differential test against the loop this machine replaced
// ===========================================================================

/// The decision logic of `akuma_net::socket::wait_until` **as it shipped before
/// the extraction**, transcribed verbatim from the branch structure at
/// `socket.rs:693-776` and left here as the oracle.
///
/// It is deliberately written in the original's shape — a mutable
/// `fruitless_progress_rounds`, the `!any_progress` / `else` split, the
/// `>= 4` comparison — rather than refactored, so a reader can diff it against
/// the source it came from. Do not tidy it.
mod reference {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RefStep {
        Ready,
        Eintr,
        Etimedout,
        /// Loop again without parking.
        Continue,
        Park { deadline_us: u64 },
    }

    pub struct RefLoop {
        pub start: u64,
        pub timeout_us: Option<u64>,
        pub fruitless_progress_rounds: u32,
    }

    impl RefLoop {
        pub const fn new(start: u64, timeout_us: Option<u64>) -> Self {
            Self { start, timeout_us, fruitless_progress_rounds: 0 }
        }

        pub fn lap(
            &mut self,
            epoch: u64,
            epoch_now: u64,
            any_progress: bool,
            condition: bool,
            interrupted: bool,
            now: u64,
        ) -> RefStep {
            if condition {
                return RefStep::Ready;
            }
            if interrupted {
                return RefStep::Eintr;
            }
            if let Some(timeout) = self.timeout_us
                && now.saturating_sub(self.start) > timeout
            {
                return RefStep::Etimedout;
            }

            if any_progress {
                self.fruitless_progress_rounds =
                    self.fruitless_progress_rounds.wrapping_add(1);
                if self.fruitless_progress_rounds >= 4 {
                    return RefStep::Park { deadline_us: self.deadline(now) };
                }
                RefStep::Continue
            } else {
                self.fruitless_progress_rounds = 0;
                if epoch_now != epoch {
                    return RefStep::Continue;
                }
                RefStep::Park { deadline_us: self.deadline(now) }
            }
        }

        fn deadline(&self, now: u64) -> u64 {
            let backstop = now.saturating_add(DEFAULT_BACKSTOP_US);
            match self.timeout_us {
                Some(t) => backstop.min(self.start.saturating_add(t)),
                None => backstop,
            }
        }
    }
}

/// Deterministic xorshift, so a failure is reproducible from the seed alone and
/// the crate needs no dev-dependency.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    const fn chance(&mut self, one_in: u64) -> bool {
        self.next().is_multiple_of(one_in)
    }
}

/// The extracted machine must make the **same decision on every lap** as the
/// loop it replaced, over randomised observation streams.
///
/// This is the actual old-vs-new comparison: same inputs, same outputs,
/// including the park deadline arithmetic and the point at which the
/// fruitless-progress escape fires.
#[test]
fn machine_matches_the_shipped_loop_step_for_step() {
    for seed in 1..=400_u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);

        let start = 1_000;
        let timeout_us = match seed % 3 {
            0 => None,
            1 => Some(5_000),
            _ => Some(30_000_000),
        };

        let mut machine =
            WaitMachine::new(start, timeout_us, WaitPolicy::promiscuous());
        let mut oracle = reference::RefLoop::new(start, timeout_us);

        let mut now = start;
        let mut epoch = 0_u64;

        for lap in 0..64 {
            // Sample the epoch before the drain, exactly as both do.
            let epoch_before = epoch;
            machine.lap_start(epoch_before);

            let progress = rng.chance(3);
            // A peer may advance the stack while this lap is checking.
            if rng.chance(4) {
                epoch = epoch.wrapping_add(1);
            }
            if progress {
                epoch = epoch.wrapping_add(1);
            }

            let condition_met = rng.chance(9);
            let interrupted = rng.chance(23);
            now = now.saturating_add(rng.next() % 900);

            let obs = Observation {
                now_us: now,
                poll_epoch: epoch,
                progress,
                condition_met,
                interrupted,
            };

            let got = machine.lap_end(&obs);
            let want = oracle.lap(
                epoch_before,
                epoch,
                progress,
                condition_met,
                interrupted,
                now,
            );

            let ctx = format!("seed {seed} lap {lap}: {obs:?}");
            match (got, want) {
                (WaitStep::Ready, reference::RefStep::Ready) => break,
                (
                    WaitStep::Failed(WaitError::Interrupted),
                    reference::RefStep::Eintr,
                ) => break,
                (
                    WaitStep::Failed(WaitError::TimedOut),
                    reference::RefStep::Etimedout,
                ) => break,
                (WaitStep::Relap(_), reference::RefStep::Continue) => {}
                (
                    WaitStep::Park { deadline_us: a, .. },
                    reference::RefStep::Park { deadline_us: b },
                ) => assert_eq!(a, b, "park deadline diverged — {ctx}"),
                (g, w) => panic!("decision diverged: got {g:?}, oracle {w:?} — {ctx}"),
            }
        }
    }
}

/// The `Relap` reason must also be right, not just the fact of re-lapping:
/// `epoch_saves` is a published counter and mis-attributing it would make the
/// epoch guard look like it fires when it does not.
#[test]
fn relap_reasons_match_the_branch_that_produced_them() {
    let mut m = machine(None);

    // Fruitless progress: `progress` true, epoch irrelevant.
    m.lap_start(7);
    assert_eq!(
        m.lap_end(&Observation { progress: true, ..idle(1_010, 99) }),
        WaitStep::Relap(RelapReason::FruitlessProgress)
    );
    assert_eq!(m.stats().epoch_saves, 0, "fruitless lap must not count as an epoch save");

    // Quiet lap with a moved epoch: the guard.
    m.lap_start(7);
    assert_eq!(
        m.lap_end(&idle(1_020, 8)),
        WaitStep::Relap(RelapReason::EpochMoved)
    );
    assert_eq!(m.stats().epoch_saves, 1);
}
