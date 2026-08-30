//! A value behind [`akuma_locks_rw::RecoverableRwLock`] — the two `UnsafeCell`
//! derefs, and nothing else.
//!
//! # Why this crate exists
//!
//! `akuma-locks-rw` deliberately carries no `T`: minting `&mut T` from `&self`
//! needs an `UnsafeCell` deref, which `#![forbid(unsafe_code)]` excludes, and
//! the protocol is the part with the bug history worth enforcing. Its module
//! header says a consumer "composes its own `UnsafeCell<T>` against the
//! tickets". Doing that literally in `akuma-ext2` would cost that crate its own
//! `forbid` — the goal of `docs/archive/AKUMA_EXT2_CLEANUP.md` §5 step 4 — so
//! the composition lives here instead, **parametric over `T`**.
//!
//! Parametric is the whole point. This crate never names `Ext2State` (or any
//! other consumer's state), so the consumer's type stays `pub(crate)` and its
//! encapsulation is untouched: nothing is exported to get it in here. The
//! proof obligation is correspondingly parametric — *a live ticket implies
//! exclusive (or shared) admission, for any `T`* — which is the same argument
//! `lock_api` makes for every `spinning_top` lock already in this tree.
//!
//! # Why not `lock_api::RawRwLock`
//!
//! That was the first choice, and reading the protocol ruled it out.
//! `RawRwLock` wants `lock_exclusive()` / `unsafe unlock_exclusive()` as
//! *separate, tid-less* calls, but `RecoverableRwLock` releases through a
//! ticket's `Drop`, and a read release needs the tid whose hold it is
//! draining. Implementing the trait would mean `mem::forget`-ing the ticket on
//! acquire and exposing public raw `release_write` / `release_read(tid)` on
//! `akuma-locks-rw` for the unlock half — reintroducing exactly the
//! "unconditional release of a lock you may not hold" surface that
//! `force_unlock_write` had and that this design exists to delete. Holding the
//! ticket *inside* the guard costs two `Deref` impls and leaves the protocol
//! crate untouched, so that is what this does.
//!
//! # The safety argument, in one paragraph
//!
//! [`RecoverableCell`] owns both the lock and an `UnsafeCell<T>`. A
//! [`WriteGuard`] exists only if a `WriteTicket` was minted, which happens only
//! on the winning `CAS(0 → WBIT)`; while that ticket is alive the lock refuses
//! every other writer and every reader, so the `&mut T` is unique. A
//! [`ReadGuard`] exists only if a `ReadTicket` was minted, which requires
//! `WBIT` and `WWAIT` both clear, and no writer can acquire while the reader
//! count is non-zero, so the `&T` cannot alias a `&mut T`. Each guard stores
//! its ticket and drops it *after* the reference it handed out is gone (field
//! order below is load-bearing). Recovery does not weaken this: a sweep for a
//! dead tid performs the same release that tid's ticket would have, and the
//! reap contract restricts it to tids with no live occupant — so it can never
//! retire a ticket a running thread still holds.
//!
//! Background: `docs/archive/AKUMA_EXT2_CLEANUP.md` §4.3a (release *is*
//! abandon), §4.5 (placement), §5 step 4 (the wiring this unblocks).

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use akuma_locks_rw::{ReadTicket, RecoverableRwLock, WriteTicket};

/// A `T` behind the recoverable reader/writer protocol.
///
/// The API mirrors `spinning_top::RwSpinlock` — `read` / `write` /
/// `try_read` / `try_write` returning `Deref` guards — so a consumer swapping
/// to it changes a type name and nothing else. What it adds is
/// [`Self::abandon_tid`]: the runtime's sweep for a thread that died holding
/// the lock, which is the same CAS-guarded release a live holder performs.
pub struct RecoverableCell<T> {
    lock: RecoverableRwLock,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serialises every access to `value` — a `WriteGuard` is
// minted only against a `WriteTicket` (exclusive: no other writer, no reader),
// and a `ReadGuard` only against a `ReadTicket` (shared: no writer). `T: Send`
// is required to move a value between threads through the lock; `T: Sync` is
// not, because `&T` escapes only inside a `ReadGuard`, which is `!Send` by
// construction (it borrows this cell) and so cannot hand a `&T` to another
// thread that the lock has not admitted.
unsafe impl<T: Send> Send for RecoverableCell<T> {}
// SAFETY: as above — shared access is admitted only through the protocol, so a
// `&RecoverableCell<T>` on two cores cannot produce overlapping `&mut T`.
unsafe impl<T: Send> Sync for RecoverableCell<T> {}

impl<T> RecoverableCell<T> {
    /// A free cell holding `value`.
    ///
    /// `const` so a consumer can build one in a `static` without a lazy init —
    /// the property `spinning_top::RwSpinlock::new` has and a `Box<dyn Any>`
    /// formulation would lose.
    pub const fn new(value: T) -> Self {
        Self {
            lock: RecoverableRwLock::new(),
            value: UnsafeCell::new(value),
        }
    }

    /// Consume the cell and return the value. Takes `self` by value, so no
    /// ticket can be outstanding.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }

    /// Shared access under `tid`, or `None` if a writer holds or waits.
    pub fn try_read_as(&self, tid: usize) -> Option<ReadGuard<'_, T>> {
        self.lock.try_read(tid).map(|ticket| ReadGuard {
            // SAFETY: the ticket is live for this guard's whole lifetime and
            // proves shared admission — `WBIT` was clear at the acquiring CAS
            // and no writer can acquire while the reader count is non-zero, so
            // no `&mut T` to this value exists.
            value: unsafe { &*self.value.get() },
            _ticket: ticket,
        })
    }

    /// Exclusive access under `tid`, or `None` if the lock is not instantly
    /// writable.
    pub fn try_write_as(&self, tid: usize) -> Option<WriteGuard<'_, T>> {
        self.lock.try_write(tid).map(|ticket| WriteGuard {
            // SAFETY: the ticket is live for this guard's whole lifetime and
            // proves exclusive admission — it exists only because this thread
            // won `CAS(0 -> WBIT)` with the reader count zero, and the lock
            // admits no other writer and no reader until it is released. So
            // this is the only reference of any kind to the value.
            value: unsafe { &mut *self.value.get() },
            _ticket: ticket,
        })
    }

    /// Block until shared access is admitted under `tid`.
    pub fn read_as(&self, tid: usize) -> ReadGuard<'_, T> {
        let ticket = self.lock.read_as(tid);
        ReadGuard {
            // SAFETY: as `try_read_as`.
            value: unsafe { &*self.value.get() },
            _ticket: ticket,
        }
    }

    /// Block until exclusive access is admitted under `tid`.
    pub fn write_as(&self, tid: usize) -> WriteGuard<'_, T> {
        let ticket = self.lock.write_as(tid);
        WriteGuard {
            // SAFETY: as `try_write_as`.
            value: unsafe { &mut *self.value.get() },
            _ticket: ticket,
        }
    }

    /// [`Self::try_read_as`] with the current thread's tid.
    pub fn try_read(&self) -> Option<ReadGuard<'_, T>> {
        self.try_read_as(akuma_primitives::preempt::current_tid())
    }

    /// [`Self::try_write_as`] with the current thread's tid.
    pub fn try_write(&self) -> Option<WriteGuard<'_, T>> {
        self.try_write_as(akuma_primitives::preempt::current_tid())
    }

    /// [`Self::read_as`] with the current thread's tid.
    pub fn read(&self) -> ReadGuard<'_, T> {
        self.read_as(akuma_primitives::preempt::current_tid())
    }

    /// [`Self::write_as`] with the current thread's tid.
    pub fn write(&self) -> WriteGuard<'_, T> {
        self.write_as(akuma_primitives::preempt::current_tid())
    }

    /// [`Self::read_as`] with a per-attempt caller guard. See
    /// [`RecoverableRwLock::write_as_holding`] — the guard covers the winning
    /// try and the resulting hold, never the wait.
    pub fn read_as_holding<H>(&self, tid: usize, hold: impl Fn() -> H) -> (ReadGuard<'_, T>, H) {
        let (ticket, h) = self.lock.read_as_holding(tid, hold);
        let guard = ReadGuard {
            // SAFETY: as `try_read_as`.
            value: unsafe { &*self.value.get() },
            _ticket: ticket,
        };
        (guard, h)
    }

    /// [`Self::write_as`] with a per-attempt caller guard. See
    /// [`RecoverableRwLock::write_as_holding`].
    pub fn write_as_holding<H>(&self, tid: usize, hold: impl Fn() -> H) -> (WriteGuard<'_, T>, H) {
        let (ticket, h) = self.lock.write_as_holding(tid, hold);
        let guard = WriteGuard {
            // SAFETY: as `try_write_as`.
            value: unsafe { &mut *self.value.get() },
            _ticket: ticket,
        };
        (guard, h)
    }

    /// [`Self::read_as_holding`] with the current thread's tid.
    pub fn read_holding<H>(&self, hold: impl Fn() -> H) -> (ReadGuard<'_, T>, H) {
        self.read_as_holding(akuma_primitives::preempt::current_tid(), hold)
    }

    /// [`Self::write_as_holding`] with the current thread's tid.
    pub fn write_holding<H>(&self, hold: impl Fn() -> H) -> (WriteGuard<'_, T>, H) {
        self.write_as_holding(akuma_primitives::preempt::current_tid(), hold)
    }

    /// Abandon everything `dead` holds — the runtime's sweep. Idempotent and
    /// CAS-guarded; see [`RecoverableRwLock::abandon_tid`] for the contract
    /// (call it at the TERMINATED→FREE transition, where the tid is known dead
    /// and cannot be reissued).
    ///
    /// Returns whether anything was recovered.
    pub fn abandon_tid(&self, dead: usize) -> bool {
        self.lock.abandon_tid(dead)
    }

    /// The protocol underneath, for the observability accessors
    /// (`writer_tid`, `reader_count`, `is_locked`, …).
    pub fn raw(&self) -> &RecoverableRwLock {
        &self.lock
    }
}

impl<T: Default> Default for RecoverableCell<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Shared access. Holds the ticket, so the lock is released when this drops.
///
/// Field order is load-bearing: `value` is declared first so it is dropped
/// before `_ticket`, i.e. the borrow ends before the hold does.
pub struct ReadGuard<'a, T> {
    value: &'a T,
    _ticket: ReadTicket<'a>,
}

impl<T> Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

/// Exclusive access. Holds the ticket, so the lock is released when this drops.
///
/// Field order is load-bearing, as in [`ReadGuard`].
pub struct WriteGuard<'a, T> {
    value: &'a mut T,
    _ticket: WriteTicket<'a>,
}

impl<T> Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value
    }
}

#[cfg(test)]
mod tests;
