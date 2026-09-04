//! The kernel-integration half of a pipe: a fixed pool of [`akuma_pipe::Pipe`]s
//! behind one spinlock, addressed by a [`PipeId`].
//!
//! Stage R. `sys_spawn` gives a child a stdout pipe (and, for an interactive
//! session, a stdin pipe); `sshd` bridges the parent ends to its SSH channel.
//! The buffer, the 64 KiB cap, the short-write-on-full rule and the
//! empty-vs-EOF distinction all live in the host-tested `akuma-pipe` leaf; this
//! module is only the `static` array, the lock, and the id bookkeeping.
//!
//! # Non-blocking, always
//!
//! [`read`] returns `None` for "empty but the writer is still around" — the
//! caller re-polls. [`write`] returns a short count when the buffer is full.
//! The cooperative single-core scheduler cannot park a task on a pipe, and
//! `sshd`'s bridge loop polls every direction each tick anyway, so a blocking
//! pipe would only be a way to deadlock it against a child that is itself
//! waiting on the other pipe. This is the one behavioural difference from
//! `akuma_syscalls_glue::pipe`, whose `pollers` map has no analogue here.

use akuma_pipe::{Pipe, ReadOutcome};
use spinning_top::Spinlock;

/// How many pipes can exist at once. Two per live session (stdin + stdout);
/// `sshd`'s cooperative build serves a handful.
const MAX_PIPES: usize = 16;

/// An opaque pipe handle. Indexes [`PIPES`].
pub type PipeId = usize;

struct Slot {
    pipe: Pipe,
    in_use: bool,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            pipe: Pipe::with_capacity(akuma_pipe::DEFAULT_CAPACITY),
            in_use: false,
        }
    }
}

static PIPES: Spinlock<[Slot; MAX_PIPES]> = Spinlock::new([const { Slot::empty() }; MAX_PIPES]);

/// Claim a fresh pipe, or `None` if all [`MAX_PIPES`] are in use.
pub fn alloc() -> Option<PipeId> {
    let mut pipes = PIPES.lock();
    for (i, s) in pipes.iter_mut().enumerate() {
        if !s.in_use {
            s.pipe.clear();
            s.in_use = true;
            return Some(i);
        }
    }
    None
}

/// Release a pipe outright. Any unread bytes are dropped — the consumer is gone.
pub fn free(id: PipeId) {
    if let Some(s) = PIPES.lock().get_mut(id) {
        s.pipe.clear();
        s.in_use = false;
    }
}

/// Append bytes. Returns how many were taken; a short count means the buffer is
/// full and the caller should retry after the reader drains.
pub fn write(id: PipeId, data: &[u8]) -> usize {
    PIPES
        .lock()
        .get_mut(id)
        .map_or(0, |s| s.pipe.write(data))
}

/// Read up to `out.len()` bytes.
///
/// * `Some(n > 0)` — data.
/// * `Some(0)` — end of file: the writer has closed and the buffer is drained.
/// * `None` — nothing available yet, but the writer is still open. Re-poll.
pub fn read(id: PipeId, out: &mut [u8]) -> Option<usize> {
    let mut pipes = PIPES.lock();
    let s = pipes.get_mut(id)?;
    match s.pipe.read(out) {
        ReadOutcome::Read(n) => Some(n),
        ReadOutcome::Eof => Some(0),
        ReadOutcome::WouldBlock => None,
    }
}

/// Mark the producer's end closed. A reader that has drained the buffer then
/// sees EOF rather than blocking forever.
pub fn close_write(id: PipeId) {
    if let Some(s) = PIPES.lock().get_mut(id) {
        s.pipe.close_write();
    }
}
