use alloc::sync::Arc;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use akuma_fdtable::{FdTable, FlagSet, Names};

use crate::process::types::FileDescriptor;
use crate::process::children::remove_child_channel;
use crate::runtime::{runtime, with_irqs_disabled};
use super::Process;

/// One process's descriptor table, and the one authority on which fd number
/// names what.
///
/// The state machine — lowest-free allocation, the dup family's numbering and
/// flag rules, the close-on-exec sweep, fork's copy with the epoll-stripping
/// policy — lives in [`akuma_fdtable`] and is host-tested there; this type is
/// its locked, kernel-side half: the three spinlocks, the `FileDescriptor`
/// payload, and the reference effects the crate *returns* which the kernel
/// *performs* ([`clone_fd_refs`] on copies, [`release_fd_entry`] on removals).
/// The name table and both flag sets deref to their plain collections, so
/// read-side callers keep their map/set code.
pub struct SharedFdTable {
    pub table: Spinlock<Names<FileDescriptor>>,
    pub cloexec: Spinlock<FlagSet>,
    pub nonblock: Spinlock<FlagSet>,
}

/// Borrow the table's three parts as one state machine.
///
/// The caller must already be inside its IRQ-masked critical section; every
/// `SharedFdTable` operation takes all three locks together for exactly the
/// span of the crate call it wraps.
macro_rules! fd_view {
    ($table:expr, $cloexec:expr, $nonblock:expr) => {
        FdTable::of(&mut *$table.lock(), &mut *$cloexec.lock(), &mut *$nonblock.lock())
    };
}

/// Take a second reference to whatever `entry` refers to.
///
/// **Call this from every site that copies a `FileDescriptor` into another
/// slot** — `fork`, `dup`, `dup3`, `fcntl(F_DUPFD)`. A copy that skips it is a
/// bare alias, and the *first* `close` of either slot destroys the object under
/// the other one.
///
/// # Why this is one function
///
/// It used to be four. This list lived here, inside `clone_deep_for_fork`, and
/// `sys_dup` / `sys_dup3` / `sys_fcntl(F_DUPFD)` each carried their own
/// four-arm copy handling only `PipeWrite`, `PipeRead`, `UnixSocket` and
/// `Socket`. Both refcounted families added later — `EventFd` and `RumpSocket`
/// — were added *here* and to none of the copies, so `dup(eventfd)` and
/// `dup(rump_socket)` produced unreferenced aliases.
///
/// The `RumpSocket` arm below records what that costs: the same omission on the
/// fork path destroyed the accepted socket under sshd's forked child and killed
/// every session at kex on the rump devbox
/// (`docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`). It was found,
/// fixed here, and never propagated to the three syscall copies — found again
/// 2026-08-24 (`docs/archive/SYSCALL_LAYER_AUDIT.md`).
///
/// Adding a refcounted `FileDescriptor` variant means adding an arm here, and
/// only here.
pub fn clone_fd_refs(entry: &FileDescriptor) {
    match entry {
        FileDescriptor::PipeWrite(id) => (runtime().pipe_clone_ref)(*id, true),
        FileDescriptor::PipeRead(id) => (runtime().pipe_clone_ref)(*id, false),
        FileDescriptor::UnixSocket { rx, tx, sock } => {
            (runtime().pipe_clone_ref)(*rx, false);
            (runtime().pipe_clone_ref)(*tx, true);
            // The socket table entry is refcounted alongside the pipes: the
            // copy is a real reference, and without this the first close would
            // destroy the name binding and the record boundaries under the
            // other still-open fd.
            (runtime().unix_sock_clone_ref)(*sock);
        }
        FileDescriptor::EventFd(id) => (runtime().eventfd_clone_ref)(*id),
        FileDescriptor::PtyMaster(n) => (runtime().pty_clone_ref)(*n, true),
        FileDescriptor::PtySlave(n) => (runtime().pty_clone_ref)(*n, false),
        // Sockets are refcounted like pipes: an fd-table copy is a real
        // reference, so the first close (child exit / exec cloexec sweep /
        // close of a dup) must not destroy the socket under the live fd.
        FileDescriptor::Socket(idx) => (runtime().socket_clone_ref)(*idx),
        // Rump sockets need the same reference the native ones take, and for
        // the same reason — but they were the one refcounted family missing
        // from this list, so the copy was a bare alias. sshd's
        // process-per-session pattern is exactly the shape that breaks on
        // that: the parent `drop`s its copy of the accepted socket right
        // after `fork`, expecting the refcount to keep the child's alive,
        // and instead `proxy_close` sent a real NetBSD `close(rump_fd)` to
        // `rump_server` and destroyed the socket the child was about to
        // speak SSH over. Every session died at kex on the rump devbox
        // (`docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`).
        FileDescriptor::RumpSocket { rump_fd, box_id, .. } => {
            (runtime().rump_socket_clone_ref)(*box_id, *rump_fd);
        }
        _ => {}
    }
}

impl SharedFdTable {
    pub const fn new() -> Self {
        Self {
            table: Spinlock::new(Names::new()),
            cloexec: Spinlock::new(FlagSet::new()),
            nonblock: Spinlock::new(FlagSet::new()),
        }
    }

    pub fn with_stdio() -> Self {
        let mut fd_map = Names::new();
        fd_map.insert(0, FileDescriptor::Stdin);
        fd_map.insert(1, FileDescriptor::Stdout);
        fd_map.insert(2, FileDescriptor::Stderr);
        Self {
            table: Spinlock::new(fd_map),
            cloexec: Spinlock::new(FlagSet::new()),
            nonblock: Spinlock::new(FlagSet::new()),
        }
    }

    /// Deep copy for fork (separate fd table, with pipe ref bumps).
    /// Strips EpollFd entries since epoll instances are not reference-counted.
    ///
    /// The copy decision and the flag handling are [`akuma_fdtable`]'s
    /// [`FdTable::fork_copy`]; the epoll-stripping rule is this kernel's
    /// policy, stated here as the `keep` predicate. The child's flag sets
    /// come back copied wholesale — a stripped entry's bit rides along,
    /// harmlessly, until an `alloc_fd_from` of its number clears it.
    ///
    /// The per-entry reference bumps are [`clone_fd_refs`] — shared with `dup`,
    /// `dup3` and `fcntl(F_DUPFD)`, which used to carry their own copies of the
    /// list and had drifted. See that function's header.
    #[must_use]
    pub fn clone_deep_for_fork(&self) -> Self {
        let forked = with_irqs_disabled(|| {
            fd_view!(self.table, self.cloexec, self.nonblock)
                .fork_copy(|fd| !matches!(fd, FileDescriptor::EpollFd(_)))
        });
        for entry in forked.names.values() {
            clone_fd_refs(entry);
        }
        Self {
            table: Spinlock::new(forked.names),
            cloexec: Spinlock::new(forked.cloexec),
            nonblock: Spinlock::new(forked.nonblock),
        }
    }

    /// Atomically reserve `count` bytes of file position on a plain (non-`O_APPEND`)
    /// `File` fd and return the reservation's starting offset — i.e. read the current
    /// position and advance it past the reservation in one lock hold, instead of two
    /// separate ones with disk I/O in between.
    ///
    /// `sys_write` used to read `.position` via [`Process::get_fd`] (a clone of the
    /// table entry), perform the actual disk write using that snapshot, and only
    /// write the new position back afterward via [`Process::update_fd`]. Both of
    /// those individual steps are lock-protected, but the *sequence* is not: two
    /// threads sharing this fd (`CLONE_FILES`, e.g. any pair of `clone_thread`
    /// siblings) racing `write()` could both clone the same not-yet-advanced
    /// `.position`, both write to the same on-disk offset, and corrupt each other's
    /// data — reproduced directly with a raw multi-thread `write()` probe (no
    /// userspace locking involved, so the race is entirely in this gap). See
    /// `docs/archive/CONCURRENT_WRITE_POSITION_RACE.md`. Returns `None` for a
    /// non-`File` fd, a missing fd, or an `O_APPEND` fd (append's position is
    /// derived from the live file size per write, a separate, pre-existing race
    /// this does not address — see the call site).
    ///
    /// Once reserved, the range belongs to this call alone: even a short/failed
    /// write must never move `.position` backward to "give back" the unused tail,
    /// since a concurrent writer may already have reserved past it — the correct
    /// on-disk result of a partial write here is a sparse hole, not a rewound
    /// position that reopens this exact race. [`reserve_write_pos_tests`] covers
    /// both the single-caller and the concurrent-callers case directly.
    pub fn reserve_write_pos(&self, fd: u32, count: usize) -> Option<usize> {
        with_irqs_disabled(|| {
            let mut table = self.table.lock();
            match table.get_mut(&fd) {
                Some(FileDescriptor::File(file))
                    if file.flags & crate::process::types::open_flags::O_APPEND == 0 =>
                {
                    let pos = file.position;
                    file.position = pos.saturating_add(count);
                    Some(pos)
                }
                _ => None,
            }
        })
    }

    /// Allocate the lowest fd number >= `min_fd` and insert `entry` at it,
    /// atomically. The decision — lowest-free walk and the stale-flag clearing
    /// that makes a reused number start clean — is [`akuma_fdtable`]'s
    /// [`FdTable::alloc_fd_from`], which carries the full history of both
    /// guarantees; the tests below pin them through this locked wrapper.
    pub fn alloc_fd_from(&self, min_fd: u32, entry: FileDescriptor) -> u32 {
        with_irqs_disabled(|| fd_view!(self.table, self.cloexec, self.nonblock).alloc_fd_from(min_fd, entry))
    }

    /// Explicitly close all underlying kernel resources and clear the table.
    /// This is used during process exit to ensure immediate cleanup.
    ///
    /// Entries are popped and closed ONE AT A TIME, not snapshot-then-cleared.
    /// The executing thread can be abandoned mid-sweep: a thread marked
    /// TERMINATED while running its own teardown is never rescheduled after the
    /// next preemption, and its kernel stack is recycled. With the old
    /// snapshot+clear, every not-yet-closed entry had already left the table, so
    /// no later pass (the `Drop` below, `cleanup_process_fds` at reclaim) could
    /// ever find it — the refcounts leaked for good. Measured 2026-08-07: a
    /// wrongly-reaped `ld` closed exactly one of its four pipe refs and was then
    /// descheduled forever; the leaked stderr write refcount kept `rustc` blocked
    /// in `read()` waiting for an EOF that could no longer arrive (the `-j4`
    /// self-host hang). Popping per-iteration bounds an abandoned sweep's damage
    /// to the single in-flight entry.
    pub fn close_all(&self) {
        let holder = self as *const Self as usize;
        loop {
            let entry = with_irqs_disabled(|| self.table.lock().pop_first());
            let Some((fd_num, fd)) = entry else { break };
            release_fd_entry(fd_num, fd, holder);
        }
    }
}

/// Drop the one reference `fd_num` held, by variant.
///
/// Factored out of [`SharedFdTable::close_all`] so the **exec close-on-exec
/// sweep** can release through the same list rather than a second copy of it.
/// A second copy is exactly how this went wrong once already: the AArch64
/// execve in `akuma-syscalls-glue` spells its own arms inline, and the amd64
/// execve — which had no sweep at all — could not borrow them.
///
/// `holder` identifies the fd table for `flock_release`; pass the
/// `&SharedFdTable` as `*const _ as usize`, which is what `close_all` does.
pub fn release_fd_entry(fd_num: u32, fd: FileDescriptor, holder: usize) {
    match fd {
        FileDescriptor::File(f) => {
            (runtime().flock_release)(&f.path, holder, fd_num);
        }
        FileDescriptor::Socket(idx) => {
            (runtime().remove_socket)(idx);
        }
        FileDescriptor::ChildStdout(child_pid) => {
            remove_child_channel(child_pid);
        }
        FileDescriptor::PipeWrite(pipe_id) => {
            (runtime().pipe_close_write)(pipe_id);
        }
        FileDescriptor::PipeRead(pipe_id) => {
            (runtime().pipe_close_read)(pipe_id);
        }
        FileDescriptor::UnixSocket { rx, tx, sock } => {
            (runtime().pipe_close_read)(rx);
            (runtime().pipe_close_write)(tx);
            (runtime().unix_sock_close)(sock);
        }
        FileDescriptor::EventFd(efd_id) => {
            (runtime().eventfd_close)(efd_id);
        }
        FileDescriptor::EpollFd(epoll_id) => {
            (runtime().epoll_destroy)(epoll_id);
        }
        FileDescriptor::PidFd(pidfd_id) => {
            (runtime().pidfd_close)(pidfd_id);
        }
        FileDescriptor::DevDsp => {
            (runtime().dsp_close)();
        }
        FileDescriptor::PtyMaster(n) => {
            (runtime().pty_close)(n, true);
        }
        FileDescriptor::PtySlave(n) => {
            (runtime().pty_close)(n, false);
        }
        _ => {}
    }
}

impl Drop for SharedFdTable {
    fn drop(&mut self) {
        self.close_all();
    }
}

impl Process {
    // ========== File Descriptor Table Methods ==========

    /// Allocate the lowest available fd number and insert the entry atomically.
    pub fn alloc_fd(&self, entry: FileDescriptor) -> u32 {
        self.alloc_fd_from(0, entry)
    }

    /// Allocate the lowest available fd number >= `min_fd` and insert the entry.
    /// Used by `fcntl(F_DUPFD)` which specifies a minimum fd. See
    /// [`SharedFdTable::alloc_fd_from`] for the flag-staleness guarantee this
    /// provides and why it lives there now.
    pub fn alloc_fd_from(&self, min_fd: u32, entry: FileDescriptor) -> u32 {
        self.fds.alloc_fd_from(min_fd, entry)
    }

    /// Run `f` against the fd table's live entry, in place — no clone.
    ///
    /// `get_fd` hands back a full deep clone of the [`FileDescriptor`], and a
    /// `KernelFile` can carry a directory snapshot (`dir_cache`) of ~100 k
    /// heap `String`s — one `get_fd` on a directory fd during `getdents64`
    /// cost ~60 ms of pure allocation per syscall even after the arm itself
    /// stopped cloning. Callers that only *read* (or mutate via
    /// [`Self::update_fd`]) an entry must go through here.
    pub fn with_fd<R>(&self, fd: u32, f: impl FnOnce(&FileDescriptor) -> R) -> Option<R> {
        with_irqs_disabled(|| {
            let table = self.fds.table.lock();
            table.get(&fd).map(f)
        })
    }

    /// Get a file descriptor entry (cloned)
    pub fn get_fd(&self, fd: u32) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fds.table.lock().get(&fd).cloned()
        })
    }

    /// Remove and return a file descriptor entry
    pub fn remove_fd(&self, fd: u32) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fds.table.lock().remove(&fd)
        })
    }

    /// Set a file descriptor entry at a specific FD number, replacing any existing entry
    pub fn set_fd(&self, fd: u32, entry: FileDescriptor) {
        with_irqs_disabled(|| {
            self.fds.table.lock().insert(fd, entry);
        });
    }

    /// Atomically replace a file descriptor, returning the old entry if one existed.
    /// Use this instead of get_fd + set_fd when you need to close the old entry,
    /// to avoid a TOCTOU race on shared fd tables (CLONE_FILES).
    pub fn swap_fd(&self, fd: u32, entry: FileDescriptor) -> Option<FileDescriptor> {
        with_irqs_disabled(|| {
            self.fds.table.lock().insert(fd, entry)
        })
    }

    /// Update a file descriptor entry (for file position updates, etc.)
    pub fn update_fd<F>(&self, fd: u32, f: F) -> bool
    where
        F: FnOnce(&mut FileDescriptor),
    {
        with_irqs_disabled(|| {
            let mut table = self.fds.table.lock();
            if let Some(entry) = table.get_mut(&fd) {
                f(entry);
                true
            } else {
                false
            }
        })
    }

    /// See [`SharedFdTable::reserve_write_pos`] — thin delegator so `sys_write` can
    /// call it the same way it calls every other per-fd `Process` method.
    pub fn reserve_write_pos(&self, fd: u32, count: usize) -> Option<usize> {
        self.fds.reserve_write_pos(fd, count)
    }

    pub fn set_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).set_cloexec(fd);
        });
    }

    pub fn clear_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).clear_cloexec(fd);
        });
    }

    pub fn is_cloexec(&self, fd: u32) -> bool {
        with_irqs_disabled(|| fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).is_cloexec(fd))
    }

    pub fn set_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).set_nonblock(fd);
        });
    }

    pub fn clear_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).clear_nonblock(fd);
        });
    }

    pub fn is_nonblock(&self, fd: u32) -> bool {
        with_irqs_disabled(|| fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).is_nonblock(fd))
    }

    /// Close all FDs marked close-on-exec, returning them for cleanup.
    /// The sweep's decision — which numbers, in which order, and the
    /// whole-set clear that takes stale numbers with it — is
    /// [`akuma_fdtable`]'s [`FdTable::sweep_cloexec`].
    pub fn close_cloexec_fds(&self) -> Vec<(u32, FileDescriptor)> {
        with_irqs_disabled(|| {
            fd_view!(self.fds.table, self.fds.cloexec, self.fds.nonblock).sweep_cloexec()
        })
    }

    /// Close every close-on-exec descriptor **and drop the references they
    /// held** — the whole of what POSIX `execve` owes its fd table.
    ///
    /// [`Self::close_cloexec_fds`] only removes the *names*; the caller is then
    /// obliged to release each entry, and a caller that forgets leaks one
    /// reference per fd. That is not a hypothetical: the amd64 `execve` had no
    /// sweep at all, so a child's `FD_CLOEXEC` pipe write end survived the exec
    /// and its reader never saw EOF. `git` hangs on exactly that — it closes
    /// the parent's end of `start_command`'s notify pipe and blocks in
    /// `read()`, waiting for the EOF that a successful exec is supposed to
    /// produce. See `docs/archive/AMD64_TRASHCAN_ISSUES.md` §1.
    ///
    /// Call this at the POSIX point of no return — **after** the image is
    /// committed, never before: a failed `execve` must leave the fd table
    /// untouched, or a libstd fork+exec child loses the very pipe it would
    /// report the failure on.
    pub fn close_cloexec_fds_releasing(&self) {
        let holder = Arc::as_ptr(&self.fds) as usize;
        for (fd_num, entry) in self.close_cloexec_fds() {
            release_fd_entry(fd_num, entry, holder);
        }
    }

    /// Get a reference to the shared fd table (for direct access in sys_close_range, etc.)
    pub fn fd_table(&self) -> &Arc<SharedFdTable> {
        &self.fds
    }
}

/// Regression coverage for `docs/archive/CONCURRENT_WRITE_POSITION_RACE.md` — the
/// TOCTOU race where `sys_write` used to read `.position` from a `get_fd()` clone,
/// do the disk write, and only write the advanced position back afterward, letting
/// two threads racing `write()` on the same fd both read the same stale position
/// and overlap on disk. `reserve_write_pos` closes it by making read-and-advance one
/// locked operation; these tests exercise that directly against `SharedFdTable`; the
/// bug itself was only ever observable end-to-end (bytes on disk), so a raw
/// multi-thread `write()` reproduction lives in that doc, not here — this covers the
/// reservation *logic* the fix actually changed.
#[cfg(test)]
mod reserve_write_pos_tests {
    use super::*;
    use crate::process::types::{KernelFile, open_flags};

    fn table_with_file(fd: u32, flags: u32) -> SharedFdTable {
        let t = SharedFdTable::new();
        t.table.lock().insert(fd, FileDescriptor::File(KernelFile::new("/x".into(), flags)));
        t
    }

    #[test]
    fn sequential_reservations_never_overlap() {
        let t = table_with_file(3, open_flags::O_WRONLY);
        // Three single-threaded reservations of different sizes must tile the file
        // exactly: each one starts where the last one ended, with no gap and no
        // overlap — the property the old clone-then-write-back sequence could
        // violate under concurrency, verified here in the simple non-racing case.
        assert_eq!(t.reserve_write_pos(3, 10), Some(0));
        assert_eq!(t.reserve_write_pos(3, 25), Some(10));
        assert_eq!(t.reserve_write_pos(3, 1), Some(35));
        assert_eq!(t.table.lock().get(&3).map(|e| match e {
            FileDescriptor::File(f) => f.position,
            _ => unreachable!(),
        }), Some(36));
    }

    #[test]
    fn append_fd_is_left_to_the_caller() {
        // O_APPEND is explicitly out of scope (its position comes from a live
        // file-size query, not `.position` — see the doc comment); the reservation
        // must refuse rather than silently do the wrong thing for it.
        let t = table_with_file(4, open_flags::O_WRONLY | open_flags::O_APPEND);
        assert_eq!(t.reserve_write_pos(4, 100), None);
    }

    #[test]
    fn missing_or_non_file_fd_returns_none() {
        let t = SharedFdTable::new(); // fd 5 was never inserted
        assert_eq!(t.reserve_write_pos(5, 10), None);
        t.table.lock().insert(6, FileDescriptor::Stdout);
        assert_eq!(t.reserve_write_pos(6, 10), None);
    }

    #[test]
    fn zero_length_reservation_is_a_true_no_op() {
        let t = table_with_file(7, open_flags::O_WRONLY);
        assert_eq!(t.reserve_write_pos(7, 0), Some(0));
        assert_eq!(t.reserve_write_pos(7, 0), Some(0)); // position did not move
        assert_eq!(t.reserve_write_pos(7, 5), Some(0));
    }

    /// The actual regression test: hammer one fd with many concurrent, real
    /// `std::thread`s each reserving a fixed-size chunk with no coordination
    /// beyond `reserve_write_pos` itself, and prove the reserved ranges tile the
    /// file perfectly — no two threads ever got an overlapping offset, and no
    /// byte range was skipped. This is exactly the invariant whose absence let
    /// `sys_write` corrupt concurrent writers' data before the fix: before it,
    /// the equivalent "read position, unlock, do work, relock, write position"
    /// sequence would have produced duplicate/overlapping starting offsets under
    /// this same contention.
    #[test]
    fn concurrent_reservations_tile_without_overlap() {
        use std::sync::Arc as StdArc;
        use std::thread;

        const THREADS: usize = 16;
        const RESERVES_PER_THREAD: usize = 500;
        const CHUNK: usize = 17; // odd size so a miscomputed overlap isn't hidden by alignment

        let table = StdArc::new(table_with_file(1, open_flags::O_WRONLY));
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let table = StdArc::clone(&table);
            handles.push(thread::spawn(move || {
                let mut starts = alloc::vec::Vec::with_capacity(RESERVES_PER_THREAD);
                for _ in 0..RESERVES_PER_THREAD {
                    starts.push(table.reserve_write_pos(1, CHUNK).expect("fd 1 is a plain File"));
                }
                starts
            }));
        }

        let mut all_starts: alloc::vec::Vec<usize> =
            handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all_starts.sort_unstable();

        // Every reserved range is CHUNK bytes wide; sorted starts must be an exact
        // arithmetic progression with no repeats (overlap) and no gaps (a lost
        // reservation — e.g. two threads both getting the same start would also
        // show up here as a repeat, and a dropped final `file.position` write
        // would show up as a gap at the very end).
        let total = THREADS * RESERVES_PER_THREAD;
        assert_eq!(all_starts.len(), total);
        for (i, &start) in all_starts.iter().enumerate() {
            assert_eq!(start, i * CHUNK, "reservation {} did not tile exactly — overlap or gap", i);
        }
        let final_pos = table.table.lock().get(&1).map(|e| match e {
            FileDescriptor::File(f) => f.position,
            _ => unreachable!(),
        });
        assert_eq!(final_pos, Some(total * CHUNK));
    }
}

/// Regression coverage for `docs/archive/NCA_FD_NONBLOCK_TOCTOU.md` — the fd-number
/// reuse race where `nonblock`/`cloexec`, tracked per raw fd number rather than per
/// underlying pipe/file object, could leak from a closed fd's stale flag onto whatever
/// a concurrent `alloc_fd` handed out next. Exercised directly against
/// [`SharedFdTable::alloc_fd_from`], the same way `reserve_write_pos_tests` covers its
/// sibling method — the bug itself was only ever observable end-to-end (a real `rustc`
/// child hitting `EAGAIN` on what should be a blocking pipe read), not reproducible as
/// a unit test on its own.
#[cfg(test)]
mod alloc_fd_tests {
    use super::*;

    #[test]
    fn reused_fd_number_does_not_inherit_a_stale_nonblock_flag() {
        let t = SharedFdTable::new();
        for fd in 0..5 {
            t.table.lock().insert(fd, FileDescriptor::Stdin);
        }
        // Simulate the *old*, buggy `sys_close`: the table slot is freed, but the
        // nonblock bit — which used to be cleared only after a whole match arm of
        // cleanup syscalls — is deliberately left set, exactly the window the fix
        // closes from the allocation side.
        t.table.lock().insert(5, FileDescriptor::Stdout);
        t.nonblock.lock().insert(5);
        t.cloexec.lock().insert(5);
        t.table.lock().remove(&5);

        let fd = t.alloc_fd_from(0, FileDescriptor::Stderr);
        assert_eq!(fd, 5, "test assumes fd 5 is the lowest free slot");
        assert!(!t.nonblock.lock().contains(&fd), "fresh fd must not inherit the stale nonblock bit");
        assert!(!t.cloexec.lock().contains(&fd), "fresh fd must not inherit the stale cloexec bit");
    }

    #[test]
    fn allocation_does_not_disturb_other_fds_flags() {
        let t = SharedFdTable::new();
        t.table.lock().insert(0, FileDescriptor::Stdin);
        t.nonblock.lock().insert(0); // fd 0 legitimately non-blocking — must survive
        let fd = t.alloc_fd_from(1, FileDescriptor::Stdout);
        assert_eq!(fd, 1);
        assert!(t.nonblock.lock().contains(&0), "an unrelated fd's flag must not be touched");
    }

    /// The actual regression test: a "closer" thread repeatedly frees fd 3 without
    /// ever clearing its `nonblock` bit itself (the old `sys_close`'s exact
    /// vulnerable window, held open deliberately with a `yield_now` to widen the
    /// race), racing an "allocator" thread that repeatedly reuses that same slot —
    /// fds 0-2 are permanently occupied so fd 3 is always the next fd `alloc_fd_from`
    /// hands out once freed. Every single time the allocator receives fd 3, its
    /// nonblock bit must already read false — proving the guarantee holds under
    /// real concurrent hammering, not just the single-threaded case above.
    #[test]
    fn concurrent_reuse_never_observes_a_stale_nonblock_flag() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        const ROUNDS: usize = 20_000;

        let table = StdArc::new(SharedFdTable::new());
        for fd in 0..3 {
            table.table.lock().insert(fd, FileDescriptor::Stdin);
        }
        // The closer starts by taking fd 3 for the allocator to eventually free again.
        table.table.lock().insert(3, FileDescriptor::Stdout);
        table.nonblock.lock().insert(3);

        let closer_table = StdArc::clone(&table);
        let closer = thread::spawn(move || {
            for _ in 0..ROUNDS {
                // Old `sys_close`'s exact shape: free the slot first...
                closer_table.table.lock().remove(&3);
                thread::yield_now(); // widen the window for the allocator to jump in
                // ...and only "remember" to clear nonblock afterward. A real
                // pre-fix run would sometimes clear a slot the allocator had
                // *already* reused — this test doesn't need that second half of
                // the bug to prove the allocator-side guarantee, so it stops here.
            }
        });

        let hits = StdArc::new(AtomicUsize::new(0));
        let alloc_table = StdArc::clone(&table);
        let alloc_hits = StdArc::clone(&hits);
        let allocator = thread::spawn(move || {
            for _ in 0..ROUNDS {
                let fd = alloc_table.alloc_fd_from(3, FileDescriptor::Stderr);
                if fd == 3 {
                    alloc_hits.fetch_add(1, Ordering::Relaxed);
                    assert!(
                        !alloc_table.nonblock.lock().contains(&3),
                        "allocator observed a stale nonblock bit on a freshly reused fd"
                    );
                    // Give the closer another slot to take back.
                    alloc_table.nonblock.lock().insert(3);
                } else {
                    // fd 3 was still occupied this round; put back what we got so
                    // the table doesn't grow unbounded, and try again next round.
                    alloc_table.table.lock().remove(&fd);
                }
                thread::yield_now();
            }
        });

        closer.join().unwrap();
        allocator.join().unwrap();
        assert!(
            hits.load(Ordering::Relaxed) > 0,
            "test never actually raced the allocator onto fd 3 — widen the window and retry"
        );
    }
}

/// Characterization tests for the fd table's observable semantics, written
/// BEFORE the `akuma-fdtable` extraction
/// (`proposals/NEXT_AGENT_AUDIT_4_3_FD_TABLE_AND_4_4_MMU_FACADE.md`, Task A
/// method step 1) so the rewire can be checked against today's behaviour
/// rather than against intentions. Each test states which syscall-level
/// behaviour it is the table-level image of; the two `PINNED DIVERGENCE`
/// tests carry the divergences `amd64/src/fd.rs`'s boot-suite self-tests pin
/// end-to-end (`fd: a dup's cursor is independent`, `fd: a dup loses
/// nonblock`) so the table layer cannot drift underneath them.
///
/// These run against the real locked table (not a model) and must keep
/// passing, unmodified, through the extraction: after the rewire they are the
/// proof that `SharedFdTable` is a thin wrapper and nothing moved.
#[cfg(test)]
mod fdtable_characterization_tests {
    use super::*;
    use crate::process::types::{KernelFile, open_flags};
    use crate::test_support::ensure_test_runtime;

    fn file_at(pos: usize) -> FileDescriptor {
        let mut f = KernelFile::new("/x".into(), open_flags::O_RDWR);
        f.position = pos;
        FileDescriptor::File(f)
    }

    fn position_of(t: &SharedFdTable, fd: u32) -> Option<usize> {
        t.table.lock().get(&fd).map(|e| match e {
            FileDescriptor::File(f) => f.position,
            _ => unreachable!("test only installs File entries here"),
        })
    }

    #[test]
    fn allocation_returns_the_lowest_free_number_and_reuses_closed_gaps() {
        ensure_test_runtime();
        let t = SharedFdTable::with_stdio(); // 0/1/2 occupied
        assert_eq!(t.alloc_fd_from(0, file_at(0)), 3);
        assert_eq!(t.alloc_fd_from(0, file_at(0)), 4);
        // sys_close's table half: remove the slot (the flags go separately,
        // and a stale one is covered by `alloc_fd_tests` above).
        t.table.lock().remove(&3);
        // The gap is reused before a fresh number is handed out.
        assert_eq!(t.alloc_fd_from(0, file_at(0)), 3);
        // `fcntl(F_DUPFD)`'s shape: a minimum is honoured even when lower
        // numbers are free.
        assert_eq!(t.alloc_fd_from(4, file_at(0)), 5);
        t.table.lock().remove(&4);
        t.table.lock().remove(&5);
        assert_eq!(t.alloc_fd_from(4, file_at(0)), 4);
    }

    #[test]
    fn alloc_clears_the_reused_numbers_own_flags_only() {
        ensure_test_runtime();
        let t = SharedFdTable::with_stdio();
        t.alloc_fd_from(0, file_at(0)); // fd 3
        t.cloexec.lock().insert(3);
        t.nonblock.lock().insert(3);
        t.table.lock().remove(&3);
        assert_eq!(t.alloc_fd_from(0, file_at(0)), 3);
        assert!(!t.cloexec.lock().contains(&3), "reuse must not inherit the closed fd's cloexec");
        assert!(!t.nonblock.lock().contains(&3), "reuse must not inherit the closed fd's nonblock");
        // An unrelated fd's flags are untouched (fd 1 keeps its own).
        t.nonblock.lock().insert(1);
        t.alloc_fd_from(0, file_at(0)); // fd 4
        assert!(t.nonblock.lock().contains(&1));
    }

    /// **PINNED DIVERGENCE** (`amd64/src/fd.rs` boot suite: "a dup's cursor is
    /// independent"): `dup` is a *by-value* copy of the `KernelFile`, so the
    /// two descriptors do not share the open file description — advancing one
    /// cursor leaves the other where the copy was taken. POSIX shares the
    /// description; this table does not, and the boot suite asserts the same
    /// thing end-to-end through `sys_lseek`.
    #[test]
    fn pinned_dup_inserts_an_independent_by_value_copy() {
        ensure_test_runtime();
        let t = SharedFdTable::with_stdio();
        t.alloc_fd_from(0, file_at(0)); // fd 3, position 0
        let entry = t.table.lock().get(&3).cloned().expect("fd 3 installed");
        let newfd = t.alloc_fd_from(0, entry); // glue `sys_dup`'s table shape
        assert_eq!(newfd, 4);
        if let Some(FileDescriptor::File(f)) = t.table.lock().get_mut(&3) {
            f.position += 16;
        }
        assert_eq!(
            position_of(&t, 4),
            Some(0),
            "PINNED DIVERGENCE: the dup's cursor is a copy taken at dup time"
        );
        assert_eq!(position_of(&t, 3), Some(16));
    }

    /// **PINNED DIVERGENCE** (`amd64/src/fd.rs` boot suite: "a dup loses
    /// nonblock"): `nonblock` is keyed by fd *number*, and glue's `sys_dup`
    /// does not copy it — the flag stays with the old number.
    #[test]
    fn pinned_nonblock_is_keyed_by_number_and_a_dup_insert_does_not_inherit_it() {
        ensure_test_runtime();
        let t = SharedFdTable::with_stdio();
        t.alloc_fd_from(0, file_at(0)); // fd 3
        t.nonblock.lock().insert(3);
        let entry = t.table.lock().get(&3).cloned().expect("fd 3 installed");
        let newfd = t.alloc_fd_from(0, entry);
        assert_eq!(newfd, 4);
        assert!(!t.nonblock.lock().contains(&4), "PINNED DIVERGENCE: the dup loses the flag");
        assert!(t.nonblock.lock().contains(&3), "the original keeps it");
    }

    /// `execve`'s close-on-exec sweep: only descriptors actually present in
    /// the table come back for release, in ascending order, and the flag set
    /// is cleared whole — including the stale number that named nothing.
    #[test]
    fn cloexec_sweep_returns_present_fds_in_order_and_clears_the_set() {
        ensure_test_runtime();
        let t = SharedFdTable::with_stdio();
        t.alloc_fd_from(0, file_at(0)); // 3
        t.alloc_fd_from(0, file_at(0)); // 4
        t.cloexec.lock().insert(3);
        t.cloexec.lock().insert(4);
        t.cloexec.lock().insert(9); // a number nothing occupies any more
        t.table.lock().remove(&3); // closed before the exec
        // `Process::close_cloexec_fds`'s exact sequence, against the fields the
        // method itself drives (the method needs a `Process`; the properties
        // pinned here belong to the table).
        let cloexec: alloc::vec::Vec<u32> = t.cloexec.lock().iter().copied().collect();
        let mut closed = alloc::vec::Vec::new();
        for fd in &cloexec {
            if let Some(entry) = t.table.lock().remove(fd) {
                closed.push((*fd, entry));
            }
        }
        t.cloexec.lock().clear();
        assert_eq!(closed.iter().map(|(fd, _)| *fd).collect::<alloc::vec::Vec<_>>(), [4]);
        assert!(matches!(closed[0].1, FileDescriptor::File(_)));
        assert!(!t.cloexec.lock().contains(&4) && !t.cloexec.lock().contains(&9), "the set is cleared whole");
        assert!(
            t.table.lock().contains_key(&0)
                && t.table.lock().contains_key(&1)
                && t.table.lock().contains_key(&2),
            "stdio survives"
        );
        assert!(position_of(&t, 4).is_none(), "the swept fd is gone from the table");
    }

    /// `fork`'s table copy: epoll instances are **stripped** (they are not
    /// reference-counted), everything else is copied with its flags, and the
    /// child's table is independent of the parent's from then on.
    #[test]
    fn fork_copy_strips_epoll_copies_flags_and_is_independent() {
        ensure_test_runtime(); // `clone_fd_refs` runs on the copies
        let t = SharedFdTable::with_stdio();
        t.table.lock().insert(3, FileDescriptor::EpollFd(7));
        t.table.lock().insert(4, file_at(0));
        t.cloexec.lock().insert(3);
        t.cloexec.lock().insert(4);
        t.nonblock.lock().insert(4);

        let child = t.clone_deep_for_fork();

        assert!(child.table.lock().get(&3).is_none(), "the epoll instance is not inherited");
        assert!(matches!(child.table.lock().get(&4), Some(FileDescriptor::File(_))));
        // The flag sets are copied **wholesale**: fd 3's cloexec bit rides
        // along even though its epoll descriptor was stripped. Harmless by
        // construction — an `alloc_fd_from` of 3 clears the stale bit before
        // the number is reused — and pinned here so the extraction does not
        // silently "fix" it.
        assert!(child.cloexec.lock().contains(&3) && child.cloexec.lock().contains(&4));
        assert!(child.nonblock.lock().contains(&4) && !child.nonblock.lock().contains(&3));

        // Independence both ways after the copy.
        t.table.lock().remove(&4);
        assert!(child.table.lock().get(&4).is_some(), "parent close does not reach the child");
        child.table.lock().remove(&4);
        assert!(t.table.lock().get(&0).is_some(), "child teardown does not reach the parent");
    }

    /// `dup2`/`dup3`'s table half: the swap returns the *displaced* entry so
    /// its reference can be released, and an empty target returns nothing to
    /// release.
    #[test]
    fn dup2_swap_hands_back_the_displaced_entry_for_release() {
        ensure_test_runtime();
        let t = SharedFdTable::with_stdio();
        t.table.lock().insert(3, file_at(64));
        t.table.lock().insert(5, file_at(0));
        let entry = t.table.lock().get(&3).cloned().expect("fd 3 installed");
        let displaced = t.table.lock().insert(5, entry);
        assert!(matches!(displaced, Some(FileDescriptor::File(_))), "fd 5's old entry comes back");
        assert_eq!(position_of(&t, 5), Some(64), "fd 5 now names fd 3's description");
        assert!(t.table.lock().insert(6, file_at(0)).is_none(), "an empty target has nothing to release");
    }
}
