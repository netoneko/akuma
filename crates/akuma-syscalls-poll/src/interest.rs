//! One epoll instance's interest list.
//!
//! This is the family's one data structure. It is a map from raw fd to the
//! registration made for it — the requested events, the opaque `data` word
//! handed back on every report, and the edge-triggered "already reported" mask
//! that [`crate::edge`] decides against.
//!
//! # What it is not
//!
//! There is no lock in this module, no IRQ masking, no fd table and no probe.
//! The kernel owns all four: `EPOLL_TABLE` is a `Spinlock<BTreeMap<u32,
//! EpollInstance>>` accessed with local IRQs masked (an AB-BA argument about
//! this kernel's IRQ discipline, with a known `EPOLL_TABLE` ↔ `PROCESS_TABLE`
//! ordering to respect), and every readiness check happens with that lock
//! *released*. So this type is a container the kernel puts inside its own
//! instance, and every method is a pure mutation on it.
//!
//! It also does not allocate a snapshot. `sys_epoll_pwait` copies the fd keys
//! into a stack array precisely to keep an allocation out of an IRQ-masked
//! hold; [`InterestList::fds`] hands out an iterator and lets the caller decide
//! where the copy goes.

use alloc::collections::BTreeMap;

use akuma_syscalls_linux::flags::epoll::{EPOLLET, EPOLL_EVENT_MASK};

use crate::ctl::{Ctl, CtlOutcome};

/// One fd's registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The raw registration word as userspace wrote it — readiness bits plus
    /// the `EPOLLET`/`EPOLLONESHOT` *registration* bits, which is why
    /// [`Self::requested`] exists rather than using this directly.
    pub events: u32,
    /// The opaque `u64` handed back verbatim in every `epoll_event` reported
    /// for this fd. The kernel never interprets it.
    pub data: u64,
    /// Bits already reported to an edge-triggered watcher. See [`crate::edge`].
    pub last_ready: u32,
}

impl Entry {
    /// The readiness bits a scan should ask for: the registration word with the
    /// registration-only bits masked off.
    ///
    /// Masking matters in both directions. `EPOLLET` is `1 << 31`; handed to a
    /// readiness probe unmasked it is simply a bit nothing matches, but handed
    /// *back* to userspace in a `revents` it would read as an edge request
    /// echoed as an event.
    #[must_use]
    pub const fn requested(self) -> u32 {
        self.events & EPOLL_EVENT_MASK
    }

    /// Whether this registration is edge-triggered.
    #[must_use]
    pub const fn is_edge_triggered(self) -> bool {
        self.events & EPOLLET != 0
    }
}

/// The fd → [`Entry`] map of one epoll instance.
#[derive(Debug, Default)]
pub struct InterestList {
    map: BTreeMap<u32, Entry>,
}

impl InterestList {
    #[must_use]
    pub const fn new() -> Self {
        Self { map: BTreeMap::new() }
    }

    /// Number of registered fds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The registered fds, ascending.
    ///
    /// Ascending because the backing map is ordered, and `epoll_pwait` fills a
    /// caller's `maxevents`-sized buffer by walking this and stopping when it
    /// is full — so with more ready fds than room, which ones get reported is
    /// decided here. Linux uses a ready-list rotation instead and is fairer
    /// under that overload; this is a known divergence, recorded in
    /// `docs/reference/subsystems/syscalls/poll.md`.
    pub fn fds(&self) -> impl Iterator<Item = u32> + '_ {
        self.map.keys().copied()
    }

    /// The registration for `fd`, if any.
    #[must_use]
    pub fn get(&self, fd: u32) -> Option<Entry> {
        self.map.get(&fd).copied()
    }

    /// Apply a decoded `epoll_ctl` op.
    ///
    /// `events`/`data` come from the `epoll_event` the caller read out of
    /// userspace before taking the lock; they are ignored for
    /// [`Ctl::Del`]/[`Ctl::Unknown`], which [`Ctl::needs_event`] says do not
    /// read one.
    ///
    /// Both write paths reset `last_ready` to 0. That is what makes a re-`MOD`
    /// the documented way to re-arm an edge-triggered fd by hand: without it, a
    /// caller that re-registered after draining would still be told its fd had
    /// "already reported" whatever it reported last.
    pub fn apply(&mut self, ctl: Ctl, fd: u32, events: u32, data: u64) -> CtlOutcome {
        match ctl {
            Ctl::Add => {
                let replaced = self
                    .map
                    .insert(fd, Entry { events, data, last_ready: 0 })
                    .is_some();
                if replaced { CtlOutcome::AddedOverExisting } else { CtlOutcome::Added }
            }
            Ctl::Mod => match self.map.get_mut(&fd) {
                Some(entry) => {
                    entry.events = events;
                    entry.data = data;
                    entry.last_ready = 0;
                    CtlOutcome::Modified
                }
                None => CtlOutcome::NotFound,
            },
            Ctl::Del => {
                if self.map.remove(&fd).is_some() {
                    CtlOutcome::Deleted
                } else {
                    CtlOutcome::NotFound
                }
            }
            Ctl::Unknown => CtlOutcome::Unknown,
        }
    }

    /// Drop `fd`'s registration, reporting whether there was one.
    ///
    /// Not an `epoll_ctl` path: this is the prune `sys_epoll_pwait` performs
    /// when it finds an interest-list entry whose fd the calling process has
    /// already closed. Real Linux drops those implicitly at `close()` time
    /// (`eventpoll_release_file` walks back-references from the file to its
    /// epitems); this kernel's `close()` does not, so the scan has to. Left
    /// unpruned, [`FdState::Missing`](crate::readiness::FdState::Missing)
    /// synthesises `EPOLLHUP|EPOLLERR` for an fd the caller already closed — an
    /// event real Linux can never produce, and one nginx dereferenced a
    /// torn-down connection object on.
    pub fn prune(&mut self, fd: u32) -> bool {
        self.map.remove(&fd).is_some()
    }

    /// Record what a scan just reported for `fd`, so the next scan can tell
    /// which bits are new.
    ///
    /// Only ever called for edge-triggered entries — see [`crate::edge::scan`],
    /// which decides.
    pub fn record_ready(&mut self, fd: u32, revents: u32) {
        if let Some(entry) = self.map.get_mut(&fd) {
            entry.last_ready = revents;
        }
    }

    /// Clear `bits` from `fd`'s "already reported" mask, if `fd` is registered
    /// **edge-triggered**, so the next time those bits go ready they count as a
    /// fresh edge.
    ///
    /// Returns whether anything changed, which the caller may trace.
    ///
    /// # Why this exists at all
    ///
    /// `last_ready` is refreshed only inside `sys_epoll_pwait`'s own loop,
    /// which recomputes readiness from scratch each pass. A level transition
    /// that happens *and un-happens* between two passes is therefore invisible
    /// to it: the mask still says "already reported" and the edge never fires
    /// again. The I/O syscalls are the only code that witnesses those
    /// transitions, so they report them back — `read`/`recvfrom`/`recvmsg`
    /// clearing `EPOLLIN` on a drain, `write`/`sendto`/`sendmsg` clearing
    /// `EPOLLOUT` on a short write or `EAGAIN`.
    ///
    /// The level-triggered guard is not an optimisation. A level-triggered
    /// entry never consults `last_ready`, so clearing it there would be
    /// invisible — but the guard is what states that, and it is what makes
    /// "which entries does a drain touch?" a question with a test.
    pub fn reset_edge(&mut self, fd: u32, bits: u32) -> bool {
        match self.map.get_mut(&fd) {
            Some(entry) if entry.is_edge_triggered() && entry.last_ready & bits != 0 => {
                entry.last_ready &= !bits;
                true
            }
            _ => false,
        }
    }
}
