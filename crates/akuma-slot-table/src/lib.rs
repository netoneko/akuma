//! The lock-free per-slot pointer store that sits under the process table.
//!
//! # What this crate is
//!
//! A [`SlotTable<T, N>`] is a fixed array of `N` slots, each holding:
//!
//! - a **state** — `FREE`, `ACTIVE`, or `RETIRED` (an [`AtomicU8`]);
//! - a **pointer** to a heap `Box<T>` (an [`AtomicPtr<T>`]), non-null while
//!   `ACTIVE` or `RETIRED`;
//! - a **reuse generation** (an [`AtomicU32`]) — bumped once per slot recycle,
//!   so `state == ACTIVE` can be told apart from "same slot, new occupant";
//! - a **retire timestamp** (an [`AtomicU64`]) — set when the slot goes
//!   `RETIRED`, read to enforce a reclamation cooldown.
//!
//! It owns **every dereference** of those pointers. The consumer keeps all
//! domain logic — what `T` is, how a predicate picks a slot, what runs when a
//! slot retires or is freed — and calls in here for each `&T` / `&mut T`.
//!
//! It was carved out of `akuma_exec::process::table` so that crate could take
//! `#![forbid(unsafe_code)]`; the same move `akuma-locks-rw-cell` and
//! `akuma-gic` made. It **cannot** forbid `unsafe` and never will: a raw
//! pointer array whose whole job is to be dereferenced is its subject matter.
//!
//! # The one stated contract
//!
//! Every method that hands out a borrow derived from a slot pointer —
//! [`SlotTable::active_ref`], [`SlotTable::with_active_mut`],
//! [`SlotTable::for_each_active`], [`SlotTable::find_active`],
//! [`SlotTable::ref_if_current`], the `*_locked` variants, and the `unsafe`
//! [`SlotTable::active_exclusive`] — relies on the consumer's **deferred
//! reclamation discipline**:
//!
//! > A slot's `Box<T>` is dropped **only** by [`SlotTable::reclaim_retired`],
//! > and only after a cooldown long enough that no core can still hold a
//! > pointer obtained from a scan that has since returned. Within one
//! > generation a slot's pointer is immutable — written exactly once by
//! > [`SlotTable::try_claim`] (after the winning CAS) and nulled exactly once
//! > by `reclaim_retired` (before the generation bump).
//!
//! On one core, an IRQ mask held across a scan is what makes "since returned"
//! true — `reclaim_retired` runs in EL1, so a masked core cannot be running it.
//! Across cores, that is the caller's lock (the BKL, in `akuma-exec`). This
//! crate provides the IRQ mask (via `akuma-primitives`); the cross-core
//! exclusion and the cooldown are the consumer's.
//!
//! `reclaim_retired`'s generation bump happens **while the slot is `RETIRED`**,
//! between the pointer-nulling swap and the store to `FREE`. Every reader
//! rejects a non-`ACTIVE` state before it looks at the generation, so no reader
//! can observe `ACTIVE` paired with a stale generation. Preserve that ordering
//! if you touch [`SlotTable::reclaim_retired`].

#![cfg_attr(not(test), no_std)]
#![allow(
    clippy::must_use_candidate,
    clippy::too_long_first_doc_paragraph,
    clippy::missing_panics_doc
)]

extern crate alloc;

use alloc::boxed::Box;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

use akuma_primitives::with_irqs_disabled;

/// Slot states.
pub mod state {
    /// Unoccupied and claimable by [`super::SlotTable::try_claim`].
    pub const FREE: u8 = 0;
    /// Occupied and visible to every lookup / scan.
    pub const ACTIVE: u8 = 1;
    /// Retired (logically removed) but not yet freed — invisible to lookups and
    /// to `try_claim`'s free-slot scan, but its `Box<T>` is still live in memory
    /// until [`super::SlotTable::reclaim_retired`] drops it after the cooldown.
    pub const RETIRED: u8 = 2;
}

/// Why [`SlotTable::ref_if_current`] declined to hand back a reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotMiss {
    /// The slot is no longer `ACTIVE` (retired, freed, or index out of range).
    Inactive,
    /// The slot is `ACTIVE` but its generation no longer matches — it was
    /// recycled since the caller stamped it. A different occupant is installed;
    /// the caller's cached pointer would be a use-after-free.
    StaleGen,
    /// The slot is `ACTIVE` with a matching generation but its pointer read
    /// back null (a transient state a racing reader can catch).
    Null,
}

/// A fixed table of `N` pointer slots with `FREE`/`ACTIVE`/`RETIRED` state, a
/// per-slot reuse generation, and a retire timestamp. See the crate docs.
///
/// `SlotTable` is `Sync` for any `T` (its fields are atomics; [`AtomicPtr<T>`]
/// is `Sync` unconditionally), so it is meant to live in a `static`. It has no
/// `Drop`: a `static` never drops, and every occupied slot's `Box<T>` is
/// released through [`Self::reclaim_retired`].
pub struct SlotTable<T, const N: usize> {
    states: [AtomicU8; N],
    slots: [AtomicPtr<T>; N],
    generations: [AtomicU32; N],
    retire_time: [AtomicU64; N],
}

impl<T, const N: usize> Default for SlotTable<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> SlotTable<T, N> {
    /// A table with every slot `FREE`, generation 0, no retire stamp.
    pub const fn new() -> Self {
        Self {
            states: [const { AtomicU8::new(state::FREE) }; N],
            slots: [const { AtomicPtr::new(core::ptr::null_mut()) }; N],
            generations: [const { AtomicU32::new(0) }; N],
            retire_time: [const { AtomicU64::new(0) }; N],
        }
    }

    /// Number of slots.
    pub const fn capacity(&self) -> usize {
        N
    }

    // ── claiming ────────────────────────────────────────────────────────────

    /// Claim the first `FREE` slot for `val`: CAS `FREE → ACTIVE`, then publish
    /// the pointer with `Release`. Returns the slot index, or hands `val` back
    /// unchanged if every slot is taken.
    ///
    /// The CAS is `SeqCst` on success (it orders against every other claimer and
    /// against `reclaim_retired`'s `FREE` store) and `Relaxed` on failure.
    pub fn try_claim(&self, val: Box<T>) -> Result<usize, Box<T>> {
        let ptr = Box::into_raw(val);
        for i in 0..N {
            if self.states[i]
                .compare_exchange(state::FREE, state::ACTIVE, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                self.slots[i].store(ptr, Ordering::Release);
                return Ok(i);
            }
        }
        // SAFETY: `ptr` came from `Box::into_raw` above and no slot took it.
        Err(unsafe { Box::from_raw(ptr) })
    }

    // ── per-slot scalars ────────────────────────────────────────────────────

    /// Whether slot `i` is `ACTIVE` right now (a `Relaxed` load).
    pub fn is_active(&self, i: usize) -> bool {
        i < N && self.states[i].load(Ordering::Relaxed) == state::ACTIVE
    }

    /// Slot `i`'s current reuse generation (`Acquire`), or 0 for an out-of-range
    /// index. Release-paired with the bump in [`Self::reclaim_retired`].
    pub fn generation(&self, i: usize) -> u32 {
        if i < N {
            self.generations[i].load(Ordering::Acquire)
        } else {
            0
        }
    }

    /// Count of `ACTIVE` slots (a scan of `Relaxed` state loads, no pointer
    /// dereference).
    pub fn active_count(&self) -> usize {
        (0..N).filter(|&i| self.states[i].load(Ordering::Relaxed) == state::ACTIVE).count()
    }

    /// Count of `RETIRED` slots awaiting [`Self::reclaim_retired`].
    pub fn retired_count(&self) -> usize {
        (0..N).filter(|&i| self.states[i].load(Ordering::Relaxed) == state::RETIRED).count()
    }

    // ── the cached-identity accessor ────────────────────────────────────────

    /// The occupant of slot `i`, iff it is `ACTIVE` **and** its generation
    /// equals `expected_gen`. Reads state first, generation second, pointer
    /// third — the reverse of the order [`Self::reclaim_retired`] writes them,
    /// which is what lets a caller cache `(slot, generation)` and re-validate
    /// with two loads and no pointer compare.
    ///
    /// The caller need not mask IRQs for this (each load stands alone), but the
    /// returned borrow's validity still rests on the crate contract: the
    /// occupant it names can retire the instant this returns, and stays live
    /// only until its cooldown elapses.
    pub fn ref_if_current(&self, i: usize, expected_gen: u32) -> Result<&T, SlotMiss> {
        if i >= N || self.states[i].load(Ordering::Relaxed) != state::ACTIVE {
            return Err(SlotMiss::Inactive);
        }
        if self.generations[i].load(Ordering::Acquire) != expected_gen {
            return Err(SlotMiss::StaleGen);
        }
        let ptr = self.slots[i].load(Ordering::Acquire);
        if ptr.is_null() {
            return Err(SlotMiss::Null);
        }
        // SAFETY: state is ACTIVE and the generation matches, so within this
        // generation the pointer is immutable and non-null (crate contract).
        Ok(unsafe { &*ptr })
    }

    // ── scans: caller already holds the mask ────────────────────────────────

    /// Raw pointer of the first `ACTIVE` slot whose occupant satisfies `pred`.
    /// **The caller must have IRQs masked** (or otherwise exclude
    /// [`Self::reclaim_retired`] on this core) for the returned pointer to be
    /// safe to dereference.
    pub fn active_ptr_locked(&self, pred: impl Fn(&T) -> bool) -> Option<*mut T> {
        for i in 0..N {
            if self.states[i].load(Ordering::Relaxed) != state::ACTIVE {
                continue;
            }
            let ptr = self.slots[i].load(Ordering::Acquire);
            // SAFETY: state is ACTIVE and, per the crate contract, the caller
            // holds the mask that keeps this slot's pointer live for the scan.
            if !ptr.is_null() && pred(unsafe { &*ptr }) {
                return Some(ptr);
            }
        }
        None
    }

    /// Call `f(index, &T)` for every `ACTIVE` slot, stopping at the first
    /// `Some`. Caller-masked (see [`Self::active_ptr_locked`]).
    pub fn find_active_locked<R>(&self, mut f: impl FnMut(usize, &T) -> Option<R>) -> Option<R> {
        for i in 0..N {
            if self.states[i].load(Ordering::Relaxed) != state::ACTIVE {
                continue;
            }
            let ptr = self.slots[i].load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            // SAFETY: as `active_ptr_locked`.
            if let Some(r) = f(i, unsafe { &*ptr }) {
                return Some(r);
            }
        }
        None
    }

    /// Call `f(index, &T)` for every `ACTIVE` slot. Caller-masked (see
    /// [`Self::active_ptr_locked`]).
    pub fn for_each_active_locked(&self, mut f: impl FnMut(usize, &T)) {
        self.find_active_locked::<()>(|i, p| {
            f(i, p);
            None
        });
    }

    // ── scans: this crate takes the mask ───────────────────────────────────

    /// `&T` for the first `ACTIVE` slot satisfying `pred`, resolved under an IRQ
    /// mask. The borrow may outlive the mask — its validity then rests on the
    /// consumer's cooldown (crate contract).
    pub fn active_ref(&self, pred: impl Fn(&T) -> bool) -> Option<&T> {
        with_irqs_disabled(|| {
            let ptr = self.active_ptr_locked(pred)?;
            // SAFETY: found ACTIVE under the mask; crate contract covers the
            // lifetime past the mask.
            Some(unsafe { &*ptr })
        })
    }

    /// Run `f` with `&mut T` for the first `ACTIVE` slot satisfying `pred`, with
    /// IRQs masked for the whole call. `f` must not allocate or block — it runs
    /// under the mask.
    pub fn with_active_mut<R>(
        &self,
        pred: impl Fn(&T) -> bool,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        with_irqs_disabled(|| {
            let ptr = self.active_ptr_locked(pred)?;
            // SAFETY: ACTIVE under the mask; the mask is exclusion enough for
            // the closure's duration (crate contract).
            Some(f(unsafe { &mut *ptr }))
        })
    }

    /// Call `f(index, &T)` for every `ACTIVE` slot, IRQs masked for the scan.
    pub fn for_each_active(&self, f: impl FnMut(usize, &T)) {
        with_irqs_disabled(|| self.for_each_active_locked(f));
    }

    /// Call `f(index, &T)` for every `ACTIVE` slot, IRQs masked, stopping at the
    /// first `Some`.
    pub fn find_active<R>(&self, f: impl FnMut(usize, &T) -> Option<R>) -> Option<R> {
        with_irqs_disabled(|| self.find_active_locked(f))
    }

    /// Run `f` with `&mut T` for the first `ACTIVE` slot satisfying `pred`,
    /// **without** an IRQ mask.
    ///
    /// # Safety
    /// The caller must guarantee exclusivity **structurally**, not via this
    /// call: the slot's occupant must be one no other core can free or mutate
    /// for the closure's duration (the caller's own process on a BKL-held path,
    /// or a not-yet-published / already-isolated one), and no other reference to
    /// it may be live on this thread across the call. This exists for the
    /// process-lifecycle windows (`execve`'s image replacement, first run) that
    /// allocate and do block I/O inside `f` and so cannot hold a mask.
    pub unsafe fn active_exclusive<R>(
        &self,
        pred: impl Fn(&T) -> bool,
        f: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let ptr = self.active_ptr_locked(pred)?;
        // SAFETY: the caller's structural exclusivity argument (see the doc).
        Some(f(unsafe { &mut *ptr }))
    }

    // ── retire / reclaim ───────────────────────────────────────────────────

    /// Retire the first `ACTIVE` slot whose occupant satisfies `pred`: CAS
    /// `ACTIVE → RETIRED`, stamp `retire_at()`, then call `on_retired(index,
    /// &T)` with the slot still valid. Returns whether this call retired a slot.
    ///
    /// `retire_at` is a closure so the timestamp is read **after** the winning
    /// CAS, not before the scan — the stamp then means "when this slot actually
    /// retired". It is not called at all if no slot matches.
    ///
    /// Losing the CAS (a racing retire of the same slot) skips to the next
    /// match. `on_retired` runs with the occupant still live and `RETIRED` — do
    /// the domain teardown that needs `&T` there; the `Box<T>` itself is dropped
    /// later, by [`Self::reclaim_retired`].
    ///
    /// Not internally masked — the caller runs on a BKL-held path (peers
    /// excluded) or masks around it as its old code did.
    pub fn retire(
        &self,
        retire_at: impl FnOnce() -> u64,
        pred: impl Fn(&T) -> bool,
        on_retired: impl FnOnce(usize, &T),
    ) -> bool {
        for i in 0..N {
            if self.states[i].load(Ordering::Relaxed) != state::ACTIVE {
                continue;
            }
            let ptr = self.slots[i].load(Ordering::Acquire);
            if ptr.is_null() {
                continue;
            }
            // SAFETY: ACTIVE; caller excludes reclaim (crate contract).
            if !pred(unsafe { &*ptr }) {
                continue;
            }
            if self.states[i]
                .compare_exchange(
                    state::ACTIVE,
                    state::RETIRED,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                continue;
            }
            self.retire_time[i].store(retire_at(), Ordering::Release);
            // SAFETY: state is RETIRED and the cooldown has not started
            // elapsing against any freer, so the occupant is provably live.
            on_retired(i, unsafe { &*ptr });
            return true;
        }
        false
    }

    /// Free every `RETIRED` slot whose cooldown has elapsed (or every one, if
    /// `ignore_cooldown`). For each: swap the pointer to null (`AcqRel`), bump
    /// the generation (`AcqRel`) **while still `RETIRED`**, store `FREE`, clear
    /// the retire stamp, call `on_free(index)`, then drop the `Box<T>`. Returns
    /// the count freed.
    ///
    /// `retire_at == 0` is treated as "no stamp" and always eligible — matches
    /// the caller that stamps a real `uptime_us` and never 0.
    ///
    /// Two racers on one slot: exactly one wins the pointer swap and frees; the
    /// other reads null and skips.
    pub fn reclaim_retired(
        &self,
        now: u64,
        cooldown: u64,
        ignore_cooldown: bool,
        mut on_free: impl FnMut(usize),
    ) -> usize {
        let mut count = 0;
        for i in 0..N {
            if self.states[i].load(Ordering::Relaxed) != state::RETIRED {
                continue;
            }
            if !ignore_cooldown {
                let t = self.retire_time[i].load(Ordering::Acquire);
                if t > 0 && now.saturating_sub(t) < cooldown {
                    continue;
                }
            }
            let old = self.slots[i].swap(core::ptr::null_mut(), Ordering::AcqRel);
            if old.is_null() {
                continue;
            }
            // Generation bump BEFORE the slot becomes claimable, while it is
            // still RETIRED. Every reader rejects a non-ACTIVE state before
            // reading the generation, so none can pair ACTIVE with the old
            // stamp. Release-paired with the `Acquire` in `ref_if_current` /
            // `generation`.
            self.generations[i].fetch_add(1, Ordering::AcqRel);
            self.states[i].store(state::FREE, Ordering::Release);
            self.retire_time[i].store(0, Ordering::Relaxed);
            on_free(i);
            // SAFETY: won the swap, so this is the sole owner of `old`; it came
            // from `Box::into_raw` in `try_claim`.
            drop(unsafe { Box::from_raw(old) });
            count += 1;
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::AtomicBool;

    #[derive(Debug)]
    struct Item {
        id: u32,
        dropped: Option<Arc<AtomicBool>>,
    }
    impl Item {
        fn new(id: u32) -> Box<Self> {
            Box::new(Self { id, dropped: None })
        }
        fn tracked(id: u32, flag: Arc<AtomicBool>) -> Box<Self> {
            Box::new(Self { id, dropped: Some(flag) })
        }
    }
    impl Drop for Item {
        fn drop(&mut self) {
            if let Some(f) = &self.dropped {
                f.store(true, Ordering::SeqCst);
            }
        }
    }

    fn find_id(t: &SlotTable<Item, 4>, id: u32) -> Option<u32> {
        t.active_ref(|p| p.id == id).map(|p| p.id)
    }

    #[test]
    fn claim_lookup_retire_reclaim_roundtrip() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        let s0 = t.try_claim(Item::new(10)).unwrap();
        let s1 = t.try_claim(Item::new(20)).unwrap();
        assert_ne!(s0, s1);
        assert_eq!(t.active_count(), 2);
        assert_eq!(find_id(&t, 10), Some(10));
        assert_eq!(find_id(&t, 20), Some(20));
        assert_eq!(find_id(&t, 99), None);

        let mut retired_idx = None;
        assert!(t.retire(|| 1_000, |p| p.id == 10, |i, p| {
            retired_idx = Some((i, p.id));
        }));
        assert_eq!(retired_idx, Some((s0, 10)));
        assert_eq!(t.active_count(), 1);
        assert_eq!(t.retired_count(), 1);
        // retired: invisible to lookup, still not freed
        assert_eq!(find_id(&t, 10), None);

        // cooldown not elapsed → not reclaimed
        assert_eq!(t.reclaim_retired(1_500, 1_000, false, |_| {}), 0);
        assert_eq!(t.retired_count(), 1);

        // cooldown elapsed → reclaimed, on_free sees the index
        let mut freed = Vec::new();
        assert_eq!(t.reclaim_retired(3_000, 1_000, false, |i| freed.push(i)), 1);
        assert_eq!(freed, alloc::vec![s0]);
        assert_eq!(t.retired_count(), 0);

        // slot is FREE again and re-claimable
        let s0b = t.try_claim(Item::new(30)).unwrap();
        assert_eq!(s0b, s0);
    }

    #[test]
    fn box_is_dropped_only_by_reclaim() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        let flag = Arc::new(AtomicBool::new(false));
        t.try_claim(Item::tracked(1, flag.clone())).unwrap();
        t.retire(|| 100, |p| p.id == 1, |_, _| {});
        assert!(!flag.load(Ordering::SeqCst), "retire must not drop");
        t.reclaim_retired(10_000, 10, false, |_| {});
        assert!(flag.load(Ordering::SeqCst), "reclaim must drop");
    }

    #[test]
    fn full_table_hands_the_box_back() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        for i in 0..4 {
            t.try_claim(Item::new(i)).unwrap();
        }
        let back = t.try_claim(Item::new(99)).unwrap_err();
        assert_eq!(back.id, 99);
    }

    #[test]
    fn ref_if_current_tracks_generation() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        let s = t.try_claim(Item::new(7)).unwrap();
        let g = t.generation(s);
        assert!(t.ref_if_current(s, g).is_ok());
        assert_eq!(t.ref_if_current(s, g).unwrap().id, 7);

        // wrong generation
        assert_eq!(t.ref_if_current(s, g + 1).unwrap_err(), SlotMiss::StaleGen);

        // retire → inactive; reclaim → generation moves
        t.retire(|| 1, |p| p.id == 7, |_, _| {});
        assert_eq!(t.ref_if_current(s, g).unwrap_err(), SlotMiss::Inactive);
        t.reclaim_retired(10_000, 1, false, |_| {});
        let s2 = t.try_claim(Item::new(8)).unwrap();
        assert_eq!(s2, s);
        // the pre-recycle stamp is now stale, not a false hit
        assert_eq!(t.ref_if_current(s, g).unwrap_err(), SlotMiss::StaleGen);
        assert!(t.ref_if_current(s, t.generation(s)).is_ok());
    }

    #[test]
    fn ignore_cooldown_frees_immediately() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        t.try_claim(Item::new(1)).unwrap();
        t.retire(|| 9_999, |p| p.id == 1, |_, _| {});
        assert_eq!(t.reclaim_retired(0, u64::MAX, true, |_| {}), 1);
    }

    #[test]
    fn iteration_visits_only_active() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        t.try_claim(Item::new(1)).unwrap();
        let s2 = t.try_claim(Item::new(2)).unwrap();
        t.try_claim(Item::new(3)).unwrap();
        t.retire(|| 1, |p| p.id == 2, |_, _| {});
        let _ = s2;

        let mut seen = Vec::new();
        t.for_each_active(|_, p| seen.push(p.id));
        seen.sort_unstable();
        assert_eq!(seen, alloc::vec![1, 3]);

        assert_eq!(t.find_active(|_, p| (p.id == 3).then_some("hit")), Some("hit"));
        assert_eq!(t.find_active::<()>(|_, p| (p.id == 2).then_some(())), None);
    }

    #[test]
    fn with_active_mut_mutates_in_place() {
        let t: SlotTable<Item, 4> = SlotTable::new();
        t.try_claim(Item::new(5)).unwrap();
        let r = t.with_active_mut(|p| p.id == 5, |p| {
            p.id = 500;
            p.id
        });
        assert_eq!(r, Some(500));
        assert_eq!(find_id(&t, 500), Some(500));
    }

    #[test]
    fn racing_reclaimers_free_each_slot_once() {
        use std::sync::Arc as StdArc;
        use std::thread;

        for _ in 0..200 {
            let t: StdArc<SlotTable<Item, 64>> = StdArc::new(SlotTable::new());
            let flags: Vec<Arc<AtomicBool>> = (0..64).map(|_| Arc::new(AtomicBool::new(false))).collect();
            for (i, f) in flags.iter().enumerate() {
                t.try_claim(Item::tracked(i as u32, f.clone())).unwrap();
            }
            for i in 0..64 {
                t.retire(|| 1, |p| p.id == i, |_, _| {});
            }
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let t = t.clone();
                    thread::spawn(move || t.reclaim_retired(10_000, 1, false, |_| {}))
                })
                .collect();
            let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
            assert_eq!(total, 64, "each slot freed exactly once");
            assert!(flags.iter().all(|f| f.load(Ordering::SeqCst)));
            assert_eq!(t.retired_count(), 0);
        }
    }

    #[test]
    fn racing_claimers_get_distinct_slots() {
        use std::sync::Arc as StdArc;
        use std::thread;

        for _ in 0..200 {
            let t: StdArc<SlotTable<Item, 32>> = StdArc::new(SlotTable::new());
            let handles: Vec<_> = (0..8)
                .map(|k| {
                    let t = t.clone();
                    thread::spawn(move || {
                        let mut got = Vec::new();
                        for j in 0..4 {
                            got.push(t.try_claim(Item::new(k * 4 + j)).unwrap());
                        }
                        got
                    })
                })
                .collect();
            let mut all: Vec<usize> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
            all.sort_unstable();
            all.dedup();
            assert_eq!(all.len(), 32, "no slot handed out twice");
        }
    }
}
