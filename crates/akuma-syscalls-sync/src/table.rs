//! The `(tgid, uaddr)` waiter table.
//!
//! This is the futex family's one data structure, and nearly every futex bug in
//! this tree's history was a bug *in it* rather than in the syscall around it:
//! a wake published to a key the waiter was not on, a dead tid left queued
//! where it silently absorbed a later wake, a requeued waiter that could no
//! longer find itself. Each of those cost a devbox boot — often a `-j4` rustc
//! self-host build, the only workload that reproduced them — to find. They are
//! all reachable from a host test here.
//!
//! # What it is not
//!
//! There is no lock in this module, no IRQ masking and no wake. The kernel owns
//! all three: `FUTEX_WAITERS` is a `Spinlock` accessed with local IRQs masked
//! (an AB-BA deadlock argument that has nothing to do with the queue algebra),
//! and wakes are deliberately fired *outside* the hold. So every method here
//! either returns the waiters to wake or reports what it did, and the caller
//! performs the effect. That split is what makes the table testable, and it is
//! also the shape the kernel already had.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use akuma_syscalls_linux::flags::futex::FUTEX_BITSET_MATCH_ANY;

/// A futex key: the namespace (see [`crate::key`]) and the user address.
///
/// The namespace half is what separates two processes that park on the same
/// virtual address — Akuma has no ASLR, so without it every copy of one binary
/// shares a queue. That is not a hypothetical: it is the musl `__tl_lock` bug
/// in [`crate::key::namespace`].
pub type Key = (u32, usize);

/// The identity a queued waiter is known by.
///
/// Generic on purpose. The kernel queues a generation-tagged `WakeHandle`,
/// whose whole point is that acting on a *bare* tid is unsafe — a tid popped
/// from this table and held across a preemption can name a recycled slot, so
/// the wake would land on whoever now owns it. This trait therefore exposes
/// only what the table's own bookkeeping needs: `tid()` identifies queue
/// entries during a scan. It never acts on a thread, and this crate cannot,
/// because it has no way to.
pub trait WaiterId: Copy {
    /// The slot index this identity refers to. Used to *find* entries, never to
    /// wake through.
    fn tid(self) -> usize;
}

/// Where a scan found a waiter relative to the key it originally parked on.
///
/// The three cases are the whole reason the wait loop is a state machine rather
/// than a membership check: a `FUTEX_REQUEUE` can move a parked waiter to
/// another key behind its back, and it has no way to know except by looking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    /// Queued nowhere under this namespace: something dequeued it, which for a
    /// waiter that has just woken means a real `FUTEX_WAKE`.
    Nowhere,
    /// Still on the key it enqueued on — a spurious wake.
    OriginalKey,
    /// Moved by a requeue. Carries the key it now sits on, which is the one any
    /// cleanup must target.
    Requeued(Key),
}

/// The futex waiter table: for each key, the waiters queued on it in FIFO
/// order, each with the bitset it will accept wakes for.
#[derive(Debug)]
pub struct WaiterTable<H: WaiterId> {
    map: BTreeMap<Key, Vec<(H, u32)>>,
}

impl<H: WaiterId> Default for WaiterTable<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: WaiterId> WaiterTable<H> {
    #[must_use]
    pub const fn new() -> Self {
        Self { map: BTreeMap::new() }
    }

    /// Number of keys with at least one waiter. An empty queue is never stored,
    /// so this is also the number of non-empty queues — see [`Self::queue`].
    #[must_use]
    pub fn keys(&self) -> usize {
        self.map.len()
    }

    /// The tids queued on `key`, FIFO, or `None` if no queue exists.
    ///
    /// `None` and `Some(empty)` are not both reachable: every removal path
    /// drops a queue that empties, so an empty `Vec` is never stored. Tests
    /// assert on that because a stale empty queue is invisible in a dump and
    /// changes what `keys()` reports.
    #[must_use]
    pub fn queue(&self, key: Key) -> Option<Vec<usize>> {
        self.map.get(&key).map(|q| q.iter().map(|(h, _)| h.tid()).collect())
    }

    /// Every `(key, waiters)` pair, for the diagnostic dump.
    pub fn iter(&self) -> impl Iterator<Item = (&Key, &Vec<(H, u32)>)> {
        self.map.iter()
    }

    /// Queue `waiter` on `key`, accepting wakes whose mask intersects `bitset`.
    ///
    /// The caller must have re-read the futex word under the same hold; this
    /// method cannot check the value and deliberately does not try to. See
    /// `futex_check_and_enqueue` in the kernel for the lost-wakeup argument
    /// that ordering exists to satisfy.
    pub fn enqueue(&mut self, key: Key, waiter: H, bitset: u32) {
        self.map.entry(key).or_default().push((waiter, bitset));
    }

    /// Pop up to `max_wake` waiters from `key` whose stored bitset intersects
    /// `wake_mask`, in FIFO order, and return them for the caller to wake.
    ///
    /// Non-matching waiters are *skipped, not stopped at*: a `FUTEX_WAKE_BITSET`
    /// that stopped at the first non-matching waiter would report fewer wakes
    /// than it could deliver. Ignoring the bitset altogether is the other
    /// failure — then a `val=1` wake is eaten by a waiter that never wanted it,
    /// and the intended waiter stays parked.
    #[must_use]
    pub fn wake(&mut self, key: Key, max_wake: u32, wake_mask: u32) -> Vec<H> {
        let mut woken = Vec::new();
        let Some(queue) = self.map.get_mut(&key) else { return woken };
        let mut i = 0;
        while i < queue.len() && (woken.len() as u32) < max_wake {
            if queue[i].1 & wake_mask != 0 {
                woken.push(queue.remove(i).0);
            } else {
                i += 1;
            }
        }
        if queue.is_empty() {
            self.map.remove(&key);
        }
        woken
    }

    /// Take up to `max_wake` waiters off `key1` for the caller to wake, then
    /// move up to `max_requeue` of the remainder onto `key2`.
    ///
    /// Returns the waiters to wake and the requeued ones (so the caller can
    /// trace them). Requeue moves waiters **regardless of bitset**, matching
    /// Linux, and each keeps the bitset it enqueued with.
    ///
    /// `key2` with a zero address means "no requeue target": `FUTEX_REQUEUE`
    /// with a null `uaddr2` degenerates to a wake.
    #[must_use]
    pub fn requeue(
        &mut self,
        key1: Key,
        key2: Key,
        max_wake: u32,
        max_requeue: u32,
    ) -> (Vec<H>, Vec<H>) {
        let has_target = key2.1 != 0;
        let Some(queue) = self.map.remove(&key1) else { return (Vec::new(), Vec::new()) };

        let mut remaining = queue;
        let wake_count = (max_wake as usize).min(remaining.len());
        let to_wake: Vec<H> = remaining.drain(..wake_count).map(|(h, _)| h).collect();

        let requeue_count =
            if has_target { (max_requeue as usize).min(remaining.len()) } else { 0 };
        let moved: Vec<(H, u32)> = remaining.drain(..requeue_count).collect();

        // Whatever neither woke nor moved goes back. Dropping this line is the
        // easy version of the bug this whole module guards against: the
        // remaining waiters would be queued nowhere, parked forever, and no
        // wake could ever reach them again.
        if !remaining.is_empty() {
            self.map.insert(key1, remaining);
        }
        if !moved.is_empty() {
            self.map.entry(key2).or_default().extend(moved.iter().copied());
        }
        (to_wake, moved.into_iter().map(|(h, _)| h).collect())
    }

    /// Remove `tid` from `key`, dropping the queue if it empties. The
    /// timeout/`EFAULT` cleanup path for a waiter still on its original key.
    pub fn dequeue(&mut self, key: Key, tid: usize) {
        if let Some(queue) = self.map.get_mut(&key) {
            queue.retain(|(h, _)| h.tid() != tid);
            if queue.is_empty() {
                self.map.remove(&key);
            }
        }
    }

    /// Find `tid` under `tgid` and report where it is, relative to `key`.
    ///
    /// If it is on `key` itself, it is **removed** — the caller is about to
    /// re-validate and re-enqueue, and leaving it would double-enqueue it. A
    /// waiter found on another key was requeued and is left exactly where it
    /// is: it is correctly parked there, and pulling it off would strand it.
    #[must_use]
    pub fn locate_and_take(&mut self, tgid: u32, tid: usize, key: Key) -> Located {
        let mut found: Option<Key> = None;
        for (&k, q) in &self.map {
            if k.0 == tgid && q.iter().any(|(h, _)| h.tid() == tid) {
                found = Some(k);
                break;
            }
        }
        match found {
            None => Located::Nowhere,
            Some(k) if k == key => {
                self.dequeue(k, tid);
                Located::OriginalKey
            }
            Some(k) => Located::Requeued(k),
        }
    }

    /// Remove `tid` from whichever key under `tgid` holds it, and report which
    /// that was.
    ///
    /// The cleanup path for a waiter that left by timeout or signal *after*
    /// being requeued: its own loop only ever computes its original key, so it
    /// cannot dequeue itself from the right place. Without this, every such
    /// waiter leaves a dead tid on the requeue target, and each dead tid
    /// silently absorbs one future wake on that address.
    ///
    /// Bounded to `tgid` because requeue never crosses thread groups.
    #[must_use]
    pub fn remove_anywhere(&mut self, tgid: u32, tid: usize) -> Option<Key> {
        let key = self
            .map
            .iter()
            .find(|(k, q)| k.0 == tgid && q.iter().any(|(h, _)| h.tid() == tid))
            .map(|(k, _)| *k)?;
        self.dequeue(key, tid);
        Some(key)
    }

    /// Drop every queued reference to `tid`, across **all** keys and namespaces,
    /// returning the keys it was found on.
    ///
    /// The slot-recycle hook. A thread killed while parked — `exit_group`, a
    /// consumed pending kill, a fault-kill — never runs its own cleanup, so its
    /// tid stays queued; once the slot is reused that entry names a live,
    /// unrelated thread, and a wake meant for a real waiter is spent on it.
    ///
    /// Unlike [`Self::remove_anywhere`] this cannot bound the scan by namespace:
    /// the caller is the slot recycler and has no process context left to
    /// derive one from.
    #[must_use]
    pub fn purge(&mut self, tid: usize) -> Vec<Key> {
        let mut touched = Vec::new();
        let mut emptied = Vec::new();
        for (&k, q) in &mut self.map {
            let before = q.len();
            q.retain(|(h, _)| h.tid() != tid);
            if q.len() != before {
                touched.push(k);
                if q.is_empty() {
                    emptied.push(k);
                }
            }
        }
        for k in emptied {
            self.map.remove(&k);
        }
        touched
    }

    /// Every tid currently queued anywhere — the input to the orphan check
    /// ("parked in `FUTEX_WAIT` but queued nowhere is a kernel bug by
    /// construction").
    #[must_use]
    pub fn queued_tids(&self) -> Vec<usize> {
        self.map.values().flatten().map(|(h, _)| h.tid()).collect()
    }
}

/// The bitset a plain (non-`BITSET`) wait or wake behaves as.
pub const MATCH_ANY: u32 = FUTEX_BITSET_MATCH_ANY;
