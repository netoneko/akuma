//! Stage 8 M3: `pipe2(2)` — this composition root's glue over the pulled crate.
//!
//! Everything that *decides* lives in `akuma-pipes` (end refcounts, the 64 KiB
//! FIFO, the read/write/EOF rules — host-tested, shared with the amd64
//! kernel). What is written new here is only what a composition root owns:
//! the table instance, the `OpenFile::Pipe` hooks and the copy loops, ~110
//! lines.
//!
//! Wakes are `()` and dropped on the floor — nothing on this kernel blocks
//! (M2's fork runs each child to completion inside the fork syscall, so a
//! pipeline's writer finishes before its reader is even forked; the scheduler
//! that could block is M4's). No poller is ever registered here.
//!
//! The rule this kernel keeps **by hand**: one table reference per descriptor
//! naming an end, because an `OpenFile` here is a plain value (indices), not
//! a handle. `pipe2` starts the pair at 1/1; `dup`/`dup2`/`fcntl(F_DUPFD)`
//! and `fork_arm`'s walk over `fork_copy`'s payloads each add one
//! ([`clone_ref`]); `close`, a displaced `dup2` target, exec's cloexec sweep
//! and a task's exit each drop theirs ([`release`]). Miss fork or the exit
//! and EOF never arrives; miss a dup and the pipe dies under a live end.

use crate::OpenFile;
use akuma_pipes::{PipeTable, WriteOutcome};
use akuma_ring3_amd64 as ring3;
use alloc::vec::Vec;

/// `EPIPE` (32). The err table this kernel consumes has no EPIPE row — its
/// only other user is the SIGPIPE path, which is M6 and out of scope here
/// (P5 tolerates the EPIPE death); name the number where it is used.
const EPIPE: u64 = 32;

/// Every live pipe (the `EVENTFDS` pattern: one task runs at a time; the
/// syscall path is the only accessor).
static mut PIPES: PipeTable<()> = PipeTable::new();

// SAFETY: syscall-path discipline (single core, IF clear), as `EVENTFDS`.
fn table() -> &'static mut PipeTable<()> {
    unsafe { &mut *core::ptr::addr_of_mut!(PIPES) }
}

/// `pipe2(fds, flags)` — x86_64 293. Read end at `fds[0]`, write end at
/// `fds[1]`. amd64's `fd::sys_pipe2` is the shape this copies (its MAX_PIPES
/// preamble is that kernel's policy; this kernel serves one shell per boot).
/// `O_CLOEXEC` and `O_NONBLOCK` are the accepted flags: the first recorded in
/// the fd table (exec's sweep honours it), the second recorded and vacuous —
/// nothing on this kernel ever blocks.
pub fn pipe2_arm(fds_va: u64, flags: u64) -> u64 {
    const O_CLOEXEC: u64 = 0o2_000_000;
    const O_NONBLOCK: u64 = 0o4000;
    if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
        return crate::e(ring3::err::EINVAL);
    }
    if !crate::user_ok(fds_va, 8) {
        return crate::e(ring3::err::EFAULT);
    }
    let id = table().create();
    let mut fds = crate::fd_table();
    let Some(rslot) = fds.free_below(3, crate::SLOT_CEILING) else {
        table().close_read(id);
        return crate::e(ring3::err::EMFILE);
    };
    let rfd = fds.alloc_fd_from(rslot, OpenFile::Pipe(id, false));
    let Some(wslot) = fds.free_below(3, crate::SLOT_CEILING) else {
        let _ = fds.close(rfd);
        table().close_read(id);
        return crate::e(ring3::err::EMFILE);
    };
    let wfd = fds.alloc_fd_from(wslot, OpenFile::Pipe(id, true));
    if flags & O_CLOEXEC != 0 {
        fds.set_cloexec(rfd);
        fds.set_cloexec(wfd);
    }
    if flags & O_NONBLOCK != 0 {
        fds.set_nonblock(rfd);
        fds.set_nonblock(wfd);
    }
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&(rfd as u32).to_ne_bytes());
    out[4..].copy_from_slice(&(wfd as u32).to_ne_bytes());
    if !crate::copy_to_user(fds_va, &out) {
        return crate::e(ring3::err::EFAULT);
    }
    0
}

/// `read` on a pipe **read** end. Drains in 512-byte chunks like the file
/// path. Empty-with-live-writers would block on Linux; here the writer has
/// already finished (the M2 fork), so that state reads as a plain 0 — the
/// answer a blocking read gives once the buffer drains and the last writer
/// closes.
pub fn read_arm(id: u32, buf_va: u64, len: u64) -> u64 {
    let mut chunk = [0u8; 512];
    let mut total: u64 = 0;
    while total < len {
        let want = ((len - total) as usize).min(chunk.len());
        let (res, _wakes) = table().read(id, &mut chunk[..want]);
        if res.bytes == 0 {
            break; // drained, or EOF — both read as 0 to the caller
        }
        if !crate::copy_to_user(buf_va + total, &chunk[..res.bytes]) {
            return crate::e(ring3::err::EFAULT);
        }
        total += res.bytes as u64;
        if res.bytes < want {
            break; // short read: the buffer is drained
        }
    }
    total
}

/// `write` on a pipe **write** end. `BrokenPipe` is EPIPE — SIGPIPE delivery
/// is M6 and out of scope this session (P5 tolerates the EPIPE). A full
/// buffer with nothing accepted is EAGAIN (the nonblocking contract; the
/// 64 KiB cap is beyond this session's gates).
pub fn write_arm(id: u32, buf_va: u64, len: u64) -> u64 {
    let mut data = Vec::new();
    data.resize(len as usize, 0);
    if !crate::copy_from_user(&mut data, buf_va) {
        return crate::e(ring3::err::EFAULT);
    }
    let (out, _wakes) = table().write(id, &data);
    match out {
        WriteOutcome::Wrote(n) if n == data.len() => n as u64,
        WriteOutcome::Wrote(0) => crate::e(ring3::err::EAGAIN),
        WriteOutcome::Wrote(n) => n as u64, // short write: caller retries
        WriteOutcome::BrokenPipe | WriteOutcome::NoSuchPipe => crate::e(EPIPE),
    }
}

/// One more descriptor names this end — `dup`/`dup2`/`fcntl(F_DUPFD)`, and
/// `fork_arm`'s walk over `fork_copy`'s payloads.
pub fn clone_ref(id: u32, is_write: bool) {
    table().clone_ref(id, is_write);
}

/// `poll(2)`'s readiness answers for a pipe end (`sock.rs`'s select arm) —
/// the crate's own predicates, no second opinion.
#[must_use]
pub fn readable(id: u32) -> bool {
    table().readable(id)
}

#[must_use]
pub fn writable(id: u32) -> bool {
    table().writable(id)
}

/// Drop the reference one descriptor held — `close`, a displaced `dup2`
/// target, exec's cloexec sweep.
pub fn release(id: u32, is_write: bool) {
    let _wakes = if is_write {
        table().close_write(id)
    } else {
        table().close_read(id)
    };
}

/// The exiting task's fds die with it (`child_exit`): drop every pipe
/// reference its table holds. Depth 2 is the fork child — a pipeline member's
/// exit is what turns the last writer close into the reader's EOF, so this is
/// the load-bearing sweep; depth 1 (the shell) normally closed its own copies
/// already, and this is the safety net.
pub fn release_open_fds() {
    let mut t = crate::fd_table();
    while let Some((_fd, d)) = t.pop_lowest() {
        if let OpenFile::Pipe(id, w) = d {
            release(id, w);
        }
    }
}
