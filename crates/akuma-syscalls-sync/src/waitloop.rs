//! The `FUTEX_WAIT` loop's outcome decision.
//!
//! After `schedule_blocking` returns, a waiter has to work out *why* it is
//! awake, and the answer is not a boolean. It may have been woken by a real
//! `FUTEX_WAKE`, woken spuriously, moved to another key by a `FUTEX_REQUEUE`
//! behind its back, timed out, or interrupted — and three of those five need
//! cleanup somewhere other than where the waiter thinks it is parked.
//!
//! Getting that wrong does not crash. It leaves a dead tid on a queue, where it
//! absorbs one future wake and strands a live thread — the shape of nearly
//! every futex incident in `docs/archive/`. So the decision is a pure function
//! of five inputs, and the tests below enumerate them.

use crate::table::{Key, Located};

/// What the wait loop does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A real `FUTEX_WAKE` took us off the queue. Return success.
    Woken,
    /// Return `EINTR`. `cleanup` names a queue we are still on and must leave.
    Interrupted { cleanup: Option<Key> },
    /// Return `ETIMEDOUT`. `cleanup` as above.
    TimedOut { cleanup: Option<Key> },
    /// Spurious wake on the original key: re-read the futex word and re-enqueue
    /// (a changed value reports `EAGAIN`, which is the lost-wake rescue).
    Revalidate,
    /// Requeued and still parked correctly: park again, and do **not**
    /// re-validate — the original futex word's contract does not apply to the
    /// key we were moved to.
    StayParked,
}

/// Decide the next step.
///
/// `located` is the result of [`crate::table::WaiterTable::locate_and_take`],
/// which has already removed us from the original key if that is where we were.
///
/// # Signal precedence, and the one thing it costs
///
/// A pending signal is checked **before** the located result, so it wins even
/// over a wake that has already dequeued us. That is a deliberate divergence
/// from Linux, which returns success for an already-delivered wake and lets the
/// signal be taken at the next opportunity; here the wake is consumed by the
/// waker (which counted it) and reported to the waiter as `EINTR`.
///
/// It is preserved exactly as the kernel had it — this crate's job was to move
/// the decision, not to change it — and pinned by
/// `signal_beats_an_already_delivered_wake`, which exists to make the next
/// person's change to it deliberate rather than accidental. See
/// `docs/reference/subsystems/syscalls/sync.md` § "Known divergences".
#[must_use]
pub fn step(
    located: Located,
    signal_pending: bool,
    key: Key,
    deadline_us: u64,
    now_us: u64,
) -> Step {
    // Where we still sit, if anywhere: only a requeue can leave us queued at
    // this point. `OriginalKey` already self-removed inside `locate_and_take`,
    // and `Nowhere` is gone by definition.
    let cleanup = match located {
        Located::Requeued(k) => Some(k),
        Located::Nowhere | Located::OriginalKey => None,
    };
    debug_assert!(cleanup != Some(key), "a requeue target cannot be the original key");

    if signal_pending {
        return Step::Interrupted { cleanup };
    }
    if matches!(located, Located::Nowhere) {
        return Step::Woken;
    }
    if crate::deadline::expired(deadline_us, now_us) {
        return Step::TimedOut { cleanup };
    }
    match located {
        Located::OriginalKey => Step::Revalidate,
        Located::Requeued(_) => Step::StayParked,
        Located::Nowhere => unreachable!(),
    }
}
