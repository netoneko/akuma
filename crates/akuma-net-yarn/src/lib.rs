//! The socket readiness wait loop, extracted as a pure state machine.
//!
//! Named for what it is: the tangle every blocking socket syscall ends up in,
//! pulled out into one thread you can follow end to end on the host.
//!
//! # Why this exists
//!
//! `akuma_net::socket::wait_until` is the loop every blocking socket syscall
//! ends in: drive the stack, check the condition, and if it still does not
//! hold, decide how to wait. That decision has accumulated five separate
//! incident-driven rules —
//!
//! - a **wake epoch** sampled before polling, because `wake_all()` drains and a
//!   wake landing mid-lap would otherwise be lost
//!   ([`AKUMA_NET_ISSUES.md` §8](../../../docs/archive/AKUMA_NET_ISSUES.md));
//! - a drain budget of **64** polls per lap;
//! - a **`fruitless_progress_rounds >= 4`** escape, added because aria2c's
//!   swarm traffic made `poll()` report progress on nearly every call, so the
//!   park branch never ran and the loop busy-spun holding the BKL until SMP=4
//!   hard-wedged (reproduced 2026-07-24);
//! - **register → re-check → park** ordering as the lost-wake correctness
//!   argument;
//! - a **3 ms backstop** so a wake that never arrives costs latency, not a hang.
//!
//! — and none of it was testable without a devbox boot. Worse, the *policy*
//! question underneath is live and has a counter-intuitive recorded answer: the
//! targeted park (`net-waker-park`) measured **slower** than the promiscuous
//! `blocking_relax` it replaced, because `wake_all` drains, so a targeted
//! waiter must re-register every lap while a `wfi` sleeper is woken by any of
//! the ~6,300 NIC interrupts per 5 s window. The root `Cargo.toml` note that
//! records this ends with a hypothesis nobody has been able to test:
//!
//! > To beat the default this needs a park that is BOTH targetable and woken by
//! > any NIC interrupt — a scheduler change (a "light sleep" state), not a net
//! > change.
//!
//! This crate makes that rankable without a scheduler change, by separating the
//! **decision** (here, pure) from the **effects** (in the kernel). It follows
//! `akuma-kacho`'s observe/decide split and `akuma-scheduler`'s
//! model-then-calibrate discipline — with the same warning attached: see
//! [`ParkKind::LightSleep`], which exists only as a model and has no kernel
//! implementation behind it.
//!
//! # What is modelled, and what is not
//!
//! Modelled: lap sequencing, the epoch guard, the drain budget, the fruitless
//! progress escape, deadline arithmetic, and which park kind is chosen.
//!
//! **Not** modelled: caches, TLBs, the BKL, scheduler queue depth, or the cost
//! of a park. `REDIS_ROUND_TRIP_STAGE_TRACE.md` §3 finds cache residency is
//! 89 % of the SMP=1 → SMP=4 gap, and nothing here can see that. This machine
//! can rank park-vs-spin policy and size the two constants; it cannot say
//! anything about per-poll cost. Keep the questions apart.

#![cfg_attr(not(test), no_std)]

/// Polls driven per lap before the condition is re-checked.
///
/// The caller stops early when `poll()` stops reporting progress, so this is a
/// ceiling, not a count. It bounds how long one lap can hold the BKL.
pub const DEFAULT_DRAIN_BUDGET: u32 = 64;

/// Consecutive laps where `poll()` reported progress but the condition stayed
/// false, after which the loop parks anyway.
///
/// Without this the `!progress` branch never runs under sustained unrelated
/// traffic and the loop busy-spins holding the BKL for the entire wait — an
/// `accept` with no timeout means forever. Reproduced 2026-07-24: baseline
/// SMP=4 hard-wedged the moment aria2c's swarm traffic started.
pub const DEFAULT_FRUITLESS_LIMIT: u32 = 4;

/// Longest a parked waiter sleeps before re-checking on its own.
///
/// A backstop for the directed wake, not the mechanism. Anchored to the
/// scheduler tick rather than `poll.rs`'s 10 ms `BLOCKING_POLL_INTERVAL_US`, so
/// a wake that never arrives is no worse than the promiscuous path it replaced.
pub const DEFAULT_BACKSTOP_US: u64 = 3_000;

/// Why a wait ended without the condition holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// A signal is pending for this thread.
    Interrupted,
    /// The caller's own timeout (`SO_RCVTIMEO` / `SO_SNDTIMEO`) expired.
    TimedOut,
}

/// How the waiter should sleep when the condition does not hold.
///
/// The first two are implemented in the kernel and selected by the
/// `net-waker-park` feature. The third is this crate's proposal. The fourth is
/// a model only — do not add a kernel arm for it without the scheduler support
/// its docs describe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkKind {
    /// Yield, then halt until **any** interrupt. Untargetable — the thread
    /// never enters `WAITING`, so a socket's `wake_all()` walks an empty list —
    /// but under load the NIC raises one interrupt every ~0.8 ms, so the wake
    /// is imprecise and plentiful. Today's default, and the one to beat.
    Promiscuous,
    /// Register the thread's waker on the socket, re-check, then park until the
    /// backstop. Targetable but **lossy**: `wake_all` drains the list, so the
    /// registration survives exactly one wake. Measured *worse* than
    /// [`Self::Promiscuous`]: parks 3,918 → 2,565 but µs/park 1,172 → 1,787.
    Targeted,
    /// Register the thread's waker on the **smoltcp socket itself**, so the
    /// wake fires from inside `process_tcp` at the state transition rather than
    /// from a list walked after `poll()` releases `NETWORK`.
    ///
    /// Still one-shot — `smoltcp::socket::WakerRegistration::wake` also
    /// `take()`s — but the window it can be lost in is a few instructions
    /// instead of a whole 64-poll drain.
    DirectWaker,
    /// Targetable **and** woken by any NIC interrupt. The hypothesis the root
    /// `Cargo.toml` names as the only thing that can beat
    /// [`Self::Promiscuous`].
    ///
    /// **Model only.** There is no kernel implementation; it needs a scheduler
    /// "light sleep" state. Selecting it in a real `wait_until` is a bug.
    LightSleep,
}

impl ParkKind {
    /// Does this kind require the waiter to announce itself before parking?
    ///
    /// [`Self::Promiscuous`] does not — that is exactly its weakness and its
    /// strength. Everything else must register **before** the final condition
    /// re-check, or a wake landing in between is lost and the wait hangs until
    /// the backstop.
    #[must_use]
    pub const fn needs_registration(self) -> bool {
        !matches!(self, Self::Promiscuous)
    }

    /// Is a wake for this kind delivered to the thread specifically, rather
    /// than by ending an untargeted halt?
    #[must_use]
    pub const fn is_targeted(self) -> bool {
        !matches!(self, Self::Promiscuous)
    }

    /// Does an unrelated interrupt end this park?
    ///
    /// True for the two kinds that halt rather than deschedule. The whole
    /// recorded surprise in `net-waker-park`'s measurement is that this
    /// property, not targeting, is what the workload wanted.
    #[must_use]
    pub const fn woken_by_any_irq(self) -> bool {
        matches!(self, Self::Promiscuous | Self::LightSleep)
    }
}

/// Tunables. Split out so a model can sweep them without touching the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitPolicy {
    /// Polls per lap. See [`DEFAULT_DRAIN_BUDGET`].
    pub drain_budget: u32,
    /// Fruitless-progress laps before parking anyway. See
    /// [`DEFAULT_FRUITLESS_LIMIT`].
    pub fruitless_limit: u32,
    /// Backstop sleep. Ignored by [`ParkKind::Promiscuous`], which has no
    /// deadline of its own.
    pub backstop_us: u64,
    /// How to sleep.
    pub park: ParkKind,
}

impl WaitPolicy {
    /// The shipping default: promiscuous halt, 64-poll drain, 4 fruitless laps.
    #[must_use]
    pub const fn promiscuous() -> Self {
        Self {
            drain_budget: DEFAULT_DRAIN_BUDGET,
            fruitless_limit: DEFAULT_FRUITLESS_LIMIT,
            backstop_us: DEFAULT_BACKSTOP_US,
            park: ParkKind::Promiscuous,
        }
    }

    /// What the `net-waker-park` feature selects today.
    #[must_use]
    pub const fn targeted() -> Self {
        Self { park: ParkKind::Targeted, ..Self::promiscuous() }
    }

    /// This crate's proposal: wakers registered on the smoltcp socket.
    #[must_use]
    pub const fn direct_waker() -> Self {
        Self { park: ParkKind::DirectWaker, ..Self::promiscuous() }
    }
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self::promiscuous()
    }
}

/// What the caller observed after driving the stack for one lap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// Uptime now, in microseconds.
    pub now_us: u64,
    /// `smoltcp_net::poll_count()` sampled **after** the drain. Compared
    /// against the value [`WaitMachine::lap_start`] was given to detect a peer
    /// advancing the stack while this lap was checking.
    pub poll_epoch: u64,
    /// Did any poll in this lap report `SocketStateChanged`?
    pub progress: bool,
    /// Does the caller's condition hold now?
    pub condition_met: bool,
    /// Is a signal pending for this thread?
    pub interrupted: bool,
}

/// What the caller should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStep {
    /// The condition holds. Return `Ok`.
    Ready,
    /// Give up and return this errno.
    Failed(WaitError),
    /// Re-drain immediately instead of parking. The reason distinguishes the
    /// two rules that produce it and is worth surfacing separately — see
    /// [`RelapReason`].
    Relap(RelapReason),
    /// Sleep. `deadline_us` is absolute and already clamped to the caller's own
    /// timeout, so a `SO_RCVTIMEO` cannot fire a whole backstop late.
    ///
    /// When `kind.needs_registration()`, the caller **must** register its waker,
    /// then re-check the condition, and only then sleep. That order is the
    /// lost-wake correctness argument, not an optimisation.
    Park { kind: ParkKind, deadline_us: u64 },
}

/// Why a lap is being re-run instead of parked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelapReason {
    /// A peer advanced the stack while this lap was checking, so the state the
    /// condition was judged against is already stale. Parking on it is how a
    /// wake gets missed and the wait lands on the backstop. Surfaced as
    /// `epoch_saves` — zero means the window this closes does not exist in
    /// practice.
    EpochMoved,
    /// `poll()` made progress, but not the progress this waiter needs, and the
    /// fruitless-lap budget is not yet spent. Spinning is deliberate here: the
    /// condition is usually about to hold.
    FruitlessProgress,
}

/// Counters a caller can surface through `[NICSTAT]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaitStats {
    /// Laps run.
    pub laps: u64,
    /// Times [`WaitStep::Relap`] fired — the epoch guard catching a peer's
    /// progress. Zero means the window it closes does not exist in practice.
    pub epoch_saves: u64,
    /// Parks issued.
    pub parks: u64,
    /// Parks taken from the fruitless-progress escape rather than from an
    /// idle lap. A high share means the BKL-starvation guard is load-bearing
    /// on this workload, not a rare backstop.
    pub fruitless_parks: u64,
}

/// The `wait_until` decision loop, with no I/O in it.
///
/// Drive it one lap at a time: [`lap_start`](Self::lap_start) before polling,
/// [`lap_end`](Self::lap_end) after evaluating the condition.
#[derive(Debug, Clone)]
pub struct WaitMachine {
    start_us: u64,
    timeout_us: Option<u64>,
    policy: WaitPolicy,
    fruitless_rounds: u32,
    lap_epoch: u64,
    stats: WaitStats,
}

impl WaitMachine {
    /// Begin a wait that started at `start_us`, optionally bounded by
    /// `timeout_us` microseconds from then.
    #[must_use]
    pub const fn new(start_us: u64, timeout_us: Option<u64>, policy: WaitPolicy) -> Self {
        Self {
            start_us,
            timeout_us,
            policy,
            fruitless_rounds: 0,
            lap_epoch: 0,
            stats: WaitStats {
                laps: 0,
                epoch_saves: 0,
                parks: 0,
                fruitless_parks: 0,
            },
        }
    }

    /// Counters accumulated so far.
    #[must_use]
    pub const fn stats(&self) -> WaitStats {
        self.stats
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> WaitPolicy {
        self.policy
    }

    /// Open a lap. `poll_epoch` must be sampled **before** driving the stack —
    /// that ordering is what makes the guard in [`lap_end`](Self::lap_end)
    /// mean anything. Returns the drain budget for this lap.
    pub const fn lap_start(&mut self, poll_epoch: u64) -> u32 {
        self.lap_epoch = poll_epoch;
        self.stats.laps += 1;
        self.policy.drain_budget
    }

    /// Close a lap and decide what happens next.
    ///
    /// Order matters and mirrors the shipped loop exactly: condition, then
    /// signal, then timeout, then the park decision. Checking the condition
    /// first means a wait that is already satisfied never reports `EINTR` or
    /// `ETIMEDOUT` for work it actually completed.
    pub fn lap_end(&mut self, obs: &Observation) -> WaitStep {
        if obs.condition_met {
            return WaitStep::Ready;
        }
        if obs.interrupted {
            return WaitStep::Failed(WaitError::Interrupted);
        }
        if self.expired(obs.now_us) {
            return WaitStep::Failed(WaitError::TimedOut);
        }

        if obs.progress {
            // poll() moved, but not in the direction we need. Bound the hold:
            // after a few fruitless laps, park anyway.
            self.fruitless_rounds = self.fruitless_rounds.saturating_add(1);
            if self.fruitless_rounds < self.policy.fruitless_limit {
                return WaitStep::Relap(RelapReason::FruitlessProgress);
            }
            self.stats.fruitless_parks += 1;
            return self.park(obs.now_us);
        }

        self.fruitless_rounds = 0;

        // Someone ELSE advanced the stack while this lap was checking. Our own
        // `progress` is false because our polls found nothing, but the state we
        // just judged the condition against is already stale.
        if obs.poll_epoch != self.lap_epoch {
            self.stats.epoch_saves += 1;
            return WaitStep::Relap(RelapReason::EpochMoved);
        }

        self.park(obs.now_us)
    }

    /// Has the caller's own timeout passed?
    ///
    /// Saturating, so a clock that appears to run backwards across a host
    /// deschedule cannot manufacture an `ETIMEDOUT`.
    #[must_use]
    pub const fn expired(&self, now_us: u64) -> bool {
        match self.timeout_us {
            Some(t) => now_us.saturating_sub(self.start_us) > t,
            None => false,
        }
    }

    /// Absolute deadline for a park at `now_us`: the backstop, never past the
    /// caller's own timeout.
    #[must_use]
    pub const fn park_deadline(&self, now_us: u64) -> u64 {
        let backstop = now_us.saturating_add(self.policy.backstop_us);
        match self.timeout_us {
            Some(t) => {
                let hard = self.start_us.saturating_add(t);
                if hard < backstop { hard } else { backstop }
            }
            None => backstop,
        }
    }

    fn park(&mut self, now_us: u64) -> WaitStep {
        self.stats.parks += 1;
        WaitStep::Park {
            kind: self.policy.park,
            deadline_us: self.park_deadline(now_us),
        }
    }
}

#[cfg(test)]
mod tests;
