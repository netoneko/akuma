//! The big kernel lock, one for both machines (B21 A6; moved from the amd64
//! half's `smp.rs`).
//!
//! User mode runs in parallel; every kernel entry holds this lock, so the
//! kernel's statics stay single-writer (`BorrowCell`s keep working: they become
//! a check that the lock is held). Re-entrant by owner core - a tick that lands
//! while this core already holds it nests - and the *depth* is per thread: a
//! switch leaves the lock with the core and hands the depth over
//! ([`transfer_depth`]), except that a thread of depth 0 (an idle thread, a task
//! in user mode) runs with the lock released.
//!
//! A ticket lock: cores are served in the order they asked. An unfair spin lock
//! lets cores that re-enter in a tight loop (four compilers making syscalls)
//! starve one that asks rarely - sshd's socket service among them.
//!
//! A machine supplies its core index and what a waiting core must keep doing
//! ([`BklCpu`]): amd64 services TLB shootdowns while it spins, because a core
//! spinning IRQ-masked here cannot take the shootdown IPI.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use akuma_primitives::safe_print;

/// What the lock asks of the machine.
pub trait BklCpu {
    /// This core's index.
    fn index() -> usize;
    /// Called on every lap of a wait for the lock.
    fn while_waiting() {}
}

const NO_OWNER: usize = usize::MAX;
static OWNER: AtomicUsize = AtomicUsize::new(NO_OWNER);
static NEXT: AtomicU32 = AtomicU32::new(0);
static SERVING: AtomicU32 = AtomicU32::new(0);
/// The owner's recursion depth (only the owner writes it).
static DEPTH: AtomicU32 = AtomicU32::new(0);
/// Threads whose depth is kept across a switch (tid-indexed).
pub const THREAD_SLOTS: usize = 128;
/// Per-thread depth, saved at a switch away.
static THREAD_DEPTH: [AtomicU32; THREAD_SLOTS] = [const { AtomicU32::new(0) }; THREAD_SLOTS];

/// Does this core own the lock?
#[must_use]
pub fn held<C: BklCpu>() -> bool {
    OWNER.load(Ordering::Relaxed) == C::index()
}

/// Take the lock (or nest, if this core already owns it).
pub fn enter<C: BklCpu>() {
    let cpu = C::index();
    if OWNER.load(Ordering::Relaxed) == cpu {
        DEPTH.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let ticket = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut spins = 0u64;
    while SERVING.load(Ordering::Acquire) != ticket {
        core::hint::spin_loop();
        C::while_waiting();
        spins += 1;
        if spins == 400_000_000 {
            safe_print!(96, "[BKL] cpu {} stuck: owner cpu {}\n", cpu, OWNER.load(Ordering::Relaxed).cast_signed());
        }
    }
    OWNER.store(cpu, Ordering::Relaxed);
    DEPTH.store(1, Ordering::Relaxed);
}

/// Hand the lock to the next ticket.
fn release() {
    DEPTH.store(0, Ordering::Relaxed);
    OWNER.store(NO_OWNER, Ordering::Relaxed);
    SERVING.fetch_add(1, Ordering::Release);
}

/// Leave one level; release at zero. A no-op from a core that does not own it.
pub fn leave<C: BklCpu>() {
    if OWNER.load(Ordering::Relaxed) != C::index() {
        return;
    }
    let d = DEPTH.load(Ordering::Relaxed);
    if d > 1 {
        DEPTH.store(d - 1, Ordering::Relaxed);
    } else {
        release();
    }
}

/// Release completely (user mode is next, or a halt), returning the depth held.
pub fn leave_all<C: BklCpu>() -> u32 {
    if OWNER.load(Ordering::Relaxed) != C::index() {
        return 0;
    }
    let d = DEPTH.load(Ordering::Relaxed);
    release();
    d
}

/// Take the lock back at depth `d` (from [`leave_all`]); `d == 0` is a no-op.
pub fn restore<C: BklCpu>(d: u32) {
    if d == 0 {
        return;
    }
    enter::<C>();
    DEPTH.store(d, Ordering::Relaxed);
}

/// Run `f` with the lock released (a halt, a spin that waits on another core).
pub fn unlocked<C: BklCpu, R>(f: impl FnOnce() -> R) -> R {
    let d = leave_all::<C>();
    let r = f();
    restore::<C>(d);
    r
}

/// The scheduler's `transfer_lock_depth`, on the outgoing core with `from`
/// still current: save this core's depth into `from`, take `to`'s.
pub fn transfer_depth<C: BklCpu>(from: usize, to: usize) {
    let held = if OWNER.load(Ordering::Relaxed) == C::index() { DEPTH.load(Ordering::Relaxed) } else { 0 };
    if let Some(t) = THREAD_DEPTH.get(from) {
        t.store(held, Ordering::Relaxed);
    }
    let want = THREAD_DEPTH.get(to).map_or(0, |t| t.load(Ordering::Relaxed));
    if want == 0 {
        let _ = leave_all::<C>();
    } else if held == 0 {
        enter::<C>();
        DEPTH.store(want, Ordering::Relaxed);
    } else {
        DEPTH.store(want, Ordering::Relaxed);
    }
}

/// A freshly claimed kernel thread is born holding the lock at depth 1 (its
/// entry code is kernel code until it crosses to user mode).
pub fn born_locked(tid: usize) {
    if let Some(t) = THREAD_DEPTH.get(tid) {
        t.store(1, Ordering::Relaxed);
    }
}
