//! `futex(2)` op decode: what the kernel should *do* with this call, decided
//! before it touches the waiter table.
//!
//! Everything here is a function of the arguments alone plus one fact the
//! kernel must supply (`uaddr_mapped`). That is the whole point: the decode has
//! three rules that are neither obvious nor derivable from the man page, and
//! all three exist because a real program broke without them.

use akuma_primitives::errno::negated::{EAGAIN, EFAULT, EINVAL, ENOSYS};
use akuma_syscalls_linux::flags::futex::{self as f, FUTEX_BITSET_MATCH_ANY};

/// What `sys_futex` should do, once the arguments have been read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Return this (already negated) errno, or this value, immediately.
    Return(u64),
    /// Enqueue and park, matching wakes against `bitset`.
    Wait { bitset: u32 },
    /// Wake up to `val` waiters whose bitset intersects `mask`.
    Wake { mask: u32 },
    /// Wake up to `val`, requeue up to `max_requeue` onto `uaddr2`. `compare`
    /// carries the value `*uaddr` must still equal (`FUTEX_CMP_REQUEUE`), or
    /// `None` for the unconditional `FUTEX_REQUEUE`.
    Requeue { compare: Option<u32> },
    /// The read-modify-write plus conditional second wake.
    WakeOp,
}

/// Decode `op`/`val3` into an [`Action`].
///
/// `uaddr_mapped` is the caller's `validate_user_ptr(uaddr, 4)` result. It is a
/// parameter rather than a callback because validating a user pointer is an
/// effect — it can demand-page — and this crate performs none.
///
/// # The three rules that are not in the man page
///
/// 1. **An unmapped `uaddr` is not always `EFAULT`.** For the wake family it is
///    *success with zero woken*, because there cannot be waiters on memory that
///    is not mapped, and Go's runtime calls
///    `futex(0xfffffffffffffffc, FUTEX_WAKE)` during exit coordination —
///    answering `EFAULT` there breaks Go's exit path and strands its goroutine
///    threads. For the wait family it is `EAGAIN` ("value changed"), which Go
///    retries and then proceeds. Only the remaining ops get `EFAULT`.
/// 2. **`FUTEX_WAIT_BITSET` with `val3 == 0` is `EINVAL`.** A zero bitset can
///    never intersect a wake mask, so the waiter would be unwakeable — the
///    kernel refuses rather than park it forever.
/// 3. **A misaligned or null `uaddr` is `EINVAL` before anything else**, ahead
///    of the mapping check, so a bad pointer cannot reach the table at all.
#[must_use]
pub fn decode(op: i32, val3: u32, uaddr: usize, uaddr_mapped: bool) -> Action {
    let cmd = f::cmd_of(op);

    if uaddr == 0 || uaddr & 3 != 0 {
        return Action::Return(EINVAL);
    }
    if !uaddr_mapped {
        return match cmd {
            f::FUTEX_WAKE | f::FUTEX_WAKE_BITSET | f::FUTEX_WAKE_OP => Action::Return(0),
            f::FUTEX_WAIT | f::FUTEX_WAIT_BITSET => Action::Return(EAGAIN),
            _ => Action::Return(EFAULT),
        };
    }

    match cmd {
        f::FUTEX_WAIT => Action::Wait { bitset: FUTEX_BITSET_MATCH_ANY },
        f::FUTEX_WAIT_BITSET => {
            if val3 == 0 {
                Action::Return(EINVAL)
            } else {
                Action::Wait { bitset: val3 }
            }
        }
        f::FUTEX_WAKE => Action::Wake { mask: FUTEX_BITSET_MATCH_ANY },
        f::FUTEX_WAKE_BITSET => Action::Wake { mask: val3 },
        f::FUTEX_REQUEUE => Action::Requeue { compare: None },
        f::FUTEX_CMP_REQUEUE => Action::Requeue { compare: Some(val3) },
        f::FUTEX_WAKE_OP => Action::WakeOp,
        // Priority inheritance: not implemented. Reported as `ENOSYS` rather
        // than emulated, because a *silently* non-inheriting PI futex is worse
        // than an absent one — glibc falls back when it sees ENOSYS.
        f::FUTEX_LOCK_PI
        | f::FUTEX_UNLOCK_PI
        | f::FUTEX_TRYLOCK_PI
        | f::FUTEX_WAIT_REQUEUE_PI
        | f::FUTEX_CMP_REQUEUE_PI
        | f::FUTEX_FD => Action::Return(ENOSYS),
        _ => Action::Return(ENOSYS),
    }
}
