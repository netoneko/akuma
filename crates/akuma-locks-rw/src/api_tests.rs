//! API-level host tests for [`RecoverableRwLock`] — the §4.7 list: forget a
//! guard under a fake tid, sweep that tid, assert recovery; both backstop
//! branches; lock churn. These drive the real atomics under `std::thread`
//! contention; `model.rs` exhaustively checks the protocol around them.
//!
//! There is no shared lock state to serialize on: every test instantiates a
//! fresh `RecoverableRwLock` and sweeps only its own instance through
//! `abandon_tid` — the same shape the wired kernel will have (one lock per
//! mount, each swept by its owner). Tests run fully parallel. The only shared
//! cell is the single-shot backstop registration, and its test payload is a
//! counting no-op, so firing it from another test's waiter is harmless.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::{register_backstop, RecoverableRwLock};

/// Forget a write guard held under tid 7, sweep tid 7, and assert the lock is
/// writable again — the §4.7 scenario verbatim, and the shape of the old
/// design's unrecoverable wedge (`panic = "abort"` leaks the guard, and only
/// the sweep can release it now).
#[test]
fn forgotten_write_guard_under_tid_7_is_reaped() {
    let lock = RecoverableRwLock::new();

    let guard = lock.write_as(7);
    assert!(lock.is_locked());
    core::mem::forget(guard);

    // Nobody can get in while the dead tid's hold leaks.
    assert!(lock.try_write(8).is_none());
    assert!(lock.try_read(8).is_none());

    assert!(lock.abandon_tid(7), "the sweep must report recovering the hold");
    assert!(!lock.is_locked(), "the lock must be open after the sweep");

    let _guard = lock.write_as(8);
}

/// A double sweep is a no-op (the CAS guard refuses the second time) and
/// sweeping a tid that holds nothing moves nothing.
#[test]
fn reap_is_idempotent_and_a_nonholder_sweep_is_a_noop() {
    let lock = RecoverableRwLock::new();
    let guard = lock.write_as(3);
    core::mem::forget(guard);

    assert!(lock.abandon_tid(3));
    assert!(!lock.is_locked());
    assert!(!lock.abandon_tid(3), "the second sweep must find nothing");

    // The lock still works after the churn.
    let g = lock.write_as(3);
    drop(g);
    assert!(!lock.is_locked());
}

/// A thread killed holding SEVERAL read guards is drained exactly: the writer
/// it was blocking gets in, and a live co-reader's hold is untouched.
#[test]
fn forgotten_read_guards_are_drained_by_the_sweep() {
    let lock = RecoverableRwLock::new();

    let r1 = lock.read_as(5);
    let r2 = lock.read_as(5);
    let live = lock.read_as(6);
    assert_eq!(lock.reader_count(), 3);
    assert_eq!(lock.reader_holds(5), 2);
    assert_eq!(lock.reader_holds(6), 1);
    assert!(lock.try_write(7).is_none(), "readers block the writer");

    core::mem::forget((r1, r2));

    assert!(lock.abandon_tid(5), "the dead tid's holds are a recovery");
    assert_eq!(lock.reader_count(), 1, "exactly the dead tid's holds drained");
    assert_eq!(lock.reader_holds(6), 1, "the live reader is untouched");

    // The writer is still shut out while the live reader holds...
    assert!(lock.try_write(7).is_none());
    // ...and gets in once that reader leaves too.
    drop(live);
    let w = lock.try_write(7).expect("writer gets in once the readers are gone");
    drop(w);
}

/// A tid that cannot be tracked (≥ `MAX_THREADS`) never acquires — an
/// untrackable hold would be unreapable.
#[test]
fn an_untrackable_tid_never_acquires() {
    let lock = RecoverableRwLock::new();
    let over = akuma_primitives::MAX_THREADS + 3;
    assert!(lock.try_write(over).is_none());
    assert!(lock.try_read(over).is_none());
    assert!(!lock.is_locked());
}

/// A blocked waiter with the backstop unregistered degrades to a plain spin
/// — no panic, no fuss — and completes as soon as anything releases; here,
/// the sweep fired from the test thread.
#[test]
fn blocked_waiter_completes_once_swept() {
    let lock = Arc::new(RecoverableRwLock::new());
    let guard = lock.write_as(4);
    core::mem::forget(guard);

    let l = Arc::clone(&lock);
    let (done_tx, done_rx) = mpsc::channel();
    let (drop_tx, drop_rx) = mpsc::channel::<()>();
    let waiter = std::thread::spawn(move || {
        let _g = l.read_as(8); // spins on the leaked writer hold
        done_tx.send(()).unwrap();
        drop_rx.recv().unwrap(); // hold until the test has swept
    });

    std::thread::sleep(Duration::from_millis(50));
    assert!(lock.abandon_tid(4));
    assert!(
        done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the waiter must proceed once the sweep opens the lock"
    );
    drop_tx.send(()).unwrap();
    waiter.join().unwrap();
}

/// The backstop's **registered** branch: a blocked waiter fires the kicker on
/// its 10k-spin cadence. The kicker here is a counting no-op, so the waiter
/// stays blocked until the test itself sweeps — proving the kick happened
/// without conflating it with recovery.
#[test]
fn registered_backstop_fires_from_a_blocked_waiter() {
    static KICK_COUNT: AtomicUsize = AtomicUsize::new(0);
    static ONCE: std::sync::Once = std::sync::Once::new();
    // Registration is single-shot; only this test ever registers, so the
    // counting no-op cannot race another registration. Another test's blocked
    // waiter firing it first only makes the counter non-zero sooner — the
    // branch is proven either way, and a counting no-op unlocks nothing.
    ONCE.call_once(|| register_backstop(|| {
        KICK_COUNT.fetch_add(1, Ordering::Relaxed);
    }));

    let lock = Arc::new(RecoverableRwLock::new());
    let guard = lock.write_as(4);
    core::mem::forget(guard);

    let l = Arc::clone(&lock);
    let (done_tx, done_rx) = mpsc::channel();
    let (drop_tx, drop_rx) = mpsc::channel::<()>();
    let waiter = std::thread::spawn(move || {
        let _g = l.read_as(8); // blocks; must fire the kicker while it waits
        done_tx.send(()).unwrap();
        drop_rx.recv().unwrap();
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while KICK_COUNT.load(Ordering::Relaxed) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a blocked waiter never fired the registered kicker"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    assert!(lock.abandon_tid(4));
    assert!(
        done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the waiter must complete after the sweep"
    );
    drop_tx.send(()).unwrap();
    waiter.join().unwrap();
}

/// Writer priority end to end: while a writer waits, a new reader is refused;
/// the writer gets in ahead of it; the reader proceeds after the writer
/// leaves.
#[test]
fn a_waiting_writer_is_served_ahead_of_new_readers() {
    let lock = Arc::new(RecoverableRwLock::new());

    // A leaked read hold keeps the lock shut; the writer must wait behind it.
    let r = lock.read_as(5);
    core::mem::forget(r);

    let l = Arc::clone(&lock);
    let (writer_in_tx, writer_in_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let _g = l.write_as(9); // announces (WWAIT), then waits
        writer_in_tx.send(()).unwrap();
        release_rx.recv().unwrap(); // hold the lock until the test has checked
    });

    std::thread::sleep(Duration::from_millis(50));
    assert!(
        lock.try_read(8).is_none(),
        "a reader must be refused while a writer waits (WWAIT)"
    );

    // The leak clears; the waiting writer — not the refused reader — gets in.
    assert!(lock.abandon_tid(5));
    writer_in_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the waiting writer must get in once the readers drain");
    assert!(lock.try_read(8).is_none(), "the writer is in; the reader still waits");

    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert!(lock.try_read(8).is_some(), "the reader proceeds once the writer leaves");
}

/// The read loop's ghost-wait self-heal: a writer that dies *waiting* leaves
/// the priority bit behind, and a reader must clear it and proceed rather
/// than wait forever for a writer who will never come.
#[test]
fn a_dead_waiters_priority_bit_does_not_block_readers_forever() {
    let lock = Arc::new(RecoverableRwLock::new());
    // Forge the ghost: WWAIT set, nothing else (the bit of a writer killed
    // between announcing and acquiring).
    lock.flag.store(crate::WWAIT, Ordering::Relaxed);

    assert!(
        lock.try_read(7).is_none(),
        "the ghost priority bit refuses a single-attempt reader"
    );
    // The wait loop heals it and proceeds (the heal fires on the backstop
    // cadence; this must complete, not hang).
    let l = Arc::clone(&lock);
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _g = l.read_as(7);
        done_tx.send(()).unwrap();
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
        "the reader must heal the ghost WWAIT and proceed"
    );
    waiter.join().unwrap();
    assert!(!lock.writer_waiting());
    assert!(!lock.is_locked());
}

/// Lock churn: create and drop a thousand locks — some dying mid-hold — and
/// confirm nothing is shared or poisoned: a fresh instance afterwards behaves
/// exactly like one from before. (There is no registry to churn; this is the
/// §4.7 churn item reduced to what a per-owner world actually has.)
#[test]
fn churn_create_drop_thousands_of_fresh_locks() {
    for i in 0..1000u32 {
        let l = RecoverableRwLock::new();
        if i % 3 == 0 {
            let g = l.write_as((i % 61) as usize);
            core::mem::forget(g); // some locks die holding
            assert!(l.is_locked());
        } else if i % 3 == 1 {
            let g = l.read_as((i % 61) as usize);
            drop(g);
            assert!(!l.is_locked());
        }
    }

    // A fresh instance is pristine — no residue from the thousand before it.
    let fresh = RecoverableRwLock::new();
    assert!(!fresh.is_locked());
    assert!(!fresh.writer_waiting());
    assert_eq!(fresh.reader_count(), 0);
    let g = fresh.write_as(13);
    drop(g);
    assert!(!fresh.is_locked());
}
