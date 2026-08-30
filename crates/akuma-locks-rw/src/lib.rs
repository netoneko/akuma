//! The recoverable reader/writer lock: release **is** abandon.
//!
//! Every shipped profile builds with `panic = "abort"`, so a thread killed at an
//! arbitrary instruction never runs its guard's `Drop`, and a lock whose release
//! lives only in that `Drop` is leaked forever — every later filesystem
//! operation spins on it. `akuma-ext2`'s old recovery polled a recorded owner
//! tid every 10 000 spins and, when the scheduler said the owner was dead,
//! `force_unlock_write()`-ed a third-party lock. That design had three defects
//! (`docs/archive/AKUMA_EXT2_CLEANUP.md` §4.2/§4.2a): the policy existed in
//! three drifting copies; a recycled tid made the liveness inference read a
//! *new* occupant's state, so recovery never fired; and `force_unlock_write` is
//! an unconditional store whose "no guard for this lock exists" contract is a
//! whole-program property no crate can check.
//!
//! The fix (§4.3a) is to own the lock's state word, so the recovery can perform
//! **the identical guarded operation a legitimate release performs**:
//!
//! - writer release is `CAS(WBIT → 0)` after clearing the owner cell, and
//!   [`RecoverableRwLock::abandon_tid`] does exactly that, CAS-guarded on the
//!   same word — a stale or repeated sweep cannot double-release, and a lock a
//!   live thread holds cannot be touched (the owner check refuses before the
//!   CAS);
//! - reader release is a decrement of the lock's reader count, and the sweep
//!   drains the dead tid's **per-tid hold count** (the
//!   `akuma-bkl::sync::ThreadTagTable` shape) and subtracts exactly that —
//!   leaked read guards, unrecoverable in the old design, are recovered too;
//! - no thread ever asks the scheduler a liveness question. Recording happens
//!   at acquire (the acquirer's tid, read natively via
//!   `akuma_primitives::preempt::current_tid()`); recovery happens when the
//!   runtime *reports* a death at the TERMINATED→FREE transition. The runtime
//!   never polls the lock, and the lock never polls the runtime.
//!
//! # Why the lock carries no `T` — the one deviation from §4.5
//!
//! The plan sketches `RecoverableRwLock<T>` behind `#![forbid(unsafe_code)]`.
//! On stable Rust those two cannot coexist: handing out `&mut T` from `&self`
//! requires an `UnsafeCell` deref, which `forbid` excludes, and there is no
//! safe, `Sync`, `no_std` interior-mutability primitive to borrow instead
//! (`RefCell`/`Cell` are `!Sync`; every spin lock's safe surface ends at a
//! leaked guard this protocol exists to recover). Applying the plan's own law —
//! *the question is never whether the operation can be made safe, it is who
//! owns the thing being vouched for* — the **protocol** is the thing this crate
//! vouches for, and it is pure atomics end to end. A consumer that wants a
//! value behind the lock composes its own `UnsafeCell<T>` at the value's owner
//! (for ext2, step 4 of §5) and dereferences it against a held
//! [`WriteTicket`]/[`ReadTicket`]: the ticket's existence proves the exclusivity
//! the deref needs, minted by the acquire CAS this crate performs. That keeps
//! the enforcement (`forbid`, a compile error to regress) on the part with the
//! bug history, and leaves the two-cell deref beside the value it vouches for.
//!
//! # The reap contract
//!
//! [`RecoverableRwLock::abandon_tid`] must be called exactly at a slot's
//! TERMINATED→FREE transition — where the runtime guarantees the tid is 100%
//! dead and the slot cannot yet be reissued. Under that contract a sweep
//! cannot hit a live holder, because the tid has no live occupant between its
//! death and its reissue. Calling `abandon_tid` for a tid that is
//! concurrently acquiring is outside the contract; the CAS guards keep such a
//! call non-corrupting (bounded phantoms, floor-guarded drains), but it is
//! not supported. The crate deliberately does not own a global list of locks
//! to sweep — see the recovery section at the bottom of this file.
//!
//! # The one upward wire
//!
//! A waiter that has spun [`BACKSTOP_SPINS`] calls the registered backstop —
//! the runtime's chance to run a late reap or yield — and otherwise degrades to
//! a plain spin, exactly like the old `THREAD_HOOKS.get()` shape. The
//! waiter-side kicker preserves the old property that any waiter alone can
//! unblock the system even if a reap is late (§4.6).
//!
//! # Relationship to `akuma-bkl`
//!
//! None, code-wise (§4.1): the BKL is a per-core FIFO ticket protocol with a
//! different unit, trigger and remedy, and it stays untouched. What moved here
//! is its *law* — recovery performs a legitimate release because the lock owns
//! its state — and its discipline: [`model`] exhaustively checks this protocol
//! the way `bkl_model.rs` checks the BKL's.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use akuma_not_even_once::Registered;

/// Flag-word bits. One writer bit, one writer-waiting bit, and a reader count.
///
/// `WWAIT` is writer priority: once a writer has announced itself, new readers
/// are refused so a stream of them cannot starve the writer out. It is
/// advisory — a writer that dies *waiting* leaves the bit behind, and the
/// readers' self-heal (below) clears a ghost rather than blocking forever.
const WBIT: u32 = 1 << 31;
const WWAIT: u32 = 1 << 30;
const READER_MASK: u32 = WWAIT - 1;
const MAX_READERS: u32 = READER_MASK;

/// The owner cell's empty value. A real tid is always `< MAX_THREADS`, so it
/// can never collide — unlike `0`, which is a legitimate tid.
const NO_OWNER: usize = usize::MAX;

/// Spins between backstop kicks in the blocking [`RecoverableRwLock::read`] /
/// [`RecoverableRwLock::write`] wait loops — the same cadence the old ext2
/// recovery polls used.
pub const BACKSTOP_SPINS: u32 = 10_000;

/// The waiter-side backstop: called by a blocked waiter every
/// [`BACKSTOP_SPINS`] spins, giving the runtime a chance to run a late reap or
/// yield the core. Unregistered (host tests, early boot), it degrades to a
/// plain spin — the same degrade-as-today shape as the old `THREAD_HOOKS.get()`.
static BACKSTOP: Registered<fn()> =
    Registered::new("akuma-locks-rw: backstop not registered — call register_backstop() first");

/// Register the waiter-side backstop kicker. Called once by the runtime;
/// a second call is ignored (registration is single-shot).
pub fn register_backstop(f: fn()) {
    BACKSTOP.register(f);
}

#[inline]
fn backstop_kick() {
    if let Some(f) = BACKSTOP.get() {
        f();
    }
}

/// A reader/writer spinlock whose release operation and whose dead-holder
/// recovery are the same CAS on its own state word.
///
/// The lock carries **no value** — see the module header for why, and for how a
/// consumer composes one. Tids are explicit (`read_as`/`write_as`) so host
/// tests can drive the protocol without a thread system; the `read`/`write`
/// conveniences read `current_tid()` natively.
///
/// Writer priority: a writer that has to wait sets the `WWAIT` bit, refusing
/// new readers until it gets in. Readers starve writers never; a reader *can*
/// still lose an admission race to a releasing writer (the lock is unfair in
/// the single-attempt sense — the model checker pins both facts).
pub struct RecoverableRwLock {
    /// `WBIT` | `WWAIT` | reader count. The single source of truth for who may
    /// proceed; both release and recovery mutate it by CAS.
    flag: AtomicU32,
    /// Tid of the current writer, `NO_OWNER` when none. Written at acquire and
    /// cleared *before* the writer bit falls (the `Ext2WriteGuard` ordering),
    /// so a sweep's owner check never observes a cleared bit behind a stale
    /// owner.
    owner: AtomicUsize,
    /// Reader hold count per tid — how many read tickets `tid` currently
    /// carries. This is what lets a sweep drain a thread killed holding
    /// several reads, and what the old design had no answer for.
    readers: [AtomicUsize; akuma_primitives::MAX_THREADS],
}

impl Default for RecoverableRwLock {
    fn default() -> Self {
        Self::new()
    }
}

impl RecoverableRwLock {
    /// A free lock.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            flag: AtomicU32::new(0),
            owner: AtomicUsize::new(NO_OWNER),
            readers: [const { AtomicUsize::new(0) }; akuma_primitives::MAX_THREADS],
        }
    }

    // ── acquire ─────────────────────────────────────────────────────────────

    /// Take a write hold under `tid`, or `None` if the lock is not instantly
    /// writable (a writer holds it, or any reader does). A tid that cannot be
    /// tracked (≥ `MAX_THREADS`) never acquires: an untrackable hold would be
    /// unreapable.
    pub fn try_write(&self, tid: usize) -> Option<WriteTicket<'_>> {
        self.readers.get(tid)?;
        let mut f = self.flag.load(Ordering::Relaxed);
        loop {
            if f & WBIT != 0 || f & READER_MASK != 0 {
                return None;
            }
            // Consuming WWAIT here is what makes it advisory: the winning
            // writer takes the priority with the bit.
            match self.flag.compare_exchange_weak(
                f,
                (f & !WWAIT) | WBIT,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.owner.store(tid, Ordering::Release);
                    return Some(WriteTicket { lock: self });
                }
                Err(cur) => f = cur,
            }
        }
    }

    /// Announce a writer, then spin until [`Self::try_write`] succeeds. The
    /// announcement (`WWAIT`) is what stops new readers from starving the
    /// writer, and it is **re-asserted on every failed attempt** — the read
    /// loop's ghost-heal may clear the bit in the window where the lock is
    /// momentarily free, and a live waiter must not be mistaken for a dead
    /// one for longer than one attempt. The backstop kick every
    /// [`BACKSTOP_SPINS`] is what lets a late reap unblock this waiter even
    /// if nobody else notices the dead holder.
    pub fn write_as(&self, tid: usize) -> WriteTicket<'_> {
        self.write_as_holding(tid, || ()).0
    }

    /// [`Self::write_as`], but taking a caller-supplied guard **per attempt**:
    /// `hold()` runs immediately before each non-blocking try and is dropped
    /// again before each backoff spin, so only the try and the resulting hold
    /// are covered — never the wait.
    ///
    /// This exists for `akuma-ext2`, whose guard masks local IRQs under
    /// `no-bkl-vfs`. It must be held across the winning try (an IRQ landing
    /// between acquire and mask can `enter_kernel()` and hard-spin for the BKL
    /// while this lock is held — the AB-BA wedge) and must **not** span the
    /// backoff (this loop is unbounded, and masking across it would starve the
    /// core's timer, so a holder on this very core could never run to release).
    /// Before this method that loop lived in the consumer, in three drifting
    /// copies, and the protocol details below — the `WWAIT` re-announcement and
    /// the backstop cadence — lived with it. Keeping one implementation here is
    /// what lets [`crate::model`] speak for every caller.
    pub fn write_as_holding<H>(&self, tid: usize, hold: impl Fn() -> H) -> (WriteTicket<'_>, H) {
        self.flag.fetch_or(WWAIT, Ordering::Relaxed);
        let mut spins = 0u32;
        loop {
            let h = hold();
            if let Some(t) = self.try_write(tid) {
                return (t, h);
            }
            drop(h);
            // Re-announce before spinning: try_write consumed the bit if it
            // won; a failed attempt must leave the announcement standing.
            self.flag.fetch_or(WWAIT, Ordering::Relaxed);
            spins = spins.wrapping_add(1);
            if spins.is_multiple_of(BACKSTOP_SPINS) {
                backstop_kick();
            }
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
    }

    /// [`Self::write_as`] with the current thread's tid, read natively — the
    /// acquire-side identity read that replaced the old thread-hook indirection.
    pub fn write(&self) -> WriteTicket<'_> {
        self.write_as(akuma_primitives::preempt::current_tid())
    }

    /// Take a read hold under `tid`, or `None` if a writer holds or waits.
    ///
    /// The hold is published in two steps — a reservation in the tid's cell,
    /// then the flag-side count — so that a sweep racing the acquisition can
    /// never underflow the count. A reservation whose publish fails is rolled
    /// back, or left as a *phantom* (a cell count with no flag-side hold) if a
    /// concurrent drain already took the slot; phantoms are harmless because
    /// every flag-side drain is floor-guarded — it can only subtract counts
    /// that were actually published.
    pub fn try_read(&self, tid: usize) -> Option<ReadTicket<'_>> {
        let cell = self.readers.get(tid)?;
        let mut f = self.flag.load(Ordering::Relaxed);
        loop {
            if f & (WBIT | WWAIT) != 0 || f & READER_MASK == MAX_READERS {
                return None;
            }
            cell.fetch_add(1, Ordering::Relaxed);
            match self.flag.compare_exchange_weak(f, f + 1, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return Some(ReadTicket { lock: self, tid }),
                Err(cur) => {
                    f = cur;
                    release_cell_hold(cell);
                }
            }
        }
    }

    /// Spin until [`Self::try_read`] succeeds. As in [`Self::write_as`], the
    /// backstop kick preserves any-waiter-can-unblock-the-system; the extra
    /// self-heal clears a *ghost* `WWAIT` — the bit of a writer that died
    /// waiting, which no acquire will ever consume — once the lock is fully
    /// free, so the writer-priority gate cannot outlive every writer.
    pub fn read_as(&self, tid: usize) -> ReadTicket<'_> {
        self.read_as_holding(tid, || ()).0
    }

    /// [`Self::read_as`] with a per-attempt caller guard — see
    /// [`Self::write_as_holding`] for why the guard must cover the winning try
    /// and nothing else.
    pub fn read_as_holding<H>(&self, tid: usize, hold: impl Fn() -> H) -> (ReadTicket<'_>, H) {
        let mut spins = 0u32;
        loop {
            let h = hold();
            if let Some(t) = self.try_read(tid) {
                return (t, h);
            }
            drop(h);
            spins = spins.wrapping_add(1);
            if spins.is_multiple_of(BACKSTOP_SPINS) {
                backstop_kick();
                self.heal_ghost_writer_wait();
            }
            for _ in 0..100 {
                core::hint::spin_loop();
            }
        }
    }

    /// [`Self::read_as`] with the current thread's tid, read natively.
    pub fn read(&self) -> ReadTicket<'_> {
        self.read_as(akuma_primitives::preempt::current_tid())
    }

    // ── release (the abandon operation) ─────────────────────────────────────

    /// Give up the write hold: clear the owner cell, then `CAS(WBIT → 0)`.
    /// Internal, but the exact operation [`Self::abandon_tid`] performs for a
    /// dead writer — that identity is the whole design.
    fn release_write(&self) {
        self.owner.store(NO_OWNER, Ordering::Release);
        let mut f = self.flag.load(Ordering::Relaxed);
        loop {
            debug_assert!(f & WBIT != 0, "release_write without the writer bit");
            match self.flag.compare_exchange_weak(
                f,
                f & !WBIT,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => f = cur,
            }
        }
    }

    /// Give up one read hold under `tid`. The same floor-guarded decrement the
    /// sweep uses, so it composes with a racing reap without underflow.
    fn release_read(&self, tid: usize) {
        if let Some(cell) = self.readers.get(tid) {
            release_cell_hold(cell);
        }
        drain_published_reader_holds(&self.flag, 1);
    }

    // ── recovery (the same abandon operation, run by the runtime) ───────────

    /// Abandon everything `dead` holds: if it owned the write hold, perform the
    /// writer release on its behalf; drain whatever read holds it carried.
    /// Lock-free, idempotent, and CAS-guarded on the lock's own word — a second
    /// call is a no-op, and a live holder is never touched because the owner
    /// check refuses before the CAS.
    ///
    /// Returns whether anything was recovered. Under the reap contract this is
    /// called from the TERMINATED→FREE transition, where the tid is known dead
    /// and cannot be reissued yet.
    pub fn abandon_tid(&self, dead: usize) -> bool {
        let mut recovered = false;

        if self.owner.load(Ordering::Acquire) == dead {
            let mut f = self.flag.load(Ordering::Relaxed);
            loop {
                if f & WBIT == 0 {
                    // The writer bit is already gone: a raced legitimate
                    // release (owner cleared first) or an earlier sweep. The
                    // CAS guard did its job — nothing to release.
                    break;
                }
                // Clearing WWAIT alongside WBIT: a dead owner's window may
                // carry the priority bit, and no acquire will consume it.
                match self.flag.compare_exchange_weak(
                    f,
                    f & !(WBIT | WWAIT),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        recovered = true;
                        break;
                    }
                    Err(cur) => f = cur,
                }
            }
            self.owner.store(NO_OWNER, Ordering::Release);
        }

        if let Some(cell) = self.readers.get(dead) {
            let n = cell.swap(0, Ordering::AcqRel);
            if n > 0 {
                drain_published_reader_holds(&self.flag, n as u32);
                recovered = true;
            }
        }

        recovered
    }

    // ── introspection (diagnostics, boot self-tests, the model's oracle) ────

    /// The tid currently owning the write hold, if any.
    pub fn writer_tid(&self) -> Option<usize> {
        let o = self.owner.load(Ordering::Acquire);
        (o != NO_OWNER).then_some(o)
    }

    /// How many read holds `tid` currently carries (including phantom
    /// reservations — see [`Self::try_read`]).
    pub fn reader_holds(&self, tid: usize) -> usize {
        self.readers.get(tid).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// The flag-side reader count.
    pub fn reader_count(&self) -> usize {
        (self.flag.load(Ordering::Relaxed) & READER_MASK) as usize
    }

    /// `true` while a writer has announced itself and not yet acquired (or its
    /// announcement has not yet been healed away — see `read_as`).
    pub fn writer_waiting(&self) -> bool {
        self.flag.load(Ordering::Relaxed) & WWAIT != 0
    }

    /// `true` while any hold is out (writer or readers).
    pub fn is_locked(&self) -> bool {
        self.flag.load(Ordering::Relaxed) & (WBIT | READER_MASK) != 0
    }

    /// Clear a `WWAIT` left by a writer that died waiting: only when the lock
    /// is completely free, so a live waiter between its own `fetch_or` and its
    /// acquire CAS loses nothing (it simply re-announces). The read wait loop
    /// calls this on its backstop cadence.
    fn heal_ghost_writer_wait(&self) {
        let f = self.flag.load(Ordering::Relaxed);
        if f & (WBIT | READER_MASK) == 0 && f & WWAIT != 0 {
            let _ = self.flag.compare_exchange(
                f,
                f & !WWAIT,
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
    }
}

/// Decrement one cell hold without ever wrapping: a concurrent sweep may have
/// already taken the count (the phantom case), and a phantom is safer than a
/// wrapped slot — the flag-side drain is floor-guarded the same way.
fn release_cell_hold(cell: &AtomicUsize) {
    let _ = cell.try_update(Ordering::Release, Ordering::Relaxed, |v| {
        if v == 0 { None } else { Some(v - 1) }
    });
}

/// Subtract `n` published reader holds from the flag, never below zero and
/// never touching `WBIT`/`WWAIT`. The floor guard is what makes the two-word
/// reservation protocol safe under a racing drain: a drain can only remove
/// holds that were actually published.
fn drain_published_reader_holds(flag: &AtomicU32, n: u32) {
    for _ in 0..n {
        let _ = flag.try_update(Ordering::AcqRel, Ordering::Relaxed, |f| {
            if f & READER_MASK == 0 {
                None
            } else {
                Some(f - 1)
            }
        });
    }
}

/// RAII proof of exclusive access. No methods: what it *means* is the API.
///
/// While a `WriteTicket` for a lock is alive (and not forgotten), the lock’s
/// writer bit is set and the owner cell names the acquiring tid, which is
/// exactly the exclusivity proof a consumer’s `UnsafeCell` deref needs.
///
/// Dropping performs the writer release — the same operation a sweep performs
/// for a dead owner. `mem::forget`-ing one leaks the hold, which is precisely
/// the situation [`RecoverableRwLock::abandon_tid`] exists to recover (and the
/// shape the host tests exercise).
pub struct WriteTicket<'a> {
    lock: &'a RecoverableRwLock,
}

impl Drop for WriteTicket<'_> {
    fn drop(&mut self) {
        self.lock.release_write();
    }
}

/// RAII proof of shared access. Carries its tid so the release can decrement
/// the right per-tid cell — and so a sweep can drain it if the holder dies.
pub struct ReadTicket<'a> {
    lock: &'a RecoverableRwLock,
    tid: usize,
}

impl Drop for ReadTicket<'_> {
    fn drop(&mut self) {
        self.lock.release_read(self.tid);
    }
}

// ── recovery ────────────────────────────────────────────────────────────────
//
// There is deliberately no process-global registry here and no free
// `reap_tid(tid)` sweep over "all locks ever made" — a crate-global list of
// weak lock handles makes every test a negotiation with shared state, and it
// makes the crate allocate (`Arc` per lock) to solve a problem it does not
// own: enumerating the locks is the business of whoever *owns* them. In the
// kernel that is the VFS mount table; the wiring step registers one reaper
// hook (the `init_inode_freed_hook` shape) whose body walks the mounts and
// calls each lock's [`RecoverableRwLock::abandon_tid`]. The protocol — the
// part with the bug history — is entirely per-lock, which is also what makes
// every host test a fresh instance with no shared state.

/// The backstop cadence as a named non-zero, for callers that want to reason
/// about wait bounds.
const _: () = assert!(BACKSTOP_SPINS != 0);

/// Real-atomics host tests for the public API (§4.7): forgotten guards reaped
/// under fake tids, both backstop branches, per-instance lock churn.
#[cfg(test)]
mod api_tests;

/// Host-only exhaustive model checker + protocol tests: mutual exclusion,
/// deadlock-freedom, writer-priority (no writer starvation), recovery after
/// abandon, double-abandon idempotence, reader-leak drain.
#[cfg(test)]
mod model;
