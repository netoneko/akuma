//! The x86_64 syscall ABI over `akuma-syscalls-glue`, for the file-descriptor
//! family: what an x86_64 kernel must do between ring 3 and glue's bodies,
//! which speak asm-generic.
//!
//! Glue is the one implementation of `openat`, `read`, `fstat`, `poll`, `dup3`
//! and the rest that the AArch64 kernel runs. An x86_64 caller differs from an
//! asm-generic one in exactly the places this crate handles:
//!
//! - **Numbers.** Eleven fd calls exist only on x86_64 (`open` 2, `stat` 4, `lstat` 6,
//!   `poll` 7, `access` 21, `pipe` 22, `select` 23, `dup2` 33, `creat` 85,
//!   `epoll_create` 213, `epoll_wait` 232); each is its `*at`/`p*` successor
//!   with fixed arguments. Two time calls do too (`gettimeofday` 96, `time` 201),
//!   served here from the wall clock glue's `clock_gettime` reads. The path-only legacy calls are
//!   `akuma_syscalls_abi::legacy::fold`'s, applied by the caller first.
//! - **Open flags.** Four `open(2)` bits are permuted between the ABIs
//!   (`akuma_syscalls_abi::open_flags`); handing glue the x86_64 bits opens a
//!   directory as `O_DIRECT` and so on.
//! - **`struct stat`.** Glue fills the asm-generic layout; x86_64's differs
//!   (`akuma_syscalls_abi::stat::to_x86_64`).
//! - **Shapes glue does not have an arm for:** `dup2`'s `old == new` rule,
//!   `poll`/`select`'s millisecond and `timeval` timeouts, and chunked large
//!   reads (glue serves at most [`MAX_IO`] bytes per call).
//!
//! Moved out of `amd64/src/fd.rs` for Taoxichuan's adoption of glue
//! (`docs/archive/TAOXICHUAN_CLEANUP_PLAN.md` step 2c-B): the alternative was a
//! third copy. A kernel's own devices (amd64's kvm, tun, wifi, framebuffer, its
//! console ends) are intercepted by the kernel *before* it calls in here; this
//! crate knows only glue's descriptors.
//!
//! Return values are syscall-return form: a count, or a negative errno as `u64`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use akuma_exec::process::FileDescriptor;
use akuma_syscalls_abi::Syscall;
use akuma_syscalls_glue as glue;
use akuma_syscalls_linux::flags::open as o;

const EBADF: u64 = (-9i64) as u64;
const EFAULT: u64 = (-14i64) as u64;
const EEXIST: u64 = (-17i64) as u64;
const ENOTDIR: u64 = (-20i64) as u64;
const EISDIR: u64 = (-21i64) as u64;
const EINVAL: u64 = (-22i64) as u64;
const ENOENT: u64 = (-2i64) as u64;
const ELOOP: u64 = (-40i64) as u64;

const AT_FDCWD: u64 = (-100i64) as u64;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_EMPTY_PATH: u64 = 0x1000;

/// The most one glue `read`/`pread64` call moves.
///
/// Glue copies through a kernel buffer of the request's size, so the length
/// from ring 3 is bounded here and a larger regular-file read is served in
/// chunks ([`read_chunked`]).
pub const MAX_IO: u64 = 64 * 1024;

/// Linux's per-call cap on a read or write (`MAX_RW_COUNT`).
const MAX_RW_COUNT: u64 = 0x7fff_f000;

const fn is_err(r: u64) -> bool {
    r.cast_signed() < 0 && r.cast_signed() > -4096
}

/// Serve the x86_64 fd-family call `nr`, or `None` when `nr` is not one.
///
/// The caller has already applied `akuma_syscalls_abi::legacy::fold` and
/// intercepted its own devices. Every call this answers goes to glue, through
/// a translation where the ABIs differ and unchanged where they agree.
#[must_use]
pub fn dispatch(nr: u64, a: [u64; 6]) -> Option<u64> {
    // The legacy x86-only numbers first: none has a `Syscall` row.
    let legacy = match nr {
        2 => Some(openat(AT_FDCWD, a[0], a[1], a[2])),
        4 => Some(newfstatat(AT_FDCWD, a[0], a[1], 0)),
        6 => Some(newfstatat(AT_FDCWD, a[0], a[1], AT_SYMLINK_NOFOLLOW)),
        7 => Some(poll(a[0], a[1], a[2])),
        21 => Some(to_glue(Syscall::Faccessat, [AT_FDCWD, a[0], a[1], 0, 0, 0])),
        22 => Some(glue::pipe::sys_pipe2(a[0], 0)),
        23 => Some(select(a[0], a[1], a[2], a[3], a[4])),
        33 => Some(dup2(a[0], a[1])),
        // creat(path, mode) = open(path, O_CREAT | O_WRONLY | O_TRUNC, mode), x86_64 bits.
        85 => Some(openat(AT_FDCWD, a[0], 0o100 | 0o1 | 0o1000, a[1])),
        96 => Some(gettimeofday(a[0])),
        201 => Some(time(a[0])),
        213 => Some(to_glue(Syscall::EpollCreate1, [0; 6])),
        232 => Some(to_glue(Syscall::EpollPwait, [a[0], a[1], a[2], a[3], 0, 0])),
        _ => None,
    };
    if legacy.is_some() {
        return legacy;
    }
    let call = Syscall::from_x86_64(nr)?;
    Some(match call {
        Syscall::Openat => openat(a[0], a[1], a[2], a[3]),
        Syscall::Read => read(a[0], a[1], a[2]),
        Syscall::Write => write(a[0], a[1], a[2]),
        // A device that takes the call whole (`glue::chardev::vectored`), else the loop.
        Syscall::Readv => glue::chardev::vectored(a[0] as u32, false, a[1], a[2])
            .unwrap_or_else(|| vectored(a[0], a[1], a[2], read)),
        Syscall::Writev => glue::chardev::vectored(a[0] as u32, true, a[1], a[2])
            .unwrap_or_else(|| vectored(a[0], a[1], a[2], write)),
        Syscall::Pread64 => pread64(a[0], a[1], a[2], a[3]),
        Syscall::Fstat => fstat(a[0], a[1]),
        Syscall::Newfstatat => newfstatat(a[0], a[1], a[2], a[3]),
        Syscall::Fcntl => fcntl(a[0], a[1], a[2]),
        Syscall::Fadvise64 => 0,
        // Identical on both ABIs: glue's arm, by its asm-generic number.
        Syscall::Close
        | Syscall::Lseek
        | Syscall::Ioctl
        | Syscall::Pwrite64
        | Syscall::Pwritev2
        | Syscall::Dup
        | Syscall::Dup3
        | Syscall::Pipe2
        | Syscall::Getdents64
        | Syscall::Statfs
        | Syscall::Fstatfs
        | Syscall::Statx
        | Syscall::Ftruncate
        | Syscall::Truncate
        | Syscall::Fallocate
        | Syscall::Fsync
        | Syscall::Fdatasync
        | Syscall::Flock
        | Syscall::Faccessat
        | Syscall::Utimensat
        | Syscall::Mkdirat
        | Syscall::Unlinkat
        | Syscall::Renameat
        | Syscall::Symlinkat
        | Syscall::Readlinkat
        | Syscall::Linkat
        | Syscall::Fchmodat
        | Syscall::Fchmod
        | Syscall::Fchownat
        | Syscall::Fchown
        | Syscall::Umask
        | Syscall::Getcwd
        | Syscall::Chdir
        | Syscall::Fchdir
        | Syscall::Ppoll
        | Syscall::Pselect6
        | Syscall::Eventfd2
        | Syscall::EpollCreate1
        | Syscall::EpollCtl
        | Syscall::EpollPwait
        | Syscall::Socketpair => to_glue(call, a),
        _ => return None,
    })
}

/// Glue's arm for `call`, by its asm-generic number.
#[must_use]
pub fn to_glue(call: Syscall, a: [u64; 6]) -> u64 {
    glue::handle_syscall(call.to_aarch64(), &a)
}

/// `openat(dirfd, path, flags, mode)` with the x86_64 flags permuted to glue's.
///
/// Also the four checks glue's `openat_path` leaves to its caller
/// (`O_NOFOLLOW` on a symlink, `O_CREAT` on a directory, `O_DIRECTORY` on a
/// file, `O_EXCL` on anything present): amd64's preamble, minus its devices.
#[must_use]
pub fn openat(dirfd: u64, path: u64, flags_x86: u64, mode: u64) -> u64 {
    let flags = akuma_syscalls_abi::open_flags::x86_64_to_aarch64(flags_x86 as u32);
    let Ok(raw) = glue::copy_from_user_str(path, 1024) else {
        return EFAULT;
    };
    let Ok(resolved) = glue::fs::resolve_path_at(dirfd as i32, &raw) else {
        return EBADF;
    };
    openat_resolved(dirfd, &resolved, flags, mode)
}

/// [`openat`] from a resolved path and asm-generic `flags`.
///
/// For a kernel that looks at the resolved path first, to serve a device of
/// its own, without reading and resolving it twice.
#[must_use]
pub fn openat_resolved(dirfd: u64, resolved: &str, flags: u32, mode: u64) -> u64 {
    if flags & o::O_NOFOLLOW != 0 && akuma_vfs_glue::is_symlink(resolved) {
        return ELOOP;
    }
    let creating = flags & o::O_CREAT != 0;
    let wants_dir = flags & o::O_DIRECTORY != 0;
    if creating || wants_dir {
        let md = akuma_vfs_glue::metadata(resolved);
        let is_dir = md.as_ref().is_ok_and(|m| m.is_dir);
        let present = md.is_ok();
        if creating && is_dir {
            return EISDIR;
        }
        if wants_dir && present && !is_dir {
            return ENOTDIR;
        }
        if creating && flags & o::O_EXCL != 0 && present {
            return EEXIST;
        }
    }
    glue::flat(glue::fs::openat_path(dirfd as i32, resolved, flags, mode as u32))
}

/// Whether `fd` names a regular file in the caller's table (not a directory,
/// not a pipe, socket or device): the reads [`read_chunked`] may split.
fn is_regular_file(fd: u64) -> bool {
    let path = akuma_exec::process::current_process_shared().and_then(|p| {
        p.with_fd(fd as u32, |d| match d {
            FileDescriptor::File(f) => Some(f.path.clone()),
            _ => None,
        })
        .flatten()
    });
    path.is_some_and(|p| !akuma_vfs_glue::metadata(&p).is_ok_and(|m| m.is_dir))
}

/// Serve `len` bytes as consecutive [`MAX_IO`] chunks through `chunk(done, want)`,
/// stopping at the first short or failed one. A failure after progress reports
/// the progress, as `read(2)` does.
fn read_chunked(len: u64, mut chunk: impl FnMut(u64, usize) -> u64) -> u64 {
    let len = len.min(MAX_RW_COUNT);
    let mut done = 0u64;
    while done < len {
        let want = (len - done).min(MAX_IO);
        let n = chunk(done, want as usize);
        if is_err(n) {
            return if done > 0 { done } else { n };
        }
        done += n;
        if n < want {
            break;
        }
    }
    done
}

/// `read(fd, buf, len)`: glue's arm, chunked for a regular file larger than [`MAX_IO`].
#[must_use]
pub fn read(fd: u64, buf: u64, len: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    if len > MAX_IO && is_regular_file(fd) {
        return read_chunked(len, |done, n| glue::fs::sys_read(fd, buf + done, n));
    }
    glue::fs::sys_read(fd, buf, len.min(MAX_IO) as usize)
}

/// `pread64(fd, buf, len, off)`: as [`read`], at an explicit offset.
#[must_use]
pub fn pread64(fd: u64, buf: u64, len: u64, off: u64) -> u64 {
    if len == 0 {
        return 0;
    }
    if len > MAX_IO && is_regular_file(fd) {
        return read_chunked(len, |done, n| {
            glue::fs::sys_pread64(fd as u32, buf + done, n, off.cast_signed().wrapping_add(done.cast_signed()))
        });
    }
    glue::fs::sys_pread64(fd as u32, buf, len.min(MAX_IO) as usize, off.cast_signed())
}

/// `write(fd, buf, len)`: glue's arm, after the one check it leaves to its
/// caller — a file opened `O_RDONLY` refuses writes with `EBADF`.
#[must_use]
pub fn write(fd: u64, buf: u64, len: u64) -> u64 {
    let read_only = akuma_exec::process::current_process_shared().and_then(|p| {
        p.with_fd(fd as u32, |d| matches!(d, FileDescriptor::File(f) if f.flags & o::O_ACCMODE == o::O_RDONLY))
    });
    if read_only == Some(true) {
        return EBADF;
    }
    glue::fs::sys_write(fd, buf, len as usize)
}

/// `readv`/`writev`: one `read`/`write` per non-empty `iovec`, stopping at the
/// first short transfer (continuing would splice the stream) and reporting
/// progress over a later error. At most 1024 entries (`IOV_MAX`).
///
/// Public so a kernel whose `read`/`write` serve devices of its own can pass
/// those as `one` and keep this loop.
#[must_use]
pub fn vectored(fd: u64, iov: u64, cnt: u64, one: fn(u64, u64, u64) -> u64) -> u64 {
    let mut total = 0u64;
    for i in 0..cnt.min(1024) {
        let mut entry = [0u64; 2];
        if akuma_user_access::read_user_into(&mut entry, iov + i * 16).is_err() {
            return if total == 0 { EFAULT } else { total };
        }
        let [base, len] = entry;
        if len == 0 {
            continue;
        }
        let n = one(fd, base, len);
        if is_err(n) {
            return if total == 0 { n } else { total };
        }
        total += n;
        if n < len {
            break;
        }
    }
    total
}

/// Write glue's asm-generic `Stat` out as the x86_64 `struct stat`.
fn put_stat(buf: u64, g: &akuma_syscalls_linux::Stat) -> u64 {
    let x = akuma_syscalls_abi::stat::to_x86_64(g);
    if akuma_user_access::write_user_val(buf, &x).is_ok() { 0 } else { EFAULT }
}

/// `fstat(fd, buf)` in the x86_64 layout.
#[must_use]
pub fn fstat(fd: u64, buf: u64) -> u64 {
    match glue::fs::fstat_fill(fd as u32) {
        Ok(g) => put_stat(buf, &g),
        Err(e) => e,
    }
}

/// `newfstatat(dirfd, path, buf, flags)` in the x86_64 layout; `stat`/`lstat`
/// are this with `AT_FDCWD`. An empty path is `fstat(dirfd)` under
/// `AT_EMPTY_PATH`, `ENOENT` otherwise.
#[must_use]
pub fn newfstatat(dirfd: u64, path: u64, buf: u64, flags: u64) -> u64 {
    let Ok(raw) = glue::copy_from_user_str(path, 4096) else {
        return EFAULT;
    };
    if raw.is_empty() {
        return if flags & AT_EMPTY_PATH != 0 { fstat(dirfd, buf) } else { ENOENT };
    }
    match glue::fs::newfstatat_fill(dirfd as i32, &raw, flags as u32) {
        Ok(g) => put_stat(buf, &g),
        Err(e) => e,
    }
}

/// `fcntl(fd, cmd, arg)`: glue's arm.
///
/// `cmd` and `arg` need no translation: the command numbers agree and the only
/// flag bit either side reads (`O_NONBLOCK`) is not a permuted one. The memfd
/// seal commands are accepted and not enforced (Firecracker seals its ring and
/// only checks the call succeeded).
#[must_use]
pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> u64 {
    const F_ADD_SEALS: u64 = 1033;
    const F_GET_SEALS: u64 = 1034;
    if cmd == F_ADD_SEALS || cmd == F_GET_SEALS {
        let open = akuma_exec::process::current_process_shared().and_then(|p| p.with_fd(fd as u32, |_| ()));
        return if open.is_some() { 0 } else { EBADF };
    }
    glue::fs::sys_fcntl(fd as u32, cmd as u32, arg)
}

/// `dup2(old, new)`: `old == new` returns `new` without closing it when `old`
/// is open (where `dup3` answers `EINVAL`); otherwise glue's `dup3` with no flags.
#[must_use]
pub fn dup2(old: u64, new: u64) -> u64 {
    if old == new {
        let open = akuma_exec::process::current_process_shared().and_then(|p| p.with_fd(old as u32, |_| ()));
        return if open.is_some() { new } else { EBADF };
    }
    glue::fs::sys_dup3(old as u32, new as u32, 0)
}

/// `poll(fds, nfds, timeout_ms)`: a negative timeout waits forever.
#[must_use]
pub fn poll(fds: u64, nfds: u64, timeout_ms: u64) -> u64 {
    let timeout = match timeout_ms as i32 {
        ms if ms < 0 => None,
        ms => Some((ms as u64).saturating_mul(1_000)),
    };
    glue::flat(glue::poll::ppoll_timeout_us(fds, nfds as usize, timeout))
}

/// `select(nfds, r, w, e, timeval*)`: a NULL `timeval` waits forever; a
/// negative or out-of-range one is `EINVAL`.
#[must_use]
pub fn select(nfds: u64, readfds: u64, writefds: u64, exceptfds: u64, timeout: u64) -> u64 {
    let deadline = if timeout == 0 {
        None
    } else {
        let mut tv = [0i64; 2];
        if akuma_user_access::read_user_into(&mut tv, timeout).is_err() {
            return EFAULT;
        }
        let [sec, usec] = tv;
        match timeval_us(sec, usec) {
            Some(us) => Some(us),
            None => return EINVAL,
        }
    };
    glue::flat(glue::poll::pselect6_timeout_us(nfds as usize, readfds, writefds, exceptfds, deadline))
}

/// `gettimeofday(tv, tz)`: the wall clock glue's `clock_gettime(CLOCK_REALTIME)`
/// reads (boot-relative until something anchors it). `tz` is never filled; every
/// real caller passes NULL.
#[must_use]
pub fn gettimeofday(tv: u64) -> u64 {
    if tv == 0 {
        return 0;
    }
    let us = akuma_primitives::clock::realtime_us(akuma_primitives::clock::uptime_us());
    let tv_val = akuma_syscalls_linux::time::Timeval::from_bits(us / 1_000_000, us % 1_000_000);
    if akuma_user_access::write_user_val(tv, &tv_val).is_ok() { 0 } else { EFAULT }
}

/// `time(tloc)`: wall-clock seconds, also written through `tloc` when it is not NULL.
#[must_use]
pub fn time(tloc: u64) -> u64 {
    let secs = akuma_primitives::clock::realtime_us(akuma_primitives::clock::uptime_us()) / 1_000_000;
    if tloc != 0 && akuma_user_access::write_user_val(tloc, &secs).is_err() {
        return EFAULT;
    }
    secs
}

/// A `struct timeval` as microseconds, or `None` when it is not a valid one.
const fn timeval_us(sec: i64, usec: i64) -> Option<u64> {
    if sec < 0 || usec < 0 || usec >= 1_000_000 {
        return None;
    }
    Some((sec as u64).saturating_mul(1_000_000).saturating_add(usec as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeval_rejects_what_linux_rejects() {
        assert_eq!(timeval_us(0, 0), Some(0));
        assert_eq!(timeval_us(2, 500_000), Some(2_500_000));
        assert_eq!(timeval_us(-1, 0), None);
        assert_eq!(timeval_us(0, -1), None);
        assert_eq!(timeval_us(0, 1_000_000), None);
        assert_eq!(timeval_us(i64::MAX, 999_999), Some(u64::MAX));
    }

    #[test]
    fn chunked_read_stops_at_the_first_short_chunk_and_keeps_progress_over_an_error() {
        let mut calls = alloc::vec::Vec::new();
        let r = read_chunked(3 * MAX_IO, |done, n| {
            calls.push((done, n));
            if done == 0 { n as u64 } else { 100 }
        });
        assert_eq!(r, MAX_IO + 100);
        assert_eq!(calls, [(0, MAX_IO as usize), (MAX_IO, MAX_IO as usize)]);

        let r = read_chunked(3 * MAX_IO, |done, n| if done == 0 { n as u64 } else { EFAULT });
        assert_eq!(r, MAX_IO, "progress beats a later error");
        assert_eq!(read_chunked(10, |_, _| EBADF), EBADF, "an error with no progress is the error");
    }

    #[test]
    fn chunked_read_caps_at_max_rw_count() {
        let mut total = 0u64;
        let r = read_chunked(u64::MAX, |_, n| {
            total += n as u64;
            n as u64
        });
        assert_eq!(r, MAX_RW_COUNT);
        assert_eq!(total, MAX_RW_COUNT);
    }

    #[test]
    fn errno_form_is_the_negative_range() {
        assert!(is_err(EBADF) && is_err(EFAULT) && is_err((-4095i64) as u64));
        assert!(!is_err(0) && !is_err(MAX_RW_COUNT) && !is_err((-4096i64) as u64));
    }

    #[test]
    fn legacy_numbers_are_not_in_the_asm_generic_table() {
        // dispatch()'s first match owns these; none may also have a Syscall row,
        // or the order of the two matches would decide which arm serves it.
        for nr in [2u64, 4, 6, 7, 21, 22, 23, 33, 85, 96, 201, 213, 232] {
            assert!(Syscall::from_x86_64(nr).is_none(), "x86_64 {nr} has a Syscall row");
        }
    }
}
