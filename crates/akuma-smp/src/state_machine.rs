//! [`CoreStateMachine`] — one core's decision logic for multi-core cooperation,
//! as a **sans-IO** state machine. Today it implements the debt-based memory-reclaim
//! protocol (§9); it is the place that will grow to make the rest of the inter-core
//! coordination decisions (liveness/failover, leadership — §12).
//!
//! [`CoreStateMachine::step`] is pure: it consumes an [`Event`] and emits [`Command`]s
//! through a caller-supplied closure, doing NO I/O itself (no ring, no PMM, no
//! console). The same logic therefore runs in two places:
//! - **kernel**: the pump feeds real events (drained ring messages, pressure
//!   samples) and executes the commands (ring push, page shed via PMM, zeroing);
//! - **host sim**: a simulator supplies events and interprets commands against an
//!   in-memory model, so the protocol is developed/tested deterministically.
//!
//! It is deliberately **alloc-free / fixed-capacity** (no `Vec`): an isolated
//! secondary's restricted page table doesn't map the kernel heap, so the state
//! machine must live entirely in its own stack/PerCpu.
//!
//! Protocol (docs/MULTIKERNEL.md §9; see the `multikernel_memory_protocol` note):
//! a core under pressure is low *because it lent pages out*. Pressure is NOT a
//! targeted recall — it's a general "repay your debts" trigger. Each debtor that
//! receives it sheds borrowed ranges **back to its own creditor** (not necessarily
//! the requester). Debt is tracked single-hop on the **borrower** only; the lender
//! keeps no record. Repayments are addressed to a specific creditor, and the
//! receiver **zeroes** acquired pages before reuse.

use crate::descriptor::MAX_CORES;

/// Max distinct borrowed ranges tracked per creditor (fixed — no heap).
///
/// Overflow means "can't track more debt to this creditor right now" (a policy
/// limit, not a correctness bug): the extra loan simply isn't recorded, so it won't
/// be repaid via this mechanism.
pub const MAX_DEBT_RANGES: usize = 4;

/// A physical page range (base address, length in the same unit the caller uses —
/// pages or bytes; the brain only moves the number around).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub base: u64,
    pub len: u64,
}

/// After this many consecutive observations of a peer's heartbeat *not* advancing,
/// we declare it offline (§12 liveness). A live peer bumps its counter every loop,
/// so any non-advance is suspicious; a few in a row is conclusive.
pub const DEAD_AFTER_STALE: u32 = 3;

/// Inputs to the state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Periodic check: re-evaluate own pressure.
    Tick,
    /// A peer signaled memory pressure ("repay your debts"). `from` is only the
    /// trigger — we repay our OWN creditors, who may differ from `from`.
    Pressure { from: u32 },
    /// A debtor repaid us (we are the creditor): these pages are coming home.
    Repaid { from: u32, range: Range },
    /// We borrowed `range` from `creditor` (records single-hop debt on us).
    Borrowed { creditor: u32, range: Range },
    /// We consumed `pages` of our own pool (raises our pressure).
    Consumed { pages: u64 },
    /// An observation of peer `core`'s liveness heartbeat counter (read from the
    /// shared descriptor). Drives offline detection.
    PeerHeartbeat { core: u32, value: u64 },
}

/// Outputs — intents the executor (kernel or sim) carries out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Send the pressure signal to peer `to`.
    SendPressure { to: u32 },
    /// Shed `range` back to `creditor` (unmap locally + deliver, addressed).
    Repay { creditor: u32, range: Range },
    /// Accept a repayment from `from`: **zero** `range`, then it's ours again.
    Accept { from: u32, range: Range },
    /// We now believe peer `core` is offline (its heartbeat stalled). A higher
    /// layer reacts (re-point caps away from it, drive a reload — §12).
    PeerDown { core: u32 },
    /// Peer `core` that we'd declared down is beating again (it was only slow).
    PeerUp { core: u32 },
}

#[derive(Clone, Copy)]
struct CreditorDebt {
    ranges: [Range; MAX_DEBT_RANGES],
    count: usize,
}

impl CreditorDebt {
    const fn new() -> Self {
        Self { ranges: [Range { base: 0, len: 0 }; MAX_DEBT_RANGES], count: 0 }
    }
}

/// Our view of one peer's liveness, updated from `PeerHeartbeat` observations.
#[derive(Clone, Copy)]
struct PeerLiveness {
    last_hb: u64,
    /// Consecutive observations with no advance.
    stale: u32,
    declared_dead: bool,
}

impl PeerLiveness {
    const fn new() -> Self {
        Self { last_hb: 0, stale: 0, declared_dead: false }
    }
}

/// One core's view of the protocol: its own free pool plus what it owes each peer.
pub struct CoreStateMachine {
    core_id: u32,
    num_cores: usize,
    free_pages: u64,
    /// Emit pressure once free drops below this; reset (re-armable) once recovered.
    low_watermark: u64,
    /// `debts[c]` = ranges this core borrowed from creditor `c`. Single-hop; the
    /// lender side keeps NOTHING (this is the only debt bookkeeping in the system).
    debts: [CreditorDebt; MAX_CORES],
    /// Whether we've already broadcast pressure for the current low episode.
    signaled: bool,
    /// Per-peer liveness tracking, indexed by core id.
    peers: [PeerLiveness; MAX_CORES],
}

impl CoreStateMachine {
    #[must_use]
    pub fn new(core_id: u32, num_cores: usize, free_pages: u64, low_watermark: u64) -> Self {
        Self {
            core_id,
            num_cores,
            free_pages,
            low_watermark,
            debts: [CreditorDebt::new(); MAX_CORES],
            signaled: false,
            peers: [PeerLiveness::new(); MAX_CORES],
        }
    }

    /// Whether we currently believe peer `core` is offline.
    #[must_use]
    pub fn peer_dead(&self, core: u32) -> bool {
        self.peers.get(core as usize).is_some_and(|p| p.declared_dead)
    }

    #[must_use]
    pub fn free_pages(&self) -> u64 {
        self.free_pages
    }

    /// Total ranges currently owed across all creditors (for tests/diagnostics).
    #[must_use]
    pub fn debt_range_count(&self) -> usize {
        self.debts.iter().map(|d| d.count).sum()
    }

    /// Record a single-hop debt to `creditor`. Returns `false` if that creditor's
    /// fixed slot set is full (the loan is then untracked — see `MAX_DEBT_RANGES`).
    fn record_debt(&mut self, creditor: u32, range: Range) -> bool {
        let Some(d) = self.debts.get_mut(creditor as usize) else {
            return false;
        };
        if d.count >= MAX_DEBT_RANGES {
            return false;
        }
        d.ranges[d.count] = range;
        d.count += 1;
        true
    }

    /// Drive the state machine one event, emitting commands via `emit`.
    pub fn step(&mut self, ev: Event, emit: &mut impl FnMut(Command)) {
        match ev {
            Event::Tick => {
                if self.free_pages < self.low_watermark {
                    if !self.signaled {
                        self.signaled = true;
                        for peer in 0..self.num_cores {
                            if peer as u32 != self.core_id {
                                emit(Command::SendPressure { to: peer as u32 });
                            }
                        }
                    }
                } else {
                    self.signaled = false; // recovered → re-arm for a future episode
                }
            }
            Event::Pressure { from: _ } => {
                // Repay OUR creditors (single-hop), not necessarily the requester.
                // Shed what we can afford; RETAIN debts we can't (compact the array
                // in place) so they're repaid on a later pressure signal — never
                // silently forgotten.
                for c in 0..self.num_cores {
                    let count = self.debts[c].count;
                    let mut kept = 0;
                    for i in 0..count {
                        let range = self.debts[c].ranges[i];
                        if self.free_pages >= range.len {
                            emit(Command::Repay { creditor: c as u32, range });
                            self.free_pages -= range.len;
                        } else {
                            self.debts[c].ranges[kept] = range; // can't afford — keep
                            kept += 1;
                        }
                    }
                    self.debts[c].count = kept;
                }
            }
            Event::Repaid { from, range } => {
                // We're the creditor; the lender kept no record, and addressing
                // guarantees this was routed to us, so accept unconditionally.
                // The executor MUST zero `range` before reuse (security rule).
                emit(Command::Accept { from, range });
                self.free_pages += range.len;
            }
            Event::Borrowed { creditor, range } => {
                self.record_debt(creditor, range);
                self.free_pages += range.len;
            }
            Event::Consumed { pages } => {
                self.free_pages = self.free_pages.saturating_sub(pages);
            }
            Event::PeerHeartbeat { core, value } => {
                let Some(p) = self.peers.get_mut(core as usize) else {
                    return;
                };
                if value > p.last_hb {
                    p.last_hb = value;
                    p.stale = 0;
                    if p.declared_dead {
                        p.declared_dead = false;
                        emit(Command::PeerUp { core }); // was only slow, not dead
                    }
                } else {
                    p.stale = p.stale.saturating_add(1);
                    if p.stale >= DEAD_AFTER_STALE && !p.declared_dead {
                        p.declared_dead = true;
                        emit(Command::PeerDown { core });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Minimal deterministic simulator: N brains + a per-core event inbox + a record
    /// of every range a receiver zeroed. Rounds are barriers — commands emitted this
    /// round are delivered for the next — so behavior is reproducible.
    struct Sim {
        brains: Vec<CoreStateMachine>,
        inbox: Vec<Vec<Event>>,
        /// (receiver_core, range) for every Accept — used to assert receiver-zeroing.
        zeroed: Vec<(u32, Range)>,
    }

    impl Sim {
        fn new(brains: Vec<CoreStateMachine>) -> Self {
            let n = brains.len();
            Self { brains, inbox: (0..n).map(|_| Vec::new()).collect(), zeroed: Vec::new() }
        }

        /// Model a loan: move `len` from creditor's pool to debtor's and record the
        /// debt on the debtor. The lender keeps NO record (protocol invariant).
        fn lend(&mut self, creditor: u32, debtor: u32, range: Range) {
            self.brains[creditor as usize].free_pages -= range.len;
            let mut sink = |_c: Command| {};
            self.brains[debtor as usize].step(Event::Borrowed { creditor, range }, &mut sink);
        }

        fn total_free(&self) -> u64 {
            self.brains.iter().map(CoreStateMachine::free_pages).sum()
        }

        fn run_round(&mut self) {
            let snapshot: Vec<Vec<Event>> =
                self.inbox.iter_mut().map(core::mem::take).collect();
            for (core, evs) in snapshot.iter().enumerate() {
                let mut cmds: Vec<Command> = Vec::new();
                {
                    let mut emit = |c: Command| cmds.push(c);
                    self.brains[core].step(Event::Tick, &mut emit);
                    for ev in evs {
                        self.brains[core].step(*ev, &mut emit);
                    }
                }
                for cmd in cmds {
                    self.route(core as u32, cmd);
                }
            }
        }

        fn route(&mut self, src: u32, cmd: Command) {
            match cmd {
                Command::SendPressure { to } => {
                    self.inbox[to as usize].push(Event::Pressure { from: src });
                }
                Command::Repay { creditor, range } => {
                    // Addressed: delivered only to the creditor's inbox.
                    self.inbox[creditor as usize].push(Event::Repaid { from: src, range });
                }
                Command::Accept { from: _, range } => {
                    self.zeroed.push((src, range)); // receiver zeroed it
                }
                // Liveness commands aren't part of the debt-flow sim scenarios.
                Command::PeerDown { .. } | Command::PeerUp { .. } => {}
            }
        }
    }

    #[test]
    fn pressure_triggers_repayment_and_conserves_memory() {
        // 3 cores. Core 0 lends 400 to core 1 and 400 to core 2, leaving it low.
        let total = 1000;
        let brains = (0..3).map(|i| CoreStateMachine::new(i, 3, total, 300)).collect();
        let mut sim = Sim::new(brains);
        let before = sim.total_free();

        sim.lend(0, 1, Range { base: 0x1000, len: 400 });
        sim.lend(0, 2, Range { base: 0x2000, len: 400 });
        assert_eq!(sim.total_free(), before, "lending conserves total memory");
        assert_eq!(sim.brains[0].free_pages(), 200); // low → under watermark

        // Run rounds: tick(0) broadcasts pressure → debtors repay creditor 0 →
        // core 0 accepts. Quiesces within a few rounds.
        for _ in 0..5 {
            sim.run_round();
        }

        assert_eq!(sim.total_free(), before, "repayment conserves total memory");
        assert_eq!(sim.brains[0].free_pages(), 1000, "creditor replenished to full");
        assert_eq!(sim.brains[1].free_pages(), 1000);
        assert_eq!(sim.brains[2].free_pages(), 1000);
        assert_eq!(sim.brains[1].debt_range_count(), 0, "debts cleared");
        assert_eq!(sim.brains[2].debt_range_count(), 0);

        // Receiver-zeroing rule: every repaid range was zeroed by the creditor (0).
        assert_eq!(sim.zeroed.len(), 2);
        assert!(sim.zeroed.iter().all(|&(rcv, _)| rcv == 0));
        assert!(sim.zeroed.iter().any(|&(_, r)| r == Range { base: 0x1000, len: 400 }));
        assert!(sim.zeroed.iter().any(|&(_, r)| r == Range { base: 0x2000, len: 400 }));
    }

    #[test]
    fn debtor_repays_its_creditor_not_the_requester() {
        // Core 1 owes core 0. Core 2 (NOT a creditor of core 1) signals pressure.
        // Core 1 must repay core 0, and core 2 must receive nothing.
        let brains = (0..3).map(|i| CoreStateMachine::new(i, 3, 1000, 300)).collect();
        let mut sim = Sim::new(brains);
        sim.lend(0, 1, Range { base: 0x5000, len: 200 });

        // Core 2 broadcasts pressure directly (simulate it being low).
        sim.inbox[1].push(Event::Pressure { from: 2 });
        sim.run_round(); // core 1 processes pressure → Repay to creditor 0
        sim.run_round(); // core 0 accepts

        // The repayment went to creditor 0, never to the requester (core 2).
        assert_eq!(sim.zeroed.len(), 1);
        assert_eq!(sim.zeroed[0].0, 0, "repayment landed at the creditor, not requester");
        assert_eq!(sim.brains[0].free_pages(), 1000, "creditor 0 got its pages back");
        assert_eq!(sim.brains[1].debt_range_count(), 0);
    }

    #[test]
    fn pressure_signal_is_armed_once_per_episode() {
        // A core that stays low should broadcast pressure once, not every tick.
        let mut brain = CoreStateMachine::new(0, 3, 100, 300); // already below watermark
        let mut sends = 0usize;
        for _ in 0..10 {
            brain.step(Event::Tick, &mut |c| {
                if matches!(c, Command::SendPressure { .. }) {
                    sends += 1;
                }
            });
        }
        // One episode → 2 peers signaled exactly once (not 10× each).
        assert_eq!(sends, 2);
    }

    #[test]
    fn detects_stalled_peer_without_false_positives() {
        // Core 0 watches cores 1 and 2. Core 1 keeps beating; core 2 freezes after
        // a few rounds (fault injection) → only core 2 is declared down.
        let mut sm = CoreStateMachine::new(0, 3, 1000, 300);
        let mut down: Vec<u32> = Vec::new();
        let (mut hb1, mut hb2) = (0u64, 0u64);
        for round in 0..10 {
            hb1 += 1;
            if round < 3 {
                hb2 += 1; // core 2 dies after round 3 — heartbeat frozen thereafter
            }
            let mut emit = |c: Command| {
                if let Command::PeerDown { core } = c {
                    down.push(core);
                }
            };
            sm.step(Event::PeerHeartbeat { core: 1, value: hb1 }, &mut emit);
            sm.step(Event::PeerHeartbeat { core: 2, value: hb2 }, &mut emit);
        }
        assert_eq!(down.as_slice(), &[2], "only the stalled peer is declared down");
        assert!(sm.peer_dead(2));
        assert!(!sm.peer_dead(1));
    }

    #[test]
    fn a_slow_peer_that_resumes_is_brought_back_up() {
        // Core 2 stalls long enough to be declared down, then resumes → PeerUp,
        // and it is no longer considered dead (split-brain / false-accusation guard).
        let mut sm = CoreStateMachine::new(0, 3, 1000, 300);
        let mut events: Vec<Command> = Vec::new();
        {
            let mut emit = |c: Command| events.push(c);
            // Frozen value for enough observations to declare it down.
            for _ in 0..=DEAD_AFTER_STALE {
                sm.step(Event::PeerHeartbeat { core: 2, value: 5 }, &mut emit);
            }
            assert!(sm.peer_dead(2));
            // It beats again (value advances) → recovered.
            sm.step(Event::PeerHeartbeat { core: 2, value: 6 }, &mut emit);
        }

        assert!(!sm.peer_dead(2));
        assert!(events.contains(&Command::PeerDown { core: 2 }));
        assert!(events.contains(&Command::PeerUp { core: 2 }));
    }

    #[test]
    fn repays_multiple_creditors_on_pressure() {
        // A debtor that owes two different cores repays BOTH (single-hop, each direct).
        let mut sm = CoreStateMachine::new(1, 3, 1000, 300);
        sm.step(Event::Borrowed { creditor: 0, range: Range { base: 0x1000, len: 50 } }, &mut |_| {});
        sm.step(Event::Borrowed { creditor: 2, range: Range { base: 0x2000, len: 30 } }, &mut |_| {});
        assert_eq!(sm.free_pages(), 1080);

        let mut repays: Vec<Command> = Vec::new();
        sm.step(Event::Pressure { from: 0 }, &mut |c| repays.push(c));

        assert!(repays.contains(&Command::Repay { creditor: 0, range: Range { base: 0x1000, len: 50 } }));
        assert!(repays.contains(&Command::Repay { creditor: 2, range: Range { base: 0x2000, len: 30 } }));
        assert_eq!(sm.free_pages(), 1000, "shed both back");
        assert_eq!(sm.debt_range_count(), 0);
    }

    #[test]
    fn retains_debt_it_cannot_afford_to_repay() {
        // If we don't currently hold enough free pages, we must NOT shed (and must
        // NOT forget the debt) — it's repaid on a later pressure signal.
        let mut sm = CoreStateMachine::new(1, 3, 1000, 300);
        // Owe 150, but only 100 free → can't repay yet.
        sm.step(Event::Borrowed { creditor: 0, range: Range { base: 0x3000, len: 150 } }, &mut |_| {});
        sm.step(Event::Consumed { pages: 1050 }, &mut |_| {}); // free: 1000+150-1050 = 100

        let mut repays = 0;
        sm.step(Event::Pressure { from: 0 }, &mut |c| {
            if matches!(c, Command::Repay { .. }) {
                repays += 1;
            }
        });
        assert_eq!(repays, 0, "can't afford → don't shed");
        assert_eq!(sm.debt_range_count(), 1, "debt retained, not forgotten");

        // Later we hold enough again → the retained debt is repaid.
        sm.step(Event::Borrowed { creditor: 2, range: Range { base: 0x9000, len: 500 } }, &mut |_| {});
        let mut repaid = Vec::new();
        sm.step(Event::Pressure { from: 0 }, &mut |c| repaid.push(c));
        assert!(repaid.contains(&Command::Repay { creditor: 0, range: Range { base: 0x3000, len: 150 } }));
    }

    #[test]
    fn consumption_below_watermark_triggers_pressure() {
        // Using our own pool down past the low watermark makes the next Tick signal.
        let mut sm = CoreStateMachine::new(0, 3, 400, 300); // free 400 > wm 300
        let mut sends = 0;
        sm.step(Event::Tick, &mut |c| if matches!(c, Command::SendPressure { .. }) { sends += 1 });
        assert_eq!(sends, 0, "above watermark → quiet");

        sm.step(Event::Consumed { pages: 200 }, &mut |_| {}); // free → 200 < 300
        sm.step(Event::Tick, &mut |c| if matches!(c, Command::SendPressure { .. }) { sends += 1 });
        assert_eq!(sends, 2, "below watermark → signal both peers once");
    }

    #[test]
    fn debt_tracking_is_capacity_bounded() {
        let mut brain = CoreStateMachine::new(1, 3, 1000, 300);
        // More loans from creditor 0 than MAX_DEBT_RANGES → extras untracked.
        for i in 0..(MAX_DEBT_RANGES as u64 + 3) {
            brain.step(
                Event::Borrowed { creditor: 0, range: Range { base: i * 0x1000, len: 10 } },
                &mut |_| {},
            );
        }
        assert_eq!(brain.debt_range_count(), MAX_DEBT_RANGES);
    }
}
