//! Host tests for the futex family.
//!
//! Each test is named for the thing that actually went wrong, not for the
//! method it calls. A test called `test_wake` tells the next person nothing; a
//! test called `a_dead_tid_left_queued_absorbs_a_later_wake` tells them what
//! breaks and roughly how long they will spend finding it in a VM.

use alloc::vec;
use alloc::vec::Vec;

use akuma_primitives::errno::negated::{EAGAIN, EFAULT, EINVAL, ENOSYS};
use akuma_syscalls_linux::flags::futex as f;

use crate::deadline::{NEVER, REVALIDATE_US, deadline_us, expired, park_deadline_us};
use crate::key::{Namespace, namespace};
use crate::op::{Action, decode};
use crate::table::{Located, MATCH_ANY, WaiterId, WaiterTable};
use crate::waitloop::{Step, step};
use crate::wakeop::WakeOp;

/// A bare tid, standing in for the kernel's generation-tagged `WakeHandle`.
///
/// The generation is what the kernel adds and this crate must not depend on:
/// the table's job is to hold identities and hand them back, and a test that
/// needed a real generation would be testing the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tid(usize);

impl WaiterId for Tid {
    fn tid(self) -> usize {
        self.0
    }
}

/// Always through this, never `WaiterTable::new()` directly: the table is
/// generic and an empty one has nothing to infer `H` from.
fn table() -> WaiterTable<Tid> {
    WaiterTable::new()
}

fn tids(v: Vec<Tid>) -> Vec<usize> {
    v.into_iter().map(|t| t.0).collect()
}

const A: (u32, usize) = (7, 0x1000);
const B: (u32, usize) = (7, 0x2000);

// ─── The waiter table ────────────────────────────────────────────────────────

#[test]
fn wake_drains_fifo_and_leaves_the_rest_queued() {
    let mut t = table();
    for id in 1..=4 {
        t.enqueue(A, Tid(id), MATCH_ANY);
    }
    assert_eq!(tids(t.wake(A, 2, MATCH_ANY)), vec![1, 2]);
    assert_eq!(t.queue(A), Some(vec![3, 4]));
}

/// An emptied queue must be *removed*, not left behind as an empty `Vec`. A
/// stale empty queue is invisible in `[FUTEX-DUMP]` output and inflates the key
/// count, so a healthy table reads as if it were leaking.
#[test]
fn an_emptied_queue_is_dropped_not_left_empty() {
    let mut t = table();
    t.enqueue(A, Tid(1), MATCH_ANY);
    assert_eq!(t.keys(), 1);
    let _ = t.wake(A, 1, MATCH_ANY);
    assert_eq!(t.keys(), 0);
    assert_eq!(t.queue(A), None);

    // Same for every other removal path.
    t.enqueue(A, Tid(1), MATCH_ANY);
    t.dequeue(A, 1);
    assert_eq!(t.keys(), 0);
    t.enqueue(A, Tid(1), MATCH_ANY);
    let _ = t.remove_anywhere(A.0, 1);
    assert_eq!(t.keys(), 0);
    t.enqueue(A, Tid(1), MATCH_ANY);
    let _ = t.purge(1);
    assert_eq!(t.keys(), 0);
}

/// `FUTEX_WAKE_BITSET` must *skip* a non-matching waiter and keep scanning. If
/// it stopped at the first one, a `val=1` wake would report zero woken with a
/// perfectly wakeable waiter sitting two entries down.
#[test]
fn a_non_matching_bitset_is_skipped_not_stopped_at() {
    let mut t = table();
    t.enqueue(A, Tid(1), 0b0010);
    t.enqueue(A, Tid(2), 0b0001);
    t.enqueue(A, Tid(3), 0b0011);

    assert_eq!(tids(t.wake(A, 1, 0b0001)), vec![2]);
    assert_eq!(t.queue(A), Some(vec![1, 3]));
    // And a non-matching waiter is never woken just because it is at the head.
    assert_eq!(tids(t.wake(A, 8, 0b0100)), Vec::<usize>::new());
    assert_eq!(t.queue(A), Some(vec![1, 3]));
}

/// A plain `FUTEX_WAKE` uses `MATCH_ANY`, so it must reach even a waiter that
/// enqueued with a narrow bitset — that is what makes `FUTEX_WAIT_BITSET` safe
/// to use for a `Condvar` that is also woken by plain wakes.
#[test]
fn match_any_reaches_a_narrow_waiter() {
    let mut t = table();
    t.enqueue(A, Tid(1), 0b1000_0000);
    assert_eq!(tids(t.wake(A, 1, MATCH_ANY)), vec![1]);
}

#[test]
fn requeue_wakes_the_first_n_and_moves_the_next_m() {
    let mut t = table();
    for id in 1..=5 {
        t.enqueue(A, Tid(id), MATCH_ANY);
    }
    let (woken, moved) = t.requeue(A, B, 1, 2);
    assert_eq!(tids(woken), vec![1]);
    assert_eq!(tids(moved), vec![2, 3]);
    // The leftovers stay where they were. Losing them is the version of this
    // bug that parks two threads forever with nothing queued anywhere.
    assert_eq!(t.queue(A), Some(vec![4, 5]));
    assert_eq!(t.queue(B), Some(vec![2, 3]));
}

/// Requeue ignores bitsets entirely, matching Linux: it is a queue move, not a
/// wake, so a waiter's wake-selectivity has no bearing on it — and it keeps
/// that bitset on the new queue.
#[test]
fn requeue_moves_regardless_of_bitset_and_preserves_it() {
    let mut t = table();
    t.enqueue(A, Tid(1), 0b0001);
    t.enqueue(A, Tid(2), 0b0010);
    let (_, moved) = t.requeue(A, B, 0, 2);
    assert_eq!(tids(moved), vec![1, 2]);
    // Each still only accepts its own bit on the new key.
    assert_eq!(tids(t.wake(B, 8, 0b0010)), vec![2]);
    assert_eq!(t.queue(B), Some(vec![1]));
}

/// `FUTEX_REQUEUE` with a null `uaddr2` is a wake with no target — the
/// remainder must stay on the original key rather than vanish.
#[test]
fn requeue_with_no_target_degenerates_to_a_wake() {
    let mut t = table();
    for id in 1..=3 {
        t.enqueue(A, Tid(id), MATCH_ANY);
    }
    let (woken, moved) = t.requeue(A, (A.0, 0), 1, 99);
    assert_eq!(tids(woken), vec![1]);
    assert_eq!(tids(moved), Vec::<usize>::new());
    assert_eq!(t.queue(A), Some(vec![2, 3]));
}

#[test]
fn requeue_on_an_empty_key_is_a_no_op() {
    let mut t = table();
    let (woken, moved) = t.requeue(A, B, 4, 4);
    assert_eq!(tids(woken), Vec::<usize>::new());
    assert_eq!(tids(moved), Vec::<usize>::new());
    assert_eq!(t.keys(), 0);
}

/// The `typenum` stall: a waiter requeued to another key, then leaving by
/// timeout, must be removed from *the key it is actually on*. Its own loop only
/// ever computes the original key, so without a search it strands a dead tid on
/// the target — where it silently absorbs one future wake.
#[test]
fn a_requeued_waiter_is_found_on_the_target_not_its_original_key() {
    let mut t = table();
    t.enqueue(A, Tid(1), MATCH_ANY);
    let _ = t.requeue(A, B, 0, 1);

    assert_eq!(t.locate_and_take(A.0, 1, A), Located::Requeued(B));
    // Located, and deliberately left in place: it is correctly parked there.
    assert_eq!(t.queue(B), Some(vec![1]));

    assert_eq!(t.remove_anywhere(A.0, 1), Some(B));
    assert_eq!(t.keys(), 0);
}

/// `locate_and_take` removes the waiter only when it is on the original key,
/// because the caller is about to re-enqueue it there. Leaving it would queue
/// the same tid twice, and it would then eat two wakes.
#[test]
fn locate_takes_the_waiter_only_off_its_original_key() {
    let mut t = table();
    t.enqueue(A, Tid(1), MATCH_ANY);
    assert_eq!(t.locate_and_take(A.0, 1, A), Located::OriginalKey);
    assert_eq!(t.queue(A), None);

    // Woken already: nothing to find, nothing to remove.
    assert_eq!(t.locate_and_take(A.0, 1, A), Located::Nowhere);
}

/// Requeue never crosses thread groups, so the search must not either: finding
/// a same-tid entry under another namespace would mean one process dequeuing
/// another's waiter.
#[test]
fn locate_does_not_look_outside_its_own_namespace() {
    let mut t = table();
    let other = (9, 0x1000);
    t.enqueue(other, Tid(1), MATCH_ANY);
    assert_eq!(t.locate_and_take(A.0, 1, A), Located::Nowhere);
    assert_eq!(t.queue(other), Some(vec![1]));
    assert_eq!(t.remove_anywhere(A.0, 1), None);
    assert_eq!(t.queue(other), Some(vec![1]));
}

/// The slot-recycle hook, and the one scan that must NOT be bounded by
/// namespace: the caller is the recycler and has no process context left. A tid
/// left queued by a thread killed while parked names a live, unrelated thread
/// once the slot is reused, and the next wake is spent on it.
#[test]
fn purge_crosses_every_namespace_because_the_recycler_has_no_context() {
    let mut t = table();
    t.enqueue((1, 0x1000), Tid(5), MATCH_ANY);
    t.enqueue((2, 0x2000), Tid(5), MATCH_ANY);
    t.enqueue((2, 0x2000), Tid(6), MATCH_ANY);

    let touched = t.purge(5);
    assert_eq!(touched.len(), 2);
    assert_eq!(t.queue((1, 0x1000)), None);
    assert_eq!(t.queue((2, 0x2000)), Some(vec![6]));
}

/// The invariant the orphan check falsifies against: parked in `FUTEX_WAIT`
/// implies queued somewhere. `queued_tids` is its input, so it must see every
/// namespace.
#[test]
fn queued_tids_reports_every_namespace() {
    let mut t = table();
    t.enqueue((1, 0x1000), Tid(1), MATCH_ANY);
    t.enqueue((2, 0x2000), Tid(2), MATCH_ANY);
    let mut got = t.queued_tids();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2]);
}

/// Two processes parking on the *same address* must not share a queue. With no
/// ASLR this is the common case, not the exotic one.
#[test]
fn the_same_address_in_two_namespaces_is_two_queues() {
    let mut t = table();
    t.enqueue((1, 0x1000), Tid(1), MATCH_ANY);
    t.enqueue((2, 0x1000), Tid(2), MATCH_ANY);
    assert_eq!(tids(t.wake((1, 0x1000), 8, MATCH_ANY)), vec![1]);
    assert_eq!(t.queue((2, 0x1000)), Some(vec![2]));
}

// ─── The key namespace ───────────────────────────────────────────────────────

/// musl's `__tl_lock` is a fixed address waited on with `priv = 0`. Keying that
/// by address alone put every musl process's thread-create/exit traffic on one
/// queue, so a wake in one process popped a waiter in another.
#[test]
fn a_non_private_op_on_ordinary_memory_still_keys_by_address_space() {
    assert_eq!(namespace(false, Some(42), false), Namespace::AddressSpace(42));
    assert_eq!(namespace(true, Some(42), false), Namespace::AddressSpace(42));
}

#[test]
fn only_genuinely_shared_memory_reaches_the_global_namespace() {
    assert_eq!(namespace(false, Some(42), true), Namespace::Shared);
    // A private op is address-space scoped even on a shared mapping: the flag
    // is the caller promising it does not need cross-process reach.
    assert_eq!(namespace(true, Some(42), true), Namespace::AddressSpace(42));
}

/// An unresolvable identity is a tripwire, not a fallback — it must be
/// distinguishable from a real shared futex even though both key to 0.
#[test]
fn an_unresolved_identity_is_degraded_not_shared() {
    assert_eq!(namespace(true, None, false), Namespace::Degraded);
    assert_eq!(namespace(false, Some(0), false), Namespace::Degraded);
    assert_eq!(Namespace::Degraded.tgid(), 0);
    assert_eq!(Namespace::Shared.tgid(), 0);
    assert_ne!(Namespace::Degraded, Namespace::Shared);
}

// ─── Op decode ───────────────────────────────────────────────────────────────

#[test]
fn the_private_and_realtime_flags_are_stripped_before_the_command_is_matched() {
    let op = f::FUTEX_WAIT_BITSET | f::FUTEX_PRIVATE_FLAG | f::FUTEX_CLOCK_REALTIME;
    assert_eq!(f::cmd_of(op), f::FUTEX_WAIT_BITSET);
    assert!(f::is_private(op) && f::is_realtime(op));
    assert_eq!(decode(op, 0b1, 0x1000, true), Action::Wait { bitset: 0b1 });
}

/// Go calls `futex(0xfffffffffffffffc, FUTEX_WAKE)` during exit coordination.
/// `EFAULT` there breaks Go's exit path and strands its threads; there cannot
/// be waiters on unmapped memory anyway, so the honest answer is "woke none".
#[test]
fn a_wake_on_unmapped_memory_reports_zero_woken_not_efault() {
    for cmd in [f::FUTEX_WAKE, f::FUTEX_WAKE_BITSET, f::FUTEX_WAKE_OP] {
        assert_eq!(decode(cmd, MATCH_ANY, 0x1000, false), Action::Return(0));
    }
}

#[test]
fn a_wait_on_unmapped_memory_is_eagain_so_the_caller_re_evaluates() {
    for cmd in [f::FUTEX_WAIT, f::FUTEX_WAIT_BITSET] {
        assert_eq!(decode(cmd, MATCH_ANY, 0x1000, false), Action::Return(EAGAIN));
    }
    // Everything else keeps the honest EFAULT.
    assert_eq!(decode(f::FUTEX_REQUEUE, 0, 0x1000, false), Action::Return(EFAULT));
}

/// A zero bitset can never intersect a wake mask, so the waiter would be
/// unwakeable. Parking it is the failure mode; `EINVAL` is the contract.
#[test]
fn wait_bitset_with_a_zero_bitset_is_refused() {
    assert_eq!(decode(f::FUTEX_WAIT_BITSET, 0, 0x1000, true), Action::Return(EINVAL));
    // Plain FUTEX_WAIT ignores val3 entirely and means MATCH_ANY.
    assert_eq!(
        decode(f::FUTEX_WAIT, 0, 0x1000, true),
        Action::Wait { bitset: MATCH_ANY }
    );
}

#[test]
fn a_null_or_misaligned_address_is_refused_before_the_mapping_is_consulted() {
    assert_eq!(decode(f::FUTEX_WAKE, 0, 0, true), Action::Return(EINVAL));
    for bad in [1usize, 2, 3, 0x1001, 0x1002, 0x1003] {
        assert_eq!(decode(f::FUTEX_WAKE, 0, bad, true), Action::Return(EINVAL));
    }
    assert_eq!(decode(f::FUTEX_WAKE, 0, 0x1004, true), Action::Wake { mask: MATCH_ANY });
    // Even with an unmapped address, alignment is judged first — the Go rule
    // above must not launder a malformed pointer into a success.
    assert_eq!(decode(f::FUTEX_WAKE, 0, 3, false), Action::Return(EINVAL));
}

#[test]
fn requeue_carries_its_comparison_and_cmp_requeue_carries_val3() {
    assert_eq!(decode(f::FUTEX_REQUEUE, 5, 0x1000, true), Action::Requeue { compare: None });
    assert_eq!(
        decode(f::FUTEX_CMP_REQUEUE, 5, 0x1000, true),
        Action::Requeue { compare: Some(5) }
    );
}

/// PI futexes are unimplemented and must say so. A silently non-inheriting PI
/// futex is worse than an absent one: glibc falls back on `ENOSYS`, but it
/// cannot detect a lock that merely fails to boost priority.
#[test]
fn the_pi_family_is_enosys_rather_than_silently_wrong() {
    for cmd in [
        f::FUTEX_LOCK_PI,
        f::FUTEX_UNLOCK_PI,
        f::FUTEX_TRYLOCK_PI,
        f::FUTEX_WAIT_REQUEUE_PI,
        f::FUTEX_CMP_REQUEUE_PI,
        f::FUTEX_FD,
        99,
    ] {
        assert_eq!(decode(cmd, 0, 0x1000, true), Action::Return(ENOSYS));
    }
}

// ─── Deadlines ───────────────────────────────────────────────────────────────

/// The rustc "futex deadlock" whose symptom grew with uptime. Rust's std emits
/// `FUTEX_WAIT_BITSET` with an ALREADY-absolute monotonic deadline for every
/// timed wait; adding uptime to it again made each wait sleep about twice the
/// current uptime.
#[test]
fn wait_bitset_timeouts_are_absolute_and_plain_wait_timeouts_are_relative() {
    let now = 5_000_000;
    // Plain FUTEX_WAIT: 1 s from now.
    assert_eq!(deadline_us(f::FUTEX_WAIT, 1_000_000, now, None), 6_000_000);
    // FUTEX_WAIT_BITSET: the value IS the deadline. Adding `now` here is the bug.
    assert_eq!(deadline_us(f::FUTEX_WAIT_BITSET, 6_000_000, now, None), 6_000_000);
    // And the flags must not change that.
    let op = f::FUTEX_WAIT_BITSET | f::FUTEX_PRIVATE_FLAG;
    assert_eq!(deadline_us(op, 6_000_000, now, None), 6_000_000);
}

#[test]
fn an_absolute_realtime_deadline_is_re_expressed_in_uptime() {
    let op = f::FUTEX_WAIT_BITSET | f::FUTEX_CLOCK_REALTIME;
    let now = 1_000_000;
    let utc = 1_700_000_000_000_000;
    // 2 s of wall clock in the future -> 2 s of uptime in the future.
    assert_eq!(deadline_us(op, utc + 2_000_000, now, Some(utc)), now + 2_000_000);
    // Already past -> expire immediately, not never.
    assert_eq!(deadline_us(op, utc - 1, now, Some(utc)), now);
    // No RTC yet: imprecise but bounded, and crucially still a deadline.
    assert_eq!(deadline_us(op, 42, now, None), 42);
}

/// A relative timeout so large it would overflow must clamp to "never", not
/// wrap to "already expired" — wrapping turns a long sleep into a busy loop.
#[test]
fn a_relative_timeout_saturates_instead_of_wrapping() {
    assert_eq!(deadline_us(f::FUTEX_WAIT, u64::MAX, 5, None), u64::MAX);
}

#[test]
fn an_untimed_wait_parks_on_a_rolling_revalidation_deadline() {
    let now = 10_000;
    assert_eq!(park_deadline_us(NEVER, now), now + REVALIDATE_US);
    // A real deadline is passed through untouched.
    assert_eq!(park_deadline_us(50_000, now), 50_000);
    // And the user-visible timeout check is unaffected by the substitution.
    assert!(!expired(NEVER, u64::MAX));
    assert!(expired(50_000, 50_000));
    assert!(!expired(50_000, 49_999));
}

// ─── WAKE_OP ─────────────────────────────────────────────────────────────────

#[test]
fn wake_op_decodes_the_four_fields() {
    // op=ADD(1) cmp=GT(4) oparg=3 cmparg=7
    let v = (1u32 << 28) | (4u32 << 24) | (3u32 << 12) | 7;
    let d = WakeOp::decode(v);
    assert_eq!((d.op, d.cmp, d.oparg, d.cmparg), (1, 4, 3, 7));
    assert_eq!(d.apply(10), Some(13));
    assert!(d.compare(10));
    assert!(!d.compare(7));
}

/// The shift bit shares the op nibble with the operation itself, so a decoder
/// that masks with `0xf` instead of `0x7` reads `ADD | SHIFT` as an undefined
/// op and returns `ENOSYS` for a perfectly ordinary call.
#[test]
fn the_oparg_shift_bit_is_resolved_at_decode_and_does_not_corrupt_the_op() {
    let v = ((1u32 | 8) << 28) | (3u32 << 12); // FUTEX_OP_ADD | FUTEX_OP_OPARG_SHIFT, oparg=3
    let d = WakeOp::decode(v);
    assert_eq!(d.op, 1, "the shift bit must not leak into the op field");
    assert_eq!(d.oparg, 8, "oparg becomes 1 << 3");
    assert_eq!(d.apply(1), Some(9));
}

/// Linux compares signed. `cmparg` is a 12-bit field sign-extended by the
/// shift-left-then-right extraction, and an unsigned comparison against a
/// negative one comes out backwards every time.
#[test]
fn the_comparison_is_signed() {
    // cmparg = 0xFFF -> the 12-bit field, extracted as 4095.
    let v = (2u32 << 24) | 0xFFF; // cmp=LT
    let d = WakeOp::decode(v);
    assert_eq!(d.cmparg, 4095);
    // oldval = -1 as u32 is far ABOVE 4095 unsigned, and below it signed.
    assert!(d.compare(u32::MAX), "-1 < 4095 signed");
    assert!(!d.compare(5000));
}

#[test]
fn every_defined_op_computes_and_an_undefined_one_reports_none() {
    let mk = |op: u32, oparg: u32| WakeOp::decode((op << 28) | (oparg << 12));
    assert_eq!(mk(0, 9).apply(3), Some(9)); // SET
    assert_eq!(mk(1, 9).apply(3), Some(12)); // ADD
    assert_eq!(mk(2, 9).apply(3), Some(11)); // OR
    assert_eq!(mk(3, 1).apply(3), Some(2)); // ANDN
    assert_eq!(mk(4, 1).apply(3), Some(2)); // XOR
    assert_eq!(mk(5, 1).apply(3), None); // undefined
    // ADD wraps rather than panicking in a debug build — the futex word is a
    // user-controlled u32 and this runs in the kernel.
    assert_eq!(mk(1, 1).apply(u32::MAX), Some(0));
}

#[test]
fn an_undefined_comparison_never_fires_the_second_wake() {
    let d = WakeOp::decode(9u32 << 24);
    assert!(!d.compare(0));
    assert!(!d.compare(u32::MAX));
}

// ─── The wait loop ───────────────────────────────────────────────────────────

#[test]
fn a_waiter_queued_nowhere_was_really_woken() {
    assert_eq!(step(Located::Nowhere, false, A, NEVER, 1), Step::Woken);
}

#[test]
fn a_spurious_wake_on_the_original_key_re_validates() {
    assert_eq!(step(Located::OriginalKey, false, A, NEVER, 1), Step::Revalidate);
    // Still inside its deadline: same answer.
    assert_eq!(step(Located::OriginalKey, false, A, 500, 499), Step::Revalidate);
}

/// A requeued waiter must NOT re-validate: the original futex word's contract
/// does not apply to the key it was moved to, so re-reading it would report a
/// spurious `EAGAIN` for a value the waiter no longer cares about.
#[test]
fn a_requeued_waiter_stays_parked_without_re_validating() {
    assert_eq!(step(Located::Requeued(B), false, A, NEVER, 1), Step::StayParked);
}

/// Timing out is where the cleanup obligation appears, and it differs by where
/// the waiter is: on the original key it already removed itself, on a requeue
/// target it has not.
#[test]
fn a_timeout_cleans_up_only_the_queue_it_is_still_on() {
    assert_eq!(
        step(Located::OriginalKey, false, A, 500, 500),
        Step::TimedOut { cleanup: None }
    );
    assert_eq!(
        step(Located::Requeued(B), false, A, 500, 500),
        Step::TimedOut { cleanup: Some(B) }
    );
}

#[test]
fn an_untimed_wait_never_times_out() {
    assert_eq!(step(Located::OriginalKey, false, A, NEVER, u64::MAX - 1), Step::Revalidate);
}

#[test]
fn a_signal_cleans_up_a_requeue_target_before_reporting_eintr() {
    assert_eq!(
        step(Located::Requeued(B), true, A, NEVER, 1),
        Step::Interrupted { cleanup: Some(B) }
    );
    assert_eq!(
        step(Located::OriginalKey, true, A, NEVER, 1),
        Step::Interrupted { cleanup: None }
    );
}

/// **Pinning a known divergence from Linux, not endorsing it.**
///
/// A pending signal is checked before the located result, so a waiter that a
/// `FUTEX_WAKE` has already dequeued still reports `EINTR`. The waker counted
/// that wake as delivered, so it is consumed: the caller must re-check its
/// condition, and a caller that treats `EINTR` as "nothing happened" can miss
/// it. Linux reports success here and delivers the signal afterwards.
///
/// This is exactly the behaviour `src/syscall/sync.rs` had before the
/// extraction and it was moved unchanged — an extraction that quietly fixes
/// something cannot be A/B'd against the thing it replaced. The test exists so
/// that changing it is a decision someone makes on purpose.
#[test]
fn signal_beats_an_already_delivered_wake() {
    assert_eq!(
        step(Located::Nowhere, true, A, NEVER, 1),
        Step::Interrupted { cleanup: None }
    );
}

/// Signals also outrank a deadline that has already passed. Both are terminal,
/// so this only decides which errno the caller sees — but a test pins it,
/// because "sometimes ETIMEDOUT, sometimes EINTR" is a flaky test in whatever
/// program is above.
#[test]
fn signal_outranks_an_expired_deadline() {
    assert_eq!(
        step(Located::OriginalKey, true, A, 500, 501),
        Step::Interrupted { cleanup: None }
    );
}

// ─── The whole family, end to end ────────────────────────────────────────────

/// One waiter, one waker, through the pieces in the order `sys_futex` uses
/// them. Not a substitute for the boot suite — there is no scheduler here — but
/// it is what catches a signature change that leaves every unit test passing.
#[test]
fn a_wait_then_wake_round_trip_composes() {
    let mut t = table();

    // Waiter: decode, key, enqueue.
    let ns = namespace(true, Some(7), false);
    assert_eq!(ns, Namespace::AddressSpace(7));
    let key = (ns.tgid(), 0x1000);
    let Action::Wait { bitset } = decode(f::FUTEX_WAIT | f::FUTEX_PRIVATE_FLAG, 0, key.1, true)
    else {
        panic!("expected a wait")
    };
    t.enqueue(key, Tid(1), bitset);

    // Waker: same op flags, same address, so it must compute the same key.
    let wns = namespace(true, Some(7), false);
    let Action::Wake { mask } = decode(f::FUTEX_WAKE | f::FUTEX_PRIVATE_FLAG, 0, key.1, true)
    else {
        panic!("expected a wake")
    };
    assert_eq!(tids(t.wake((wns.tgid(), key.1), 1, mask)), vec![1]);

    // Waiter wakes up, finds itself queued nowhere, and reports success.
    assert_eq!(t.locate_and_take(key.0, 1, key), Located::Nowhere);
    assert_eq!(step(Located::Nowhere, false, key, NEVER, 0), Step::Woken);
    assert_eq!(t.keys(), 0);
}
