//! This kernel's pipe surface: names, ids and the block/wake protocol, over
//! **the tree's one pipe table**.
//!
//! # One table, since 4b batch 2b
//!
//! The rules — the buffer, the 64 KiB cap, end reference counts, the waiter set
//! and everything joining them — have been [`akuma_pipes::PipeTable`] since
//! 2026-09-06 (`docs/archive/AKUMA_PIPES_EXTRACTION.md`). What this module used
//! to add was a **second instance** of it: its own `static PIPES`, its own id
//! space, its own `fire`.
//!
//! Two instances of one implementation is not duplicate logic, and that is
//! exactly why it was easy to leave alone. What it is, is a duplicate **id
//! space** — and that is what blocks folding `close(2)` into
//! `akuma-syscalls-glue`, whose `sys_close` reaches `glue::pipe` for a
//! `PipeRead(id)`/`PipeWrite(id)` descriptor. Handing this kernel's pipe id to
//! that table is not an error: it is a *different pipe*, closed silently, while
//! the one the descriptor named leaks. So the table had to become one before
//! the arm could move.
//!
//! Every function below is now a name for a `glue::pipe` call. What stays here
//! is what is genuinely this kernel's:
//!
//! - **the id type.** `usize`, because every call site here spells it that way;
//!   the fd table stores it as a `u32` inside `FileDescriptor::Pipe{Read,Write}`
//!   and widens it back. The cast is confined to this module.
//! - **the machine-wide ceiling.** [`MAX_PIPES`] is a policy, not a table rule.
//! - **two readiness divergences**, [`readable`] and [`writable`], which are
//!   stated where they are made rather than being a second opinion buried in a
//!   shared predicate.
//! - **the wake sink.** Registered at boot
//!   (`boot::install_shared_sinks` → [`glue::pipe::set_wake_sink`]), because a
//!   wake here goes through [`crate::sched::wake`] to keep the `[SCHED] wakes`
//!   counter honest. The state change underneath is identical on both kernels:
//!   this target's threads *are* `akuma-threading` slots, and `sched::wake` is
//!   `wake_by_handle` plus that counter.
//!
//! # Reading and writing block through the table, not by spinning
//!
//! [`read`] returns `None` for "empty but a writer is still around" and
//! [`write`] returns a short count when the buffer is full — the table never
//! blocks. `fd::read_pipe` and `fd::write_pipe` register through
//! [`check_set_reader`]/[`check_set_writer`] and park.
//!
//! Registering is one step with the test on purpose. The two-step spelling
//! (`read` → empty → `add_poller` → sleep) has a TOCTOU window: a write landing
//! between them fires its wake with no waiter registered, and the caller sleeps
//! through the event it was waiting for. The crate owns that rule; this module
//! only has to not open a second window, which is why the caller arms with
//! `sched::prepare_block` *before* asking.
//!
//! [`glue::pipe::set_wake_sink`]: akuma_syscalls_glue::pipe::set_wake_sink

use akuma_syscalls_glue::pipe as glue;

/// An opaque pipe handle.
///
/// `usize` rather than [`akuma_pipes::PipeId`]'s `u32` — see the module header.
pub type PipeId = usize;

/// How many pipes can exist at once.
///
/// The table is a `BTreeMap` and would grow without bound; this is the cap the
/// fixed `[Slot; 64]` array that predates it imposed for free. It is kept, and
/// deliberately small, for the reason that array's own comment gave: each pipe
/// is up to 64 KiB of kernel buffer allocated on a userspace request, and a
/// number this size means a leak announces itself instead of being absorbed.
/// A shell pipeline takes one per `|`, on top of two per live ssh session.
///
/// **A policy, not a table rule**, which is why it lives here and the shared
/// table does not have one: the AArch64 kernel runs workloads (a `-j4`
/// self-host build) whose pipe count this would refuse outright.
const MAX_PIPES: usize = 64;

/// The wake effect this kernel registers with the shared table.
///
/// `sched::wake` is `akuma_threading::wake_by_handle` plus the `[SCHED] wakes`
/// counter the boot suite reports, so routing through it keeps that number
/// counting pipe wakes as it always has.
pub fn wake_sink(tid: usize, _handle: akuma_exec::threading::WakeHandle) {
    crate::sched::wake(tid);
}

/// Claim a fresh pipe with one reader end and one writer end, or `None` once
/// [`MAX_PIPES`] are live — which callers report as `ENFILE`: the *machine* is
/// out of pipes, not this process out of descriptors.
pub fn alloc() -> Option<PipeId> {
    (glue::pipe_live_count() < MAX_PIPES).then(|| glue::pipe_create() as PipeId)
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
    glue::pipe_destroy(id as u32);
}

/// One more open file description names this end — `dup`, or a `fork`
/// inheriting it.
///
/// Counts **descriptions, not descriptors**: the fd table's own refcount is
/// what keeps a description alive under `dup`, and only its final release
/// reaches [`close_read`]/[`close_write`]. Counting names here as well would
/// double-count every inherited pipe and leak it.
#[allow(dead_code)]
pub fn clone_ref(id: PipeId, is_write: bool) {
    glue::pipe_clone_ref(id as u32, is_write);
}

/// Append bytes.
///
/// * `Some(n)` — `n` bytes taken. A short count (including 0) means the buffer
///   is full and the caller should retry after the reader drains.
/// * `None` — every reader is gone, or there is no such pipe: `EPIPE`. A caller
///   that loops on a short write must check it or it spins forever against a
///   dead reader.
///
/// [`glue::pipe_write_no_sigpipe`] rather than `pipe_write`: there is no signal
/// delivery on this target, so `EPIPE` is the whole of Linux's answer that
/// applies. Raising `SIGPIPE` here would run the shared kernel's default
/// disposition — terminate, inline — through an exit path this target does not
/// use.
pub fn write(id: PipeId, data: &[u8]) -> Option<usize> {
    glue::pipe_write_no_sigpipe(id as u32, data).ok()
}

/// Read up to `out.len()` bytes.
///
/// * `Some(n > 0)` — data.
/// * `Some(0)` — end of file: every writer has closed and the buffer is drained.
/// * `None` — nothing available yet, but a writer is still open. Re-poll.
pub fn read(id: PipeId, out: &mut [u8]) -> Option<usize> {
    let (bytes, eof) = glue::pipe_read(id as u32, out);
    if bytes > 0 {
        Some(bytes)
    } else if eof {
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
/// where one told "ready" reads a 0 and stops.
///
/// **Deliberate divergence from the AArch64 kernel**, whose `pipe_can_read`
/// guards on existence first — kept by spelling the missing case out here
/// rather than by adding a second predicate to the shared module. That guard is
/// legacy this target does not share: the fixed slot array `pipe_can_read`'s
/// callers predate reported on the array entry rather than on a live pipe, so
/// "missing" was never a state it could answer `false` for.
#[must_use]
pub fn readable(id: PipeId) -> bool {
    let id = id as u32;
    glue::pipe_can_read(id) || !glue::pipe_exists(id)
}

/// Is there room for a [`write`] right now? Non-destructive; for `poll(2)`.
///
/// A pipe with no readers left — or none at all — is reported **writable**, not
/// blocked: the caller then writes, gets `None`, and reports `EPIPE`. Linux
/// answers the same shape (`POLLERR` on the write end, and `select(2)` marks it
/// writable) for the same reason — a broken pipe is an event to act on, not a
/// wait.
///
/// The second divergence from `pipe_can_write`, which answers `false` for both
/// of those, and built from [`glue::pipe_counts`] rather than from a predicate
/// that would have to encode two kernels' `poll` rules at once.
#[must_use]
pub fn writable(id: PipeId) -> bool {
    match glue::pipe_counts(id as u32) {
        // No pipe, or no reader: an event, not a wait.
        None => true,
        Some((0, _)) => true,
        Some(_) => glue::pipe_can_write(id as u32),
    }
}

/// "Is there anything to read, and if not, register me as a waiter" — one step.
///
/// Returns `true` when the caller must **not** park: there are bytes, or every
/// writer is gone (EOF), or the pipe does not exist. `false` means the caller is
/// registered and should park; the next [`write`], [`close_write`] or
/// [`close_read`] on this pipe fires a wake at it, through this kernel's
/// [`wake_sink`].
///
/// The single step is the point — see the module header for the window the
/// two-step spelling leaves open.
#[must_use]
pub fn check_set_reader(id: PipeId) -> bool {
    glue::pipe_check_set_reader(id as u32, crate::sched::current_task())
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
    glue::pipe_check_set_writer(id as u32, crate::sched::current_task())
}

/// Drop one writer. Losing the **last** one is EOF: a reader that has drained
/// the buffer then sees end-of-file rather than polling forever.
pub fn close_write(id: PipeId) {
    glue::pipe_close_write(id as u32);
}

/// Drop one reader. Losing the **last** one breaks the pipe, and every
/// subsequent [`write`] reports it.
///
/// The mirror of [`close_write`]: a writer parked on a full buffer can only
/// learn the pipe broke by being woken and retrying, which is what
/// `busybox yes | busybox head -n 1` depends on.
pub fn close_read(id: PipeId) {
    glue::pipe_close_read(id as u32);
}

#[cfg(not(feature = "no-tests"))]
/// **One table, seen from both sides.**
///
/// The invariant 4b batch 2b exists to establish, asserted the only way that
/// distinguishes it from the arrangement it replaced: an id minted through this
/// module must be the *same pipe* `akuma-syscalls-glue` sees under that number.
/// With two `PipeTable` instances every check below still passed on its own
/// side — that is precisely why two id spaces were easy to keep — and
/// `pipe_exists` answered `false` for a pipe this kernel had just created.
///
/// The two readiness divergences are asserted here as well, next to where they
/// are now expressed, so a later "simplification" onto `pipe_can_read` /
/// `pipe_can_write` is a red check rather than a `poll` loop that waits forever
/// on a pipe nobody will ever write to.
pub fn smoke_test(t: &mut akuma_selftest::Suite) {
    let Some(id) = alloc() else {
        t.check("pipe: a pipe can be allocated", false);
        return;
    };
    t.check("pipe: glue sees the pipe this kernel just created", glue::pipe_exists(id as u32));
    t.check(
        "pipe: and agrees it has one reader and one writer",
        glue::pipe_counts(id as u32) == Some((1, 1)),
    );

    let mut buf = [0u8; 8];
    t.check_eq("pipe: a write takes every byte", write(id, b"hi").unwrap_or(0) as u64, 2);
    t.check_eq("pipe: and the read gets them back", read(id, &mut buf).unwrap_or(0) as u64, 2);
    t.check("pipe: the bytes are the ones written", &buf[..2] == b"hi");
    // Drained with a writer still open is "not yet", not EOF — the distinction
    // `fd::read_pipe` parks on.
    t.check("pipe: a drained pipe with a live writer is not EOF", read(id, &mut buf).is_none());

    close_write(id);
    t.check_eq("pipe: losing the last writer is EOF", read(id, &mut buf).unwrap_or(9) as u64, 0);
    close_read(id);
    t.check("pipe: the last close destroys it, in the shared table", !glue::pipe_exists(id as u32));

    // The two divergences, on the destroyed id.
    t.check("pipe: a gone pipe reads ready (it returns EOF)", readable(id));
    t.check("pipe: a gone pipe writes ready (it returns EPIPE)", writable(id));
}
