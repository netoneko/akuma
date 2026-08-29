//! `epoll_ctl(2)` op decode, and the errno set of applying it.
//!
//! Split in two because the kernel's call is split in two, for a reason worth
//! keeping: the 16-byte `epoll_event` is read out of userspace **before**
//! `EPOLL_TABLE` is taken, since `read_user_into` prefaults and the prefault's
//! `LazySource::File` arm reads a page in through the VFS — block I/O, which is
//! barred outright inside an IRQ-masked hold (`locking.md`). So [`decode`] runs
//! first and says whether an event struct is needed at all, and
//! [`InterestList::apply`](crate::interest::InterestList::apply) runs inside
//! the hold.

use akuma_primitives::errno::negated::{EINVAL, ENOENT};
use akuma_syscalls_linux::flags::epoll::{EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD};

/// A decoded `epoll_ctl` op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctl {
    /// `EPOLL_CTL_ADD`.
    Add,
    /// `EPOLL_CTL_MOD`.
    Mod,
    /// `EPOLL_CTL_DEL`.
    Del,
    /// Anything else — `EINVAL`, decided before the table is touched.
    Unknown,
}

impl Ctl {
    /// Whether this op reads a `struct epoll_event` from userspace.
    ///
    /// `DEL` does not: Linux has ignored `epoll_ctl`'s fourth argument for
    /// `EPOLL_CTL_DEL` since 2.6.9, and callers pass `NULL` there routinely.
    /// Reading it would turn a correct program into `EFAULT`.
    #[must_use]
    pub const fn needs_event(self) -> bool {
        matches!(self, Self::Add | Self::Mod)
    }
}

/// Decode `op`.
///
/// Total by construction: an unknown op is a value, not an error return, so the
/// caller cannot forget to handle it.
#[must_use]
pub const fn decode(op: i32) -> Ctl {
    match op {
        EPOLL_CTL_ADD => Ctl::Add,
        EPOLL_CTL_MOD => Ctl::Mod,
        EPOLL_CTL_DEL => Ctl::Del,
        _ => Ctl::Unknown,
    }
}

/// What applying a [`Ctl`] to an interest list did.
///
/// Carries more than the errno because the kernel's trace distinguishes a fresh
/// `ADD` from an `ADD` that landed on an fd already in the list — and that
/// second case is a **divergence from Linux**, kept deliberately. See
/// [`Self::errno`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtlOutcome {
    /// `ADD` on an fd not in the list: inserted.
    Added,
    /// `ADD` on an fd already in the list. Linux answers `EEXIST`; this kernel
    /// overwrites the entry, exactly as `MOD` would, and answers success.
    AddedOverExisting,
    /// `MOD` on a present fd: events and data replaced, edge state reset.
    Modified,
    /// `DEL` on a present fd: removed.
    Deleted,
    /// `MOD` or `DEL` on an fd not in the list.
    NotFound,
    /// The op was not one of `ADD`/`MOD`/`DEL`.
    Unknown,
}

impl CtlOutcome {
    /// The syscall return value, already negated for `x0`.
    ///
    /// # Known divergence: `ADD` on a present fd is not `EEXIST`
    ///
    /// Linux returns `EEXIST` and leaves the existing registration alone. This
    /// kernel treats it as a `MOD` — it overwrites `events`/`data` and resets
    /// the edge state — and returns 0. Preserved as-is by the extraction rather
    /// than fixed, so the move stays behaviour-preserving and A/B-able; pinned
    /// by `an_add_on_a_present_fd_overwrites_instead_of_reporting_eexist`, and
    /// recorded under "Known divergences" in
    /// `docs/reference/subsystems/syscalls/poll.md`.
    ///
    /// The practical difference is narrow — a library that re-`ADD`s where it
    /// meant to `MOD` silently works here and fails on Linux, which is the safe
    /// direction — but a program that *tests* for `EEXIST` to discover whether
    /// it has already registered an fd will conclude it has not.
    #[must_use]
    pub const fn errno(self) -> u64 {
        match self {
            Self::Added | Self::AddedOverExisting | Self::Modified | Self::Deleted => 0,
            Self::NotFound => ENOENT,
            Self::Unknown => EINVAL,
        }
    }

    /// The tag this outcome is traced under, or `None` for the outcomes the
    /// kernel does not trace (it logs only successful `ADD`s).
    #[must_use]
    pub const fn trace_tag(self) -> Option<&'static str> {
        match self {
            Self::Added => Some("ADD"),
            Self::AddedOverExisting => Some("ADD->MOD"),
            _ => None,
        }
    }
}
