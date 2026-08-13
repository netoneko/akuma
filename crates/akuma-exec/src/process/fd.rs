use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::process::types::FileDescriptor;
use crate::process::children::remove_child_channel;
use crate::runtime::{runtime, with_irqs_disabled};
use super::Process;

pub struct SharedFdTable {
    pub table: Spinlock<BTreeMap<u32, FileDescriptor>>,
    pub cloexec: Spinlock<BTreeSet<u32>>,
    pub nonblock: Spinlock<BTreeSet<u32>>,
}

impl SharedFdTable {
    pub fn new() -> Self {
        Self {
            table: Spinlock::new(BTreeMap::new()),
            cloexec: Spinlock::new(BTreeSet::new()),
            nonblock: Spinlock::new(BTreeSet::new()),
        }
    }

    pub fn with_stdio() -> Self {
        let mut fd_map = BTreeMap::new();
        fd_map.insert(0, FileDescriptor::Stdin);
        fd_map.insert(1, FileDescriptor::Stdout);
        fd_map.insert(2, FileDescriptor::Stderr);
        Self {
            table: Spinlock::new(fd_map),
            cloexec: Spinlock::new(BTreeSet::new()),
            nonblock: Spinlock::new(BTreeSet::new()),
        }
    }

    /// Find the lowest fd number >= `min_fd` not present in `table`.
    fn lowest_available_fd(table: &BTreeMap<u32, FileDescriptor>, min_fd: u32) -> u32 {
        let mut fd = min_fd;
        for (&key, _) in table.range(min_fd..) {
            if key != fd { break; }
            fd += 1;
        }
        fd
    }

    /// Deep copy for fork (separate fd table, with pipe ref bumps).
    /// Strips EpollFd entries since epoll instances are not reference-counted.
    #[must_use]
    pub fn clone_deep_for_fork(&self) -> Self {
        let cloned: BTreeMap<u32, FileDescriptor> = with_irqs_disabled(|| {
            self.table.lock().iter()
                .filter(|(_, fd)| !matches!(fd, FileDescriptor::EpollFd(_)))
                .map(|(&k, v)| (k, v.clone()))
                .collect()
        });
        for entry in cloned.values() {
            match entry {
                FileDescriptor::PipeWrite(id) => (crate::runtime::runtime().pipe_clone_ref)(*id, true),
                FileDescriptor::PipeRead(id) => (crate::runtime::runtime().pipe_clone_ref)(*id, false),
                FileDescriptor::UnixSocket { rx, tx } => {
                    (crate::runtime::runtime().pipe_clone_ref)(*rx, false);
                    (crate::runtime::runtime().pipe_clone_ref)(*tx, true);
                }
                FileDescriptor::EventFd(id) => (crate::runtime::runtime().eventfd_clone_ref)(*id),
                // Sockets are refcounted like pipes: the child's fd-table copy is a
                // real reference, so the first close (child exit / exec cloexec
                // sweep) must not destroy the socket under the parent's live fd.
                FileDescriptor::Socket(idx) => (crate::runtime::runtime().socket_clone_ref)(*idx),
                // Rump sockets need the same reference the native ones take, and for
                // the same reason — but they were the one refcounted family missing
                // from this list, so the child's copy was a bare alias. sshd's
                // process-per-session pattern is exactly the shape that breaks on
                // that: the parent `drop`s its copy of the accepted socket right
                // after `fork`, expecting the refcount to keep the child's alive,
                // and instead `proxy_close` sent a real NetBSD `close(rump_fd)` to
                // `rump_server` and destroyed the socket the child was about to
                // speak SSH over. Every session died at kex on the rump devbox
                // (`docs/archive/RUMP_SSHD_FORKED_SESSION_CLOSES_SOCKET.md`).
                FileDescriptor::RumpSocket { rump_fd, box_id, .. } => {
                    (crate::runtime::runtime().rump_socket_clone_ref)(*box_id, *rump_fd);
                }
                _ => {}
            }
        }
        let cloexec_clone = with_irqs_disabled(|| self.cloexec.lock().clone());
        let nonblock_clone = with_irqs_disabled(|| self.nonblock.lock().clone());
        Self {
            table: Spinlock::new(cloned),
            cloexec: Spinlock::new(cloexec_clone),
            nonblock: Spinlock::new(nonblock_clone),
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
        loop {
            let entry = with_irqs_disabled(|| self.table.lock().pop_first());
            let Some((_fd, fd)) = entry else { break };
            match fd {
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
                FileDescriptor::UnixSocket { rx, tx } => {
                    (runtime().pipe_close_read)(rx);
                    (runtime().pipe_close_write)(tx);
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
                _ => {}
            }
        }
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
    /// Used by `fcntl(F_DUPFD)` which specifies a minimum fd.
    pub fn alloc_fd_from(&self, min_fd: u32, entry: FileDescriptor) -> u32 {
        with_irqs_disabled(|| {
            let mut table = self.fds.table.lock();
            let fd = SharedFdTable::lowest_available_fd(&table, min_fd);
            table.insert(fd, entry);
            fd
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
            self.fds.cloexec.lock().insert(fd);
        });
    }

    pub fn clear_cloexec(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.cloexec.lock().remove(&fd);
        });
    }

    pub fn is_cloexec(&self, fd: u32) -> bool {
        with_irqs_disabled(|| {
            self.fds.cloexec.lock().contains(&fd)
        })
    }

    pub fn set_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.nonblock.lock().insert(fd);
        });
    }

    pub fn clear_nonblock(&self, fd: u32) {
        with_irqs_disabled(|| {
            self.fds.nonblock.lock().remove(&fd);
        });
    }

    pub fn is_nonblock(&self, fd: u32) -> bool {
        with_irqs_disabled(|| {
            self.fds.nonblock.lock().contains(&fd)
        })
    }

    /// Close all FDs marked close-on-exec, returning them for cleanup.
    pub fn close_cloexec_fds(&self) -> Vec<(u32, FileDescriptor)> {
        with_irqs_disabled(|| {
            let cloexec: Vec<u32> = self.fds.cloexec.lock().iter().copied().collect();
            let mut closed = Vec::new();
            let mut table = self.fds.table.lock();
            for fd in &cloexec {
                if let Some(entry) = table.remove(fd) {
                    closed.push((*fd, entry));
                }
            }
            self.fds.cloexec.lock().clear();
            closed
        })
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
