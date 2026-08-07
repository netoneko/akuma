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

    /// Explicitly close all underlying kernel resources and clear the table.
    /// This is used during process exit to ensure immediate cleanup.
    pub fn close_all(&self) {
        let fds: Vec<FileDescriptor> = with_irqs_disabled(|| {
            let mut table = self.table.lock();
            let items: Vec<FileDescriptor> = table.values().cloned().collect();
            table.clear();
            items
        });
        
        for fd in fds {
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
                FileDescriptor::RemoteFd { owner, handle, kind } => {
                    // Forward a close to the owner core so it frees the backing file/socket
                    // (multikernel §8.1). No-op on a build without the hook (BSP/single-kernel,
                    // where RemoteFd is never constructed anyway).
                    if let Some(close) = runtime().remote_fd_close {
                        close(owner, handle, kind);
                    }
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
    /// userspace locking involved, so the race is entirely in this gap). Returns
    /// `None` for a non-`File` fd, a missing fd, or an `O_APPEND` fd (append's
    /// position is derived from the live file size per write, a separate,
    /// pre-existing race this does not address — see the call site).
    ///
    /// Once reserved, the range belongs to this call alone: even a short/failed
    /// write must never move `.position` backward to "give back" the unused tail,
    /// since a concurrent writer may already have reserved past it — the correct
    /// on-disk result of a partial write here is a sparse hole, not a rewound
    /// position that reopens this exact race.
    pub fn reserve_write_pos(&self, fd: u32, count: usize) -> Option<usize> {
        with_irqs_disabled(|| {
            let mut table = self.fds.table.lock();
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
