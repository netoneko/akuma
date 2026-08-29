//! `select(2)` fd-set marshalling: the bit arithmetic on the caller's three
//! `fd_set` bitmaps.
//!
//! Pure, fiddly, and untested until this crate existed. Fiddly because there
//! are four things to get right at once and three of them are silent when
//! wrong: which words the kernel is allowed to touch (`nfds` bounds a *bit*
//! count, the copy is in whole 8-byte words, and the last word is partial), the
//! word/bit split, that a fd asked about in both sets counts **twice** toward
//! the return value, and that `select` reports by *overwriting* — a set the
//! kernel does not write comes back exactly as the caller passed it in.
//!
//! That last one is not hypothetical. It is the whole of
//! `docs/runbooks/cargo-cannot-reach-crates-io.md`: `exceptfds` was received
//! and never written, so every fd the caller put in it came back still flagged,
//! libcurl synthesised `POLLPRI` from that, mapped it to `CURL_CSELECT_ERR`,
//! and abandoned a socket that had just reached `Established` with
//! `SO_ERROR == 0`. Every `cargo fetch` failed one RTT into a connection that
//! had in fact succeeded.
//!
//! The copies themselves stay in the kernel — they are user-memory accesses,
//! and `MAX_WORDS`-sized stack buffers are its business, not this crate's.

use akuma_syscalls_linux::flags::epoll::{EPOLLIN, EPOLLOUT};

/// The highest fd number `select(2)` accepts here, exclusive.
///
/// A **hard cap** the `ppoll`/`epoll` paths do not have: `nfds > MAX_FDS` is
/// `EINVAL`, so a caller `select()`-ing on a very high fd number fails where
/// `poll()` would work. It exists because the fd sets are fixed stack buffers;
/// Linux has no such limit.
pub const MAX_FDS: usize = 1024;

/// `MAX_FDS` worth of `u64` words — the size of the kernel's stack buffers.
pub const MAX_WORDS: usize = MAX_FDS / 64;

/// Whether `nfds` is in range. `false` is `EINVAL`.
#[must_use]
pub const fn nfds_ok(nfds: usize) -> bool {
    nfds <= MAX_FDS
}

/// The number of `u64` words `nfds` bits occupy.
#[must_use]
pub const fn words(nfds: usize) -> usize {
    nfds.div_ceil(64)
}

/// The number of bytes of each `fd_set` the kernel copies in and out.
///
/// Whole words, so for a `nfds` that is not a multiple of 64 this covers bits
/// past `nfds`. That is correct and matches Linux — `fd_set` is an array of
/// words and the tail bits are simply never set — but it is why the write-back
/// buffers must be zeroed rather than assumed clean.
#[must_use]
pub const fn bytes(nfds: usize) -> usize {
    words(nfds) * 8
}

/// Whether bit `fd` is set in a bitmap.
///
/// Out-of-range words read as clear rather than panicking: the caller's buffer
/// is `MAX_WORDS` long but only `words(nfds)` of it was filled from userspace.
#[must_use]
pub fn is_set(bits: &[u64], fd: usize) -> bool {
    bits.get(fd / 64).is_some_and(|w| w & (1u64 << (fd % 64)) != 0)
}

/// Set bit `fd` in a bitmap. Out-of-range is a no-op, for the same reason.
pub fn set(bits: &mut [u64], fd: usize) {
    if let Some(w) = bits.get_mut(fd / 64) {
        *w |= 1u64 << (fd % 64);
    }
}

/// One fd the caller asked about, and in which direction(s).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest {
    pub fd: usize,
    pub in_read: bool,
    pub in_write: bool,
}

impl Interest {
    /// The `EPOLL*` mask to probe this fd with.
    #[must_use]
    pub const fn requested(self) -> u32 {
        let mut r = 0;
        if self.in_read {
            r |= EPOLLIN;
        }
        if self.in_write {
            r |= EPOLLOUT;
        }
        r
    }

    /// Write this fd's result into the output sets, returning how much it adds
    /// to `select`'s return value.
    ///
    /// **Two, for a fd that is ready in both directions.** `select(2)` returns
    /// the total number of *bits* left set across all three sets, not the
    /// number of fds — a caller that sized a loop by the return value and got
    /// one per fd would stop early.
    ///
    /// Each direction is gated on having been *asked* about, not just on the
    /// probe's answer: an fd in `readfds` only is never reported writable, even
    /// though the probe reports both bits for many fd kinds.
    pub fn record(self, revents: u32, out_read: &mut [u64], out_write: &mut [u64]) -> u64 {
        let mut n = 0;
        if self.in_read && revents & EPOLLIN != 0 {
            set(out_read, self.fd);
            n += 1;
        }
        if self.in_write && revents & EPOLLOUT != 0 {
            set(out_write, self.fd);
            n += 1;
        }
        n
    }
}

/// Every fd below `nfds` that appears in either set, ascending.
///
/// Fds in neither set are skipped rather than probed — probing registers a
/// waker with the underlying resource, so probing an fd the caller never asked
/// about would add a wakeup source out of nowhere.
pub fn interests(readfds: &[u64], writefds: &[u64], nfds: usize) -> impl Iterator<Item = Interest> {
    (0..nfds).filter_map(move |fd| {
        let in_read = is_set(readfds, fd);
        let in_write = is_set(writefds, fd);
        (in_read || in_write).then_some(Interest { fd, in_read, in_write })
    })
}

/// Every fd below `nfds` set in either bitmap, ascending — the fd numbers alone.
///
/// Used for the per-call cadence question ("does any polled fd want the rump
/// poll interval?"), which needs the numbers but not the directions.
pub fn set_fds(readfds: &[u64], writefds: &[u64], nfds: usize) -> impl Iterator<Item = usize> {
    interests(readfds, writefds, nfds).map(|i| i.fd)
}
