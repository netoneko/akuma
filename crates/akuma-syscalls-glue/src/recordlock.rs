//! `fcntl(F_SETLK / F_SETLKW / F_GETLK)` — POSIX advisory record locks.
//!
//! The table and its rules are [`akuma_reclock`] (host-tested); this file is the
//! acting half: the `struct flock` copies, resolving `l_whence`, the lock, and
//! the `F_SETLKW` wait.
//!
//! Until 2026-10-03 these commands answered success unconditionally, so nothing
//! excluded anything, and SQLite's WAL index (serialised on byte-range locks in
//! the `-shm` file) was written by several goose processes at once —
//! `database disk image is malformed`.
//!
//! **Owner** is the fd table's identity, the same `holder` [`flock`](super::flock)
//! uses: `CLONE_FILES` threads share it (POSIX: locks belong to the process), a
//! `fork` child has its own table and inherits nothing. **Release** rides
//! [`flock::flock_release`](super::flock::flock_release), which every fd-teardown
//! path already calls, so closing any descriptor for a file drops the process's
//! locks on it — POSIX's rule — and process exit drops the rest.
//!
//! Keyed by path, like `flock`; two paths to one inode do not contend. Not done:
//! deadlock detection (`EDEADLK`), `F_OFD_*`, `SEEK_END`.

use super::*;
use akuma_reclock::{Kind, Table};
use alloc::sync::Arc;

static TABLE: Spinlock<Table> = Spinlock::new(Table::new());

const F_GETLK: u32 = 5;
const F_SETLK: u32 = 6;
// F_SETLKW (7) is "anything that is not F_SETLK or F_GETLK" below.
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;
const SEEK_SET: i16 = 0;
const SEEK_CUR: i16 = 1;

/// `struct flock` — identical on x86_64 and AArch64 (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Flock {
    l_type: i16,
    l_whence: i16,
    _pad0: i32,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
    _pad1: i32,
}

/// Drop `holder`'s locks on `path` — called from `flock_release`.
pub(super) fn release_file(path: &str, holder: usize) {
    akuma_primitives::irq::with_irqs_disabled(|| TABLE.lock().release_file(path, holder as u64));
}

/// Handle one of the three commands. `fd` is already known to be open.
pub(super) fn sys_fcntl_lock(fd: u32, cmd: u32, arg: u64) -> u64 {
    let Some(proc) = akuma_exec::process::current_process_shared() else {
        return ESRCH;
    };
    let (path, position) = match proc.get_fd(fd) {
        Some(akuma_exec::process::FileDescriptor::File(f)) => (f.path, f.position as i64),
        // Locks on pipes, sockets and devices are meaningless; accept as before.
        Some(_) => return if cmd == F_GETLK { write_unlocked(arg) } else { 0 },
        None => return EBADF,
    };
    if arg == 0 {
        return EFAULT;
    }
    let mut fl = Flock::default();
    if copy_from_user(as_user_bytes_mut(core::slice::from_mut(&mut fl)), arg).is_err() {
        return EFAULT;
    }
    let base = match fl.l_whence {
        SEEK_SET => 0,
        SEEK_CUR => position,
        _ => return EINVAL, // SEEK_END: not modelled
    };
    let Some(start) = fl.l_start.checked_add(base) else { return EINVAL };
    let Some((s, e)) = akuma_reclock::range(start, fl.l_len) else {
        // `l_len == 0` at a negative resolved start, or an empty range.
        return EINVAL;
    };
    let kind = match fl.l_type {
        F_RDLCK => Kind::Read,
        F_WRLCK => Kind::Write,
        F_UNLCK => {
            if cmd == F_GETLK {
                return EINVAL;
            }
            let owner = Arc::as_ptr(&proc.fds) as u64;
            akuma_primitives::irq::with_irqs_disabled(|| TABLE.lock().unlock(&path, owner, s, e));
            return 0;
        }
        _ => return EINVAL,
    };
    let owner = Arc::as_ptr(&proc.fds) as u64;
    let pid = proc.tgid;

    if cmd == F_GETLK {
        let blocker = akuma_primitives::irq::with_irqs_disabled(|| TABLE.lock().conflict(&path, owner, kind, s, e));
        let out = match blocker {
            None => Flock { l_type: F_UNLCK, ..fl },
            Some(b) => Flock {
                l_type: if b.kind == Kind::Write { F_WRLCK } else { F_RDLCK },
                l_whence: SEEK_SET,
                l_start: b.start as i64,
                l_len: if b.end == u64::MAX { 0 } else { (b.end - b.start) as i64 },
                l_pid: b.pid as i32,
                ..fl
            },
        };
        return if write_user_val(arg, &out).is_ok() { 0 } else { EFAULT };
    }

    loop {
        let r = akuma_primitives::irq::with_irqs_disabled(|| TABLE.lock().try_lock(&path, owner, pid, kind, s, e));
        if r.is_ok() {
            return 0;
        }
        if cmd == F_SETLK {
            return EAGAIN; // POSIX allows EACCES too; Linux says EAGAIN
        }
        if akuma_exec::process::should_interrupt_blocking_syscall() {
            return EINTR;
        }
        // No waiter list, same trade `flock` makes: unlock wakes nobody, so
        // re-poll on a short cadence. SQLite's contended path is rare.
        let now = akuma_primitives::clock::uptime_us();
        akuma_exec::threading::schedule_blocking(now + 5_000);
    }
}

/// `F_GETLK` on a non-file: nothing can hold a lock, say so.
fn write_unlocked(arg: u64) -> u64 {
    if arg != 0 && write_user_val(arg, &F_UNLCK).is_ok() { 0 } else { EFAULT }
}
