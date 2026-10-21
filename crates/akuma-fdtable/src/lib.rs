//! The fd table as a pure, host-tested state machine.
//!
//! # What this crate is
//!
//! One process's descriptor table: which fd numbers name which open
//! descriptions, which numbers carry `FD_CLOEXEC` and the per-number
//! non-blocking bit, and what each table operation *decides*. It is generic
//! over the description payload `D` and never names a descriptor type, a lock,
//! a hook or a kernel object: an operation that would have an effect outside
//! the table — release a pipe reference on close, bump one on a fork copy —
//! **returns the payload and lets the caller perform the effect** (the
//! `akuma-pipes`/`akuma-tty` shape). Nothing in here allocates beyond
//! `alloc`'s collection nodes and nothing here can block.
//!
//! [`FdTable<'t, D>`] is a *borrowing view* over the table's three parts —
//! [`Names<D>`], and the two [`FlagSet`]s — because the kernel holds them
//! under three separate spinlocks and takes them together inside one
//! IRQ-masked critical section. The view is the state machine; the parts are
//! its state. A caller that owns the parts outright borrows them the same way.
//!
//! # Where every fd property lives (the ownership table)
//!
//! | property | owner | keyed by |
//! |---|---|---|
//! | fd number → open description | [`Names<D>`] | fd number |
//! | `FD_CLOEXEC` | [`FlagSet`] (the `cloexec` part) | fd number |
//! | `O_NONBLOCK` | [`FlagSet`] (the `nonblock` part) | fd number |
//! | file offset (`KernelFile::position`) | the payload `D` itself | the description |
//!
//! The two flag sets are keyed by **fd number**, not by description. That is a
//! pinned divergence from POSIX (where both flags live on the open file
//! description and `dup` shares them), adopted by `akuma-syscalls-glue`'s
//! table and pinned by the boot suite (`amd64/src/fd.rs`: "a dup loses
//! nonblock") and by `akuma-exec`'s characterization tests. [`FdTable::dup_to`]
//! copies the non-blocking bit old→new the way glue's `sys_dup3` does, and
//! [`FdTable::dup`] does not copy it the way glue's `sys_dup` does not;
//! [`FdTable::alloc_fd_from`] clears both bits for the number it hands out, so
//! a stale flag can never survive a close-reuse cycle
//! (`docs/archive/NCA_FD_NONBLOCK_TOCTOU.md`).
//!
//! # Why `dup` copies the payload by value, and what sharing would take
//!
//! In this crate — as in the kernel it was extracted from — `dup` inserts an
//! independent copy of the payload: two descriptors on one file carry
//! **independent cursors** (`D` is cloned by [`FdTable::dup`]/[`FdTable::dup_to`]),
//! and the kernel's per-description references (pipe ends, sockets, inode
//! pins) are bumped once per copy and released once per close, which is the
//! invariant `akuma_exec::process::clone_fd_refs`/`release_fd_entry` implement.
//! POSIX shares one open file description across `dup`s (one cursor, one
//! flags word). Fixing that honestly is a change to `D` itself — the payload
//! would have to move behind one refcounted cell per description, with the
//! *description count* as the kernel-reference count — and it is deliberately
//! NOT done here. `docs/archive/AKUMA_FDTABLE_EXTRACTION.md` § "The dup fix"
//! carries the design and the reason it stayed a recommendation.
//!
//! # Allocation
//!
//! The state is three `BTreeMap`/`BTreeSet`s, one node per occupied fd. The
//! alternatives and why they lost:
//!
//! - **A fixed `[Option<DescId>; 1024]` name array** is 4 KiB per table plus
//!   a payload arena — and the table is *forked* on every `fork`, so every
//!   child would copy (or COW-manage) the full 4 KiB to inherit three stdio
//!   descriptors. The kernel spawns shells constantly; paying per-fd is the
//!   right gradient. It would also lose `const`-constructible emptiness,
//!   which `amd64/src/fd.rs`'s `KERNEL_TABLE` static needs.
//! - **`akuma-slot-table`** is the *process* table's lock-free store: atomic
//!   slot states, reuse generations, deferred reclamation for readers that
//!   can outrun a reaper. Nothing reads this table without holding its
//!   spinlock, so there is no reader to protect and the machinery would be
//!   dead weight.
//! - **A sorted small-vec** makes `close` O(n) shifts on the hottest path
//!   (shell pipelines close 5+ fds per command) to save a node that is
//!   smaller than the payload it sits beside.
//!
//! A `BTreeMap` node costs ~40 B against a payload that starts at ~120 B
//! (`KernelFile` with its path `String`), the map is `const`-constructible
//! empty, iteration order is ascending fd — which the sweep and `close_all`
//! semantics are pinned against — and `range(min_fd..)` makes lowest-free
//! allocation O(log n + gap). The measured incident that motivated extraction
//! (`docs/archive/AKUMA_FDTABLE_EXTRACTION.md`) was a 60 ms *deep clone* on
//! the `getdents64` path, not map overhead; the cure for that is
//! `with_fd`-style borrows, which this crate's [`FdTable::with`] and
//! [`FdTable::update`] preserve.
//!
//! # The ceiling
//!
//! The state machine itself is ceiling-free; a ceiling is *policy*, and the
//! kernel's 1024 (`MAX_FDS`) is enforced at its call sites
//! (`amd64/src/fd.rs::install`, `sys_dup2`). [`FdTable::free_below`] is the
//! parameterized query for any caller that wants the check where the crate
//! can see it: it answers `None` instead of a number at or above the ceiling.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

/// The fd number → description map: the table's spine.
///
/// A thin newtype over `BTreeMap<u32, D>` that derefs to it, so read-side
/// callers (`/proc/<pid>/fd` renderers, spawn-time stdio checks) keep their
/// plain-map code while the write-side operations live on [`FdTable`].
#[derive(Debug, Clone)]
pub struct Names<D>(BTreeMap<u32, D>);

impl<D> Names<D> {
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    #[must_use]
    pub fn into_inner(self) -> BTreeMap<u32, D> {
        self.0
    }
}

impl<D> Default for Names<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D> core::ops::Deref for Names<D> {
    type Target = BTreeMap<u32, D>;
    fn deref(&self) -> &BTreeMap<u32, D> {
        &self.0
    }
}

impl<D> core::ops::DerefMut for Names<D> {
    fn deref_mut(&mut self) -> &mut BTreeMap<u32, D> {
        &mut self.0
    }
}

/// The set of fd numbers carrying one per-number flag (`FD_CLOEXEC`, or the
/// per-number non-blocking bit).
///
/// Keyed by fd number, not by description — the pinned divergence the module
/// doc records. Newtype over `BTreeSet<u32>` for the same reason [`Names`] is.
#[derive(Debug, Clone, Default)]
pub struct FlagSet(BTreeSet<u32>);

impl FlagSet {
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeSet::new())
    }

    #[must_use]
    pub fn into_inner(self) -> BTreeSet<u32> {
        self.0
    }
}

impl core::ops::Deref for FlagSet {
    type Target = BTreeSet<u32>;
    fn deref(&self) -> &BTreeSet<u32> {
        &self.0
    }
}

impl core::ops::DerefMut for FlagSet {
    fn deref_mut(&mut self) -> &mut BTreeSet<u32> {
        &mut self.0
    }
}

/// One table's three parts, borrowed together: the state machine.
///
/// The kernel holds each part under its own spinlock and locks all three
/// inside one IRQ-masked critical section; this view is what the critical
/// section runs. Every effect outside the table is *returned*: closes hand
/// back the payload to release, forks hand back the child's parts to bump,
/// dups hand back the copied payload to take a reference on.
#[derive(Debug)]
pub struct FdTable<'t, D> {
    names: &'t mut Names<D>,
    cloexec: &'t mut FlagSet,
    nonblock: &'t mut FlagSet,
}

impl<'t, D> FdTable<'t, D> {
    /// Borrow the three parts of one table.
    #[must_use]
    pub fn of(
        names: &'t mut Names<D>,
        cloexec: &'t mut FlagSet,
        nonblock: &'t mut FlagSet,
    ) -> Self {
        Self { names, cloexec, nonblock }
    }

    // ---------- allocation ----------

    /// The lowest free fd number at or above `min_fd`, walking the occupied
    /// numbers in order. Unbounded, exactly as the extracted kernel's
    /// `lowest_available_fd` was: the 1024 ceiling is caller policy
    /// ([`Self::free_below`] is the bounded query).
    #[must_use]
    pub fn lowest_free_from(&self, min_fd: u32) -> u32 {
        let mut fd = min_fd;
        for (&key, _) in self.names.range(min_fd..) {
            if key != fd {
                break;
            }
            fd += 1;
        }
        fd
    }

    /// [`Self::lowest_free_from`] under a ceiling: `None` when every number
    /// in `min_fd..ceiling` is occupied. This is `EMFILE`'s question.
    #[must_use]
    pub fn free_below(&self, min_fd: u32, ceiling: u32) -> Option<u32> {
        let fd = self.lowest_free_from(min_fd);
        (fd < ceiling).then_some(fd)
    }

    /// Allocate the lowest fd number at or above `min_fd` for `entry`.
    ///
    /// Clears both flags for the chosen number **before** the insert makes it
    /// visible: the flags are tracked per raw fd number, so a number a closed
    /// descriptor left marked would otherwise carry the bit to its next
    /// occupant (`docs/archive/NCA_FD_NONBLOCK_TOCTOU.md`). An fd number that
    /// is merely unmarked-but-free keeps that cleanliness; an unrelated fd's
    /// flags are never touched.
    pub fn alloc_fd_from(&mut self, min_fd: u32, entry: D) -> u32 {
        let fd = self.lowest_free_from(min_fd);
        self.nonblock.remove(&fd);
        self.cloexec.remove(&fd);
        self.names.insert(fd, entry);
        fd
    }

    // ---------- dup family ----------

    /// `dup(oldfd)`: insert an independent by-value copy of `oldfd`'s payload
    /// at the lowest free number from 0, with that number's flags cleared.
    ///
    /// Returns the new number and the copied payload — the copy is the
    /// caller's signal to take the kernel-side reference (glue's
    /// `clone_fd_refs`), exactly once, before any close of either name can
    /// run. `None` when `oldfd` names nothing (`EBADF`).
    ///
    /// The non-blocking bit is **not** copied: it is keyed by number and this
    /// is the composition glue's `sys_dup` performs (pinned divergence; see
    /// the module doc).
    pub fn dup(&mut self, old: u32) -> Option<Dup<D>>
    where
        D: Clone,
    {
        let payload = self.names.get(&old)?.clone();
        let fd = self.alloc_fd_from(0, payload.clone());
        Some(Dup { fd, payload })
    }

    /// `dup2`/`dup3`: copy `oldfd`'s payload onto the exact number `newfd`,
    /// displacing whatever occupied it.
    ///
    /// `Err(SameFd)` for `old == new` — glue's `EINVAL`, which the `dup2`
    /// wrapper inherits by delegating to `dup3` (real Linux answers `newfd`
    /// there; the divergence is pinned at the syscall layer, not here).
    /// `Err(BadFd)` when `oldfd` names nothing. Otherwise: the non-blocking
    /// bit is copied old→new (glue's `sys_dup3` rule — the flag follows the
    /// description it dups *onto an occupied number for*), and `cloexec_flag`
    /// (`O_CLOEXEC` in dup3's `flags`) decides the new number's `FD_CLOEXEC`.
    ///
    /// Returns the copied payload — bump its kernel reference before
    /// releasing [`DupTo::displaced`], the order glue uses so a concurrent
    /// close of `oldfd` cannot destroy the object between the two steps.
    pub fn dup_to(&mut self, old: u32, new: u32, cloexec_flag: bool) -> Result<DupTo<D>, DupError>
    where
        D: Clone,
    {
        if old == new {
            return Err(DupError::SameFd);
        }
        let Some(payload) = self.names.get(&old).cloned() else {
            return Err(DupError::BadFd);
        };
        let displaced = self.names.insert(new, payload.clone());
        if self.nonblock.contains(&old) {
            self.nonblock.insert(new);
        } else {
            self.nonblock.remove(&new);
        }
        if cloexec_flag {
            self.cloexec.insert(new);
        } else {
            self.cloexec.remove(&new);
        }
        Ok(DupTo { payload, displaced })
    }

    /// `fcntl(oldfd, F_DUPFD | F_DUPFD_CLOEXEC, min)`: copy `oldfd`'s payload
    /// to the lowest free number at or above `min_fd`. `cloexec_flag` selects
    /// `F_DUPFD_CLOEXEC`'s marking; the non-blocking bit is not copied (same
    /// pinned per-number rule as [`Self::dup`]). `None` is `EBADF`.
    pub fn dup_min(&mut self, old: u32, min_fd: u32, cloexec_flag: bool) -> Option<Dup<D>>
    where
        D: Clone,
    {
        let payload = self.names.get(&old)?.clone();
        let fd = self.alloc_fd_from(min_fd, payload.clone());
        if cloexec_flag {
            self.cloexec.insert(fd);
        }
        Some(Dup { fd, payload })
    }

    // ---------- close family ----------

    /// `close(fd)`: remove the name and both of its flag bits, returning the
    /// payload for the caller to release (the kernel-side reference). `None`
    /// when the number names nothing.
    pub fn close(&mut self, fd: u32) -> Option<D> {
        let payload = self.names.remove(&fd)?;
        self.cloexec.remove(&fd);
        self.nonblock.remove(&fd);
        Some(payload)
    }

    /// `close_all`'s step: pop the lowest occupied number, flags left alone
    /// (harmless — [`Self::alloc_fd_from`] clears whatever it reuses).
    pub fn pop_lowest(&mut self) -> Option<(u32, D)> {
        self.names.pop_first()
    }

    /// `execve`'s close-on-exec sweep: remove every *occupied* number in the
    /// cloexec set, ascending, and clear the set **whole** — including stale
    /// numbers that named nothing. Returns the removed `(fd, payload)` pairs
    /// for release.
    pub fn sweep_cloexec(&mut self) -> Vec<(u32, D)> {
        let marked: Vec<u32> = self.cloexec.iter().copied().collect();
        let mut closed = Vec::new();
        for fd in marked {
            if let Some(payload) = self.names.remove(&fd) {
                closed.push((fd, payload));
            }
        }
        self.cloexec.clear();
        closed
    }

    /// `close_range(first, last)`'s decision: remove every occupied number in
    /// the inclusive range — ascending, flags cleared per number at removal,
    /// the same remove-before-release ordering `sys_close` uses — and return
    /// the payloads for release. `last` is not clamped: numbers beyond it are
    /// simply not occupied, which is the wired behaviour.
    pub fn close_range(&mut self, first: u32, last: u32) -> Vec<(u32, D)> {
        let mut closed = Vec::new();
        for fd in self.occupied_in_range(first, last) {
            if let Some(payload) = self.close(fd) {
                closed.push((fd, payload));
            }
        }
        closed
    }

    /// The occupied numbers in the inclusive range, ascending — the question
    /// `close_range(CLOSE_RANGE_CLOEXEC)` asks before marking each one.
    #[must_use]
    pub fn occupied_in_range(&self, first: u32, last: u32) -> Vec<u32> {
        self.names.range(first..=last).map(|(&fd, _)| fd).collect()
    }

    // ---------- fork ----------

    /// `fork`'s table copy. Copies every name whose payload `keep` accepts —
    /// the kernel's policy is "not an epoll instance", epoll instances being
    /// the one description flavour without references to bump — and copies
    /// both flag sets **wholesale**, stale bits and all (pinned: a bit whose
    /// number was stripped survives into the child, and the next
    /// [`Self::alloc_fd_from`] of that number clears it).
    ///
    /// The returned [`Forked`] names the effects: every payload in
    /// `forked.names` needs exactly one kernel-side reference taken (iterate
    /// the values; the extraction's `clone_fd_refs`), and the parts are the
    /// child's own table, independent of this one from here on.
    pub fn fork_copy(&self, keep: impl Fn(&D) -> bool) -> Forked<D>
    where
        D: Clone,
    {
        let names: BTreeMap<u32, D> = self
            .names
            .iter()
            .filter(|(_, payload)| keep(payload))
            .map(|(&fd, payload)| (fd, payload.clone()))
            .collect();
        Forked {
            names: Names(names),
            cloexec: self.cloexec.clone(),
            nonblock: self.nonblock.clone(),
        }
    }

    // ---------- flags ----------

    pub fn set_cloexec(&mut self, fd: u32) {
        self.cloexec.insert(fd);
    }

    pub fn clear_cloexec(&mut self, fd: u32) {
        self.cloexec.remove(&fd);
    }

    #[must_use]
    pub fn is_cloexec(&self, fd: u32) -> bool {
        self.cloexec.contains(&fd)
    }

    pub fn set_nonblock(&mut self, fd: u32) {
        self.nonblock.insert(fd);
    }

    pub fn clear_nonblock(&mut self, fd: u32) {
        self.nonblock.remove(&fd);
    }

    #[must_use]
    pub fn is_nonblock(&self, fd: u32) -> bool {
        self.nonblock.contains(&fd)
    }

    // ---------- reads ----------

    /// Borrow `fd`'s description for the duration of `f` — the no-clone read
    /// path (`with_fd`), the cure for the deep-clone cost the module doc
    /// records.
    pub fn with<R>(&self, fd: u32, f: impl FnOnce(&D) -> R) -> Option<R> {
        self.names.get(&fd).map(f)
    }

    /// Mutate `fd`'s description in place under `f` (position updates);
    /// `false` when the number names nothing.
    pub fn update(&mut self, fd: u32, f: impl FnOnce(&mut D)) -> bool {
        match self.names.get_mut(&fd) {
            Some(payload) => {
                f(payload);
                true
            }
            None => false,
        }
    }
}

/// What [`FdTable::dup`] and [`FdTable::dup_min`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dup<D> {
    /// The number the copy landed on.
    pub fd: u32,
    /// The copied payload: take the kernel-side reference on it exactly once.
    pub payload: D,
}

/// What [`FdTable::dup_to`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupTo<D> {
    /// The copied payload: take the kernel-side reference on it exactly once,
    /// *before* releasing `displaced` (glue's ordering).
    pub payload: D,
    /// Whatever `newfd` named before, for the caller to release; `None` when
    /// the number was free.
    pub displaced: Option<D>,
}

/// Why [`FdTable::dup_to`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DupError {
    /// `oldfd` names nothing — `EBADF`.
    BadFd,
    /// `old == new` — glue's `EINVAL` for `dup3`, inherited by its `dup2`
    /// (pinned; real Linux answers `newfd` and succeeds).
    SameFd,
}

/// `fork`'s outcome: the child's parts, and the bump effects.
#[derive(Debug)]
pub struct Forked<D> {
    /// The child's name table. Every payload in here needs exactly one
    /// kernel-side reference taken — iterate the values — and none for the
    /// payloads `keep` refused.
    pub names: Names<D>,
    /// The child's cloexec set, copied wholesale (pinned; see
    /// [`FdTable::fork_copy`]).
    pub cloexec: FlagSet,
    /// The child's nonblock set, copied wholesale.
    pub nonblock: FlagSet,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload every test uses: an opaque id, so "the copy is by value"
    /// is visible as two equal ids that the caller must bump separately.
    type Tok = u32;

    fn table() -> (Names<Tok>, FlagSet, FlagSet) {
        (Names::new(), FlagSet::new(), FlagSet::new())
    }

    fn view(parts: &mut (Names<Tok>, FlagSet, FlagSet)) -> FdTable<'_, Tok> {
        FdTable::of(&mut parts.0, &mut parts.1, &mut parts.2)
    }

    /// stdio at 0/1/2, the shape every registered kernel process starts with.
    fn with_stdio(parts: &mut (Names<Tok>, FlagSet, FlagSet)) {
        let mut t = view(parts);
        assert_eq!(t.alloc_fd_from(0, 100), 0);
        assert_eq!(t.alloc_fd_from(0, 101), 1);
        assert_eq!(t.alloc_fd_from(0, 102), 2);
    }

    #[test]
    fn lowest_free_walks_occupied_numbers_and_lands_in_gaps() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        assert_eq!(t.lowest_free_from(0), 3);
        assert_eq!(t.alloc_fd_from(0, 7), 3);
        assert_eq!(t.alloc_fd_from(0, 8), 4);
        assert_eq!(t.close(3), Some(7));
        assert_eq!(t.lowest_free_from(0), 3, "the gap is reused before a fresh number");
        assert_eq!(t.lowest_free_from(4), 5, "4 is occupied; the walk continues past it");
        assert_eq!(t.free_below(0, 5), Some(3));
        for fd in 1..5 {
            t.close(fd);
        }
        assert_eq!(t.free_below(0, 1), None, "only fd 0 is occupied, ceiling 1: full");
        assert_eq!(t.free_below(0, 5), Some(1));
        assert_eq!(t.alloc_fd_from(4, 8), 4);
        assert_eq!(t.free_below(4, 5), None, "4 is occupied, ceiling 5: full");
    }

    #[test]
    fn alloc_clears_the_reused_numbers_stale_flags_only() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        assert_eq!(t.alloc_fd_from(0, 7), 3);
        t.set_cloexec(3);
        t.set_nonblock(3);
        assert_eq!(t.close(3), Some(7));
        // A `sys_close` that forgot the flag half of its job must not leak
        // into the next occupant (NCA_FD_NONBLOCK_TOCTOU's guarantee).
        assert_eq!(t.alloc_fd_from(0, 8), 3);
        assert!(!t.is_cloexec(3));
        assert!(!t.is_nonblock(3));
        // fd 1's own nonblock is none of fd 3's business.
        t.set_nonblock(1);
        t.alloc_fd_from(0, 9);
        assert!(t.is_nonblock(1));
    }

    #[test]
    fn dup_copies_by_value_to_the_lowest_number_without_the_nonblock_bit() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        assert_eq!(t.alloc_fd_from(0, 7), 3);
        t.set_nonblock(3);
        let d = t.dup(3).expect("dup of an open fd");
        assert_eq!(d.fd, 4, "the lowest free number");
        assert_eq!(d.payload, 7, "the copy, for the caller's reference bump");
        assert!(!t.is_nonblock(4), "a fresh number has no stale bits");
        t.set_nonblock(3);
        let d = t.dup(3).expect("dup again");
        assert_eq!(d.fd, 5);
        assert!(!t.is_nonblock(5), "PINNED DIVERGENCE: the dup does not inherit the flag");
        assert!(t.dup(99).is_none(), "EBADF");
    }

    #[test]
    fn dup_to_pins_same_fd_and_bad_fd_then_displaces_and_copies_flags() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        assert_eq!(t.alloc_fd_from(0, 7), 3);
        assert_eq!(t.alloc_fd_from(5, 55), 5);
        t.set_nonblock(3);
        t.set_cloexec(5);

        assert_eq!(t.dup_to(3, 3, false), Err(DupError::SameFd), "glue's EINVAL, pinned");
        assert_eq!(t.dup_to(99, 5, false), Err(DupError::BadFd));

        let out = t.dup_to(3, 5, true).expect("dup3 with O_CLOEXEC");
        assert_eq!(out.payload, 7);
        assert_eq!(out.displaced, Some(55), "fd 5's occupant comes back for release");
        assert!(t.is_cloexec(5), "O_CLOEXEC marks the new number");
        assert!(t.is_nonblock(5), "dup3 copies the nonblock bit old→new");
        t.set_nonblock(3);
        let out = t.dup_to(3, 6, false).expect("dup2 without O_CLOEXEC");
        assert_eq!(out.displaced, None, "an empty target has nothing to release");
        assert!(!t.is_cloexec(6) && t.is_nonblock(6));
    }

    #[test]
    fn dup_min_honours_the_floor_and_f_dupfd_cloexec() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        assert_eq!(t.alloc_fd_from(0, 7), 3);
        let d = t.dup_min(3, 10, false).expect("F_DUPFD");
        assert_eq!(d.fd, 10);
        assert!(!t.is_cloexec(10), "plain F_DUPFD leaves the bit clear");
        let d = t.dup_min(3, 4, true).expect("F_DUPFD_CLOEXEC");
        assert_eq!(d.fd, 4, "the floor is a floor, not a fixed address");
        assert!(t.is_cloexec(4));
        assert!(t.dup_min(99, 0, false).is_none(), "EBADF");
    }

    #[test]
    fn close_removes_name_and_flags_and_answers_none_when_absent() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        assert_eq!(t.alloc_fd_from(0, 7), 3);
        t.set_cloexec(3);
        assert_eq!(t.close(3), Some(7));
        assert!(t.close(3).is_none(), "double close is EBADF, not a second payload");
        assert!(!t.is_cloexec(3));
        assert!(t.close(42).is_none());
    }

    #[test]
    fn pop_lowest_drains_ascending_with_flags_left_alone() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        t.alloc_fd_from(0, 7);
        t.set_cloexec(3);
        assert_eq!(t.pop_lowest(), Some((0, 100)));
        assert_eq!(t.pop_lowest(), Some((1, 101)));
        assert!(t.is_cloexec(3), "close_all leaves flags; alloc clears what it reuses");
        for _ in [2, 3] {
            t.pop_lowest();
        }
        assert_eq!(t.pop_lowest(), None);
    }

    #[test]
    fn sweep_returns_occupied_marked_numbers_ascending_and_clears_the_set_whole() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        t.alloc_fd_from(0, 7); // 3
        t.alloc_fd_from(0, 8); // 4
        t.set_cloexec(3);
        t.set_cloexec(4);
        t.set_cloexec(9); // a number nothing occupies
        t.close(3); // closed before the exec
        let swept = t.sweep_cloexec();
        assert_eq!(swept, [(4, 8)], "only what is present, in ascending order");
        assert!(t.is_cloexec(4) || t.cloexec.is_empty(), "sentinel is unreachable");
        assert!(t.cloexec.is_empty(), "the set is cleared whole, stale numbers included");
    }

    #[test]
    fn fork_copy_applies_the_policy_and_copies_flag_sets_wholesale() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        t.alloc_fd_from(0, 7); // 3 = "the epoll instance" under the policy below
        t.alloc_fd_from(0, 8); // 4
        t.set_cloexec(3);
        t.set_cloexec(4);
        t.set_nonblock(4);

        let forked = t.fork_copy(|tok| *tok != 7);
        assert!(!forked.names.contains_key(&3), "the policy stripped fd 3");
        assert!(forked.names.contains_key(&4));
        // Wholesale, stale bit and all — pinned by akuma-exec's
        // characterization suite; alloc_fd_from clears what it reuses.
        assert!(forked.cloexec.contains(&3) && forked.cloexec.contains(&4));
        assert!(forked.nonblock.contains(&4) && !forked.nonblock.contains(&3));
        // The child is its own table.
        let mut child_parts = (forked.names, forked.cloexec, forked.nonblock);
        let mut child = view(&mut child_parts);
        assert_eq!(child.close(4), Some(8));
        assert!(t.with(4, |tok| *tok).is_some(), "parent untouched by the child's close");
    }

    #[test]
    fn close_range_and_occupied_in_range_cover_the_inclusive_span() {
        let mut parts = table();
        with_stdio(&mut parts);
        let mut t = view(&mut parts);
        t.alloc_fd_from(0, 7); // 3
        t.alloc_fd_from(0, 8); // 4
        t.set_nonblock(3);
        t.set_cloexec(3);
        assert_eq!(t.occupied_in_range(2, 3), [2, 3]);
        assert_eq!(t.occupied_in_range(9, 100), Vec::<u32>::new());
        let closed = t.close_range(1, 3);
        assert_eq!(closed, [(1, 101), (2, 102), (3, 7)]);
        assert!(!t.is_nonblock(3) && !t.is_cloexec(3), "each closed number loses its flags");
        assert!(t.with(4, |tok| *tok).is_some(), "outside the range survives");
    }

    /// The same operation sequence driven through the state machine and
    /// through a naive re-statement of the kernel's compositions (the shape
    /// `akuma-syscalls-glue` spells at its call sites), comparing the chosen
    /// numbers, the effects and both sides' whole observable state after every
    /// step. A deterministic LCG instead of a rng dependency.
    #[test]
    fn differential_against_the_kernel_composition_model() {
        #[derive(Default)]
        struct Model {
            names: BTreeMap<u32, Tok>,
            cloexec: BTreeSet<u32>,
            nonblock: BTreeSet<u32>,
        }
        impl Model {
            fn lowest(&self, min: u32) -> u32 {
                let mut fd = min;
                for (&k, _) in self.names.range(min..) {
                    if k != fd {
                        break;
                    }
                    fd += 1;
                }
                fd
            }
            fn alloc(&mut self, min: u32, tok: Tok) -> u32 {
                let fd = self.lowest(min);
                self.nonblock.remove(&fd);
                self.cloexec.remove(&fd);
                self.names.insert(fd, tok);
                fd
            }
        }

        let mut parts = table();
        let mut model = Model::default();
        let mut t = view(&mut parts);
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;

        let mut next = move |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };

        for _ in 0..2_000 {
            let op = next(6);
            match op {
                0 | 1 => {
                    let min = next(8) as u32;
                    let tok = 1000 + next(50) as u32;
                    assert_eq!(t.alloc_fd_from(min, tok), model.alloc(min, tok));
                }
                2 => {
                    let old = next(12) as u32;
                    let got = t.dup(old).map(|d| (d.fd, d.payload));
                    let want = model.names.get(&old).copied().map(|tok| {
                        (model.alloc(0, tok), tok)
                    });
                    assert_eq!(got, want);
                }
                3 => {
                    let old = next(12) as u32;
                    let new = next(12) as u32;
                    let cx = next(2) == 0;
                    let got = t.dup_to(old, new, cx).map(|o| (o.payload, o.displaced));
                    let want: Result<(Tok, Option<Tok>), DupError> = if old == new {
                        Err(DupError::SameFd)
                    } else {
                        match model.names.get(&old).copied() {
                            None => Err(DupError::BadFd),
                            Some(tok) => {
                                if model.nonblock.contains(&old) {
                                    model.nonblock.insert(new);
                                } else {
                                    model.nonblock.remove(&new);
                                }
                                if cx {
                                    model.cloexec.insert(new);
                                } else {
                                    model.cloexec.remove(&new);
                                }
                                let displaced = model.names.insert(new, tok);
                                Ok((tok, displaced))
                            }
                        }
                    };
                    assert_eq!(got, want);
                }
                4 => {
                    let fd = next(12) as u32;
                    assert_eq!(t.close(fd), model.names.remove(&fd));
                    model.nonblock.remove(&fd);
                    model.cloexec.remove(&fd);
                }
                _ => {
                    let swept = t.sweep_cloexec();
                    let want: Vec<(u32, Tok)> = model
                        .cloexec
                        .iter()
                        .copied()
                        .filter_map(|fd| model.names.remove(&fd).map(|tok| (fd, tok)))
                        .collect();
                    model.cloexec.clear();
                    assert_eq!(swept, want);
                }
            }
            assert_eq!(&**t.names, &model.names, "names diverged after op {op}");
            assert_eq!(&**t.cloexec, &model.cloexec, "cloexec diverged after op {op}");
            assert_eq!(&**t.nonblock, &model.nonblock, "nonblock diverged after op {op}");
        }
    }
}
