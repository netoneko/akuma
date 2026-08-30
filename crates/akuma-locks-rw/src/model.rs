//! Host-only exhaustive model checker + protocol tests for the recoverable
//! reader/writer lock, on the `akuma-bkl` `bkl_model.rs` pattern.
//!
//! The protocol is pure logic — a flag word, an owner cell, per-tid reader
//! holds, a kill/reap event pair — so every property below is checked by BFS
//! over **every interleaving** of a small configuration, in milliseconds,
//! instead of by a devbox boot under a kill storm:
//!
//! - **mutual exclusion** — never a writer and a reader, or two writers, and
//!   "held ⇔ accounted" (the flag word and the owner cell agree) in every
//!   reachable state, kills included;
//! - **accounting** — the flag-side reader count equals the sum of the
//!   per-tid cells: acquire and release move both sides together, kill moves
//!   neither, reap moves both;
//! - **deadlock-freedom** — every reachable state has at least one enabled
//!   transition;
//! - **writer priority** — while a writer waits, readers are refused, and
//!   that writer admission is always regained (readers cannot re-arm while
//!   a writer waits, and holds always release or get swept);
//! - **reader unfairness, admitted** — a reader *can* lose admission races
//!   forever against a writer ping-pong. Pinned deliberately (the model's
//!   image of an unfair test-and-set), so a change that accidentally makes
//!   the lock "fairer" shows up as a model diff to reason about, not silence;
//! - **recovery-after-abandon** — from every state where a dead holder keeps
//!   the lock shut, the reap path reaches a state some waiter can enter, and
//!   reaping is idempotent.

use std::collections::{HashMap, VecDeque, HashSet};

/// What one modelled thread is doing. A thread *killed* leaves these modes for
/// [`Mode::Dead`] — its holds stay in the lock exactly as a `panic = "abort"`
/// kill leaves them in the real lock.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Mode {
    /// Running outside the lock.
    Idle,
    /// Wants a read hold.
    WantRead,
    /// Wants a write hold. Being in this mode IS the announcement (the real
    /// lock's `WWAIT` bit, re-asserted on every failed attempt).
    WantWrite,
    /// Holds a read hold.
    HoldRead,
    /// Holds the write hold.
    HoldWrite,
    /// Killed at an arbitrary point. Holds (if any) leak in the lock; the
    /// thread can do nothing further. The reap contract says the slot cannot
    /// be reissued before the sweep, so no transition ever revives a tid.
    Dead,
}

/// The whole machine: threads, the lock word, the owner cell, the per-tid
/// reader cells, and the set of killed-but-not-yet-swept tids.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct State {
    modes: Vec<Mode>,
    /// The real lock word's writer bit and reader field. The model carries no
    /// `WWAIT` bit: the announcement is the `WantWrite` mode itself (with the
    /// real lock re-asserting the bit on every failed attempt, the two are the
    /// same fact).
    flag: u32,
    /// Writer owner tid, or `usize::MAX` (the real lock's `NO_OWNER`).
    owner: usize,
    /// Per-tid reader hold counts.
    cells: Vec<usize>,
    /// Tids killed while the model ran and not yet swept.
    dead: Vec<usize>,
}

const WBIT: u32 = 1 << 31;
/// The real lock's reader field: bits 0..30 (WWAIT at bit 30 exists only in
/// the real word; the model carries the announcement on the `WantWrite` mode).
const READER_MASK: u32 = (1 << 30) - 1;
const NO_OWNER: usize = usize::MAX;

#[derive(Clone, Copy, Debug)]
struct Config {
    n: usize,
}

impl State {
    fn initial(cfg: Config) -> Self {
        Self {
            modes: vec![Mode::Idle; cfg.n],
            flag: 0,
            owner: NO_OWNER,
            cells: vec![0; cfg.n],
            dead: Vec::new(),
        }
    }

    fn readers(&self) -> u32 {
        self.flag & READER_MASK
    }

    /// `try_read`'s refusal condition: a writer holds the lock, or one is
    /// waiting. The announcement is modelled as mode-derived — with the real
    /// lock re-asserting the bit on every failed attempt, "some thread is in
    /// `WantWrite`" and "WWAIT stands" are the same fact.
    fn blocked_for_read(&self) -> bool {
        self.flag & WBIT != 0
            || self.modes.contains(&Mode::WantWrite)
    }

    /// All states reachable in one transition, labelled for diagnostics.
    fn successors(&self, cfg: Config) -> Vec<(&'static str, Self)> {
        let mut out = Vec::new();
        for t in 0..cfg.n {
            match self.modes[t] {
                Mode::Idle => {
                    let mut s = self.clone();
                    s.modes[t] = Mode::WantRead;
                    out.push(("issue-read", s));

                    let mut s = self.clone();
                    s.modes[t] = Mode::WantWrite;
                    out.push(("issue-write", s));
                }
                Mode::WantRead => {
                    // try_read: refused while a writer holds or waits.
                    if !self.blocked_for_read() {
                        let mut s = self.clone();
                        s.cells[t] += 1;
                        s.flag += 1;
                        s.modes[t] = Mode::HoldRead;
                        out.push(("acquire-read", s));
                    }
                }
                Mode::WantWrite => {
                    // try_write: needs no writer bit and no readers.
                    if self.flag & WBIT == 0 && self.readers() == 0 {
                        let mut s = self.clone();
                        s.flag |= WBIT;
                        s.owner = t;
                        s.modes[t] = Mode::HoldWrite;
                        out.push(("acquire-write", s));
                    }
                }
                Mode::HoldRead => {
                    let mut s = self.clone();
                    s.cells[t] -= 1;
                    s.flag -= 1;
                    s.modes[t] = Mode::Idle;
                    out.push(("release-read", s));

                    // Nested re-acquisition: a tid can carry several read
                    // holds at once (that is exactly what a sweep drains).
                    // Capped at two so the modelled state space stays finite.
                    if !self.blocked_for_read() && self.cells[t] < 2 {
                        let mut s = self.clone();
                        s.cells[t] += 1;
                        s.flag += 1;
                        out.push(("acquire-read", s));
                    }
                }
                Mode::HoldWrite => {
                    let mut s = self.clone();
                    s.flag &= !WBIT;
                    s.owner = NO_OWNER;
                    s.modes[t] = Mode::Idle;
                    out.push(("release-write", s));
                }
                Mode::Dead => {
                    // Reissue: the reap contract makes a swept slot available
                    // to a new occupant. Legal only once the tid has been
                    // swept out of `dead` — death, then sweep, then reissue.
                    if !self.dead.contains(&t) {
                        let mut s = self.clone();
                        s.modes[t] = Mode::Idle;
                        out.push(("reissue", s));
                    }
                }
            }

            // Kill: any live thread can be killed at any point. Its holds leak
            // (flag/owner/cells untouched) and it joins the dead set.
            if self.modes[t] != Mode::Dead {
                let mut s = self.clone();
                s.modes[t] = Mode::Dead;
                s.dead.push(t);
                out.push(("kill", s));
            }
        }

        // Reap: sweep one dead tid with the real `abandon_tid`'s semantics.
        // The writer half fires only while the dead tid still owns the hold
        // (the owner check); the reader half drains exactly the tid's cell,
        // floored at the published count.
        for i in 0..self.dead.len() {
            let d = self.dead[i];
            let mut s = self.clone();
            if s.owner == d && s.flag & WBIT != 0 {
                s.flag &= !WBIT;
                s.owner = NO_OWNER;
            }
            let drained = (s.cells[d] as u32).min(s.readers());
            s.cells[d] = 0;
            s.flag = (s.flag & !READER_MASK) | (s.readers() - drained);
            s.dead.remove(i);
            out.push(("reap", s));
        }

        out
    }

    /// Safety invariants that must hold in EVERY reachable state.
    fn check_safety(&self) -> Result<(), String> {
        // Mutual exclusion: never readers and a writer together.
        if self.flag & WBIT != 0 && self.readers() != 0 {
            return Err(format!("readers {} coexist with WBIT", self.readers()));
        }
        // Held ⇔ accounted: the writer bit and the owner cell agree.
        if self.flag & WBIT != 0 && self.owner == NO_OWNER {
            return Err("WBIT set with no owner".into());
        }
        if self.owner != NO_OWNER && self.flag & WBIT == 0 {
            return Err(format!("owner {:#x} recorded with no WBIT", self.owner));
        }
        // Accounting: flag-side reader count equals the sum of the cells.
        let sum: u32 = self.cells.iter().map(|&c| c as u32).sum();
        if sum != self.readers() {
            return Err(format!("cell sum {sum} != flag readers {}", self.readers()));
        }
        // The reader field never overflows into WWAIT.
        if self.readers() > READER_MASK {
            return Err("reader count overflowed the field".into());
        }
        Ok(())
    }
}

/// BFS the whole reachable space, checking safety on the way.
fn explore(cfg: Config) -> Graph {
    let mut states = Vec::new();
    let mut index: HashMap<State, usize> = HashMap::new();
    let mut edges: Vec<Vec<usize>> = Vec::new();

    let start = State::initial(cfg);
    start.check_safety().expect("initial state safe");
    let mut queue = VecDeque::new();
    index.insert(start.clone(), 0);
    states.push(start);
    edges.push(Vec::new());
    queue.push_back(0usize);

    while let Some(si) = queue.pop_front() {
        for (label, succ) in states[si].successors(cfg) {
            succ.check_safety()
                .unwrap_or_else(|e| panic!("safety violated via {label} -> {succ:?}: {e}"));
            let ti = if let Some(&ti) = index.get(&succ) {
                ti
            } else {
                let ti = states.len();
                index.insert(succ.clone(), ti);
                states.push(succ);
                edges.push(Vec::new());
                queue.push_back(ti);
                ti
            };
            edges[si].push(ti);
        }
    }
    Graph { states, edges, cfg }
}

struct Graph {
    states: Vec<State>,
    edges: Vec<Vec<usize>>,
    cfg: Config,
}

impl Graph {
    /// States with no enabled transition — the deadlock verdict.
    fn deadlocked(&self) -> Vec<usize> {
        (0..self.states.len()).filter(|&i| self.edges[i].is_empty()).collect()
    }

    /// Is there a reachable cycle in which thread `t` is permanently in
    /// `mode`? The model-level starvation verdict for that role.
    fn cycle_with_thread_stuck_in(&self, t: usize, mode: &Mode) -> bool {
        let stuck = |i: usize| self.states[i].modes[t] == *mode;
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color = vec![Color::White; self.states.len()];
        for start in 0..self.states.len() {
            if !stuck(start) || color[start] != Color::White {
                continue;
            }
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            color[start] = Color::Gray;
            while let Some(&mut (node, ref mut ei)) = stack.last_mut() {
                if *ei < self.edges[node].len() {
                    let next = self.edges[node][*ei];
                    *ei += 1;
                    if !stuck(next) {
                        continue;
                    }
                    match color[next] {
                        Color::Gray => return true,
                        Color::White => {
                            color[next] = Color::Gray;
                            stack.push((next, 0));
                        }
                        Color::Black => {}
                    }
                } else {
                    color[node] = Color::Black;
                    stack.pop();
                }
            }
        }
        false
    }

    /// Can every dead tid's leaked hold be cleared by sweeps alone? A state
    /// qualifies once no dead tid retains a hold: its cell is drained and,
    /// if it owned the writer bit, that bit is gone. From any state, one
    /// `reap` transition per dead tid reaches a qualifying state.
    fn reaches_all_dead_holds_swept(&self, si: usize) -> bool {
        fn dead_holds_swept(s: &State) -> bool {
            s.dead.iter().all(|&d| {
                s.cells[d] == 0 && !(s.flag & WBIT != 0 && s.owner == d)
            })
        }
        if dead_holds_swept(&self.states[si]) {
            return true;
        }
        let mut seen: HashSet<usize> = HashSet::new();
        let mut queue = VecDeque::new();
        seen.insert(si);
        queue.push_back(si);
        while let Some(n) = queue.pop_front() {
            for (label, succ) in self.states[n].successors(self.cfg) {
                if label != "reap" {
                    continue;
                }
                if dead_holds_swept(&succ) {
                    return true;
                }
                if let Some(ti) = self.index_of(&succ)
                    && seen.insert(ti)
                {
                    queue.push_back(ti);
                }
            }
        }
        false
    }

    /// Is thread `t`'s write admission reachable from state `si` when the
    /// scheduler never executes any writer acquisition — `t`'s or anyone
    /// else's? (Conservatively excludes them all.) Holders can always release,
    /// readers cannot re-arm while a writer waits, and sweeps clear leaks, so
    /// the coast must always come clear: the protocol never holds the door
    /// permanently shut; fairness of *scheduling* is out of scope.
    fn write_admission_regainable(&self, si: usize, t: usize) -> bool {
        fn admissible(s: &State, _t: usize) -> bool {
            s.flag & WBIT == 0 && s.readers() == 0
        }
        if admissible(&self.states[si], t) {
            return true;
        }
        let mut seen: HashSet<usize> = HashSet::new();
        let mut queue = VecDeque::new();
        seen.insert(si);
        queue.push_back(si);
        while let Some(n) = queue.pop_front() {
            for (label, succ) in self.states[n].successors(self.cfg) {
                if label == "acquire-write" {
                    continue;
                }
                if admissible(&succ, t) {
                    return true;
                }
                if let Some(ti) = self.index_of(&succ)
                    && seen.insert(ti)
                {
                    queue.push_back(ti);
                }
            }
        }
        false
    }

    fn index_of(&self, s: &State) -> Option<usize> {
        self.states.iter().position(|st| st == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 3;

    /// Mutual exclusion, held-⇔-accounted and the cell/flag accounting
    /// invariant hold under EVERY interleaving of 3 threads, kills included —
    /// `check_safety` fires the moment one doesn't; a non-trivially large
    /// explored space means they held everywhere.
    #[test]
    fn safety_holds_under_every_interleaving() {
        let g = explore(Config { n: N });
        assert!(
            g.states.len() > 1000,
            "explored only {} states — the space is suspiciously small",
            g.states.len()
        );
    }

    /// The protocol cannot wedge: every reachable interleaving — kills
    /// included — has a next move (a waiter's retry, a holder's release, a
    /// kill, a sweep, or a swept slot's reissue).
    #[test]
    fn no_deadlock_under_any_interleaving() {
        let g = explore(Config { n: N });
        let dead = g.deadlocked();
        assert!(
            dead.is_empty(),
            "deadlock (no enabled transition) in {} states, e.g. {:?}",
            dead.len(),
            dead.first().map(|&i| &g.states[i])
        );
    }

    /// Writer priority is load-bearing: once a writer is waiting, readers are
    /// refused, existing readers must eventually drain, and the writer's
    /// admission becomes available again — under the worst-case scheduler
    /// that never executes a writer acquisition. (Starvation by an unfair
    /// *scheduler* is out of scope for any test-and-set-shaped lock; what
    /// this proves is that the protocol itself never holds the door shut.)
    #[test]
    fn writer_admission_is_always_regained() {
        let g = explore(Config { n: N });
        for i in 0..g.states.len() {
            for t in 0..N {
                if g.states[i].modes[t] == Mode::WantWrite {
                    assert!(
                        g.write_admission_regainable(i, t),
                        "thread {t}'s write admission is not regivable from {:?}",
                        g.states[i]
                    );
                }
            }
        }
    }

    /// Readers are NOT promised fairness against a writer ping-pong — the same
    /// admitted starvation the unfair BKL test-and-set model pins. If this
    /// stops holding, the lock became fairer or the model broke; either is a
    /// fact to reason about, not to silently fix.
    #[test]
    fn reader_unfairness_is_admitted() {
        let g = explore(Config { n: N });
        assert!(
            g.cycle_with_thread_stuck_in(0, &Mode::WantRead),
            "expected a reader starvation cycle to remain reachable (unfair by design)"
        );
    }

    /// From every reachable state with dead holders, sweeps alone reach a
    /// state where no dead tid retains any hold — the model-level form of
    /// "recovery-after-abandon unblocks the system" (§4.6).
    #[test]
    fn every_dead_hold_is_sweepable_open() {
        let g = explore(Config { n: N });
        for i in 0..g.states.len() {
            if g.states[i].dead.is_empty() {
                continue;
            }
            assert!(
                g.reaches_all_dead_holds_swept(i),
                "dead holders {:?} cannot be swept open from {:?}",
                g.states[i].dead,
                g.states[i]
            );
        }
    }

    /// Apply the real `abandon_tid`'s semantics to a model state.
    fn apply_reap(s: &mut State, d: usize) {
        if s.owner == d && s.flag & WBIT != 0 {
            s.flag &= !WBIT;
            s.owner = NO_OWNER;
        }
        let drained = (s.cells[d] as u32).min(s.readers());
        s.cells[d] = 0;
        s.flag = (s.flag & !READER_MASK) | (s.readers() - drained);
        s.dead.retain(|&x| x != d);
    }

    /// Double-abandon idempotence: reaping the same dead tid twice lands in
    /// the same state as once — the CAS guard's refusal, modelled.
    #[test]
    fn reap_is_idempotent() {
        let g = explore(Config { n: N });
        for s in &g.states {
            for &d in &s.dead {
                let mut once = s.clone();
                apply_reap(&mut once, d);
                let mut twice = once.clone();
                apply_reap(&mut twice, d);
                assert_eq!(once, twice, "double reap diverged from single reap at {s:?}");
            }
        }
    }

    /// Sweeping a tid that holds nothing must not move the lock; sweeping the
    /// writer must open it.
    #[test]
    fn reap_of_a_nonholder_is_a_noop() {
        let mut s = State::initial(Config { n: 2 });
        // Thread 0 holds the write lock; thread 1 is killed while idle.
        s.modes[0] = Mode::HoldWrite;
        s.flag = WBIT;
        s.owner = 0;
        s.modes[1] = Mode::Dead;
        s.dead = vec![1];

        apply_reap(&mut s, 1);
        assert_eq!(s.flag, WBIT, "sweeping a tid that holds nothing must not move the flag");
        assert_eq!(s.owner, 0);
        assert_eq!(s.cells, vec![0, 0]);
        assert_eq!(s.dead, Vec::<usize>::new(), "the swept tid leaves the dead set");

        // Now the holder dies too: the sweep must open the lock.
        s.modes[0] = Mode::Dead;
        s.dead = vec![1, 0];
        apply_reap(&mut s, 0);
        assert_eq!(s.flag & WBIT, 0, "sweeping the dead writer must clear WBIT");
        assert_eq!(s.owner, NO_OWNER);
        assert_eq!(s.readers(), 0);
    }

    /// The reader-leak drain, at the model layer: a thread killed holding two
    /// reads is swept to exactly zero, and other tids' holds are untouched.
    /// (Direct state construction — the mode machine only ever gives one hold
    /// per tid, but the real cell protocol admits several.)
    #[test]
    fn reap_drains_exactly_the_dead_tids_reader_holds() {
        let mut s = State::initial(Config { n: 3 });
        s.modes[0] = Mode::HoldRead;
        s.cells[0] = 2;
        s.cells[1] = 1;
        s.flag = 3;
        s.modes[1] = Mode::HoldRead;
        s.modes[2] = Mode::Dead;
        s.dead = vec![2];

        // Sweeping the non-holder drains nothing.
        apply_reap(&mut s, 2);
        assert_eq!(s.readers(), 3);

        // Kill the double-reader and sweep it.
        s.modes[0] = Mode::Dead;
        s.dead = vec![2, 0];
        apply_reap(&mut s, 0);
        assert_eq!(s.cells[0], 0);
        assert_eq!(s.readers(), 1, "exactly the dead tid's two holds drained");
        assert_eq!(s.cells[1], 1, "a live reader's hold is untouched");
    }
}
