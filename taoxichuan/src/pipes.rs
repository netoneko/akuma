//! Stage 8 M3: `pipe2(2)` — this composition root's glue over the pulled crate.
//!
//! Everything that *decides* lives in `akuma-pipes` (end refcounts, the 64 KiB
//! FIFO, the read/write/EOF rules — host-tested, shared with the amd64
//! kernel). What is written new here is only what a composition root owns:
//! the table instance, the `OpenFile::Pipe` hooks and the copy loops, ~110
//! lines.
//!
//! M4 slice 3: the arms PARK and the wakes are FIRED — see `may_park`'s
//! note below. The M3 rule this kernel still keeps **by hand**: one table
//! reference per descriptor naming an end, because an `OpenFile` here is a
//! plain value (indices), not a handle. `pipe2` starts the pair at 1/1;
//! `dup`/`dup2`/`fcntl(F_DUPFD)` and `fork_arm`'s walk over `fork_copy`'s
//! payloads each add one ([`clone_ref`]); `close`, a displaced `dup2`
//! target, exec's cloexec sweep and a task's exit each drop theirs
//! ([`release`]). Miss fork or the exit and EOF never arrives; miss a dup
//! and the pipe dies under a live end.

use crate::OpenFile;
use akuma_pipes::{PipeTable, WriteOutcome};
use akuma_ring3_amd64 as ring3;
use alloc::vec::Vec;

/// `EPIPE` (32). The err table this kernel consumes has no EPIPE row — its
/// only other user is the SIGPIPE path, which is M6 and out of scope here
/// (P5 tolerates the EPIPE death); name the number where it is used.
const EPIPE: u64 = 32;

/// M4 slice 3 (session pf, Fix B): the arms park. The M3 header said "Wakes
/// are `()` and dropped on the floor — nothing on this kernel blocks"; that
/// ended when `wait4_real` proved a task can park on its own band and be
/// woken back (boots G/H/I, session pe). What remains of the M3 rule is the
/// tid discipline: a parked caller is registered under its TASK SLOT
/// (`exec_twin::current_slot()` — the same tid space as `wait4_real`'s
/// `waiter`), and every `Wakes` an op returns is fired through the threading
/// waker, exactly as `task_exit_real` wakes the reap-waiter. The one-step
/// `check_set_*` forms are the crate's anti-TOCTOU shape: probe-and-register
/// is one table step, so a wake landing between "empty" and "registering"
/// cannot be lost.

/// May this caller park at all? The boot task (`NO_SLOT`) drives the bridge
/// and the exec pump — the machine's only guaranteed-runner — so it keeps
/// the M3 non-blocking answers (read 0 / write EAGAIN). A task slot in
/// range parks like `wait4_real` does.
fn may_park() -> Option<usize> {
    let t = crate::exec_twin::current_slot();
    if t == crate::exec_twin::NO_SLOT || t >= crate::child::NSLOTS {
        None
    } else {
        Some(t)
    }
}

/// The `wait4_real` park, verbatim: the stub's user-RSP stash is a GLOBAL
/// that every syscall entry overwrites, so the parked task's own stash must
/// survive the park and be restored before its syscall returns.
fn park_once() {
    let stash = ring3::user_rsp_stash();
    akuma_threading::schedule_blocking(u64::MAX);
    ring3::set_user_rsp_stash(stash);
}

/// Fire one returned wake: the handle is minted at wake time, as
/// `task_exit_real` does for the reap-waiter. Safe while the target stays
/// parked (a WAITING slot is not claimable, so it cannot be recycled under
/// us) — the same bounded staleness `waiter` has.
fn fire(wakes: akuma_pipes::Wakes<()>) {
    wakes.fire(|tid, ()| akuma_threading::get_waker_for_thread(tid).wake());
}

/// Every live pipe (the `EVENTFDS` pattern: one task runs at a time; the
/// syscall path is the only accessor).
static mut PIPES: PipeTable<()> = PipeTable::new();

// SAFETY: syscall-path discipline (single core, IF clear), as `EVENTFDS`.
fn table() -> &'static mut PipeTable<()> {
    unsafe { &mut *core::ptr::addr_of_mut!(PIPES) }
}

/// `pipe(fds)` — x86_64 22: musl's `pipe()` is the raw `SYS_pipe`, one
/// argument, no flags word (boot 1: `[sc] unhandled ? (22)` then ash's
/// `can't create pipe: Bad file descriptor`). The ring3 table has no name
/// for the number; the dispatcher keys the arm on the literal.
pub fn pipe_arm(fds_va: u64) -> u64 {
    pipe2_arm(fds_va, 0)
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
/// path. Empty-with-live-writers PARKS the caller (M4 slice 3) until a
/// writer's wake or the last close; a caller that may not park (the boot
/// task) still gets the M3 answer, 0.
pub fn read_arm(id: u32, buf_va: u64, len: u64) -> u64 {
    let mut chunk = [0u8; 512];
    let mut total: u64 = 0;
    while total < len {
        let want = ((len - total) as usize).min(chunk.len());
        let (res, wakes) = table().read(id, &mut chunk[..want]);
        fire(wakes);
        if res.bytes == 0 {
            // Empty. EOF (no writers, or the pipe is gone) and "data just
            // landed" both answer through the one-step form: true ⇒ do not
            // block (the read below/next lap sees which); false ⇒ this
            // caller is the pipe's registered reader — park until a write
            // or a close wakes it, then re-read.
            let Some(tid) = may_park() else {
                break;
            };
            if table().check_set_reader(id, tid, ()) {
                break;
            }
            park_once();
            continue;
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

/// `write` on a pipe **write** end. A full buffer PARKS the caller (M4
/// slice 3) until a reader's wake; the boot task gets the M3 EAGAIN. A
/// full-blocking `write(2)` returns only when everything is written, so a
/// partial acceptance loops instead of answering short (musl/ash retry
/// either way). `BrokenPipe` is EPIPE — and for a REAL task only, the
/// documented SIGPIPE-default stand-in: the task dies by its own exit arm
/// with 13<<8, which is what Linux's default SIGPIPE disposition does to a
/// pipeline member (`yes` after `head` closes). M2 children and the boot
/// task keep the bare EPIPE (P5 tolerates it; no signals exist here).
pub fn write_arm(id: u32, buf_va: u64, len: u64) -> u64 {
    let mut data = Vec::new();
    data.resize(len as usize, 0);
    if !crate::copy_from_user(&mut data, buf_va) {
        return crate::e(ring3::err::EFAULT);
    }
    let mut done: usize = 0;
    loop {
        let (out, wakes) = table().write(id, &data[done..]);
        fire(wakes);
        match out {
            WriteOutcome::Wrote(n) if n > 0 => {
                done += n;
                if done == data.len() {
                    return done as u64;
                }
                // short accept: the buffer filled — next lap parks on it
            }
            WriteOutcome::Wrote(0) => {
                // Full. `check_set_writer` true here means the readers
                // vanished between the write and the check (room cannot
                // have appeared): EPIPE, not EAGAIN.
                let Some(tid) = may_park() else {
                    return crate::e(ring3::err::EAGAIN);
                };
                if table().check_set_writer(id, tid, ()) {
                    return crate::e(EPIPE);
                }
                park_once();
            }
            WriteOutcome::BrokenPipe | WriteOutcome::NoSuchPipe => {
                if crate::child::is_real_task() {
                    crate::child::task_exit_real(13 << 8);
                }
                return crate::e(EPIPE);
            }
        }
    }
}

/// The `ExecRuntime::pipe_write` arm (session pc, slice 1): write into a
/// pipe by id from **kernel** memory, without raising `SIGPIPE` — the
/// caller is never the pipe's own process (akuma-exec's stdin pump is the
/// only reader today, and nothing reaches it until `fork_process`). The
/// kernel-slice twin of `write_arm`, which copies from the user first.
pub fn write_no_sigpipe(id: u32, data: &[u8]) -> Result<usize, i32> {
    let (out, _wakes) = table().write(id, data);
    match out {
        // A full pipe answers Ok(0) — the short write that tells the pump
        // the reader is applying backpressure (the hook's own contract).
        WriteOutcome::Wrote(n) => Ok(n),
        WriteOutcome::BrokenPipe | WriteOutcome::NoSuchPipe => Err(-1),
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
/// target, exec's cloexec sweep. The close arms' wakes are FIRED (M4 slice
/// 3): the last reader's close is what tells a parked writer the pipe broke,
/// the last writer's close is what tells a parked reader EOF — the crate's
/// own `close_read` doc names `busybox yes | busybox head -n 1` as the
/// victim of exactly the dropped wake.
pub fn release(id: u32, is_write: bool) {
    let wakes = if is_write {
        table().close_write(id)
    } else {
        table().close_read(id)
    };
    fire(wakes);
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
