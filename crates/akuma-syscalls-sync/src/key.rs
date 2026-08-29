//! Which namespace a futex key belongs to.
//!
//! Two lines of logic guarding a bug that took a `-j4` rustc self-host build to
//! reproduce, and that no single-process futex probe can ever show.

/// The namespace half of a futex key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// Scoped to one address space, by thread-group leader pid. The normal case.
    AddressSpace(u32),
    /// The VA-only global namespace (`tgid = 0`), reserved for memory genuinely
    /// shared *between* address spaces.
    Shared,
    /// The caller could not resolve its own identity, so the key falls back to
    /// the global namespace.
    ///
    /// Distinct from [`Self::Shared`] because it is **not** a graceful
    /// fallback, it is a correctness event: `(0, uaddr)` is keyed by virtual
    /// address alone, and with no ASLR every process running the same binary
    /// parks on the same addresses. N copies of one program then collapse into
    /// one queue, where a `FUTEX_WAKE(uaddr, 1)` from one pops a *different*
    /// process's waiter, counts it as woken, and leaves the real waiter parked
    /// forever. The caller must treat this as a tripwire, not a branch.
    Degraded,
}

impl Namespace {
    /// The `tgid` half of the key.
    #[must_use]
    pub const fn tgid(self) -> u32 {
        match self {
            Self::AddressSpace(tgid) => tgid,
            Self::Shared | Self::Degraded => 0,
        }
    }
}

/// Pick the namespace for a futex key.
///
/// `own_tgid` is the caller's thread-group leader pid, `None` if it could not
/// be resolved. `shared_file_mapping` is whether `uaddr` falls in a mapping
/// shared across address spaces — the caller resolves that, since it needs the
/// memory map.
///
/// # Why `FUTEX_PRIVATE_FLAG` alone does not decide this
///
/// Linux keys a futex by address space whenever the page is anonymous,
/// **whether or not `FUTEX_PRIVATE` was passed**: `get_futex_key` only reaches
/// the `(inode, index)` form for a page that has a `page->mapping`, and falls
/// back to `(mm, address)` otherwise. Keying every non-private op to
/// `(0, uaddr)` therefore diverges from Linux — and with no ASLR, `(0, uaddr)`
/// is the *same* key in every process running the same binary.
///
/// That is not a corner case, it is musl's thread-list lock. `__tl_lock` /
/// `__tl_unlock` wait and wake on `&__thread_list_lock`, a `libc.bss` global at
/// a fixed address, with `priv = 0` — and `pthread_create` hands the kernel
/// that same address as the `CLONE_CHILD_CLEARTID` word. So *every thread
/// create and exit in every musl process* shared one global queue:
/// `FUTEX_WAKE(&__thread_list_lock, 1)` in process A pops the FIFO head, which
/// is often a thread of process B — B wakes spuriously, the wake is counted as
/// delivered, and A's own waiter stays parked forever. It takes several
/// multi-threaded processes running at once to show, which is why the
/// single-process futex probes passed throughout.
#[must_use]
pub const fn namespace(
    is_private: bool,
    own_tgid: Option<u32>,
    shared_file_mapping: bool,
) -> Namespace {
    let Some(tgid) = own_tgid else { return Namespace::Degraded };
    if tgid == 0 {
        return Namespace::Degraded;
    }
    if !is_private && shared_file_mapping {
        // Genuinely cross-address-space memory: the global namespace is the
        // point. Still VA-keyed, so it only pairs processes that mapped it at
        // the same address — the pre-existing limit of Akuma's shared-futex
        // support, and now the only thing that can land in this namespace.
        return Namespace::Shared;
    }
    Namespace::AddressSpace(tgid)
}
