//! The kernel-integration half of a pipe on amd64: the lock, the id cast, and
//! the (currently empty) wake effect.
//!
//! The table itself — the buffer, the 64 KiB cap, **end reference counts**, the
//! waiter set and every rule joining them — is
//! [`akuma_pipes::PipeTable`], shared with the AArch64 kernel since 2026-09-06
//! (`docs/archive/AKUMA_PIPES_EXTRACTION.md`). Before that this module was a
//! fixed `[Slot; 64]` with a single `write_closed` flag and no counts at all,
//! which meant none of those rules were even expressible here: a `fork` or `dup`
//! of a pipe end made the pipe closable by the first `close`, and losing the
//! last *reader* was not an event, so `busybox yes | busybox head -n 1` filled
//! the buffer and never learnt the pipe had broken.
//!
//! # Wakes are still no-ops — for now
//!
//! `PipeTable` hands back a [`Wakes`] set rather than firing it, and this kernel
//! drops it: its scheduler has no blocked state (`Unused | Reserved | Runnable |
//! Finished`), so a waiter is always runnable and learns of an event by polling
//! — the same trade its futex and `wait4` already make. [`fire`] is the single
//! place that changes when that scheduler grows a blocked state; nothing in the
//! crate does.
//!
//! # Reading and writing do not block here
//!
//! [`read`] returns `None` for "empty but a writer is still around" and the
//! caller re-polls; [`write`] returns a short count when the buffer is full.
//! `sshd`'s bridge loop polls every direction each tick, so a blocking pipe
//! would only be a way to deadlock it against a child that is itself waiting on
//! the other pipe.

use akuma_pipes::{PipeTable, Wakes, WriteOutcome};
use spinning_top::Spinlock;

/// An opaque pipe handle.
///
/// `usize` rather than [`akuma_pipes::PipeId`]'s `u32` because every call site
/// here already spells it that way — the fd table stores it as a `u32` inside
/// `FileDescriptor::Pipe{Read,Write}` and widens it back on the way out. The
/// cast is confined to this module.
pub type PipeId = usize;

static PIPES: Spinlock<PipeTable<()>> = Spinlock::new(PipeTable::new());

/// Run `f` against the table with the lock held.
///
/// Nothing that can re-enter belongs inside `f`, which is why every wake it
/// produces is *returned* rather than fired — see the module header.
fn with_table<R>(f: impl FnOnce(&mut PipeTable<()>) -> R) -> R {
    f(&mut PIPES.lock())
}

/// Make every returned waiter runnable — which on this kernel is nothing to do.
///
/// Kept as a real function, and called on every path that produces wakes, so
/// that giving the scheduler a blocked state is a change to this body and
/// nowhere else.
fn fire(_wakes: Wakes<()>) {}

/// How many pipes can exist at once.
///
/// The table itself is a `BTreeMap` and would grow without bound; this is the
/// cap the fixed `[Slot; 64]` array used to impose for free. It is kept, and
/// deliberately small, for the reason that array's own comment gave: each pipe
/// is up to 64 KiB of kernel buffer allocated on a userspace request, and a
/// number this size means a leak announces itself instead of being absorbed.
/// A shell pipeline takes one per `|`, on top of two per live ssh session.
const MAX_PIPES: usize = 64;

/// Claim a fresh pipe with one reader end and one writer end, or `None` once
/// [`MAX_PIPES`] are live — which callers report as `ENFILE`: the *machine* is
/// out of pipes, not this process out of descriptors.
pub fn alloc() -> Option<PipeId> {
    with_table(|t| {
        (t.live_count() < MAX_PIPES).then(|| t.create() as PipeId)
    })
}

/// Remove a pipe outright, whatever its end counts say.
///
/// For the lifetimes `sys_spawn` manages by hand: a spawned child's stdin pipe
/// is read by the child *by number* rather than through a descriptor, so its
/// read end never closes and refcounting alone would never free it. `waitpid`
/// is what knows the child is gone.
///
/// A `pipe(2)` pair must **not** come through here — it is destroyed by the
/// last [`close_read`]/[`close_write`], and short-circuiting that frees the
/// buffer under a live peer.
pub fn free(id: PipeId) {
    let (_, wakes) = with_table(|t| t.destroy(id as akuma_pipes::PipeId));
    fire(wakes);
}

/// One more open file description names this end — `dup`, or a `fork`
/// inheriting it.
///
/// Counts **descriptions, not descriptors**: `fd::FILES`' own refcount is what
/// keeps a description alive under `dup`, and only its final release reaches
/// [`close_read`]/[`close_write`]. Counting names here as well would
/// double-count every inherited pipe and leak it.
#[allow(dead_code)]
pub fn clone_ref(id: PipeId, is_write: bool) {
    with_table(|t| t.clone_ref(id as akuma_pipes::PipeId, is_write));
}

/// Append bytes.
///
/// * `Some(n)` — `n` bytes taken. A short count (including 0) means the buffer
///   is full and the caller should retry after the reader drains.
/// * `None` — every reader is gone, or there is no such pipe: `EPIPE`. This
///   case did not exist before end counts did, and a caller that loops on a
///   short write must check it or it spins forever against a dead reader.
pub fn write(id: PipeId, data: &[u8]) -> Option<usize> {
    let (outcome, wakes) = with_table(|t| t.write(id as akuma_pipes::PipeId, data));
    fire(wakes);
    match outcome {
        WriteOutcome::Wrote(n) => Some(n),
        WriteOutcome::BrokenPipe | WriteOutcome::NoSuchPipe => None,
    }
}

/// Read up to `out.len()` bytes.
///
/// * `Some(n > 0)` — data.
/// * `Some(0)` — end of file: every writer has closed and the buffer is drained.
/// * `None` — nothing available yet, but a writer is still open. Re-poll.
pub fn read(id: PipeId, out: &mut [u8]) -> Option<usize> {
    let (r, wakes) = with_table(|t| t.read(id as akuma_pipes::PipeId, out));
    fire(wakes);
    if r.bytes > 0 {
        Some(r.bytes)
    } else if r.eof {
        Some(0)
    } else {
        None
    }
}

/// Would a [`read`] return something other than `None` right now — data waiting
/// or EOF to report? Non-destructive; for `poll(2)`.
///
/// A **gone** pipe is ready, because a read of one returns EOF: a `poll` loop
/// that is told "not yet" about an id nothing will ever write to waits forever,
/// where one told "ready" reads a 0 and stops. This is the crate's answer taken
/// verbatim, and it is a **deliberate divergence from the AArch64 kernel**,
/// whose `pipe_can_read` guards on existence first. That guard is legacy this
/// target does not share: the fixed slot array this replaces reported on the
/// array entry rather than on a live pipe, so "missing" was never a state it
/// could answer `false` for. See `crates/akuma-syscalls-glue/src/pipe.rs`.
#[must_use]
pub fn readable(id: PipeId) -> bool {
    with_table(|t| t.readable(id as akuma_pipes::PipeId))
}

/// Is there room for a [`write`] right now? Non-destructive; for `poll(2)`.
///
/// A pipe with no readers left is reported **writable**, not blocked: the
/// caller then writes, gets `None`, and reports `EPIPE`. Linux answers the same
/// shape (`POLLERR` on the write end, and `select(2)` marks it writable) for
/// the same reason — a broken pipe is an event to act on, not a wait.
#[must_use]
pub fn writable(id: PipeId) -> bool {
    with_table(|t| t.writable(id as akuma_pipes::PipeId))
}

/// Drop one writer. Losing the **last** one is EOF: a reader that has drained
/// the buffer then sees end-of-file rather than polling forever.
pub fn close_write(id: PipeId) {
    let (_, wakes) = with_table(|t| t.close_write(id as akuma_pipes::PipeId));
    fire(wakes);
}

/// Drop one reader. Losing the **last** one breaks the pipe, and every
/// subsequent [`write`] reports it.
///
/// The mirror of [`close_write`], and the one this kernel had no way to express
/// before: a pipe whose consumer has gone used to keep accepting bytes into a
/// buffer nobody would ever read.
pub fn close_read(id: PipeId) {
    let (_, wakes) = with_table(|t| t.close_read(id as akuma_pipes::PipeId));
    fire(wakes);
}
