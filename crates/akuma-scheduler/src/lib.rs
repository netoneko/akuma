//! Discrete-event simulator for Akuma's scheduler placement policy and the
//! netpoll wake policy.
//!
//! # Why this exists
//!
//! `docs/archive/CROSS_CORE_THREAD_COLLAPSE.md` left two open items that are
//! both *policy* questions, and both expensive to answer on hardware — every
//! candidate costs a kernel build, a devbox boot, and a llama.cpp sweep, with
//! the wall-clock variance recorded in `project_fpcache_evict_hot_pages`
//! (+-4x) sitting on top of the answer:
//!
//! 1. **Placement.** Today's scheduler is one global round-robin with a bounded
//!    displacement bypass (`crates/akuma-exec/src/threading/mod.rs`, the
//!    `displacement_bypass` closure). Would spreading the disturbance evenly
//!    across threads — same single queue, better placement, "a little k8s
//!    control plane" — beat it, and by how much?
//! 2. **Netpoll.** The BSP netpoll loop occupies ~93-97% of one core forever.
//!    Should its wake rate track measured traffic instead of the timer tick?
//!
//! This crate answers both *cheaply and wrongly-but-usefully*: it is a model,
//! not the kernel. Its one claim to credibility is the calibration check in
//! [`scenarios::calibration`] — the same model, unchanged, must reproduce the
//! measured shape of the real system (`-t 3` at peak, `-t 4` collapsing) before
//! any of its predictions are worth reading.
//!
//! # What is modelled
//!
//! - `cores` CPUs, a global run queue, and a periodic timer tick.
//! - A **barrier-synchronous compute group** of N threads, which is what makes
//!   this workload pathological: ggml's threads do a slice of work, then wait
//!   at a barrier for every peer. They spin for a budget first and only then
//!   futex-park, so a displaced thread does not merely lose its own time — it
//!   stalls the entire group, and the amplification *emerges* from the model
//!   rather than being an input to it.
//! - A **netpoll thread** woken by packet arrivals and/or a periodic timer.
//!
//! # What is NOT modelled
//!
//! Caches, TLBs, memory bandwidth, the BKL, and any EL1 excursion. So the
//! simulator can rank placement policies and size a netpoll wake period; it
//! **cannot** predict tok/s. Read ratios from it, never absolutes.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

/// Simulation timestep. Fine enough to resolve a barrier hand-off, coarse
/// enough that a 10 s scenario is a million steps and runs instantly.
pub const US_PER_STEP: u64 = 10;

// ============================================================================
// Configuration
// ============================================================================

/// How the scheduler places runnable work on cores at each timer tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchedPolicy {
    /// Pre-2026-08-19 behaviour: on every tick, any core running a thread
    /// gives it up if any other non-idle thread is READY. No notion of idle
    /// cores, no immunity.
    RoundRobin,

    /// Today's kernel: a thread doing real work may decline displacement while
    /// an idle peer core exists to take the queued work instead, for at most
    /// `ticks - 1` consecutive ticks, then rotates unconditionally.
    Immunity { ticks: u32 },

    /// The proposal: same single global queue, smarter placement.
    ///
    /// Three differences from [`SchedPolicy::Immunity`]:
    /// 1. **At most one core is disturbed per tick.** Immunity is per-thread,
    ///    so at `N+1` threads on `N` cores several cores can rotate on the same
    ///    tick; each one is a separate barrier stall for the group.
    /// 2. **The victim is the thread that has been disturbed least so far**
    ///    (lowest cumulative displaced time), not whoever the rotation points
    ///    at — the even-spreading part.
    /// 3. **Displacement only happens when someone is actually starving**: a
    ///    READY thread that has waited longer than `starvation_us`. With free
    ///    cores or a satisfied queue, nothing moves at all.
    Spread {
        /// Starvation bound for a throughput-class thread (the compute group).
        /// Longer is better for them: every displacement is a barrier stall.
        starvation_us: u64,
        /// Starvation bound for a **latency-class** thread — netpoll. Without
        /// a separate, much shorter bound, "spread evenly" means the compute
        /// group's starvation bound also governs packet service, and the
        /// governor buys its throughput by holding packets for milliseconds.
        /// This is the QoS-class distinction, and the simulation shows it is
        /// not optional.
        latency_starvation_us: u64,
    },

    /// Hard affinity: each thread gets a home core at creation (round-robin)
    /// and never runs anywhere else. Threads sharing a home core still take
    /// turns on the tick; they simply cannot migrate to an idle peer.
    ///
    /// The appeal is obvious — no migration means no displacement means no
    /// barrier stall. The catch is that at `N+1` threads on `N` cores, one core
    /// is permanently doubled up, and a barrier group runs at the speed of its
    /// slowest member, so the *whole group* inherits that core's share. Whether
    /// that trade pays depends entirely on how much CPU the co-tenant wants —
    /// which is why the traffic sweep matters more here than anywhere else.
    Pinned,
}

/// How the netpoll thread decides when to wake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetpollPolicy {
    /// Today: the loop halts in WFI and the periodic timer tick wakes it, so it
    /// runs once per tick whether or not a packet arrived.
    EveryTick,

    /// The proposal: an RX interrupt still wakes it immediately (so arrival
    /// latency is untouched), but the *periodic* wake backs off toward
    /// `idle_period_us` when the traffic seen over the trailing `window_us` is
    /// low, and tightens to the tick when it is high.
    TrafficAdaptive {
        /// Trailing window over which packets are counted. The user's "per 10s".
        window_us: u64,
        /// Periodic wake period at zero traffic.
        idle_period_us: u64,
        /// Packets per second at or above which the period collapses to one tick.
        busy_pps: u64,
    },
}

/// What a *woken* thread has to wait for before it can get a core.
///
/// This is `CROSS_CORE_THREAD_COLLAPSE.md` §2.3 — "a woken thread joins the
/// BACK of the rotation" — expressed as a switch, so the model can be run with
/// and without it and the difference attributed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakePlacement {
    /// `ThreadWaker::wake` rings a scheduler SGI and the woken thread takes any
    /// free core at once. The idealised path.
    Immediate,
    /// The wake marks the thread READY but nothing runs the placement pass
    /// until the next timer tick, so the thread waits out up to a full tick
    /// even with a core sitting idle. The "eligibility is not execution"
    /// short-sleep floor.
    NextTick,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub cores: usize,
    pub tick_us: u64,
    pub sim_us: u64,

    /// Compute threads in the barrier group (`llama-bench -t N`).
    pub compute_threads: usize,
    /// CPU time the group as a whole needs between two barriers. It is **split
    /// across the threads** (`total / N`), because that is what `-t N` does to
    /// a matmul — without the split, adding threads could never help and the
    /// model would report a flat line for any policy.
    pub total_phase_work_us: u64,
    /// How long a thread spins at the barrier before it futex-parks. Spinning
    /// burns its core; parking frees the core but costs a wake to come back.
    pub barrier_spin_us: u64,
    /// SGI + scheduler cost to make a parked thread runnable again. Queueing on
    /// top of this is emergent, not an input.
    pub wake_latency_us: u64,

    /// CPU time one netpoll iteration costs.
    pub netpoll_work_us: u64,
    /// Steady packet arrival rate.
    pub traffic_pps: u64,

    pub sched: SchedPolicy,
    pub netpoll: NetpollPolicy,
    pub wake: WakePlacement,
}

impl Config {
    /// CPU time *one* thread needs between two barriers.
    #[must_use]
    pub const fn phase_work_us(&self) -> u64 {
        self.total_phase_work_us / self.compute_threads as u64
    }

    /// The devbox shape the llama.cpp measurements were taken on: SMP=4,
    /// 1 ms tick, `qwen3.5-0.8b-q4` decode, an idle SSH session for traffic.
    ///
    /// `phase_work_us` and `barrier_spin_us` are the two fitted parameters —
    /// see [`crate::scenarios::calibration`] for what they were fitted against
    /// and how far the fit can be trusted.
    #[must_use]
    pub fn devbox(compute_threads: usize) -> Self {
        Self {
            cores: 4,
            tick_us: 1_000,
            sim_us: 10_000_000,
            compute_threads,
            total_phase_work_us: 800,
            barrier_spin_us: 100,
            wake_latency_us: 60,
            netpoll_work_us: 120,
            traffic_pps: 20,
            sched: SchedPolicy::Immunity { ticks: 5 },
            netpoll: NetpollPolicy::EveryTick,
            wake: WakePlacement::Immediate,
        }
    }
}

// ============================================================================
// Threads
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Compute,
    Netpoll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Wants a core.
    Ready,
    /// Has a core (see `Sim::on_core`).
    Running,
    /// Off-CPU until `wake_at`, or until an event makes it Ready.
    Blocked,
}

#[derive(Clone, Debug)]
struct Thread {
    kind: Kind,
    state: State,
    /// Core index, when Running.
    core: Option<usize>,
    /// Remaining CPU time in the current work phase.
    work_left_us: u64,
    /// Compute only: at the barrier, still spinning, with this much budget left.
    spinning: bool,
    spin_left_us: u64,
    /// Blocked threads become Ready at this time.
    wake_at: u64,
    /// When this thread last became Ready without a core (for starvation).
    ready_since: u64,
    /// Cumulative time this thread was Ready-but-not-running. The
    /// even-spreading metric.
    displaced_us: u64,
    /// Consecutive ticks this thread has declined displacement.
    immunity_used: u32,
    /// Total CPU consumed, for the utilization report.
    cpu_us: u64,
    /// Home core under [`SchedPolicy::Pinned`]; ignored by every other policy.
    home: usize,
}

impl Thread {
    fn new(kind: Kind, work_left_us: u64) -> Self {
        Self {
            kind,
            state: State::Ready,
            core: None,
            work_left_us,
            spinning: false,
            spin_left_us: 0,
            wake_at: 0,
            ready_since: 0,
            displaced_us: 0,
            immunity_used: 0,
            cpu_us: 0,
            home: 0,
        }
    }
}

// ============================================================================
// Results
// ============================================================================

#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Barriers the compute group completed — the tok/s proxy.
    pub iterations: u64,
    /// Iterations per simulated second.
    pub iters_per_sec: f64,
    /// Mean wall time between barrier completions.
    pub iter_mean_us: f64,
    /// Standard deviation of that interval. "Stable output" is this being small.
    pub iter_stddev_us: f64,
    /// 99th percentile interval — the stall tail.
    pub iter_p99_us: u64,
    /// Involuntary displacements of a compute thread.
    pub compute_preemptions: u64,
    /// Times a compute thread exhausted its spin budget and had to park.
    pub barrier_parks: u64,
    /// Fraction of one core the netpoll thread consumed.
    pub netpoll_core_frac: f64,
    /// Times netpoll ran.
    pub netpoll_wakes: u64,
    /// Mean delay from packet arrival to netpoll servicing it.
    pub packet_latency_mean_us: f64,
    /// Worst such delay.
    pub packet_latency_max_us: u64,
    /// Packets that arrived.
    pub packets: u64,
    /// Total compute CPU delivered, as a fraction of `cores * sim_us`.
    pub compute_core_frac: f64,
}

// ============================================================================
// Simulator
// ============================================================================

pub struct Sim {
    cfg: Config,
    threads: Vec<Thread>,
    /// Global run queue: thread indices, FIFO. The single queue is deliberate —
    /// every policy here shares it; only placement differs.
    queue: VecDeque<usize>,
    /// `core -> thread index`.
    on_core: Vec<Option<usize>>,
    now: u64,

    // Barrier state for the compute group.
    arrived: usize,
    iterations: u64,
    last_iter_at: u64,
    iter_intervals: Vec<u64>,

    // Netpoll state.
    netpoll_idx: usize,
    /// Arrival timestamps of packets not yet serviced.
    pending: VecDeque<u64>,
    /// Arrival timestamps within the trailing traffic window.
    traffic_window: VecDeque<u64>,
    next_packet_at: u64,
    next_periodic_wake_at: u64,

    // Counters.
    compute_preemptions: u64,
    barrier_parks: u64,
    netpoll_wakes: u64,
    packets: u64,
    packet_latency_sum: u64,
    packet_latency_max: u64,
    serviced: u64,
}

impl Sim {
    #[must_use]
    pub fn new(cfg: Config) -> Self {
        let per_thread = cfg.phase_work_us();
        let mut threads: Vec<Thread> = (0..cfg.compute_threads)
            .map(|_| Thread::new(Kind::Compute, per_thread))
            .collect();
        let netpoll_idx = threads.len();
        let mut np = Thread::new(Kind::Netpoll, cfg.netpoll_work_us);
        np.state = State::Blocked;
        np.wake_at = 0;
        threads.push(np);

        for (n, t) in threads.iter_mut().enumerate() {
            t.home = n % cfg.cores;
        }
        let queue: VecDeque<usize> = (0..cfg.compute_threads).collect();
        let on_core = vec![None; cfg.cores];
        let next_packet_at = if cfg.traffic_pps == 0 { u64::MAX } else { 1_000_000 / cfg.traffic_pps };

        Self {
            cfg,
            threads,
            queue,
            on_core,
            now: 0,
            arrived: 0,
            iterations: 0,
            last_iter_at: 0,
            iter_intervals: Vec::new(),
            netpoll_idx,
            pending: VecDeque::new(),
            traffic_window: VecDeque::new(),
            next_packet_at,
            next_periodic_wake_at: 0,
            compute_preemptions: 0,
            barrier_parks: 0,
            netpoll_wakes: 0,
            packets: 0,
            packet_latency_sum: 0,
            packet_latency_max: 0,
            serviced: 0,
        }
    }

    #[must_use]
    pub fn run(mut self) -> Report {
        let steps = self.cfg.sim_us / US_PER_STEP;
        for step in 0..steps {
            self.now = step * US_PER_STEP;
            self.arrivals();
            self.expire_wakes();
            if self.now.is_multiple_of(self.cfg.tick_us) {
                self.schedule();
            }
            self.advance();
            self.account();
        }
        self.finish()
    }

    // ------------------------------------------------------------------
    // Traffic
    // ------------------------------------------------------------------

    fn arrivals(&mut self) {
        while self.now >= self.next_packet_at {
            self.packets += 1;
            self.pending.push_back(self.next_packet_at);
            self.traffic_window.push_back(self.next_packet_at);
            // An RX interrupt makes netpoll runnable at once, under BOTH
            // policies. This is why backing off the *periodic* wake does not
            // cost arrival latency: the periodic wake was never what serviced a
            // packet promptly.
            self.make_ready(self.netpoll_idx);
            self.next_packet_at = self
                .next_packet_at
                .saturating_add(1_000_000 / self.cfg.traffic_pps.max(1));
        }
        if let NetpollPolicy::TrafficAdaptive { window_us, .. } = self.cfg.netpoll {
            while self.traffic_window.front().is_some_and(|t| self.now.saturating_sub(*t) > window_us) {
                self.traffic_window.pop_front();
            }
        }
    }

    /// Periodic (not arrival-driven) netpoll wake period under the active policy.
    fn netpoll_period_us(&self) -> u64 {
        match self.cfg.netpoll {
            NetpollPolicy::EveryTick => self.cfg.tick_us,
            NetpollPolicy::TrafficAdaptive { window_us, idle_period_us, busy_pps } => {
                // `rate_per_sec` over the elapsed span, not the nominal window:
                // before the window has filled, dividing by its full width
                // under-reports the rate and the policy reacts late.
                let pps = akuma_kacho::rate_per_sec(
                    self.traffic_window.len() as u64,
                    self.now.min(window_us),
                );
                // Descending ramp: the idle period at zero traffic, one tick at
                // and above `busy_pps`. This is the shipping decision function,
                // not a simulation-only approximation of it.
                akuma_kacho::ramp(pps, idle_period_us, self.cfg.tick_us, busy_pps)
            }
        }
    }

    fn expire_wakes(&mut self) {
        // The netpoll periodic wake.
        if self.now >= self.next_periodic_wake_at {
            self.make_ready(self.netpoll_idx);
            self.next_periodic_wake_at = self.now + self.netpoll_period_us();
        }
        for i in 0..self.threads.len() {
            if self.threads[i].state == State::Blocked
                && self.threads[i].wake_at != u64::MAX
                && self.now >= self.threads[i].wake_at
            {
                self.make_ready(i);
            }
        }
    }

    fn make_ready(&mut self, i: usize) {
        if self.threads[i].state != State::Blocked {
            return;
        }
        self.threads[i].state = State::Ready;
        self.threads[i].ready_since = self.now;
        self.threads[i].wake_at = u64::MAX;
        self.queue.push_back(i);
        // Under `Immediate`, the wake's scheduler SGI lets the thread take a
        // free core at once; under `NextTick` it must wait for the tick even
        // with a core idle. Modelling this wrong is what made an earlier run of
        // this simulator report the tick period as the bottleneck.
        if self.cfg.wake == WakePlacement::Immediate {
            self.fill_idle_cores();
        }
        // Spread additionally runs its placement decision *on the wake*, not
        // only on the next tick. Without this the latency class is capped by
        // tick granularity no matter how short its starvation bound is, which
        // is exactly the "eligibility is not execution" floor of
        // CROSS_CORE_THREAD_COLLAPSE.md §2.3: the SGI already fires, nothing
        // acts on it until the tick.
        if let SchedPolicy::Spread { starvation_us, .. } = self.cfg.sched
            && self.threads[i].kind == Kind::Netpoll
            && self.threads[i].state == State::Ready
        {
            self.sched_spread(starvation_us, 0);
        }
    }

    /// Place queued work on any core that has none. Never leaves a core idle
    /// with work waiting, under every policy.
    fn fill_idle_cores(&mut self) {
        let pinned = self.cfg.sched == SchedPolicy::Pinned;
        for c in 0..self.cfg.cores {
            if self.on_core[c].is_some() {
                continue;
            }
            // Under Pinned, a core may only take a thread that calls it home —
            // so an idle core can sit idle with work queued elsewhere. That is
            // the whole cost of affinity, and it has to be modelled, not hidden.
            let pick = if pinned {
                self.queue.iter().position(|&i| self.threads[i].home == c)
            } else {
                (!self.queue.is_empty()).then_some(0)
            };
            let Some(pos) = pick else { continue };
            let i = self.queue.remove(pos).expect("position came from this queue");
            self.assign(c, i);
        }
    }

    // ------------------------------------------------------------------
    // Scheduling — the part under test
    // ------------------------------------------------------------------

    fn schedule(&mut self) {
        // Phase 1, common to every policy: fill genuinely idle cores.
        self.fill_idle_cores();
        if self.queue.is_empty() {
            for t in &mut self.threads {
                t.immunity_used = 0;
            }
            return;
        }

        // Phase 2: cores are all busy and work is still queued. Who gives way?
        match self.cfg.sched {
            SchedPolicy::RoundRobin => self.sched_round_robin(),
            SchedPolicy::Immunity { ticks } => self.sched_immunity(ticks),
            SchedPolicy::Spread { starvation_us, latency_starvation_us } => {
                self.sched_spread(starvation_us, latency_starvation_us);
            }
            SchedPolicy::Pinned => self.sched_pinned(),
        }
    }

    /// Every busy core rotates, every tick, as long as anything is queued.
    fn sched_round_robin(&mut self) {
        for c in 0..self.cfg.cores {
            let Some(cur) = self.on_core[c] else { continue };
            let Some(next) = self.queue.pop_front() else { break };
            self.preempt(c, cur);
            self.assign(c, next);
        }
    }

    /// Today's kernel. A working thread declines displacement while an idle
    /// peer exists to take the queued work — but every core evaluates this
    /// independently, so at `N+1` on `N` there is never an idle peer and every
    /// core rotates once its immunity runs out.
    fn sched_immunity(&mut self, ticks: u32) {
        for c in 0..self.cfg.cores {
            let Some(cur) = self.on_core[c] else { continue };
            if self.queue.is_empty() {
                break;
            }
            let idle_peer = self.on_core.iter().any(Option::is_none);
            let may_decline = idle_peer && self.threads[cur].immunity_used + 1 < ticks;
            if may_decline {
                self.threads[cur].immunity_used += 1;
                continue;
            }
            self.threads[cur].immunity_used = 0;
            let Some(next) = self.queue.pop_front() else { break };
            self.preempt(c, cur);
            self.assign(c, next);
        }
    }

    /// The proposal. One disturbance per tick, aimed at the least-disturbed
    /// thread, and only when something is genuinely starving.
    fn sched_spread(&mut self, starvation_us: u64, latency_starvation_us: u64) {
        // Is anyone actually starving? If the queue is short-lived churn, do
        // nothing: a barrier group pays for every disturbance, so the default
        // must be to leave running threads alone.
        //
        // The bound is per class, not global. Netpoll waiting 2 ms is a stalled
        // ACK; a compute thread waiting 2 ms is one barrier. Scanning
        // latency-class threads first also means a packet never queues behind
        // the compute group's much laxer bound.
        let bound = |k: Kind| {
            if k == Kind::Netpoll { latency_starvation_us } else { starvation_us }
        };
        let mut starving = None;
        for &i in &self.queue {
            let waited = self.now.saturating_sub(self.threads[i].ready_since);
            if waited < bound(self.threads[i].kind) {
                continue;
            }
            // A latency-class thread outranks any throughput-class candidate.
            if self.threads[i].kind == Kind::Netpoll {
                starving = Some(i);
                break;
            }
            starving.get_or_insert(i);
        }
        let Some(next) = starving else { return };

        // Victim: the running thread disturbed least so far. Ties break toward
        // the one that has held its core longest, so the choice is total.
        let victim = (0..self.cfg.cores)
            .filter_map(|c| self.on_core[c].map(|i| (c, i)))
            .min_by_key(|&(_, i)| (self.threads[i].displaced_us, self.threads[i].cpu_us));
        let Some((c, cur)) = victim else { return };

        // Exactly one core moves this tick.
        self.queue.retain(|&i| i != next);
        self.preempt(c, cur);
        self.assign(c, next);
    }

    /// Threads sharing a home core take turns on the tick. No migration, ever.
    fn sched_pinned(&mut self) {
        for c in 0..self.cfg.cores {
            let Some(cur) = self.on_core[c] else { continue };
            let Some(pos) = self.queue.iter().position(|&i| self.threads[i].home == c) else {
                continue;
            };
            let next = self.queue.remove(pos).expect("position came from this queue");
            self.preempt(c, cur);
            self.assign(c, next);
        }
    }

    fn assign(&mut self, core: usize, i: usize) {
        self.on_core[core] = Some(i);
        self.threads[i].state = State::Running;
        self.threads[i].core = Some(core);
        self.threads[i].immunity_used = 0;
    }

    fn preempt(&mut self, core: usize, i: usize) {
        self.on_core[core] = None;
        self.threads[i].state = State::Ready;
        self.threads[i].core = None;
        self.threads[i].ready_since = self.now;
        self.queue.push_back(i);
        if self.threads[i].kind == Kind::Compute {
            self.compute_preemptions += 1;
        }
    }

    // ------------------------------------------------------------------
    // Execution
    // ------------------------------------------------------------------

    fn advance(&mut self) {
        for c in 0..self.cfg.cores {
            let Some(i) = self.on_core[c] else { continue };
            self.threads[i].cpu_us += US_PER_STEP;
            match self.threads[i].kind {
                Kind::Compute => self.advance_compute(c, i),
                Kind::Netpoll => self.advance_netpoll(c, i),
            }
        }
    }

    fn advance_compute(&mut self, core: usize, i: usize) {
        if self.threads[i].spinning {
            // Spinning at the barrier burns the core and buys nothing but the
            // chance to avoid a park. Budget exhausted -> park, free the core.
            self.threads[i].spin_left_us = self.threads[i].spin_left_us.saturating_sub(US_PER_STEP);
            if self.threads[i].spin_left_us == 0 {
                self.threads[i].spinning = false;
                self.block(core, i, u64::MAX);
                self.barrier_parks += 1;
            }
            return;
        }

        self.threads[i].work_left_us = self.threads[i].work_left_us.saturating_sub(US_PER_STEP);
        if self.threads[i].work_left_us > 0 {
            return;
        }

        // Arrived at the barrier.
        self.arrived += 1;
        if self.arrived < self.cfg.compute_threads {
            self.threads[i].spinning = true;
            self.threads[i].spin_left_us = self.cfg.barrier_spin_us;
            return;
        }

        // Barrier complete: release the group.
        self.arrived = 0;
        self.iterations += 1;
        if self.iterations > 1 {
            self.iter_intervals.push(self.now - self.last_iter_at);
        }
        self.last_iter_at = self.now;
        for j in 0..self.cfg.compute_threads {
            self.threads[j].work_left_us = self.cfg.phase_work_us();
            self.threads[j].spinning = false;
            self.threads[j].spin_left_us = 0;
            if self.threads[j].state == State::Blocked {
                // Parked at the barrier: costs a wake to come back, and then
                // has to get a core, which is where the policy shows up.
                self.threads[j].wake_at = self.now + self.cfg.wake_latency_us;
            }
        }
    }

    fn advance_netpoll(&mut self, core: usize, i: usize) {
        self.threads[i].work_left_us = self.threads[i].work_left_us.saturating_sub(US_PER_STEP);
        if self.threads[i].work_left_us > 0 {
            return;
        }
        // One netpoll iteration done: it drains everything queued.
        self.netpoll_wakes += 1;
        while let Some(arrived_at) = self.pending.pop_front() {
            let lat = self.now.saturating_sub(arrived_at);
            self.packet_latency_sum += lat;
            self.packet_latency_max = self.packet_latency_max.max(lat);
            self.serviced += 1;
        }
        self.threads[i].work_left_us = self.cfg.netpoll_work_us;
        self.block(core, i, u64::MAX);
    }

    fn block(&mut self, core: usize, i: usize, wake_at: u64) {
        self.on_core[core] = None;
        self.threads[i].state = State::Blocked;
        self.threads[i].core = None;
        self.threads[i].wake_at = wake_at;
        // The core it just freed goes to whatever is queued, without waiting for
        // a tick — `schedule_blocking` reschedules on the spot.
        self.fill_idle_cores();
    }

    fn account(&mut self) {
        for t in &mut self.threads {
            if t.state == State::Ready {
                t.displaced_us += US_PER_STEP;
            }
        }
    }

    // ------------------------------------------------------------------

    fn finish(self) -> Report {
        let secs = self.cfg.sim_us as f64 / 1_000_000.0;
        let n = self.iter_intervals.len() as f64;
        let mean = if n > 0.0 { self.iter_intervals.iter().sum::<u64>() as f64 / n } else { 0.0 };
        let var = if n > 0.0 {
            self.iter_intervals
                .iter()
                .map(|&x| {
                    let d = x as f64 - mean;
                    d * d
                })
                .sum::<f64>()
                / n
        } else {
            0.0
        };
        let mut sorted = self.iter_intervals.clone();
        sorted.sort_unstable();
        let p99 = if sorted.is_empty() {
            0
        } else {
            sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)]
        };
        let compute_cpu: u64 = self
            .threads
            .iter()
            .filter(|t| t.kind == Kind::Compute)
            .map(|t| t.cpu_us)
            .sum();

        Report {
            iterations: self.iterations,
            iters_per_sec: self.iterations as f64 / secs,
            iter_mean_us: mean,
            iter_stddev_us: libm::sqrt(var),
            iter_p99_us: p99,
            compute_preemptions: self.compute_preemptions,
            barrier_parks: self.barrier_parks,
            netpoll_core_frac: self.threads[self.netpoll_idx].cpu_us as f64 / self.cfg.sim_us as f64,
            netpoll_wakes: self.netpoll_wakes,
            packet_latency_mean_us: if self.serviced > 0 {
                self.packet_latency_sum as f64 / self.serviced as f64
            } else {
                0.0
            },
            packet_latency_max_us: self.packet_latency_max,
            packets: self.packets,
            compute_core_frac: compute_cpu as f64 / (self.cfg.sim_us * self.cfg.cores as u64) as f64,
        }
    }
}

// ============================================================================
// Scenarios
// ============================================================================

pub mod scenarios {
    use super::{Config, NetpollPolicy, Report, SchedPolicy, Sim, WakePlacement};
    use alloc::vec::Vec;

    /// The netpoll policy proposed in the open items: RX still wakes it
    /// instantly, the periodic wake backs off to 100 ms when the trailing 10 s
    /// window is quiet, and tightens to the tick at 1000 pps.
    #[must_use]
    pub const fn adaptive_netpoll() -> NetpollPolicy {
        NetpollPolicy::TrafficAdaptive {
            window_us: 10_000_000,
            idle_period_us: 100_000,
            busy_pps: 1_000,
        }
    }

    /// One `(policy, netpoll)` pair swept over `-t 1..=4`, idealised wakes.
    #[must_use]
    pub fn sweep(sched: SchedPolicy, netpoll: NetpollPolicy) -> Vec<Report> {
        sweep_with(sched, netpoll, WakePlacement::Immediate)
    }

    /// As [`sweep`], with the wake path under explicit control.
    #[must_use]
    pub fn sweep_with(sched: SchedPolicy, netpoll: NetpollPolicy, wake: WakePlacement) -> Vec<Report> {
        (1..=4)
            .map(|t| {
                let mut cfg = Config::devbox(t);
                cfg.sched = sched;
                cfg.netpoll = netpoll;
                cfg.wake = wake;
                Sim::new(cfg).run()
            })
            .collect()
    }

    /// Peak thread count and the `peak / -t 4` ratio for one sweep.
    #[must_use]
    pub fn shape(r: &[Report]) -> (usize, f64) {
        let peak = r
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.iters_per_sec.total_cmp(&b.1.iters_per_sec))
            .map_or(0, |(i, _)| i + 1);
        let ratio = if r[3].iters_per_sec > 0.0 {
            r[peak - 1].iters_per_sec / r[3].iters_per_sec
        } else {
            f64::INFINITY
        };
        (peak, ratio)
    }

    /// **The credibility check, and the headline finding.**
    ///
    /// The model must reproduce the shape the hardware showed — peak at `-t 3`,
    /// collapse at `-t 4` — before any of its predictions are worth reading.
    /// It does, but **only with [`WakePlacement::NextTick`]**. With
    /// [`WakePlacement::Immediate`] the same model, same oversubscription, same
    /// netpoll cost, shows `-t 4` within ~15% of `-t 3` and no collapse at all.
    ///
    /// That is the result: **fair-share arithmetic cannot produce the measured
    /// collapse.** Five runnable threads on four cores costs ~10-15%, not 14.6x.
    /// The collapse needs a woken thread to be unable to take an idle core, and
    /// that is exactly §2.3, the still-open explicit-wake latency item.
    ///
    /// Returns `(peak_thread_count, t4_collapse_ratio)`.
    #[must_use]
    pub fn calibration() -> (usize, f64) {
        shape(&sweep_with(
            SchedPolicy::Immunity { ticks: 5 },
            NetpollPolicy::EveryTick,
            WakePlacement::NextTick,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::scenarios::{adaptive_netpoll, sweep};
    use super::*;

    /// Sanity: with one compute thread and four cores nothing should ever be
    /// displaced, under any policy.
    #[test]
    fn single_thread_is_never_displaced() {
        for sched in [
            SchedPolicy::RoundRobin,
            SchedPolicy::Immunity { ticks: 5 },
            SchedPolicy::Spread { starvation_us: 2_000, latency_starvation_us: 100 },
        ] {
            let mut cfg = Config::devbox(1);
            cfg.sched = sched;
            cfg.traffic_pps = 0;
            let r = Sim::new(cfg).run();
            assert_eq!(r.compute_preemptions, 0, "{sched:?} displaced a lone thread");
        }
    }

    /// Work conservation: the compute group can never receive more CPU than the
    /// machine has, minus what netpoll took. Catches accounting drift in the
    /// model itself.
    #[test]
    fn compute_never_exceeds_the_machine() {
        for t in 1..=4 {
            let r = Sim::new(Config::devbox(t)).run();
            // `compute_core_frac` is normalised by `cores`, `netpoll_core_frac`
            // by ONE core — adding them raw compares different units.
            let cores = 4.0;
            let used = r.compute_core_frac * cores + r.netpoll_core_frac;
            assert!(used <= cores + 0.001, "-t {t}: {used:.3} of {cores} cores used");
        }
    }

    /// Locks in the headline finding: neither oversubscription alone nor
    /// next-tick wake placement gets anywhere near the measured 14.6x collapse
    /// at `-t 4`. If a future model change makes this pass at 14x, the finding
    /// has changed and the write-up must change with it.
    #[test]
    fn fair_share_cannot_explain_the_measured_collapse() {
        let (_, ideal) = scenarios::shape(&scenarios::sweep_with(
            SchedPolicy::Immunity { ticks: 5 },
            NetpollPolicy::EveryTick,
            WakePlacement::Immediate,
        ));
        let (_, next_tick) = scenarios::calibration();
        assert!(ideal < 2.0, "idealised wakes already collapse {ideal:.1}x — model bug");
        assert!(
            next_tick < 5.0,
            "next-tick wakes now reproduce {next_tick:.1}x; the write-up says they cannot"
        );
        assert!(next_tick > ideal, "next-tick wake placement should cost something");
    }

    /// Wake latency is the parameter the collapse is most sensitive to, so the
    /// model must at least be monotonic in it.
    #[test]
    fn collapse_grows_with_wake_latency() {
        let ratio_at = |lat: u64| {
            let r: Vec<_> = (1..=4)
                .map(|t| {
                    let mut c = Config::devbox(t);
                    c.wake_latency_us = lat;
                    c.wake = WakePlacement::NextTick;
                    Sim::new(c).run()
                })
                .collect();
            scenarios::shape(&r).1
        };
        assert!(ratio_at(5_000) > ratio_at(60), "collapse should worsen with wake latency");
    }

    /// Backing the periodic wake off must not cost packet latency, because an
    /// RX interrupt still wakes netpoll immediately.
    #[test]
    fn adaptive_netpoll_keeps_latency() {
        let mut a = Config::devbox(3);
        a.traffic_pps = 20;
        let base = Sim::new(a.clone()).run();
        a.netpoll = adaptive_netpoll();
        let adapt = Sim::new(a).run();
        assert!(
            adapt.packet_latency_mean_us <= base.packet_latency_mean_us + 50.0,
            "adaptive latency {:.0}us vs baseline {:.0}us",
            adapt.packet_latency_mean_us,
            base.packet_latency_mean_us
        );
        assert!(
            adapt.netpoll_core_frac < base.netpoll_core_frac,
            "adaptive netpoll did not reduce core occupancy"
        );
    }

    /// Under heavy traffic the adaptive policy must collapse back to the tick,
    /// i.e. behave like today. A knob that only works when idle is a trap.
    #[test]
    fn adaptive_netpoll_degrades_to_today_under_load() {
        let mut cfg = Config::devbox(2);
        cfg.traffic_pps = 5_000;
        let base = Sim::new(cfg.clone()).run();
        cfg.netpoll = adaptive_netpoll();
        let adapt = Sim::new(cfg).run();
        let delta = (adapt.netpoll_core_frac - base.netpoll_core_frac).abs();
        assert!(delta < 0.10, "busy-traffic netpoll differs by {delta:.3} core");
    }

    #[test]
    fn sweeps_produce_four_points() {
        assert_eq!(sweep(SchedPolicy::RoundRobin, NetpollPolicy::EveryTick).len(), 4);
    }
}
