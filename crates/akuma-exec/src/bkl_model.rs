//! Host-only verification of the Big Kernel Lock (BKL) concurrency protocol.
//!
//! Verifying the shared-kernel SMP locking on real hardware means intermittent, minutes-long
//! QEMU `SMP=4` boots. This module gives a fast, deterministic host substitute for the parts
//! that are pure logic:
//!
//! * [`mod checker`] — an **exhaustive state-space model checker**. It encodes the BKL
//!   acquire / release / reconcile protocol and N concurrent cores that each cycle between
//!   userspace (EL0, no lock) and kernel excursions (EL1, lock held for a bounded number of
//!   "work" steps). BFS enumerates *every* interleaving over a small configuration and checks:
//!     - **mutual exclusion** — never two cores in EL1 at once, and the owner-tracked invariant
//!       "BKL held ⇔ exactly one core in EL1" holds in every reachable state;
//!     - **deadlock-freedom** — every reachable state has at least one enabled transition
//!       (no interleaving wedges all cores);
//!     - **starvation ⇔ fairness** — with an *unfair* acquire (today's test-and-set BKL) a
//!       reachable cycle exists in which one waiter is never served; with a *FIFO* acquire it
//!       does not. This is the model-level image of the `owner=1` monopolization we root-caused
//!       for M5c step-2 (see `docs/runbooks/debug-smp.md`).
//!     - **bounded wait vs. hold length** — under the fair lock the worst-case wait a waiter
//!       observes grows with the kernel hold length, quantifying why shortening BKL holds
//!       (dropping it around block I/O) reduces contention latency.
//!
//! * [`kernel_lock_concurrent_stress`] — a **real-atomics stress test** that runs the actual
//!   [`crate::sync::KernelLock`] under `std::thread` contention with a watchdog, asserting
//!   mutual exclusion and idempotent nested acquire hold in the shipping code, not just the model.
//!
//! **Scope / fidelity (honest limits):** the model checks the *lock protocol and its
//! contention properties*. It deliberately does **not** model the scheduler's ready-pool, the
//! `eret`/frame-SPSR reconcile arithmetic, or timer timing — those are only exercised by a real
//! boot. So this catches lock-ordering deadlocks and the fairness/monopoly class; it does not
//! replace the QEMU `SMP=4` run for the low-level context-switch path.

use std::collections::{HashMap, VecDeque};

/// State of one core in the model.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Mode {
    /// Running its thread in userspace (EL0). Holds no lock.
    User,
    /// Trapped into the kernel (syscall/IRQ) and spinning to acquire the BKL.
    Want,
    /// In the kernel (EL1) holding the BKL, with `n` work steps left before it can return.
    /// `n` models a bounded kernel hold — e.g. an ELF-load/`read_file` excursion. The lock is
    /// coarse: the core cannot release mid-excursion (that is the whole point of the BKL), so
    /// while `n > 0` this core is pinned holding the lock.
    Kernel(u8),
}

/// A whole-machine state: per-core mode, the lock owner, and (fair mode only) the FIFO waiter
/// queue. Hashable so BFS can dedup the reachable set.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct State {
    /// Lock owner core, or `None` when free. Mirrors `KernelLock`'s `owner` (0 = free there).
    bkl: Option<u8>,
    cores: Vec<Mode>,
    /// FIFO order of cores currently in `Want`. Empty and unused when `Config::fair` is false.
    queue: Vec<u8>,
}

/// A model configuration. Kept tiny so the state space is exhaustively searchable in ms.
#[derive(Clone, Copy, Debug)]
struct Config {
    /// Number of cores.
    n: u8,
    /// Kernel hold length in work steps (how long a core holds the BKL per excursion).
    work: u8,
    /// `true` = FIFO hand-off (the proposed fair/queued BKL); `false` = the current unfair
    /// test-and-set (any waiter may win).
    fair: bool,
}

impl State {
    fn initial(cfg: &Config) -> Self {
        State {
            bkl: None,
            cores: vec![Mode::User; cfg.n as usize],
            queue: Vec::new(),
        }
    }

    /// All states reachable in one transition, each with a short label (for diagnostics).
    fn successors(&self, cfg: &Config) -> Vec<(&'static str, State)> {
        let mut out = Vec::new();
        for c in 0..cfg.n {
            let ci = c as usize;
            match self.cores[ci] {
                Mode::User => {
                    // Trap into the kernel (issue a syscall / take an IRQ).
                    let mut s = self.clone();
                    s.cores[ci] = Mode::Want;
                    if cfg.fair {
                        s.queue.push(c);
                    }
                    out.push(("syscall", s));
                }
                Mode::Want => {
                    // Acquire the BKL if free. Unfair: any waiter may win. Fair: only the
                    // queue head may win. This is the single knob that distinguishes today's
                    // test-and-set lock from a FIFO hand-off.
                    let may_win = if cfg.fair {
                        self.queue.first() == Some(&c)
                    } else {
                        true
                    };
                    if self.bkl.is_none() && may_win {
                        let mut s = self.clone();
                        s.cores[ci] = Mode::Kernel(cfg.work);
                        s.bkl = Some(c);
                        if cfg.fair {
                            // Remove c (the head) from the waiter queue.
                            s.queue.retain(|&x| x != c);
                        }
                        out.push(("acquire", s));
                    }
                }
                Mode::Kernel(w) => {
                    if w > 0 {
                        // Make progress inside the excursion (still holding the lock).
                        let mut s = self.clone();
                        s.cores[ci] = Mode::Kernel(w - 1);
                        out.push(("work", s));
                    } else {
                        // Excursion done: reconcile back to EL0 and release the lock.
                        let mut s = self.clone();
                        s.cores[ci] = Mode::User;
                        s.bkl = None;
                        out.push(("release", s));
                    }
                }
            }
        }
        out
    }

    /// Safety invariants that must hold in EVERY reachable state.
    fn check_safety(&self, cfg: &Config) -> Result<(), String> {
        // Mutual exclusion: at most one core in EL1 (Kernel).
        let in_kernel: Vec<u8> = (0..cfg.n)
            .filter(|&c| matches!(self.cores[c as usize], Mode::Kernel(_)))
            .collect();
        if in_kernel.len() > 1 {
            return Err(format!("mutual exclusion violated: cores {in_kernel:?} both in EL1"));
        }
        // "Held iff in EL1": the lock owner is exactly the (unique) kernel core.
        match (self.bkl, in_kernel.first()) {
            (Some(owner), Some(&k)) if owner == k => {}
            (None, None) => {}
            (bkl, k) => {
                return Err(format!(
                    "held-iff-EL1 violated: bkl={bkl:?} but kernel core={k:?}"
                ));
            }
        }
        Ok(())
    }
}

/// Exhaustive BFS explorer + property checks over a model [`Config`].
mod checker {
    use super::*;

    /// The full reachable graph: state → its successor states (by index into `states`).
    pub struct Graph {
        pub states: Vec<State>,
        pub edges: Vec<Vec<usize>>,
    }

    /// BFS the whole reachable state space, checking safety on the way, and return the graph.
    /// Panics on a safety violation (with the offending state) — that is a test failure.
    pub fn explore(cfg: &Config) -> Graph {
        let mut states = Vec::new();
        let mut index: HashMap<State, usize> = HashMap::new();
        let mut edges: Vec<Vec<usize>> = Vec::new();

        let start = State::initial(cfg);
        start.check_safety(cfg).expect("initial state safe");
        let mut queue = VecDeque::new();
        index.insert(start.clone(), 0);
        states.push(start);
        edges.push(Vec::new());
        queue.push_back(0usize);

        while let Some(si) = queue.pop_front() {
            // `successors` returns an owned Vec, so the immutable borrow of `states` ends
            // before we mutate `states` (push new nodes) in the loop body.
            let succs = states[si].successors(cfg);
            for (_label, succ) in succs {
                succ
                    .check_safety(cfg)
                    .unwrap_or_else(|e| panic!("safety violated reaching {succ:?}: {e}"));
                let ti = match index.get(&succ) {
                    Some(&ti) => ti,
                    None => {
                        let ti = states.len();
                        index.insert(succ.clone(), ti);
                        states.push(succ.clone());
                        edges.push(Vec::new());
                        queue.push_back(ti);
                        ti
                    }
                };
                edges[si].push(ti);
            }
        }
        Graph { states, edges }
    }

    /// Deadlock-freedom: every reachable state has ≥1 enabled transition (successor).
    pub fn deadlocked_states(g: &Graph) -> Vec<usize> {
        (0..g.states.len()).filter(|&i| g.edges[i].is_empty()).collect()
    }

    /// Is there a reachable cycle in which core `c` is `Want` in every state?
    /// Such a cycle means an interleaving exists where `c` waits forever → starvation.
    /// Detected by cycle-finding in the subgraph induced by states with `cores[c] == Want`.
    pub fn starvation_cycle_exists(g: &Graph, c: u8) -> bool {
        let want = |i: usize| matches!(g.states[i].cores[c as usize], Mode::Want);
        // Iterative DFS with a color map over the induced subgraph.
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color = vec![Color::White; g.states.len()];
        // Stack holds (node, next-edge-index) for iterative DFS that can detect back-edges.
        for start in 0..g.states.len() {
            if !want(start) || color[start] != Color::White {
                continue;
            }
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            color[start] = Color::Gray;
            while let Some(&mut (node, ref mut ei)) = stack.last_mut() {
                if *ei < g.edges[node].len() {
                    let next = g.edges[node][*ei];
                    *ei += 1;
                    if !want(next) {
                        continue; // stay inside the induced subgraph
                    }
                    match color[next] {
                        Color::Gray => return true, // back-edge → cycle with c always Want
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

    /// Worst-case wait, in transitions, that core `c` can observe between entering `Want` and
    /// reaching `Kernel` — i.e. the longest simple path in the `Want`-induced subgraph from any
    /// entry into `Want` to a state from which `c` acquires. Returns `None` if unbounded (a
    /// starvation cycle exists). Used to quantify how hold length affects contention latency.
    pub fn max_wait(g: &Graph, c: u8) -> Option<u64> {
        if starvation_cycle_exists(g, c) {
            return None;
        }
        let want = |i: usize| matches!(g.states[i].cores[c as usize], Mode::Want);
        // DAG (no cycle inside the subgraph) → longest path via memoized DFS. Depth = number
        // of transitions c endures while still Want before it leaves Want (by acquiring).
        let mut memo: HashMap<usize, u64> = HashMap::new();
        fn longest(
            node: usize,
            g: &Graph,
            want: &dyn Fn(usize) -> bool,
            memo: &mut HashMap<usize, u64>,
        ) -> u64 {
            if let Some(&v) = memo.get(&node) {
                return v;
            }
            memo.insert(node, 0); // guard (subgraph is acyclic here)
            let mut best = 0u64;
            for &next in &g.edges[node] {
                if want(next) {
                    best = best.max(1 + longest(next, g, want, memo));
                }
            }
            memo.insert(node, best);
            best
        }
        let mut worst = 0u64;
        for i in 0..g.states.len() {
            if want(i) {
                worst = worst.max(longest(i, g, &want, &mut memo));
            }
        }
        Some(worst)
    }
}

#[cfg(test)]
mod tests {
    use super::checker::*;
    use super::*;

    // ---- A. Exhaustive model-checker properties ------------------------------------------

    /// The BKL protocol never lets two cores into EL1 at once and preserves "held ⇔ in EL1",
    /// under every interleaving of 3 cores. (Safety is asserted inside `explore`; reaching a
    /// nonzero state count means the invariants held all the way through.)
    #[test]
    fn mutual_exclusion_holds_all_interleavings() {
        for &fair in &[false, true] {
            let cfg = Config { n: 3, work: 2, fair };
            let g = explore(&cfg);
            assert!(g.states.len() > 1, "explored a nontrivial state space");
        }
    }

    /// The lock protocol cannot deadlock: every reachable interleaving has a next move.
    /// This is the direct answer to "check the state machine for potential deadlocks".
    #[test]
    fn no_deadlock_under_any_interleaving() {
        for &fair in &[false, true] {
            for work in 1..=3u8 {
                let cfg = Config { n: 3, work, fair };
                let g = explore(&cfg);
                let dead = deadlocked_states(&g);
                assert!(
                    dead.is_empty(),
                    "deadlock (no enabled transition) in {:?} at states {:?}",
                    cfg,
                    dead.iter().map(|&i| &g.states[i]).collect::<Vec<_>>()
                );
            }
        }
    }

    /// Today's UNFAIR test-and-set BKL admits starvation: there is a reachable cycle in which a
    /// waiter is never served. This is the model-level reproduction of the M5c `owner=1`
    /// monopolization. If this ever stops holding, the model no longer reflects the real lock.
    #[test]
    fn unfair_lock_admits_starvation() {
        let cfg = Config { n: 3, work: 2, fair: false };
        let g = explore(&cfg);
        assert!(
            starvation_cycle_exists(&g, 0),
            "expected the unfair lock to admit a starvation cycle for a waiter"
        );
    }

    /// A FIFO (fair/queued) BKL eliminates starvation entirely: no reachable cycle leaves any
    /// core waiting forever. This is the model-level proof that a fair hand-off fixes the
    /// monopolization class — the validation target for a fair-BKL change.
    #[test]
    fn fair_lock_eliminates_starvation() {
        let cfg = Config { n: 3, work: 3, fair: true };
        let g = explore(&cfg);
        for c in 0..cfg.n {
            assert!(
                !starvation_cycle_exists(&g, c),
                "fair lock must not starve any core; core {c} had a starvation cycle"
            );
        }
    }

    /// Under the fair lock the worst-case wait is bounded and grows with the kernel hold
    /// length — quantifying why shortening BKL holds (dropping the lock around block I/O)
    /// reduces cross-core contention latency.
    #[test]
    fn fair_lock_wait_bounded_and_grows_with_hold_length() {
        let mut prev: Option<u64> = None;
        for work in 1..=4u8 {
            let cfg = Config { n: 3, work, fair: true };
            let g = explore(&cfg);
            let w = max_wait(&g, 0).expect("fair lock has bounded wait");
            if let Some(p) = prev {
                assert!(
                    w >= p,
                    "worst-case wait should be non-decreasing in hold length: work={work} wait={w} < prev={p}"
                );
            }
            prev = Some(w);
        }
    }

    // ---- B. Real-atomics stress on the shipping KernelLock -------------------------------

    /// Hammer the ACTUAL [`crate::sync::KernelLock`] from several OS threads (standing in for
    /// cores) and assert mutual exclusion + idempotent nested acquire hold under genuine
    /// contention. A watchdog turns a hang (real deadlock) into an abort instead of a timeout.
    #[test]
    fn kernel_lock_concurrent_stress() {
        use crate::sync::KernelLock;
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
        use std::sync::Arc;
        use std::thread;
        use std::time::{Duration, Instant};

        const FREE: u32 = u32::MAX;
        let n_cores: u32 = 4;
        let iters: u64 = 40_000;

        let bkl = Arc::new(KernelLock::new());
        let holder = Arc::new(AtomicU32::new(FREE));
        let progress = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));

        // Watchdog: if global progress stalls for 10s, the lock deadlocked — abort loudly
        // rather than hang the whole test binary.
        let wd = {
            let progress = Arc::clone(&progress);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut last = 0u64;
                let mut last_change = Instant::now();
                while !done.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(200));
                    let now = progress.load(Ordering::Relaxed);
                    if now != last {
                        last = now;
                        last_change = Instant::now();
                    } else if last_change.elapsed() > Duration::from_secs(10) {
                        eprintln!("[bkl_model] DEADLOCK: progress stalled at {now}");
                        std::process::abort();
                    }
                }
            })
        };

        let mut handles = Vec::new();
        for core in 0..n_cores {
            let bkl = Arc::clone(&bkl);
            let holder = Arc::clone(&holder);
            let progress = Arc::clone(&progress);
            handles.push(thread::spawn(move || {
                for _ in 0..iters {
                    bkl.acquire(core);
                    // We hold it: claim the critical section and assert it was free.
                    let prev = holder.swap(core, Ordering::AcqRel);
                    assert_eq!(prev, FREE, "two cores in the critical section at once");
                    // Idempotent nested re-acquire (models an IRQ nesting inside a syscall).
                    bkl.acquire(core);
                    assert!(bkl.held_by(core));
                    // Release the critical section, then the lock (one release per excursion).
                    holder.store(FREE, Ordering::Release);
                    bkl.release(core);
                    progress.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked (assertion failed)");
        }
        done.store(true, Ordering::Relaxed);
        wd.join().ok();

        assert_eq!(progress.load(Ordering::Relaxed), n_cores as u64 * iters);
        assert!(!bkl.is_held(), "lock left held after all excursions balanced");
    }
}
