//! `poll(2)`'s `struct pollfd` marshalling: `POLL*` ↔ `EPOLL*`.
//!
//! The two bit vocabularies agree numerically for the bits both define —
//! `POLLIN == EPOLLIN == 1`, `POLLOUT == EPOLLOUT == 4`, and so on, asserted in
//! `akuma_syscalls_linux::flags` — so this looks like a translation that could
//! be a cast. It cannot, for two reasons that are the whole content of this
//! module: `revents` is an `i16` while the readiness map speaks `u32`
//! (`EPOLLRDHUP` is `0x2000`, and `EPOLLET` does not fit at all), and the two
//! directions mask differently.

use akuma_syscalls_linux::flags::poll::{POLLERR, POLLHUP, POLLIN, POLLOUT};
use akuma_syscalls_linux::flags::epoll::{EPOLLERR, EPOLLHUP, EPOLLIN, EPOLLOUT};

/// The `EPOLL*` mask to probe a `pollfd` with.
///
/// # Known divergence: only `POLLIN` and `POLLOUT` are carried
///
/// `POLLPRI` and `POLLRDHUP` are accepted from the caller and dropped, so a
/// `poll()` for `POLLRDHUP` never reports one even though `epoll` on the same
/// socket does. `POLLPRI` is honest — Akuma has no out-of-band TCP data, so no
/// fd ever has an exceptional condition — but `POLLRDHUP` is a real gap, and
/// the reason `select(2)`'s `exceptfds` bug was found through libcurl rather
/// than here. Preserved by the extraction rather than fixed; see "Known
/// divergences" in `docs/reference/subsystems/syscalls/poll.md`.
#[must_use]
pub const fn requested(events: i16) -> u32 {
    let mut r = 0;
    if events & POLLIN != 0 {
        r |= EPOLLIN;
    }
    if events & POLLOUT != 0 {
        r |= EPOLLOUT;
    }
    r
}

/// Map a probed readiness mask back into a `pollfd.revents`.
///
/// # Which bits are masked by `events`, and which are not
///
/// `POLLIN`/`POLLOUT` are reported only if the caller asked for them.
/// `POLLHUP`/`POLLERR` are reported regardless — POSIX says they are always
/// possible in `revents` and cannot be requested, and a `poll()` that could not
/// report a hangup unless asked would leave a caller waiting on a dead fd
/// forever.
///
/// `POLLNVAL` is never produced: an fd the calling process does not have
/// reaches the readiness map as
/// [`FdState::Missing`](crate::readiness::FdState::Missing) and comes back as
/// `EPOLLHUP|EPOLLERR`, so `poll()` reports `POLLHUP|POLLERR` where Linux
/// reports `POLLNVAL`. A caller that distinguishes "bad fd" from "peer hung up"
/// sees the wrong one. Another preserved divergence.
#[must_use]
pub const fn report(events: i16, revents: u32) -> i16 {
    let mut r: i16 = 0;
    if revents & EPOLLIN != 0 && events & POLLIN != 0 {
        r |= POLLIN;
    }
    if revents & EPOLLOUT != 0 && events & POLLOUT != 0 {
        r |= POLLOUT;
    }
    if revents & EPOLLHUP != 0 {
        r |= POLLHUP;
    }
    if revents & EPOLLERR != 0 {
        r |= POLLERR;
    }
    r
}
