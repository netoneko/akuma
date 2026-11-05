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
    /// Mask interrupts on this core and return what [`Self::irq_restore`] needs to put them back as they were. [`enter`] wraps its whole
    /// wait in this pair; the default does nothing, which is right only for a machine that never takes an interrupt on a path that can
    /// reach the lock.
    #[must_use]
    fn irq_save() -> u64 {
        0
    }
    /// Undo [`Self::irq_save`].
    fn irq_restore(_saved: u64) {}
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
///
/// **Interrupts are masked from before the ticket is taken until the lock is owned.** A ticket lock is served in order, so a core that
/// is interrupted while it waits and whose handler also asks for the lock queues the handler *behind* the suspended outer wait: the
/// outer ticket is served first, cannot run until the handler returns, and the handler is waiting for a ticket that comes up after
/// the outer one. All cores end up spinning with nobody owning the lock (`SERVING` pointing at a ticket no running core holds). That
/// is the wedge seen as `[BKL] cpu N stuck: owner cpu -1`. With interrupts held off for the wait, a tick that arrives meanwhile is taken
/// once the lock is owned, where the handler's `enter` nests. The wait still services the machine's polled work (`while_waiting`:
/// a TLB shootdown) because a masked core cannot take the IPI.
pub fn enter<C: BklCpu>() {
    let cpu = C::index();
    if OWNER.load(Ordering::Relaxed) == cpu {
        DEPTH.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let saved_irq = C::irq_save();
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
    C::irq_restore(saved_irq);
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

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use core::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicBool;
    use std::{thread, thread_local};

    use super::*;

    thread_local! {
        static CPU: Cell<usize> = const { Cell::new(0) };
        static IRQ_ON: Cell<bool> = const { Cell::new(true) };
        static IN_HANDLER: Cell<bool> = const { Cell::new(false) };
    }
    /// Laps the waiter has spun, across the whole test: the holder lets go after `RELEASE_AT`, the waiter gives up after `DEADLOCK_AT`.
    static LAPS: AtomicU64 = AtomicU64::new(0);
    const RELEASE_AT: u64 = 20_000;
    const DEADLOCK_AT: u64 = 3_000_000;
    static HANDLER_RAN: AtomicBool = AtomicBool::new(false);

    /// A core whose timer tick, if interrupts are on, lands on the first lap of a wait and asks for the lock itself.
    struct Core;
    impl BklCpu for Core {
        fn index() -> usize {
            CPU.get()
        }
        fn while_waiting() {
            let laps = LAPS.fetch_add(1, Ordering::Relaxed);
            assert!(laps < DEADLOCK_AT, "a handler that waited behind its own suspended wait: every core spins, nobody owns the lock");
            if IRQ_ON.get() && !IN_HANDLER.get() {
                IN_HANDLER.set(true);
                HANDLER_RAN.store(true, Ordering::Relaxed);
                enter::<Core>(); // the tick handler wants the lock
                leave::<Core>();
                IN_HANDLER.set(false);
            }
        }
        fn irq_save() -> u64 {
            u64::from(IRQ_ON.replace(false))
        }
        fn irq_restore(saved: u64) {
            IRQ_ON.set(saved != 0);
        }
    }

    /// One test owns the statics (the lock is process-wide).
    #[test]
    fn a_tick_that_lands_while_a_core_waits_does_not_queue_behind_the_wait() {
        // Core 2 owns the lock and lets go once the waiter has spun for a while.
        let holder = thread::spawn(|| {
            CPU.set(2);
            enter::<Core>();
            while LAPS.load(Ordering::Relaxed) < RELEASE_AT {
                core::hint::spin_loop();
            }
            leave::<Core>();
        });
        while OWNER.load(Ordering::Relaxed) != 2 {
            core::hint::spin_loop();
        }
        CPU.set(1);
        // Interrupts are on when core 1 asks. With the old `enter` the simulated tick took a second ticket behind this wait and the
        // test died on `DEADLOCK_AT`; now interrupts are off for the whole wait, so the tick is held until the lock is owned.
        enter::<Core>();
        assert!(held::<Core>(), "core 1 got the lock once core 2 let go");
        assert!(IRQ_ON.get(), "interrupts are back on as they were");
        assert!(!HANDLER_RAN.load(Ordering::Relaxed), "no tick was taken while the core waited");
        // The tick that was held off now arrives: this core owns the lock, so the handler's `enter` nests and its `leave` unnests.
        enter::<Core>();
        leave::<Core>();
        assert!(held::<Core>(), "still owned at depth 1 after the nested handler");
        leave::<Core>();
        assert!(!held::<Core>());
        holder.join().unwrap();
        assert_eq!(NEXT.load(Ordering::Relaxed), SERVING.load(Ordering::Relaxed), "every ticket taken was served");
    }
}
