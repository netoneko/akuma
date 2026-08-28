//! The process-table slot lifecycle, small enough to enumerate exhaustively.
//!
//! # What this decides
//!
//! [`IDENTITY_CACHE_SMP_REVIEW.md`](../../../../docs/archive/IDENTITY_CACHE_SMP_REVIEW.md)
//! records two use-after-free findings, both **found by inspection**, both with
//! the same failure mode — *a silent write into a reallocated heap block* — and
//! neither reproducible on demand. A green `forktest_smp_matrix` run is
//! evidence the windows are narrow, not evidence they are closed; the doc says
//! so itself, twice.
//!
//! That is what a bounded exhaustive search is for. The slot lifecycle is five
//! operations over a handful of states, so "does an admissible interleaving
//! exist?" is a question with an *answer*, not an estimate. This module models
//! `claim / retire / reclaim / stamp / validate` at 2 cores × 2 slots and
//! enumerates every ordering.
//!
//! Results, all reproduced by the tests in `tests.rs`:
//!
//! | question | answer |
//! |---|---|
//! | Finding A — epilogue writes through the prologue's pointer | **witness at depth 2** |
//! | …with the epilogue re-reading the cache instead | **no witness** to the search bound |
//! | Finding B — `ACTIVE`-only validation | **witness at depth 3** |
//! | …with the shipped [`Validation::Generation`] | **no witness** |
//! | …with [`Validation::PointerOnly`] | **witness** — address reuse, exactly as the doc argues |
//! | …with [`Validation::PointerAndPid`] | **no witness** — sound, and costlier |
//!
//! # What this is not
//!
//! A model of the *memory* ordering. Every operation here is atomic and
//! sequentially consistent, so this search can tell you an interleaving is
//! admissible; it cannot tell you a barrier is missing. It also does not model
//! the BKL, IRQ masking, or how long a window stays open — a witness at depth 2
//! says nothing about how often depth 2 occurs, which is precisely why Finding
//! A read `epi_stale=0` under a real SMP=4 soak while still being a defect.
//!
//! Keep those apart: **enumeration answers "can it?", the soak answers "does
//! it?", and the second is not a substitute for the first.**

/// Slots in the modelled table.
///
/// The real `MAX_PROCESSES` is 256; two is enough for every interleaving that
/// matters, because a witness only ever needs the slot under test plus one
/// other to contend for the freed allocation.
pub const SLOTS: usize = 2;

/// `table::SLOT_STATES`. The real lifecycle is
/// `FREE → ACTIVE → RETIRED → (reclaim) → FREE`, and readers fall back on any
/// non-`ACTIVE` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Claimable.
    Free,
    /// Live. The only state a cache read accepts.
    Active,
    /// Unregistered but not yet freed. `PROCESS_RECLAIM_COOLDOWN_US` (10 ms)
    /// is the whole of the grace period, and every idle core is a drain site.
    Retired,
}

/// One `Process` allocation.
///
/// `addr` and `pid` are what a reader can see. `id` is **ghost state**: the
/// true incarnation, visible only to the checker. Separating them is the entire
/// point — the doc's argument against pointer-equality validation is that a
/// reader cannot distinguish two incarnations that share an address, and a
/// model where the reader can see `id` would quietly assume that away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Occupant {
    /// Heap address of the `Process` box. Reused by the modelled allocator as
    /// aggressively as possible, because `Process` is a fixed-size allocation
    /// and that is the case the check has to survive.
    pub addr: u32,
    /// The pid. Never reused in this model.
    pub pid: u32,
    /// Ghost: which incarnation this really is.
    pub id: u32,
}

/// A cached identity, as `table::THREAD_IDENTITY` holds it.
///
/// `generation` is the `SLOT_GEN` stamp taken when the entry was written. `id` is
/// ghost state again: what the entry *meant* when it was stamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheEntry {
    pub slot: usize,
    pub generation: u32,
    pub addr: u32,
    pub pid: u32,
    pub id: u32,
}

/// How a cache read decides the entry still names its occupant.
///
/// The three schemes `IDENTITY_CACHE_SMP_REVIEW.md` § "Why pointer-equality is
/// not enough" weighs against each other, plus the one that shipped before
/// Finding B was fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// `state == ACTIVE` and nothing else — what `identity_get` did when the
    /// review was written. `ACTIVE` cannot distinguish "still ours" from
    /// "freed and re-issued".
    ActiveOnly,
    /// `state == ACTIVE && SLOT_GEN[slot] == stamp`. Two loads, no `Process`
    /// deref. **The scheme that shipped.**
    Generation,
    /// `state == ACTIVE && PROCESS_SLOTS[slot] == cached_ptr`. The cheap-looking
    /// fix the doc rejects: `Process` is a fixed-size allocation, so the
    /// allocator can hand the same address to the next occupant.
    PointerOnly,
    /// `state == ACTIVE && PROCESS_SLOTS[slot] == cached_ptr && (*ptr).pid == pid`,
    /// in that order — the pointer check first is what makes the `pid` read
    /// safe. Sound, and one more load on the `Process` cache line.
    PointerAndPid,
}

/// Where the epilogue gets the identity it writes through.
///
/// Mirrors [`crate::IdentitySource`]; kept as its own type so this module does
/// not drag the excursion machine into a search that is about the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpilogueSource {
    /// Reuse the pointer the prologue resolved. No validation at all.
    Hoisted,
    /// Read the cache again, under `validation`.
    Reread,
}

/// The epilogue policy under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub source: EpilogueSource,
    pub validation: Validation,
}

/// One modelled step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `try_claim_free_slot`: `FREE → ACTIVE`, then `PROCESS_SLOTS[slot]` is
    /// written. The modelled allocator hands back the most recently freed
    /// address when there is one.
    Claim(usize),
    /// `unregister_process`: `ACTIVE → RETIRED`. Does not free, does not touch
    /// `SLOT_GEN`, and — the live trigger for Finding A — happens while the
    /// owning thread may still be executing kernel code.
    Retire(usize),
    /// `reclaim_retired_processes_internal`: `RETIRED → FREE`, the pointer is
    /// swapped out and dropped, and `SLOT_GEN` is bumped **between** the two.
    Reclaim(usize),
}

/// Every op the search can take, in a fixed order so a witness is reproducible.
pub const ALL_OPS: [Op; SLOTS * 3] = [
    Op::Claim(0),
    Op::Retire(0),
    Op::Reclaim(0),
    Op::Claim(1),
    Op::Retire(1),
    Op::Reclaim(1),
];

/// The modelled table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct World {
    state: [SlotState; SLOTS],
    /// `PROCESS_SLOTS`.
    occ: [Option<Occupant>; SLOTS],
    /// `SLOT_GEN`.
    generation: [u32; SLOTS],
    next_pid: u32,
    next_id: u32,
    /// Most recently freed address, handed straight back on the next claim.
    ///
    /// A real allocator *may* do this; this model always does, because the
    /// question is whether a scheme survives the worst case, not what the
    /// average case looks like.
    recycled_addr: Option<u32>,
    next_addr: u32,
}

impl World {
    /// A table with `slot` already claimed, as every interesting search starts.
    #[must_use]
    pub fn with_active(slot: usize) -> Self {
        let mut w = Self {
            state: [SlotState::Free; SLOTS],
            occ: [None; SLOTS],
            generation: [0; SLOTS],
            next_pid: 100,
            next_id: 1,
            recycled_addr: None,
            next_addr: 0x1000,
        };
        w.apply(Op::Claim(slot));
        w
    }

    /// Slot states, for assertions.
    #[must_use]
    pub const fn state(&self, slot: usize) -> SlotState {
        self.state[slot]
    }

    /// The live occupant of `slot`, if any.
    #[must_use]
    pub const fn occupant(&self, slot: usize) -> Option<Occupant> {
        self.occ[slot]
    }

    /// Is `op` a legal transition from here? Illegal ops are skipped by the
    /// search rather than counted, so the depth bound means "real steps".
    #[must_use]
    pub const fn enabled(&self, op: Op) -> bool {
        match op {
            Op::Claim(s) => matches!(self.state[s], SlotState::Free),
            Op::Retire(s) => matches!(self.state[s], SlotState::Active),
            Op::Reclaim(s) => matches!(self.state[s], SlotState::Retired),
        }
    }

    /// Take a step. Callers check [`Self::enabled`] first.
    pub fn apply(&mut self, op: Op) {
        match op {
            Op::Claim(s) => {
                let addr = self.recycled_addr.take().unwrap_or_else(|| {
                    let a = self.next_addr;
                    self.next_addr += 0x100;
                    a
                });
                let o = Occupant { addr, pid: self.next_pid, id: self.next_id };
                self.next_pid += 1;
                self.next_id += 1;
                self.occ[s] = Some(o);
                self.state[s] = SlotState::Active;
            }
            Op::Retire(s) => {
                self.state[s] = SlotState::Retired;
            }
            Op::Reclaim(s) => {
                // The real sequence: swap the pointer out, drop the box, bump
                // the generation, then store FREE. The slot is RETIRED across
                // the bump, and readers already fall back on any non-ACTIVE
                // state, so no reader can observe ACTIVE paired with a stale
                // stamp — that argument is what this ordering encodes.
                if let Some(o) = self.occ[s].take() {
                    self.recycled_addr = Some(o.addr);
                }
                self.generation[s] = self.generation[s].wrapping_add(1);
                self.state[s] = SlotState::Free;
            }
        }
    }

    /// `identity_store_locked`: stamp `slot`'s current occupant into an entry.
    ///
    /// Returns `None` for a slot with no live occupant — the real code stores
    /// an invalid marker there, which reads as a permanent miss (the separate
    /// performance defect fixed by `IDENTITY_CACHE_LAZY_RESTAMP.md`).
    #[must_use]
    pub fn stamp(&self, slot: usize) -> Option<CacheEntry> {
        let o = self.occ[slot]?;
        Some(CacheEntry {
            slot,
            generation: self.generation[slot],
            addr: o.addr,
            pid: o.pid,
            id: o.id,
        })
    }

    /// `identity_get`: what a reader believes it resolved, under `v`.
    ///
    /// Returns the `(addr, pid)` the reader would dereference — deliberately
    /// **not** an [`Occupant`], because a reader cannot see `id`. The checker
    /// compares against the entry's ghost `id` separately.
    #[must_use]
    pub fn validate(&self, e: CacheEntry, v: Validation) -> Option<(u32, u32)> {
        if self.state[e.slot] != SlotState::Active {
            return None;
        }
        match v {
            Validation::ActiveOnly => {}
            Validation::Generation => {
                if self.generation[e.slot] != e.generation {
                    return None;
                }
            }
            Validation::PointerOnly => {
                // Pointer first: it is what proves the cached address is the
                // slot's current live occupant.
                if self.occ[e.slot].map(|o| o.addr) != Some(e.addr) {
                    return None;
                }
            }
            Validation::PointerAndPid => {
                let live = self.occ[e.slot]?;
                if live.addr != e.addr || live.pid != e.pid {
                    return None;
                }
            }
        }
        Some((e.addr, e.pid))
    }

    /// Is a write through `addr`, believed to belong to incarnation `id`, safe?
    ///
    /// Two ways it is not, and they are the two halves of the doc's "silent
    /// write into a reallocated block":
    ///
    /// - the address belongs to no live occupant — a write into freed memory;
    /// - it belongs to a *different* incarnation — a write into someone else's
    ///   `Process`, which is the worse one, because nothing about it is even
    ///   detectable as corruption at the time.
    #[must_use]
    pub fn write_is_safe(&self, addr: u32, id: u32) -> bool {
        self.occ
            .iter()
            .flatten()
            .any(|o| o.addr == addr && o.id == id)
    }
}

/// An interleaving that breaks the invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    /// The ops, in order, that reach the fault. Replay them from
    /// `World::with_active(slot)` to reproduce.
    pub ops: [Option<Op>; MAX_DEPTH],
    /// Why the epilogue's write was unsafe.
    pub fault: Fault,
}

impl Witness {
    /// The op sequence without the trailing `None` padding.
    pub fn steps(&self) -> impl Iterator<Item = Op> + '_ {
        self.ops.iter().flatten().copied()
    }

    /// How many real steps the witness needed.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.ops.iter().flatten().count()
    }
}

/// What went wrong at the epilogue's write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Wrote into an address no live occupant holds.
    WriteAfterFree,
    /// Wrote into an address a *different* incarnation now holds.
    WriteToWrongOccupant,
}

/// Ceiling on the search, and the width of [`Witness::ops`].
///
/// Six is comfortably past every known witness (the deepest is 3) and past the
/// point where more ops add reachable states: with two slots, `claim / retire /
/// reclaim` cycles, so beyond a couple of full cycles the search only revisits
/// states it has already judged. Raising it costs `6^n`.
pub const MAX_DEPTH: usize = 6;

/// Enumerate every interleaving up to `depth` and return the first one where
/// the epilogue's write is unsafe.
///
/// The modelled excursion is the one Finding A is about:
///
/// 1. the prologue stamps and resolves an identity on `slot`;
/// 2. **the dispatch runs** — an open-ended `ppoll` / futex / blocking `read`,
///    during which peers take any sequence of table operations;
/// 3. the epilogue writes through an identity obtained per `policy`.
///
/// Step 2 is where the whole question lives. The search is over *what peers can
/// do while this thread is inside a syscall*, which is why the op budget is a
/// budget on peer activity and not on anything the caller does.
///
/// `None` means no witness exists within `depth` — not that none exists.
#[must_use]
pub fn search(policy: Policy, depth: usize) -> Option<Witness> {
    let world = World::with_active(0);
    let entry = world.stamp(0).expect("slot 0 was just claimed");
    let mut trail = [None; MAX_DEPTH];
    explore(&world, entry, policy, depth.min(MAX_DEPTH), 0, &mut trail)
}

/// Depth-first over [`ALL_OPS`], checking the epilogue at every prefix.
///
/// Checking at every prefix rather than only at the leaves is deliberate: the
/// epilogue can run at any point during the peers' activity, so a witness at
/// depth 2 must be reported as depth 2 and not padded out to the bound.
fn explore(
    world: &World,
    entry: CacheEntry,
    policy: Policy,
    depth: usize,
    at: usize,
    trail: &mut [Option<Op>; MAX_DEPTH],
) -> Option<Witness> {
    if let Some(fault) = epilogue_fault(world, entry, policy) {
        return Some(Witness { ops: *trail, fault });
    }
    if at == depth {
        return None;
    }
    for op in ALL_OPS {
        if !world.enabled(op) {
            continue;
        }
        let mut next = world.clone();
        next.apply(op);
        trail[at] = Some(op);
        if let Some(w) = explore(&next, entry, policy, depth, at + 1, trail) {
            return Some(w);
        }
        trail[at] = None;
    }
    None
}

/// Run the epilogue's identity read and its write, and report a fault if the
/// write lands somewhere it should not.
///
/// A policy that *skips* the write is correct, not faulty: that is exactly what
/// the pre-cache epilogue did when its lookup returned `None`, and restoring it
/// is the whole of the Finding A fix.
fn epilogue_fault(world: &World, entry: CacheEntry, policy: Policy) -> Option<Fault> {
    let (addr, _pid) = match policy.source {
        // No validation at all — the prologue's pointer, straight through.
        EpilogueSource::Hoisted => (entry.addr, entry.pid),
        EpilogueSource::Reread => world.validate(entry, policy.validation)?,
    };
    if world.write_is_safe(addr, entry.id) {
        return None;
    }
    // Distinguish the two, because they are different bugs to read about in a
    // crash dump: one writes into free memory, the other into a live stranger.
    let taken = world.occ.iter().flatten().any(|o| o.addr == addr);
    Some(if taken { Fault::WriteToWrongOccupant } else { Fault::WriteAfterFree })
}
