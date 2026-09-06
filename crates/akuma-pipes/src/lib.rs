//! The kernel pipe **table**: end reference counts, registered waiters, and the
//! rules joining them — shared by both kernels.
//!
//! The buffer itself is [`akuma_pipe::Pipe`]; this crate is everything wrapped
//! around one. Extracted 2026-09-06 from `akuma_syscalls_glue::pipe`, which was
//! the mature implementation, and which the amd64 kernel had independently
//! re-implemented a smaller version of. The two agreed on the 64 KiB cap and on
//! nothing else: aarch64 counted read and write ends and woke registered
//! waiters, amd64 had a single `close_write` flag and polled. Every rule below
//! was learned by the aarch64 side the hard way, and the amd64 kernel did not
//! have any of them.
//!
//! # Wakes are returned, never performed
//!
//! Every operation that can make a waiter runnable hands back a [`Wakes`] set
//! instead of firing it. That is the seam that lets one implementation serve
//! two kernels, and it is not merely a portability device — it is the shape the
//! aarch64 code arrived at by debugging:
//!
//! - Raising `SIGPIPE` *inside* the pipe lock self-deadlocked a core. Default
//!   disposition runs the terminate action inline, which reaches
//!   `close_all` → `pipe_close_write` → the same lock, IRQs masked, BKL still
//!   held. Root-caused live 2026-07-24 (`aria2c | head -1`).
//! - Destroying a pipe takes the AF_UNIX socket-table lock, and taking that
//!   while the pipe table is held is the mirror inversion.
//!
//! Returning the effects makes "act outside the locked section" the only thing
//! a caller *can* do, rather than a rule each caller has to remember.
//!
//! The two kernels then differ only in what a wake *is*. aarch64 fires
//! `akuma_threading::wake_by_handle` at a generation-validated `WakeHandle`.
//! amd64 fires nothing: its scheduler has no blocked state (`Unused`,
//! `Reserved`, `Runnable`, `Finished`), so a waiter is always runnable and
//! learns of the event by polling — the same trade its futex and `wait4`
//! already make. When that scheduler grows a blocked state, only the effect
//! changes; nothing here does.
//!
//! # The waiter set is one set, not a reader set and a writer set
//!
//! `pollers` holds every thread interested in the pipe for any reason — a
//! blocked reader, a blocked writer, an `epoll`/`poll` watcher. Each event
//! drains **all** of them and lets each re-test its own condition. That is
//! deliberate: a reader parked on an empty buffer and a writer parked on a full
//! one are woken by different events, and the state that distinguishes them
//! (`buffer.is_empty()`, `read_count`, `write_count`) is re-read on the retry
//! anyway. Splitting the set would mean deciding at *registration* time which
//! event a waiter cares about, and `pipe_write_all_blocking` cares about both.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::BTreeMap;

pub use akuma_pipe::{DEFAULT_CAPACITY, ReadOutcome};

/// A pipe's identity. Monotonic; never reused while the pipe is live.
pub type PipeId = u32;

/// Waiters to make runnable, handed back by whatever operation freed them.
///
/// Carries the caller's own wake token — a `WakeHandle` on aarch64, `()` on a
/// kernel that polls. Moved out of the table wholesale, so producing it
/// allocates nothing; the caller drops it after firing.
#[derive(Debug)]
pub struct Wakes<W>(BTreeMap<usize, W>);

impl<W> Wakes<W> {
    /// Nothing to wake.
    #[must_use]
    pub fn none() -> Self {
        Self(BTreeMap::new())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The waiters, as `(tid, token)`.
    pub fn drain(self) -> impl Iterator<Item = (usize, W)> {
        self.0.into_iter()
    }

    /// Fire each token. Call this **after** releasing the pipe lock.
    pub fn fire(self, mut f: impl FnMut(usize, W)) {
        for (tid, w) in self.0 {
            f(tid, w);
        }
    }
}

/// What a [`PipeTable::write`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Bytes accepted. **May be fewer than offered** — a full buffer accepts a
    /// short count, and `Wrote(0)` for non-empty data means "full, retry".
    /// Treating `Wrote(0)` as success silently drops data, which for a framed
    /// protocol (the rump sysproxy) desyncs the stream for good.
    Wrote(usize),
    /// Every reader is gone. Linux delivers `SIGPIPE` **and** returns `EPIPE`;
    /// the caller owns both, and must raise the signal outside the lock.
    BrokenPipe,
    /// No such pipe. Distinct from [`Self::BrokenPipe`]: nothing to signal.
    NoSuchPipe,
}

/// What a [`PipeTable::read`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadResult {
    pub bytes: usize,
    /// The buffer is drained **and** the last writer is gone. A missing pipe
    /// reads as EOF too — a reader must not block on an id nobody holds.
    pub eof: bool,
}

/// What closing an end did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseResult {
    /// Both counts reached zero and the pipe is gone from the table. The caller
    /// detaches whatever it had hung off the id — on aarch64, the AF_UNIX
    /// framing metadata, which must not outlive the pipe or the next pipe to
    /// reuse the id inherits its record boundaries.
    pub destroyed: bool,
}

struct Entry<W> {
    buffer: akuma_pipe::Pipe,
    read_count: u32,
    write_count: u32,
    pollers: BTreeMap<usize, W>,
}

impl<W> Entry<W> {
    fn take_pollers(&mut self) -> Wakes<W> {
        Wakes(core::mem::take(&mut self.pollers))
    }
}

/// Every live pipe in the system.
pub struct PipeTable<W> {
    pipes: BTreeMap<PipeId, Entry<W>>,
    next_id: PipeId,
    capacity: usize,
}

impl<W> Default for PipeTable<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W> PipeTable<W> {
    #[must_use]
    pub const fn new() -> Self {
        Self { pipes: BTreeMap::new(), next_id: 1, capacity: DEFAULT_CAPACITY }
    }

    /// A table whose pipes hold `capacity` bytes each.
    #[must_use]
    pub const fn with_capacity(capacity: usize) -> Self {
        Self { pipes: BTreeMap::new(), next_id: 1, capacity }
    }

    /// A fresh pipe with **one** reader and **one** writer.
    ///
    /// Both counts start at 1, not 0: `pipe(2)` hands out two descriptors and
    /// the pipe is live from that moment. A table that started at zero would
    /// report EOF to the reader before the writer had done anything.
    pub fn create(&mut self) -> PipeId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pipes.insert(
            id,
            Entry {
                buffer: akuma_pipe::Pipe::with_capacity(self.capacity),
                read_count: 1,
                write_count: 1,
                pollers: BTreeMap::new(),
            },
        );
        id
    }

    /// One more descriptor names this end — `dup`, or a `fork` inheriting it.
    pub fn clone_ref(&mut self, id: PipeId, is_write: bool) {
        if let Some(p) = self.pipes.get_mut(&id) {
            if is_write {
                p.write_count = p.write_count.saturating_add(1);
            } else {
                p.read_count = p.read_count.saturating_add(1);
            }
        }
    }

    #[must_use]
    pub fn exists(&self, id: PipeId) -> bool {
        self.pipes.contains_key(&id)
    }

    /// `(read_count, write_count)`, for diagnostics and tests.
    #[must_use]
    pub fn counts(&self, id: PipeId) -> Option<(u32, u32)> {
        self.pipes.get(&id).map(|p| (p.read_count, p.write_count))
    }

    #[must_use]
    pub fn buffered(&self, id: PipeId) -> usize {
        self.pipes.get(&id).map_or(0, |p| p.buffer.len())
    }

    #[must_use]
    pub fn live_count(&self) -> usize {
        self.pipes.len()
    }

    /// Would a read return something other than "nothing yet" — data or EOF?
    /// Non-destructive; for `poll(2)`. A missing pipe is ready (it reads EOF).
    #[must_use]
    pub fn readable(&self, id: PipeId) -> bool {
        self.pipes
            .get(&id)
            .is_none_or(|p| !p.buffer.is_empty() || p.write_count == 0)
    }

    /// Is there room for a write right now? A missing pipe is "ready" so the
    /// caller proceeds and gets `NoSuchPipe` rather than blocking forever.
    #[must_use]
    pub fn writable(&self, id: PipeId) -> bool {
        self.pipes.get(&id).is_none_or(|p| p.buffer.room() > 0)
    }

    /// Register `tid` as interested in this pipe.
    pub fn add_poller(&mut self, id: PipeId, tid: usize, token: W) {
        if let Some(p) = self.pipes.get_mut(&id) {
            p.pollers.insert(tid, token);
        }
    }

    /// How many waiters have announced themselves on this pipe.
    ///
    /// Evidence, not decoration: `sys_pselect6` passed `None` for its waker for
    /// as long as nobody could see this number.
    #[must_use]
    pub fn poller_count(&self, id: PipeId) -> usize {
        self.pipes.get(&id).map_or(0, |p| p.pollers.len())
    }

    #[must_use]
    pub fn is_poller_registered(&self, id: PipeId, tid: usize) -> bool {
        self.pipes.get(&id).is_some_and(|p| p.pollers.contains_key(&tid))
    }

    /// Every live pipe, as `(id, read_count, write_count, buffered, pollers)`.
    pub fn iter(&self) -> impl Iterator<Item = (PipeId, u32, u32, usize, usize)> + '_ {
        self.pipes
            .iter()
            .map(|(id, p)| (*id, p.read_count, p.write_count, p.buffer.len(), p.pollers.len()))
    }

    /// The tids waiting on `id`, for a diagnostic dump.
    pub fn poller_tids(&self, id: PipeId) -> impl Iterator<Item = usize> + '_ {
        self.pipes.get(&id).into_iter().flat_map(|p| p.pollers.keys().copied())
    }

    /// Append bytes, waking everyone waiting.
    ///
    /// The wake fires on a **successful** write only. A write that finds the
    /// buffer full changes nothing, so there is nothing for a waiter to
    /// re-test, and waking them would be a spin.
    pub fn write(&mut self, id: PipeId, data: &[u8]) -> (WriteOutcome, Wakes<W>) {
        let Some(p) = self.pipes.get_mut(&id) else {
            return (WriteOutcome::NoSuchPipe, Wakes::none());
        };
        if p.read_count == 0 {
            return (WriteOutcome::BrokenPipe, Wakes::none());
        }
        let n = p.buffer.write(data);
        if n == 0 {
            return (WriteOutcome::Wrote(0), Wakes::none());
        }
        (WriteOutcome::Wrote(n), p.take_pollers())
    }

    /// Take bytes out, waking everyone waiting.
    ///
    /// The wake on a **read** is not symmetry for its own sake: draining makes
    /// room, and a writer parked on a full buffer has no other way to learn it.
    pub fn read(&mut self, id: PipeId, out: &mut [u8]) -> (ReadResult, Wakes<W>) {
        let Some(p) = self.pipes.get_mut(&id) else {
            // A reader holding an id nobody else does must see EOF, not block.
            return (ReadResult { bytes: 0, eof: true }, Wakes::none());
        };
        match p.buffer.read(out) {
            // `Read(0)` is reachable only for an empty `out` — `read(fd, buf, 0)`.
            // It drains nothing, so it makes no room and there is nothing for a
            // parked writer to re-test; waking on it would be a spin, the same
            // rule `write` applies to `Wrote(0)`. It falls through to the arm
            // below so a zero-length read of a writer-less pipe still reports
            // EOF, which is what the AArch64 implementation this replaces did.
            ReadOutcome::Read(n) if n > 0 => {
                (ReadResult { bytes: n, eof: false }, p.take_pollers())
            }
            // `akuma_pipe` reports EOF from its own `write_closed` flag, which
            // this table sets when the last writer goes; asking `write_count`
            // directly keeps the one source of truth here rather than in two
            // places that can disagree.
            ReadOutcome::Read(_) | ReadOutcome::Eof | ReadOutcome::WouldBlock => (
                ReadResult { bytes: 0, eof: p.write_count == 0 },
                Wakes::none(),
            ),
        }
    }

    /// Drop one writer. Losing the **last** one is EOF, and an event for
    /// blocked readers.
    pub fn close_write(&mut self, id: PipeId) -> (CloseResult, Wakes<W>) {
        let Some(p) = self.pipes.get_mut(&id) else {
            return (CloseResult { destroyed: false }, Wakes::none());
        };
        p.write_count = p.write_count.saturating_sub(1);
        let wakes = if p.write_count == 0 {
            // Tell the buffer too, so a `ReadOutcome::Eof` and `write_count`
            // can never disagree.
            p.buffer.close_write();
            p.take_pollers()
        } else {
            Wakes::none()
        };
        let destroyed = p.write_count == 0 && p.read_count == 0;
        if destroyed {
            self.pipes.remove(&id);
        }
        (CloseResult { destroyed }, wakes)
    }

    /// Drop one reader. Losing the **last** one breaks the pipe, and is an
    /// event for blocked *writers*.
    ///
    /// The mirror of `close_write`'s wake, and the one that is easy to miss: a
    /// writer parked on a full buffer can only learn the pipe broke by retrying
    /// and seeing `read_count == 0`. Without this wake it never retries, never
    /// gets `EPIPE`, and sleeps forever. Invisible until pipes were capped —
    /// an uncapped pipe never blocked a writer, so this wake had nothing to
    /// wake. `busybox yes | busybox head -n 1` hits it every time.
    pub fn close_read(&mut self, id: PipeId) -> (CloseResult, Wakes<W>) {
        let Some(p) = self.pipes.get_mut(&id) else {
            return (CloseResult { destroyed: false }, Wakes::none());
        };
        p.read_count = p.read_count.saturating_sub(1);
        let wakes = if p.read_count == 0 { p.take_pollers() } else { Wakes::none() };
        let destroyed = p.write_count == 0 && p.read_count == 0;
        if destroyed {
            self.pipes.remove(&id);
        }
        (CloseResult { destroyed }, wakes)
    }

    /// Remove a pipe outright, regardless of its end counts, waking anyone
    /// parked on it.
    ///
    /// For a pipe whose lifetime an allocator manages by hand rather than by
    /// refcount: the amd64 kernel's `sys_spawn` pipes, whose child end is
    /// reached by *number* rather than by descriptor and so never closes. A
    /// `pipe(2)` pair must **not** come through here — it is destroyed by the
    /// last [`close_read`](Self::close_read)/[`close_write`](Self::close_write),
    /// and short-circuiting that frees the buffer under a live peer.
    ///
    /// Returns whether there was a pipe to remove.
    pub fn destroy(&mut self, id: PipeId) -> (bool, Wakes<W>) {
        match self.pipes.remove(&id) {
            Some(mut p) => (true, p.take_pollers()),
            None => (false, Wakes::none()),
        }
    }

    /// "Is there something to read, and if not, register me" — in one step.
    ///
    /// Returns `true` if the caller must **not** block. The two-step spelling
    /// (`read` → empty → `add_poller` → sleep) has a TOCTOU window: a write
    /// landing between the two fires its wake with no waiter registered, and
    /// the caller then sleeps through the event it was waiting for.
    pub fn check_set_reader(&mut self, id: PipeId, tid: usize, token: W) -> bool {
        let Some(p) = self.pipes.get_mut(&id) else {
            return true; // pipe gone → EOF, do not block
        };
        if !p.buffer.is_empty() || p.write_count == 0 {
            return true;
        }
        p.pollers.insert(tid, token);
        false
    }

    /// The writer's half of [`Self::check_set_reader`], with the same window
    /// and the same fix.
    pub fn check_set_writer(&mut self, id: PipeId, tid: usize, token: W) -> bool {
        let Some(p) = self.pipes.get_mut(&id) else {
            return true;
        };
        if p.buffer.room() > 0 || p.read_count == 0 {
            return true;
        }
        p.pollers.insert(tid, token);
        false
    }
}

#[cfg(test)]
mod tests;
