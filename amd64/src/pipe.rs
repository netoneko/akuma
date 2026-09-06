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

/// How many pipes can exist at once.
///
/// Was 16, sized for "two per live session (stdin + stdout); `sshd`'s
/// cooperative build serves a handful". `pipe(2)` reaching userspace changes
/// who allocates these: a shell pipeline takes one per `|`, and a `make`- or
/// `cargo`-driven build has several running at once on top of every live ssh
/// session. 64 is still a fixed array and still small enough that a leak
/// announces itself.
const MAX_PIPES: usize = 64;

/// An opaque pipe handle. Indexes [`PIPES`].
pub type PipeId = usize;

struct Slot {
    pipe: Pipe,
    in_use: bool,
    /// How many descriptor *ends* are outstanding, or `None` for a pipe whose
    /// lifetime the allocator manages by hand.
    ///
    /// The two kinds genuinely differ and conflating them breaks one of them:
    ///
    /// - A **spawn** pipe ([`alloc`]) is owned by `sys_spawn`. Only one end is
    ///   ever a descriptor — the parent's — while the child's end is reached by
    ///   number through `current_stdout_pipe`. Closing the parent's read end
    ///   *is* the end of the pipe, and closing a write end must **not** free it
    ///   (the child may still be draining buffered input). `None`.
    /// - A **`pipe(2)`** pair ([`alloc_pair`]) has two open file descriptions,
    ///   either of which may be closed first. It is freed when the last one
    ///   goes. `Some(n)`.
    ///
    /// It counts **descriptions, not descriptors**. `dup` and `fork` add a
    /// name for a description that already exists, and `fd::FILES`' own
    /// refcount is what keeps that description alive; only its final release
    /// reaches [`drop_end`]. Counting names here as well would double-count
    /// every inherited pipe and leak it.
    ends: Option<u8>,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            pipe: Pipe::with_capacity(akuma_pipe::DEFAULT_CAPACITY),
            in_use: false,
            ends: None,
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
        s.ends = None;
    }
}

/// Claim a pipe for `pipe(2)`: two ends, freed when the last one closes.
///
/// Separate from [`alloc`] because the *lifetime rule* differs, not the buffer
/// — see [`Slot::ends`].
pub fn alloc_pair() -> Option<PipeId> {
    let id = alloc()?;
    if let Some(s) = PIPES.lock().get_mut(id) {
        s.ends = Some(2);
    }
    Some(id)
}

/// Drop one end. Returns `true` if this pipe accounts for ends at all — i.e.
/// whether the caller's own release rule has been superseded.
///
/// The return value is what keeps the two lifetimes apart: `fd::release` frees
/// a spawn-owned read end itself and must not do so for a `pipe(2)` pair whose
/// writer is still open.
pub fn drop_end(id: PipeId) -> bool {
    let free_now = {
        let mut pipes = PIPES.lock();
        let Some(s) = pipes.get_mut(id) else {
            return false;
        };
        let Some(n) = s.ends else {
            return false;
        };
        let left = n.saturating_sub(1);
        s.ends = Some(left);
        left == 0
    };
    if free_now {
        free(id);
    }
    true
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

/// Would a [`read`] return something other than `None` right now — data waiting
/// or EOF to report? Non-destructive; for `poll(2)`.
#[must_use]
pub fn readable(id: PipeId) -> bool {
    PIPES.lock().get(id).is_some_and(|s| s.pipe.is_readable())
}

/// Is there room for a [`write`] right now? Non-destructive; for `poll(2)`.
#[must_use]
pub fn writable(id: PipeId) -> bool {
    PIPES.lock().get(id).is_some_and(|s| s.pipe.room() > 0)
}

/// Mark the producer's end closed. A reader that has drained the buffer then
/// sees EOF rather than blocking forever.
pub fn close_write(id: PipeId) {
    if let Some(s) = PIPES.lock().get_mut(id) {
        s.pipe.close_write();
    }
}
