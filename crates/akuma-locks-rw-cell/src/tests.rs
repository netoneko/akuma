//! Tests for the value half. The *protocol* is exhaustively model-checked in
//! `akuma-locks-rw`; what is left to check here is that a guard's reference
//! tracks its ticket — that admission, mutation, release and the sweep all
//! reach the value correctly, and that a recovered lock hands out a value that
//! is still coherent.

use super::*;

const A: usize = 3;
const B: usize = 7;

#[test]
fn a_write_guard_mutates_and_releases() {
    let cell = RecoverableCell::new(41u32);
    {
        let mut g = cell.write_as(A);
        *g += 1;
    }
    assert_eq!(*cell.read_as(B), 42);
    assert!(!cell.raw().is_locked(), "both guards dropped");
}

#[test]
fn readers_share_and_a_writer_is_refused_while_they_do() {
    let cell = RecoverableCell::new(9u32);
    let r1 = cell.try_read_as(A).unwrap();
    let r2 = cell.try_read_as(B).unwrap();
    assert_eq!((*r1, *r2), (9, 9));
    assert!(cell.try_write_as(A).is_none(), "a reader holds");
    drop(r1);
    assert!(cell.try_write_as(A).is_none(), "the other reader still holds");
    drop(r2);
    assert!(cell.try_write_as(A).is_some());
}

#[test]
fn a_writer_excludes_everyone() {
    let cell = RecoverableCell::new(0u32);
    let w = cell.try_write_as(A).unwrap();
    assert!(cell.try_read_as(B).is_none());
    assert!(cell.try_write_as(B).is_none());
    drop(w);
    assert!(cell.try_read_as(B).is_some());
}

/// The point of the whole design: a holder that dies without running `Drop`
/// (`panic = "abort"` kills every guard's destructor) must not wedge the value.
#[test]
fn a_forgotten_write_guard_is_recovered_and_the_value_survives() {
    let cell = RecoverableCell::new(100u32);

    let mut g = cell.write_as(A);
    *g += 5;
    core::mem::forget(g); // the kill: no Drop, the hold leaks

    assert!(cell.try_write_as(B).is_none(), "still wedged before the sweep");
    assert!(cell.abandon_tid(A), "the sweep recovers A's write hold");

    // The mutation that landed before the death is still there — recovery
    // releases the lock, it does not roll the value back.
    assert_eq!(*cell.read_as(B), 105);
    assert!(cell.try_write_as(B).is_some());
}

#[test]
fn forgotten_read_guards_are_recovered() {
    let cell = RecoverableCell::new(1u32);
    core::mem::forget(cell.read_as(A));
    core::mem::forget(cell.read_as(A));
    assert!(cell.try_write_as(B).is_none());

    assert!(cell.abandon_tid(A), "drains both of A's holds");
    let mut w = cell.try_write_as(B).expect("writable once swept");
    *w = 2;
    drop(w);
    assert_eq!(*cell.read_as(B), 2);
}

#[test]
fn sweeping_a_live_holder_does_not_take_its_lock() {
    let cell = RecoverableCell::new(0u32);
    let w = cell.try_write_as(A).unwrap();
    assert!(!cell.abandon_tid(B), "B holds nothing");
    assert!(cell.try_write_as(B).is_none(), "A's hold is untouched");
    drop(w);
}

#[test]
fn a_second_sweep_is_a_noop() {
    let cell = RecoverableCell::new(0u32);
    core::mem::forget(cell.write_as(A));
    assert!(cell.abandon_tid(A));
    assert!(!cell.abandon_tid(A), "idempotent");
    // And the lock is genuinely free, not double-released into a broken state.
    let w = cell.try_write_as(B).unwrap();
    assert!(cell.try_read_as(A).is_none());
    drop(w);
    assert!(cell.try_read_as(A).is_some());
}

/// `const fn new` is what lets a consumer build one in a `static`. A
/// `Box<dyn Any>` formulation could not, which is half of why this crate is
/// generic rather than dynamically typed.
#[test]
fn is_usable_from_a_static() {
    static CELL: RecoverableCell<u64> = RecoverableCell::new(7);
    assert_eq!(*CELL.read_as(A), 7);
}

/// The guard must hand out the cell's own storage, not a copy — a `DerefMut`
/// that wrote to a temporary would pass every test above that reads through
/// the same guard.
#[test]
fn a_write_through_the_guard_is_visible_to_a_later_reader() {
    let cell = RecoverableCell::new([0u8; 4]);
    cell.write_as(A)[2] = 0xAB;
    assert_eq!(cell.read_as(B)[2], 0xAB);
    assert_eq!(cell.into_inner(), [0, 0, 0xAB, 0]);
}

/// A non-`Copy`, allocating payload: the guard must not move or drop it.
#[test]
fn holds_a_non_copy_value() {
    let cell = RecoverableCell::new(alloc_string("hello"));
    cell.write_as(A).push_str(" world");
    assert_eq!(&*cell.read_as(B), "hello world");
}

fn alloc_string(s: &str) -> String {
    s.to_string()
}
