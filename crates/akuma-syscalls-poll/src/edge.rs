//! The `EPOLLET` armed-state decision: given what a scan found, what does this
//! fd contribute to the `epoll_wait` return, and what does the interest list
//! remember afterwards?
//!
//! Two lines of kernel code, and the source of more hangs in this tree than any
//! other two. The failure mode is why: a lost edge is **invisible**. The fd
//! stays ready, the watcher stays parked, nothing returns an error and no trace
//! fires. What you see is a healthy process that has stopped.

use akuma_syscalls_linux::flags::epoll::EPOLLET;

/// What one scanned fd contributes to this `epoll_wait` return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scan {
    /// Bits to hand back in an `epoll_event`. Zero means report nothing — for
    /// an edge-triggered entry that is the `SUPPRESSED` case, and it is normal.
    pub report: u32,
    /// The new `last_ready` for this entry, or `None` to leave it untouched.
    ///
    /// `None` for level-triggered entries. Recording there would be harmless
    /// today — nothing reads `last_ready` unless `EPOLLET` is set — but it is
    /// not what the kernel does, and a stored value nothing maintains is how a
    /// later `EPOLLET` re-registration would inherit a stale mask.
    pub record: Option<u32>,
}

/// Decide what to report for one fd.
///
/// `events` is the raw registration word (the `EPOLLET` bit is read from it);
/// `revents` is what the readiness map just returned; `last_ready` is what the
/// previous scan recorded.
///
/// # The level-triggered case
///
/// Report whatever is ready, every pass, and remember nothing. A caller that
/// does not drain the fd is told again next pass, which is the contract.
///
/// # The edge-triggered case
///
/// Report `revents & !last_ready` — the bits that were not ready last time —
/// and record `revents`, **not** the reported subset. Recording the subset
/// instead is the subtle version of the bug: a bit that stayed ready across two
/// passes would drop out of the mask on the second and re-fire on the third, so
/// an edge-triggered fd would spuriously re-arm itself every other pass.
///
/// Note that the record happens even when nothing is reported, and even when
/// `revents` is 0. That is what closes the edge: readiness going *away* has to
/// be written down, or the next arrival is not a new bit.
#[must_use]
pub const fn scan(events: u32, revents: u32, last_ready: u32) -> Scan {
    if events & EPOLLET == 0 {
        Scan { report: revents, record: None }
    } else {
        Scan { report: revents & !last_ready, record: Some(revents) }
    }
}
