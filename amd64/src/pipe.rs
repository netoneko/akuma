//! The kernel-integration half of a pipe on amd64: the lock, the id cast, and
//! the wake effect.
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
//! # Wakes are real (2026-09-07)
//!
//! `PipeTable` hands back a [`Wakes`] set rather than firing it, and until the
//! scheduler grew a blocked state this kernel dropped it: a waiter was always
//! runnable and learnt of an event by polling. [`fire`] was an empty function,
//! written as a real one and called on every path that produces wakes precisely
//! so that this would be a change to one body. It was.
//!
//! The token type is still `()`, and that is not an oversight: `Wakes<W>` yields
//! `(tid, W)` pairs and this kernel's `tid` **is** the scheduler task slot, so
//! the identity a wake needs is already the key. There is nothing for a token to
//! carry.
//!
//! # Reading and writing block through the table, not by spinning
//!
//! [`read`] still returns `None` for "empty but a writer is still around" and
//! [`write`] still returns a short count when the buffer is full — the table
//! never blocks. What changed is what the *caller* does with that answer:
//! `fd::read_pipe` and `fd::write_pipe` used to `yield_now` and re-poll, and now
//! register through [`check_set_reader`]/[`check_set_writer`] and park.
//!
//! Registering is one step with the test on purpose. The two-step spelling
//! (`read` → empty → `add_poller` → sleep) has a TOCTOU window: a write landing
//! between them fires its wake with no waiter registered, and the caller sleeps
//! through the event it was waiting for. The crate owns that rule; this module
//! only has to not open a second window, which is why the caller arms with
//! `sched::prepare_block` *before* asking.

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

/// Make every returned waiter runnable.
///
/// **Called with the table lock released**, which is the property `akuma_pipes`
/// returning its wakes rather than firing them is there to make unavoidable:
/// `sched::wake` takes no lock of its own, but it is reached from paths that
/// hold others, and firing inside `PIPES` is how the AArch64 kernel
/// self-deadlocked a core (`docs/archive/AKUMA_PIPES_EXTRACTION.md`).
///
/// A wake naming a task that has since exited is not an error: `sched::wake`
/// answers `false` for a slot that is `Unused` or `Finished` and records
/// nothing, so a poller entry that outlived its reader cannot arm a wake for
/// whoever recycles the slot.
fn fire(wakes: Wakes<()>) {
    wakes.fire(|tid, ()| {
        crate::sched::wake(tid);
    });
}

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

/// "Is there anything to read, and if not, register me as a waiter" — one step.
///
/// Returns `true` when the caller must **not** park: there are bytes, or every
/// writer is gone (EOF), or the pipe does not exist. `false` means the caller is
/// registered and should park; the next [`write`], [`close_write`] or
/// [`close_read`] on this pipe will [`fire`] a wake at it.
///
/// The single step is the point — see the module header for the window the
/// two-step spelling leaves open.
#[must_use]
pub fn check_set_reader(id: PipeId) -> bool {
    let me = crate::sched::current_task();
    with_table(|t| t.check_set_reader(id as akuma_pipes::PipeId, me, ()))
}

/// The writer's half of [`check_set_reader`]: `true` when there is room, or
/// when every reader is gone and the write is about to fail with `EPIPE`.
///
/// That second case is why this is not simply "is there room". A pipe with no
/// readers never gains any, so a writer that parked on it would park forever;
/// answering `true` sends the caller back to [`write`], which reports the broken
/// pipe. It is the same rule [`writable`] states for `poll`.
#[must_use]
pub fn check_set_writer(id: PipeId) -> bool {
    let me = crate::sched::current_task();
    with_table(|t| t.check_set_writer(id as akuma_pipes::PipeId, me, ()))
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
